use crate::generator::MarketGenerator;
use actix_web::{HttpResponse, Responder, web};
use dashmap::DashMap;
use prometheus::{Encoder, TextEncoder};
use serde::Serialize;
use std::sync::Arc;

type AppState = web::Data<Arc<DashMap<String, MarketGenerator>>>;

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
    let symbols: Vec<String> = state.iter().map(|r| r.key().clone()).collect();
    HttpResponse::Ok().json(SymbolsResponse { symbols })
}

/// GET /api/v1/tick/{symbol}
/// Symbol path uses dash separator, e.g. SOL-USDC → SOL/USDC
pub async fn tick(state: AppState, path: web::Path<String>) -> impl Responder {
    let symbol = raw_symbol_to_symbol(path.into_inner());

    match state.get(&symbol) {
        Some(item) => {
            let generator = item.value();
            match generator.latest_tick() {
                Some(tick) => HttpResponse::Ok().json(tick),
                None => HttpResponse::ServiceUnavailable().json(ErrorResponse {
                    error: format!("No ticks generated yet for {}", symbol),
                }),
            }
        }
        None => HttpResponse::NotFound().json(ErrorResponse {
            error: format!("Symbol '{}' not found", symbol),
        }),
    }
}

/// GET /api/v1/bbo/{symbol}
pub async fn bbo(state: AppState, path: web::Path<String>) -> impl Responder {
    let symbol = raw_symbol_to_symbol(path.into_inner());

    match state.get(&symbol) {
        Some(item) => HttpResponse::Ok().json(item.value().current_bbo()),
        None => HttpResponse::NotFound().json(ErrorResponse {
            error: format!("Symbol '{}' not found", symbol),
        }),
    }
}

/// GET /api/v1/ohlcv/{symbol}?minutes=N&interval=1m|5m|15m|1h|1d
#[derive(serde::Deserialize)]
pub struct OhlcvQuery {
    minutes: Option<usize>,
    interval: Option<String>,
}

pub async fn ohlcv(
    state: AppState,
    path: web::Path<String>,
    client: web::Data<clickhouse::Client>,
    query: web::Query<OhlcvQuery>,
) -> impl Responder {
    let symbol = raw_symbol_to_symbol(path.into_inner());
    let minutes = query.minutes.unwrap_or(60).clamp(1, 10_080);
    let interval = query.interval.as_deref().unwrap_or("1m");

    let candles = crate::db::get_historical_ohlc(&client, &symbol, minutes, interval).await;

    if candles.is_empty() {
        return match state.get(&symbol) {
            Some(item) => {
                let candles = item.value().build_ohlcv(minutes);
                HttpResponse::Ok().json(candles)
            }
            None => HttpResponse::NotFound().json(ErrorResponse {
                error: format!("Symbol '{}' not found", symbol),
            }),
        };
    }

    HttpResponse::Ok().json(candles)
}

pub async fn prometheus_metrics() -> impl Responder {
    let encoder = TextEncoder::new();
    let mut buffer = Vec::new();
    let metric_families = crate::db::REGISTRY.gather();
    if let Err(e) = encoder.encode(&metric_families, &mut buffer) {
        return HttpResponse::InternalServerError().json(ErrorResponse {
            error: format!("Failed to encode metrics: {}", e),
        });
    }

    HttpResponse::Ok()
        .content_type("text/plain; version=0.0.4")
        .body(buffer)
}

fn raw_symbol_to_symbol(raw_symbol: String) -> String {
    raw_symbol.replace('-', "/")
}
