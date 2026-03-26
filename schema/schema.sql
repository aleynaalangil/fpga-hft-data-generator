-- FPGA HFT Data Generator — ClickHouse schema
-- Run this once before starting the service:
--   clickhouse-client < schema/schema.sql
-- Or using HTTP:
--   curl -u user:pass http://localhost:8123/ --data-binary @schema/schema.sql

CREATE DATABASE IF NOT EXISTS hft_dashboard;

-- Raw trade ticks written at up to 100/sec per symbol.
-- Partitioned by day; ordered by (symbol, timestamp) for efficient
-- time-range queries filtering on symbol.
CREATE TABLE IF NOT EXISTS hft_dashboard.historical_trades
(
    symbol     LowCardinality(String),
    side       Int8,                     -- 1 = buy, 2 = sell
    price      Decimal64(8),
    amount     Decimal64(8),
    timestamp  DateTime64(6, 'UTC'),
    order_id   String,
    trader_id  UInt32
)
ENGINE = MergeTree()
PARTITION BY toYYYYMMDD(timestamp)
ORDER BY (symbol, timestamp)
TTL timestamp + INTERVAL 30 DAY;

-- Pre-aggregated 1-minute OHLCV candles.
-- ReplacingMergeTree deduplicates on (symbol, candle_time) during merges,
-- so the backfill INSERT … SELECT and the live inserter are both idempotent.
CREATE TABLE IF NOT EXISTS hft_dashboard.market_ohlc
(
    symbol      LowCardinality(String),
    candle_time DateTime64(6, 'UTC'),
    open        Decimal64(8),
    high        Decimal64(8),
    low         Decimal64(8),
    close       Decimal64(8),
    volume      Decimal64(8)
)
ENGINE = ReplacingMergeTree()
PARTITION BY toYYYYMM(candle_time)
ORDER BY (symbol, candle_time)
TTL candle_time + INTERVAL 90 DAY;
