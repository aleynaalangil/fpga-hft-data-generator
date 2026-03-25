use crate::models::{MarketTick, OhlcvBar};
use clickhouse::Client;
use lazy_static::lazy_static;
use prometheus::{opts, GaugeVec, IntCounter, Registry};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::sync::mpsc::Receiver;
use tracing::{error, info};

pub enum InserterPayload {
    Trade(TradeRow),
    Ohlcv(OhlcvRow),
    Flush,
}
lazy_static! {
    pub static ref REGISTRY: Registry = Registry::new();
    pub static ref TICK_COUNTER: IntCounter =
        IntCounter::new("hft_ticks_total", "Total number of market ticks generated")
            .expect("metric can be created");
    pub static ref PRICE_GAUGE: GaugeVec = GaugeVec::new(
        opts!("hft_asset_price", "Current price of the asset"),
        &["symbol"]
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
}

pub mod ch_decimal {
    use super::*;
    use rust_decimal::prelude::ToPrimitive;
    use serde::Serializer;

    pub fn serialize<S>(decimal: &Decimal, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // ClickHouse Decimal64(8) native representation is an i64 multiplied by 10^8
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
        // ClickHouse DateTime64(6) native representation is an i64 of microseconds since epoch
        let dt = chrono::DateTime::parse_from_rfc3339(&time_str.to_owned().replace("Z", "+00:00"))
            .unwrap_or_else(|_| chrono::Utc::now().into());
        let epoch_micros = dt.timestamp_micros();
        serializer.serialize_i64(epoch_micros)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<String, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let val = i64::deserialize(deserializer)?;
        // Handle both seconds (DateTime64(0)) and micros (DateTime64(6))
        // If val is > 10^12, it's likely micros/millis.
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

#[derive(clickhouse::Row, Serialize, Deserialize, Debug, Clone)]
pub struct TradeRow {
    pub symbol: String,
    pub side: i8, // 1 for 'buy', 2 for 'sell'

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

pub async fn run_unified_lazy_inserter(mut rx: Receiver<InserterPayload>, client: Client) {
    let mut trade_buffer = Vec::new();
    let mut ohlcv_buffer = Vec::new();

    let mut flush_interval = tokio::time::interval(Duration::from_secs(1));

    info!("🐂 Unified ClickHouse Inserter initialized.");

    while let Some(payload) = {
        tokio::select! {
        res = rx.recv() => res,
        _ = flush_interval.tick() => Some(InserterPayload::Flush)
        }
    } {
        match payload {
            InserterPayload::Trade(t) => {
                trade_buffer.push(t);
                if trade_buffer.len() >= 1000
                    && let Err(e) = flush_trades(&client, &mut trade_buffer).await
                {
                    error!("❌ Failed to flush trades: {}", e);
                }
            }
            InserterPayload::Ohlcv(o) => ohlcv_buffer.push(o),
            InserterPayload::Flush => {
                if !trade_buffer.is_empty()
                    && let Err(e) = flush_trades(&client, &mut trade_buffer).await
                {
                    error!("❌ Failed to flush trades on interval: {}", e);
                }
                if !ohlcv_buffer.is_empty()
                    && let Err(e) = flush_ohlcv(&client, &mut ohlcv_buffer).await
                {
                    error!("❌ Failed to flush OHLCV on interval: {}", e);
                }
            }
        }
    }
    info!("🧹 Shutdown signal received. Finalizing ClickHouse flush...");
    if !trade_buffer.is_empty()
        && let Err(e) = flush_trades(&client, &mut trade_buffer).await
    {
        error!("❌ Failed to flush trades on shutdown: {}", e);
    }
    if !ohlcv_buffer.is_empty()
        && let Err(e) = flush_ohlcv(&client, &mut ohlcv_buffer).await
    {
        error!("❌ Failed to flush OHLCV on shutdown: {}", e);
    }
    info!("✅ All data flushed. Safe to exit.");
}

async fn flush_ohlcv(
    client: &Client,
    buffer: &mut Vec<OhlcvRow>,
) -> Result<(), clickhouse::error::Error> {
    let mut insert = client.insert("market_ohlc")?;
    for row in buffer.iter() {
        insert.write(row).await?;
    }
    insert.end().await?;
    buffer.clear();
    Ok(())
}

async fn flush_trades(
    client: &Client,
    buffer: &mut Vec<TradeRow>,
) -> Result<(), clickhouse::error::Error> {
    let mut insert = client.insert("historical_trades")?;
    for row in buffer.iter() {
        insert.write(row).await?;
    }

    insert.end().await?;
    buffer.clear();
    Ok(())
}

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
    let query = format!(
        "SELECT symbol, candle_time, open, high, low, close, volume FROM hft_dashboard.market_ohlc \
         WHERE symbol = ? AND candle_time >= (now() - interval {} minute) \
         ORDER BY candle_time ASC",
        minutes
    );

    match client
        .query(&query)
        .bind(symbol)
        .fetch_all::<OhlcvRow>()
        .await
    {
        Ok(rows) => rows.into_iter().map(|r| r.into()).collect(),
        Err(e) => {
            error!("❌ ClickHouse OHLC fetch failed: {}", e);
            vec![]
        }
    }
}

pub async fn create_clickhouse_client() -> Client {
    let url =
        std::env::var("CLICKHOUSE_URL").unwrap_or_else(|_| "http://localhost:8123".to_string());
    let user = std::env::var("CLICKHOUSE_USER").unwrap_or_else(|_| "inserter_user".to_string());
    let password =
        std::env::var("CLICKHOUSE_PASSWORD").unwrap_or_else(|_| "inserter_pass".to_string());
    let database = std::env::var("CLICKHOUSE_DB").unwrap_or_else(|_| "hft_dashboard".to_string());

    Client::default()
        .with_url(url)
        .with_user(user)
        .with_password(password)
        .with_database(database)
}
