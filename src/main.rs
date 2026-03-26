pub mod api;
pub mod config;
pub mod db;
pub mod generator;
pub mod models;
pub mod ws;

use crate::config::AppConfig;
use crate::db::{OhlcvRow, TradeRow, create_clickhouse_client};
use actix_cors::Cors;
use actix_governor::{Governor, GovernorConfigBuilder};
use actix_web::{App, HttpServer, web};
use dashmap::DashMap;
use generator::MarketGenerator;
use std::sync::Arc;
use std::time::Instant;
use tokio::join;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    db::register_metrics();

    let cfg = AppConfig::load();
    let bind_addr = format!("0.0.0.0:{}", cfg.port);

    info!("Bull Tech — FPGA HFT Data Generator");
    info!("   Starting server on http://{}", bind_addr);

    let ch_client = create_clickhouse_client().await;
    db::backfill_missing_candles(&ch_client).await;

    // Initialize one generator per configured symbol
    let generators: DashMap<String, MarketGenerator> = DashMap::new();
    for sym in &cfg.symbols {
        generators.insert(
            sym.name.clone(),
            MarketGenerator::new(
                &sym.name,
                sym.initial_price,
                sym.drift,
                sym.volatility,
                sym.spread,
                cfg.tick_history_size,
            ),
        );
    }

    // DB inserter channel
    let (db_tx, db_rx) =
        tokio::sync::mpsc::channel::<db::InserterPayload>(cfg.db_insert_buffer_size);
    let inserter_client = ch_client.clone();
    let flush_secs = cfg.db_flush_interval_secs;
    let buf_size = cfg.db_insert_buffer_size;
    let inserter_handle = tokio::spawn(async move {
        db::run_unified_lazy_inserter(db_rx, inserter_client, flush_secs, buf_size).await;
    });

    let state = web::Data::new(Arc::new(generators));

    let token = CancellationToken::new();
    let tick_token = token.clone();

    let (tx, _rx) = broadcast::channel::<String>(cfg.ws_broadcast_capacity);

    let bg_state = state.clone();
    let bg_broadcast_tx = tx.clone();
    let broadcast_tx = web::Data::new(tx.clone());
    let bg_db_tx = db_tx.clone();

    let shutdown_tx = db_tx.clone();
    let shutdown_token = token.clone();

    let tick_interval_ms = cfg.tick_interval_ms;
    let nominal_tps = (1000.0 / tick_interval_ms as f64) as u32;

    // Spawn background tick generation task
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(tokio::time::Duration::from_millis(tick_interval_ms));
        loop {
            tokio::select! {
                _ = tick_token.cancelled() => {
                    info!("Tick generation task received shutdown signal.");
                    break;
                }
                _ = interval.tick() => {
                    db::TICK_COUNTER.inc();

                    for mut item in bg_state.iter_mut() {
                        let tick_start = Instant::now();

                        let generator = item.value_mut();
                        let tick = generator.advance();

                        let tick_latency_ms = tick_start.elapsed().as_secs_f64() * 1000.0;
                        db::TICK_LATENCY_HISTOGRAM.observe(tick_latency_ms);

                        db::PRICE_GAUGE
                            .with_label_values(&[&generator.symbol])
                            .set(generator.current_price());

                        let _ = bg_db_tx.try_send(db::InserterPayload::Trade(TradeRow::from(&tick)));

                        let ws_msg = generator.to_ws_message(tick_latency_ms, nominal_tps);
                        if let Ok(json) = serde_json::to_string(&ws_msg) {
                            let _ = bg_broadcast_tx.send(json);
                        }

                        if let Some(bar) = generator.check_candle_closure() {
                            let _ = bg_db_tx
                                .try_send(db::InserterPayload::Ohlcv(OhlcvRow::from(&bar)));
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
    let poll_interval_secs = cfg.change_poll_interval_secs;

    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(tokio::time::Duration::from_secs(poll_interval_secs));
        loop {
            tokio::select! {
                _ = poll_token.cancelled() => {
                    info!("24h Poller task shutting down.");
                    break;
                }
                _ = interval.tick() => {
                    let symbols: Vec<String> =
                        poll_state.iter().map(|r| r.key().clone()).collect();

                    for symbol in symbols {
                        let (price_1h, price_24h) = join!(
                            db::poll_1h_change(&poller_client, &symbol),
                            db::poll_24h_change(&poller_client, &symbol)
                        );

                        if price_1h.is_some() || price_24h.is_some() {
                            if let Some(mut item) = poll_state.get_mut(&symbol) {
                                let generator = item.value_mut();
                                let current = generator.current_price();

                                if let Some(old) = price_1h {
                                    generator.change_1h = Some(((current - old) / old) * 100.0);
                                }

                                if let Some(old) = price_24h {
                                    generator.change_24h = Some(((current - old) / old) * 100.0);
                                }
                            }
                        }
                    }
                }
            }
        }
    });

    info!("   ─────────────────────────────────────");
    for sym in &cfg.symbols {
        info!("   Symbol: {}", sym.name);
    }
    info!("   Tick rate: {}/sec ({}ms)", nominal_tps, tick_interval_ms);
    info!("   REST API:  http://localhost:{}/api/v1/", cfg.port);
    info!("   WebSocket: ws://localhost:{}/v1/feed", cfg.port);
    info!("   ─────────────────────────────────────");

    let server_client = ch_client.clone();

    // Token-bucket rate limiter per source IP
    let governor_conf = GovernorConfigBuilder::default()
        .per_second(cfg.rate_limit_per_second)
        .burst_size(cfg.rate_limit_burst)
        .finish()
        .expect("invalid rate limiter configuration");

    let ws_cfg = web::Data::new(ws::WsConfig {
        heartbeat_timeout_secs: cfg.ws_heartbeat_timeout_secs,
        ping_interval_secs: cfg.ws_ping_interval_secs,
    });

    let server = HttpServer::new(move || {
        let cors = Cors::default()
            .allow_any_origin()
            .allow_any_method()
            .allow_any_header()
            .max_age(3600);

        let app_client = server_client.clone();
        App::new()
            .wrap(cors)
            .wrap(Governor::new(&governor_conf))
            .app_data(web::Data::new(app_client.clone()))
            .app_data(state.clone())
            .app_data(broadcast_tx.clone())
            .app_data(ws_cfg.clone())
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
    .workers(2)
    .bind(&bind_addr)?
    .run();

    let server_handle = server.handle();
    tokio::spawn(async move {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to listen for Ctrl+C");
        info!("\nCtrl+C detected! Starting graceful shutdown...");
        shutdown_token.cancel();
        server_handle.stop(true).await;
        drop(shutdown_tx);
    });

    server.await?;
    drop(db_tx);

    info!("Waiting for database inserter to finish...");
    let _ = inserter_handle.await;

    info!("Shutdown complete.");
    Ok(())
}
