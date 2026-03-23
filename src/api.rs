use crate::generator::MarketGenerator;
use actix_web::{web, HttpResponse, Responder};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

type AppState = web::Data<Arc<Mutex<HashMap<String, MarketGenerator>>>>;

#[derive(Serialize)]
struct HealthResponse {
    status: String,
    uptime_note: String,
}

#[derive(Serialize)]
struct SymbolsResponse {
    symbols: Vec<String>,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

/// GET /api/v1/health
pub async fn health() -> impl Responder {
    HttpResponse::Ok().json(HealthResponse {
        status: "ok".to_string(),
        uptime_note: "FPGA HFT Data Generator running".to_string(),
    })
}

/// GET /api/v1/symbols
pub async fn symbols(state: AppState) -> impl Responder {
    let gens = state.lock().unwrap();
    let symbols: Vec<String> = gens.keys().cloned().collect();
    HttpResponse::Ok().json(SymbolsResponse { symbols })
}

/// GET /api/v1/tick/{symbol}
/// Symbol path uses dash separator, e.g. SOL-USDC → SOL/USDC
pub async fn tick(state: AppState, path: web::Path<String>) -> impl Responder {
    let raw_symbol = path.into_inner();
    let symbol = raw_symbol.replace('-', "/");

    let gens = state.lock().unwrap();
    match gens.get(&symbol) {
        Some(generator) => match generator.latest_tick() {
            Some(tick) => HttpResponse::Ok().json(tick),
            None => HttpResponse::ServiceUnavailable().json(ErrorResponse {
                error: format!("No ticks generated yet for {}", symbol),
            }),
        },
        None => HttpResponse::NotFound().json(ErrorResponse {
            error: format!("Symbol '{}' not found", symbol),
        }),
    }
}

/// GET /api/v1/bbo/{symbol}
pub async fn bbo(state: AppState, path: web::Path<String>) -> impl Responder {
    let raw_symbol = path.into_inner();
    let symbol = raw_symbol.replace('-', "/");

    let gens = state.lock().unwrap();
    match gens.get(&symbol) {
        Some(generator) => HttpResponse::Ok().json(generator.current_bbo()),
        None => HttpResponse::NotFound().json(ErrorResponse {
            error: format!("Symbol '{}' not found", symbol),
        }),
    }
}

/// GET /api/v1/ohlcv/{symbol}?minutes=N
#[derive(serde::Deserialize)]
pub struct OhlcvQuery {
    minutes: Option<usize>,
}

pub async fn ohlcv(
    state: AppState,
    path: web::Path<String>,
    query: web::Query<OhlcvQuery>,
) -> impl Responder {
    let raw_symbol = path.into_inner();
    let symbol = raw_symbol.replace('-', "/");
    let minutes = query.minutes.unwrap_or(60);

    let gens = state.lock().unwrap();
    match gens.get(&symbol) {
        Some(generator) => {
            let candles = generator.build_ohlcv(minutes);
            HttpResponse::Ok().json(candles)
        }
        None => HttpResponse::NotFound().json(ErrorResponse {
            error: format!("Symbol '{}' not found", symbol),
        }),
    }
}
