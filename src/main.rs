pub mod models;
pub mod generator;
pub mod api;
pub mod ws;

use actix_cors::Cors;
use actix_web::{web, App, HttpServer};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use generator::MarketGenerator;

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let port = std::env::var("PORT").unwrap_or_else(|_| "8080".to_string());
    let bind_addr = format!("0.0.0.0:{}", port);

    println!("🐂 Bull Tech — FPGA HFT Data Generator");
    println!("   Starting server on http://{}", bind_addr);

    // Initialize generators for each symbol
    let mut generators = HashMap::new();
    generators.insert(
        "SOL/USDC".to_string(),
        MarketGenerator::new("SOL/USDC", 150.0, 0.0001, 0.002, 0.01),
    );
    generators.insert(
        "BTC/USDC".to_string(),
        MarketGenerator::new("BTC/USDC", 65_000.0, 0.0001, 0.001, 10.0),
    );

    let state = web::Data::new(Arc::new(Mutex::new(generators)));

    // Spawn background tick generation task
    let bg_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(10));
        loop {
            interval.tick().await;
            if let Ok(mut gens) = bg_state.lock() {
                for generator in gens.values_mut() {
                    generator.advance();
                }
            }
        }
    });

    println!("   Symbols: SOL/USDC, BTC/USDC");
    println!("   Tick rate: 100/sec (10ms)");
    println!("   REST API: http://localhost:8080/api/v1/");
    println!("   WebSocket: ws://localhost:8080/v1/feed");
    println!("   ─────────────────────────────────────");

    HttpServer::new(move || {
        let cors = Cors::default()
            .allow_any_origin()
            .allow_any_method()
            .allow_any_header()
            .max_age(3600);

        App::new()
            .wrap(cors)
            .app_data(state.clone())
            // REST endpoints
            .route("/api/v1/health", web::get().to(api::health))
            .route("/api/v1/symbols", web::get().to(api::symbols))
            .route("/api/v1/tick/{symbol}", web::get().to(api::tick))
            .route("/api/v1/bbo/{symbol}", web::get().to(api::bbo))
            .route("/api/v1/ohlcv/{symbol}", web::get().to(api::ohlcv))
            // WebSocket endpoint
            .route("/v1/feed", web::get().to(ws::feed))
    })
    .bind(&bind_addr)?
    .run()
    .await
}
