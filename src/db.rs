use crate::models::{MarketTick, OhlcvBar};
use clickhouse::Client;
use lazy_static::lazy_static;
use prometheus::{
    opts, Gauge, GaugeVec, Histogram, HistogramOpts, IntCounter, IntCounterVec, Opts, Registry,
};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::Receiver;
use tracing::{error, info};

pub enum InserterPayload {
    Trade(TradeRow),
    Ohlcv(OhlcvRow),
    Flush,
}

lazy_static! {
    pub static ref REGISTRY: Registry = Registry::new();

    // --- existing metrics ---
    pub static ref TICK_COUNTER: IntCounter =
        IntCounter::new("hft_ticks_total", "Total number of market ticks generated")
            .expect("metric can be created");

    pub static ref PRICE_GAUGE: GaugeVec = GaugeVec::new(
        opts!("hft_asset_price", "Current price of the asset"),
        &["symbol"]
    )
    .expect("metric can be created");

    // --- tick generation latency histogram ---
    pub static ref TICK_LATENCY_HISTOGRAM: Histogram = Histogram::with_opts(
        HistogramOpts::new(
            "hft_tick_latency_ms",
            "Tick generation wall-clock time in milliseconds",
        )
        .buckets(vec![0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 25.0])
    )
    .expect("metric can be created");

    // --- active WebSocket client count ---
    pub static ref WS_CLIENT_GAUGE: Gauge =
        Gauge::with_opts(Opts::new("hft_ws_clients", "Active WebSocket connections"))
            .expect("metric can be created");

    // --- lagged WebSocket broadcast events ---
    pub static ref WS_LAGGED_COUNTER: IntCounter =
        IntCounter::new("hft_ws_lagged_total", "Broadcast lag events on WebSocket connections")
            .expect("metric can be created");

    // --- DB error counter keyed by operation ---
    pub static ref DB_ERROR_COUNTER: IntCounterVec = IntCounterVec::new(
        Opts::new("hft_db_errors_total", "ClickHouse operation error counts"),
        &["operation"]
    )
    .expect("metric can be created");

    // --- DB flush duration histogram ---
    pub static ref DB_FLUSH_DURATION: Histogram = Histogram::with_opts(
        HistogramOpts::new(
            "hft_db_flush_duration_ms",
            "ClickHouse batch flush duration in milliseconds",
        )
        .buckets(vec![1.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0])
    )
    .expect("metric can be created");
}

pub fn register_metrics() {
    REGISTRY
        .register(Box::new(TICK_COUNTER.clone()))
        .expect("TICK_COUNTER registration failed");
    REGISTRY
        .register(Box::new(PRICE_GAUGE.clone()))
        .expect("PRICE_GAUGE registration failed");
    REGISTRY
        .register(Box::new(TICK_LATENCY_HISTOGRAM.clone()))
        .expect("TICK_LATENCY_HISTOGRAM registration failed");
    REGISTRY
        .register(Box::new(WS_CLIENT_GAUGE.clone()))
        .expect("WS_CLIENT_GAUGE registration failed");
    REGISTRY
        .register(Box::new(WS_LAGGED_COUNTER.clone()))
        .expect("WS_LAGGED_COUNTER registration failed");
    REGISTRY
        .register(Box::new(DB_ERROR_COUNTER.clone()))
        .expect("DB_ERROR_COUNTER registration failed");
    REGISTRY
        .register(Box::new(DB_FLUSH_DURATION.clone()))
        .expect("DB_FLUSH_DURATION registration failed");
}

// ---------------------------------------------------------------------------
// Custom serde modules for ClickHouse binary protocol
// ---------------------------------------------------------------------------

pub mod ch_decimal {
    use super::*;
    use rust_decimal::prelude::ToPrimitive;
    use serde::Serializer;

    pub fn serialize<S>(decimal: &Decimal, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // ClickHouse Decimal64(8) native representation: i64 × 10^8
        let scaled = decimal * Decimal::new(100_000_000, 0);
        let val_i64 = scaled.to_i64().unwrap_or(0);
        serializer.serialize_i64(val_i64)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Decimal, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let val = i64::deserialize(deserializer)?;
        Ok(Decimal::new(val, 8))
    }
}

pub mod ch_datetime {
    use super::*;
    use chrono::TimeZone;
    use serde::Serializer;

    pub fn serialize<S>(time_str: &str, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // ClickHouse DateTime64(6): i64 microseconds since epoch
        let dt = chrono::DateTime::parse_from_rfc3339(&time_str.replace('Z', "+00:00"))
            .unwrap_or_else(|_| chrono::Utc::now().into());
        serializer.serialize_i64(dt.timestamp_micros())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<String, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let val = i64::deserialize(deserializer)?;
        // Auto-detect seconds vs microseconds by magnitude
        let dt = if val > 10_000_000_000 {
            chrono::Utc
                .timestamp_micros(val)
                .single()
                .unwrap_or_else(chrono::Utc::now)
        } else {
            chrono::Utc
                .timestamp_opt(val, 0)
                .single()
                .unwrap_or_else(chrono::Utc::now)
        };
        Ok(dt.to_rfc3339_opts(chrono::SecondsFormat::Micros, true))
    }
}

// ---------------------------------------------------------------------------
// Row types for ClickHouse
// ---------------------------------------------------------------------------

#[derive(clickhouse::Row, Serialize, Deserialize, Debug, Clone)]
pub struct TradeRow {
    pub symbol: String,
    pub side: i8, // 1=buy, 2=sell
    #[serde(with = "ch_decimal")]
    pub price: Decimal,
    #[serde(with = "ch_decimal")]
    pub amount: Decimal,
    #[serde(with = "ch_datetime")]
    pub timestamp: String,
    pub order_id: String,
    pub trader_id: u32,
}

#[derive(clickhouse::Row, Serialize, Deserialize, Debug, Clone)]
pub struct OhlcvRow {
    pub symbol: String,
    #[serde(with = "ch_datetime")]
    pub candle_time: String,
    #[serde(with = "ch_decimal")]
    pub open: Decimal,
    #[serde(with = "ch_decimal")]
    pub high: Decimal,
    #[serde(with = "ch_decimal")]
    pub low: Decimal,
    #[serde(with = "ch_decimal")]
    pub close: Decimal,
    #[serde(with = "ch_decimal")]
    pub volume: Decimal,
}

impl From<&OhlcvBar> for OhlcvRow {
    fn from(bar: &OhlcvBar) -> Self {
        Self {
            symbol: bar.symbol.clone(),
            candle_time: bar.candle_time.clone(),
            open: bar.open,
            high: bar.high,
            low: bar.low,
            close: bar.close,
            volume: bar.volume,
        }
    }
}

impl From<OhlcvRow> for OhlcvBar {
    fn from(row: OhlcvRow) -> Self {
        Self {
            symbol: row.symbol,
            candle_time: row.candle_time,
            open: row.open,
            high: row.high,
            low: row.low,
            close: row.close,
            volume: row.volume,
        }
    }
}

impl From<&MarketTick> for TradeRow {
    fn from(tick: &MarketTick) -> Self {
        let side = if tick.side == "buy" { 1 } else { 2 };
        Self {
            symbol: tick.symbol.clone(),
            side,
            price: tick.price,
            amount: tick.amount,
            timestamp: tick.timestamp.clone(),
            order_id: tick.order_id.clone(),
            trader_id: tick.trader_id,
        }
    }
}

// ---------------------------------------------------------------------------
// Unified lazy inserter
// ---------------------------------------------------------------------------

pub async fn run_unified_lazy_inserter(
    mut rx: Receiver<InserterPayload>,
    client: Client,
    flush_interval_secs: u64,
    buffer_size: usize,
) {
    let mut trade_buffer: Vec<TradeRow> = Vec::new();
    let mut ohlcv_buffer: Vec<OhlcvRow> = Vec::new();
    let mut flush_interval = tokio::time::interval(Duration::from_secs(flush_interval_secs));

    info!(
        "Unified ClickHouse inserter initialized (flush every {}s, buffer {})",
        flush_interval_secs, buffer_size
    );

    while let Some(payload) = {
        tokio::select! {
            res = rx.recv() => res,
            _ = flush_interval.tick() => Some(InserterPayload::Flush),
        }
    } {
        match payload {
            InserterPayload::Trade(t) => {
                trade_buffer.push(t);
                if trade_buffer.len() >= buffer_size {
                    if let Err(e) = flush_trades(&client, &mut trade_buffer).await {
                        error!("Failed to flush trades: {}", e);
                        DB_ERROR_COUNTER.with_label_values(&["flush_trades"]).inc();
                    }
                }
            }
            InserterPayload::Ohlcv(o) => ohlcv_buffer.push(o),
            InserterPayload::Flush => {
                if !trade_buffer.is_empty() {
                    if let Err(e) = flush_trades(&client, &mut trade_buffer).await {
                        error!("Failed to flush trades on interval: {}", e);
                        DB_ERROR_COUNTER
                            .with_label_values(&["flush_trades_interval"])
                            .inc();
                    }
                }
                if !ohlcv_buffer.is_empty() {
                    if let Err(e) = flush_ohlcv(&client, &mut ohlcv_buffer).await {
                        error!("Failed to flush OHLCV on interval: {}", e);
                        DB_ERROR_COUNTER
                            .with_label_values(&["flush_ohlcv_interval"])
                            .inc();
                    }
                }
            }
        }
    }

    info!("Shutdown signal received — finalizing ClickHouse flush...");
    if !trade_buffer.is_empty() {
        if let Err(e) = flush_trades(&client, &mut trade_buffer).await {
            error!("Failed to flush trades on shutdown: {}", e);
        }
    }
    if !ohlcv_buffer.is_empty() {
        if let Err(e) = flush_ohlcv(&client, &mut ohlcv_buffer).await {
            error!("Failed to flush OHLCV on shutdown: {}", e);
        }
    }
    info!("All data flushed. Safe to exit.");
}

async fn flush_trades(
    client: &Client,
    buffer: &mut Vec<TradeRow>,
) -> Result<(), clickhouse::error::Error> {
    let start = Instant::now();
    let mut insert = client.insert("historical_trades")?;
    for row in buffer.iter() {
        insert.write(row).await?;
    }
    insert.end().await?;
    buffer.clear();
    DB_FLUSH_DURATION.observe(start.elapsed().as_secs_f64() * 1000.0);
    Ok(())
}

async fn flush_ohlcv(
    client: &Client,
    buffer: &mut Vec<OhlcvRow>,
) -> Result<(), clickhouse::error::Error> {
    let start = Instant::now();
    let mut insert = client.insert("market_ohlc")?;
    for row in buffer.iter() {
        insert.write(row).await?;
    }
    insert.end().await?;
    buffer.clear();
    DB_FLUSH_DURATION.observe(start.elapsed().as_secs_f64() * 1000.0);
    Ok(())
}

// ---------------------------------------------------------------------------
// Historical queries
// ---------------------------------------------------------------------------

pub async fn poll_24h_change(client: &Client, symbol: &str) -> Option<f64> {
    client
        .query(
            "SELECT price FROM hft_dashboard.historical_trades \
             WHERE symbol = ? AND timestamp >= (now() - interval 1 day) \
             ORDER BY timestamp ASC LIMIT 1",
        )
        .bind(symbol)
        .fetch_one::<TradeRow>()
        .await
        .ok()
        .and_then(|row| row.price.to_f64())
}

pub async fn poll_1h_change(client: &Client, symbol: &str) -> Option<f64> {
    client
        .query(
            "SELECT price FROM hft_dashboard.historical_trades \
             WHERE symbol = ? AND timestamp >= (now() - interval 1 hour) \
             ORDER BY timestamp ASC LIMIT 1",
        )
        .bind(symbol)
        .fetch_one::<TradeRow>()
        .await
        .ok()
        .and_then(|row| row.price.to_f64())
}

pub async fn get_historical_ohlc(client: &Client, symbol: &str, minutes: usize) -> Vec<OhlcvBar> {
    let ohlc_query = format!(
        "SELECT symbol, candle_time, open, high, low, close, volume
         FROM hft_dashboard.market_ohlc
         WHERE symbol = ? AND candle_time >= (now() - interval {} minute)
         ORDER BY candle_time ASC",
        minutes
    );

    let rows = client.query(&ohlc_query).bind(symbol).fetch_all::<OhlcvRow>().await;
    if let Ok(rows) = rows {
        if !rows.is_empty() {
            return rows.into_iter().map(|r| r.into()).collect();
        }
    }

    // Fallback: aggregate from raw trades
    let trades_query = format!(
        "SELECT
            symbol,
            toStartOfMinute(timestamp) AS candle_time,
            argMin(price, timestamp) AS open,
            max(price)               AS high,
            min(price)               AS low,
            argMax(price, timestamp) AS close,
            sum(amount)              AS volume
         FROM hft_dashboard.historical_trades
         WHERE symbol = ? AND timestamp >= (now() - interval {} minute)
         GROUP BY symbol, candle_time
         ORDER BY candle_time ASC",
        minutes
    );

    match client.query(&trades_query).bind(symbol).fetch_all::<OhlcvRow>().await {
        Ok(rows) => rows.into_iter().map(|r| r.into()).collect(),
        Err(e) => {
            error!("ClickHouse trades aggregation failed: {}", e);
            DB_ERROR_COUNTER.with_label_values(&["get_historical_ohlc"]).inc();
            vec![]
        }
    }
}

pub async fn backfill_missing_candles(client: &Client) {
    let query =
        "INSERT INTO hft_dashboard.market_ohlc
         SELECT
            symbol,
            toStartOfMinute(timestamp) AS candle_time,
            argMin(price, timestamp)   AS open,
            max(price)                 AS high,
            min(price)                 AS low,
            argMax(price, timestamp)   AS close,
            sum(amount)                AS volume
         FROM hft_dashboard.historical_trades
         WHERE timestamp >= now() - INTERVAL 2 HOUR
             AND toStartOfMinute(timestamp) NOT IN (
               SELECT candle_time FROM hft_dashboard.market_ohlc
               WHERE candle_time >= now() - INTERVAL 2 HOUR
           )
         GROUP BY symbol, candle_time";

    if let Err(e) = client.query(query).execute().await {
        error!("Backfill failed: {}", e);
        DB_ERROR_COUNTER.with_label_values(&["backfill"]).inc();
    } else {
        info!("Candle backfill complete.");
    }
}

pub async fn create_clickhouse_client() -> Client {
    let url =
        std::env::var("CLICKHOUSE_URL").unwrap_or_else(|_| "http://localhost:8123".to_string());
    let user = std::env::var("CLICKHOUSE_USER").unwrap_or_else(|_| "inserter_user".to_string());
    let password =
        std::env::var("CLICKHOUSE_PASSWORD").unwrap_or_else(|_| "inserter_pass".to_string());
    let database = std::env::var("CLICKHOUSE_DB").unwrap_or_else(|_| "hft_dashboard".to_string());

    if user == "inserter_user" || password == "inserter_pass" {
        tracing::warn!(
            "Default ClickHouse credentials in use. \
             Set CLICKHOUSE_USER and CLICKHOUSE_PASSWORD before deploying outside localhost."
        );
    }

    Client::default()
        .with_url(url)
        .with_user(user)
        .with_password(password)
        .with_database(database)
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use rust_decimal::Decimal;

    #[test]
    fn ch_decimal_round_trip() {
        // 123.45678901 represented as Decimal with scale 8
        let original = Decimal::new(12345678901_i64, 8);
        // Simulate serialize: decimal × 10^8 → i64
        let scaled = original * Decimal::new(100_000_000, 0);
        let as_i64 = scaled.to_i64().unwrap();
        // Simulate deserialize: i64 → Decimal with scale 8
        let recovered = Decimal::new(as_i64, 8);
        assert_eq!(original, recovered);
    }

    #[test]
    fn ch_datetime_round_trip() {
        let ts = "2024-06-15T10:30:00.000000Z";
        let dt = chrono::DateTime::parse_from_rfc3339(&ts.replace('Z', "+00:00")).unwrap();
        let micros = dt.timestamp_micros();
        assert!(micros > 0);

        let recovered = chrono::Utc
            .timestamp_micros(micros)
            .single()
            .unwrap()
            .to_rfc3339_opts(chrono::SecondsFormat::Micros, true);

        // Year-month-day and hour:minute must survive the round-trip
        assert_eq!(&ts[..16], &recovered[..16]);
    }
}
