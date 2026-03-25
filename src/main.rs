pub mod api;
pub mod db;
pub mod generator;
pub mod models;
pub mod ws;

use crate::db::{OhlcvRow, TradeRow, create_clickhouse_client};
use actix_cors::Cors;
use actix_web::{App, HttpServer, web};
use dashmap::DashMap;
use generator::MarketGenerator;
use std::sync::Arc;
use tokio::join;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt()
        // This looks for RUST_LOG, but defaults to 'info' if not found
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    db::register_metrics();
    let port = std::env::var("PORT").unwrap_or_else(|_| "8080".to_string());
    let bind_addr = format!("0.0.0.0:{}", port);

    info!("🐂 Bull Tech — FPGA HFT Data Generator");
    info!("   Starting server on http://{}", bind_addr);

    let ch_client = create_clickhouse_client().await;

    // Initialize generators for each symbol
    let generators = DashMap::new();
    generators.insert(
        "SOL/USDC".to_string(),
        MarketGenerator::new("SOL/USDC", 150.0, 0.0001, 0.002, 0.01),
    );
    generators.insert(
        "BTC/USDC".to_string(),
        MarketGenerator::new("BTC/USDC", 65_000.0, 0.0001, 0.001, 10.0),
    );

    // Setup DB Inserter Channels (buffered)
    let (db_tx, db_rx) = tokio::sync::mpsc::channel::<db::InserterPayload>(100_000);
    let inserter_client = ch_client.clone();
    let inserter_handle = tokio::spawn(async move {
        db::run_unified_lazy_inserter(db_rx, inserter_client).await;
    });

    let state = web::Data::new(Arc::new(generators));

    let token = CancellationToken::new();
    let tick_token = token.clone();

    let (tx, _rx) = broadcast::channel::<String>(1024);

    // Spawn background tick generation task
    let bg_state = state.clone();
    let bg_broadcast_tx = tx.clone();
    let broadcast_tx = web::Data::new(tx.clone());
    let bg_db_tx = db_tx.clone();

    let shutdown_tx = db_tx.clone();
    let shutdown_token = token.clone();

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(10));
        loop {
            tokio::select! {
                _ = tick_token.cancelled()=> {
                    info!("📉 Tick generation task received shutdown signal.");
                    break;
                }
                _ = interval.tick()=> {
                    db::TICK_COUNTER.inc();
                    //For my current setup with 2–10 symbols, a single loop is actually faster due to lower context-switching overhead. However, if I was planning to scale, these actions were needed to be taken:
                    // Current (Single Loop): Best for < 50 symbols. It keeps all "ticks" perfectly synchronized in time.
                    // Future (Multi-Spawn): If I scale to 500+ symbols, I should tokio::spawn a separate task for each MarketGenerator. This allows the OS to spread the load across all CPU cores.
                    // !!!: If I do this, I'll need to move the broadcast::Sender into each task so they can all broadcast their own JSON independently.
                    for mut item in bg_state.iter_mut() {
                        let generator = item.value_mut(); // This gives you &mut MarketGenerator
                        let tick = generator.advance();

                        db::PRICE_GAUGE.with_label_values(&[&generator.symbol]).set(generator.current_price());

                        let _ = bg_db_tx.try_send(db::InserterPayload::Trade(TradeRow::from(&tick)));

                        let ws_msg = generator.to_ws_message();
                        if let Ok(json) = serde_json::to_string(&ws_msg) {
                            let _ = bg_broadcast_tx.send(json);
                        }
                        if let Some(bar) = generator.check_candle_closure() {
                            let _ = bg_db_tx.try_send(db::InserterPayload::Ohlcv(OhlcvRow::from(&bar)));
                        }
                    }
                }
            }
        }
    });

    // Spawn 24h change poller task
    let poll_state = state.clone();
    let poller_client = ch_client.clone();
    let poll_token = token.clone();

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(5));
        loop {
            tokio::select! {
                _= poll_token.cancelled() =>{
                    info!("📉 24h Poller task shutting down.");
                    break;
                }
                _ = interval.tick() => {
                    let symbols: Vec<String> = poll_state.iter().map(|r| r.key().clone()).collect();

                    for symbol in symbols {
                        // 1. Run both database queries concurrently
                        // This returns a tuple of the results once BOTH are finished.
                        let (price_1h, price_24h) = join!(
                        db::poll_1h_change(&poller_client, &symbol),
                        db::poll_24h_change(&poller_client, &symbol)
                        );

                        // 2. Lock the state once to apply any results we got
                        if (price_1h.is_some() || price_24h.is_some())
                            && let Some(mut item) = poll_state.get_mut(&symbol)
                        {
                            let generator = item.value_mut();
                            let current = generator.current_price();

                            // Update 1h if it exists
                            if let Some(old) = price_1h {
                                generator.change_1h = Some(((current - old) / old) * 100.0);
                            }

                            // Update 24h if it exists
                            if let Some(old) = price_24h {
                                generator.change_24h = Some(((current - old) / old) * 100.0);
                            }
                        }
                    }
                }
            }
        }
    });
    info!("   ─────────────────────────────────────");
    info!("   Symbols: SOL/USDC, BTC/USDC");
    info!("   Tick rate: 100/sec (10ms)");
    info!("   REST API: http://localhost:8080/api/v1/");
    info!("   WebSocket: ws://localhost:8080/v1/feed");
    info!("   ─────────────────────────────────────");
    let server_client = ch_client.clone();

    let server = HttpServer::new(move || {
        let cors = Cors::default()
            .allow_any_origin()
            .allow_any_method()
            .allow_any_header()
            .max_age(3600);

        let app_client = server_client.clone();
        App::new()
            .wrap(cors)
            .app_data(web::Data::new(app_client.clone()))
            .app_data(state.clone())
            .app_data(broadcast_tx.clone())
            // REST endpoints
            .route("/api/v1/health", web::get().to(api::health))
            .route("/api/v1/symbols", web::get().to(api::symbols))
            .route("/api/v1/tick/{symbol}", web::get().to(api::tick))
            .route("/api/v1/bbo/{symbol}", web::get().to(api::bbo))
            .route("/api/v1/ohlcv/{symbol}", web::get().to(api::ohlcv))
            .route("/api/v1/metrics", web::get().to(api::prometheus_metrics))
            // WebSocket endpoint
            .route("/v1/feed", web::get().to(ws::feed))
    })
    .bind(&bind_addr)?
    .run();

    let server_handle = server.handle();
    tokio::spawn(async move {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to listen for Ctrl+C");
        info!("\n🛑 Ctrl+C detected! Starting graceful shutdown...");
        shutdown_token.cancel();
        server_handle.stop(true).await;
        drop(shutdown_tx)
    });
    server.await?;
    drop(db_tx);

    info!("⏳ Waiting for database inserter to finish...");
    let _ = inserter_handle.await;

    info!("🚀 Shutdown complete.");
    Ok(())
}
