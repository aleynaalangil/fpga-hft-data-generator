use rust_decimal::Decimal;
use serde::Serialize;

/// A single raw trade tick — aligned with the ClickHouse `historical_trades` schema.
/// Uses `rust_decimal::Decimal` for price/amount to match Decimal64(8) in the DB.
#[derive(Debug, Clone, Serialize)]
pub struct MarketTick {
    pub symbol: String,
    pub side: String,
    pub price: Decimal,
    pub amount: Decimal,
    pub timestamp: String,
    pub order_id: String,
    pub trader_id: u32,
}

/// Best Bid/Offer snapshot using u64 fixed-point representation (value × 10⁸),
/// which is the standard representation in HFT systems for zero-copy price comparison.
#[derive(Debug, Clone, Serialize)]
pub struct BboSnapshot {
    pub symbol: String,
    pub best_bid: u64,
    pub best_ask: u64,
    pub bid_size: u64,
    pub ask_size: u64,
    pub spread: u64,
    pub timestamp: String,
}

/// Pre-computed 1-minute OHLCV candle — mirrors the ClickHouse materialized view.
#[derive(Debug, Clone, Serialize)]
pub struct OhlcvBar {
    pub symbol: String,
    pub candle_time: String,
    pub open: Decimal,
    pub high: Decimal,
    pub low: Decimal,
    pub close: Decimal,
    pub volume: Decimal,
}

/// The WebSocket message envelope — matches the frontend Zustand store shape
/// `{ price, volume, symbol }`. Uses f64 because JSON/JavaScript has no native Decimal.
#[derive(Debug, Clone, Serialize)]
pub struct MarketDataMessage {
    pub price: f64,
    pub volume: f64,
    pub symbol: String,
}

/// Fixed-point scaling factor: 10^8 (matching ClickHouse Decimal64(8) precision)
pub const FIXED_POINT_SCALE: u64 = 100_000_000;

/// Convert an f64 price to a u64 fixed-point representation.
pub fn to_fixed_point(value: f64) -> u64 {
    (value * FIXED_POINT_SCALE as f64) as u64
}
