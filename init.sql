CREATE DATABASE IF NOT EXISTS hft_dashboard;

CREATE TABLE IF NOT EXISTS hft_dashboard.historical_trades
(
    symbol      String,
    side        Int8,                  -- 1 = buy, 2 = sell
    price       Decimal64(8),
    amount      Decimal64(8),
    timestamp   DateTime64(6),
    order_id    String,
    trader_id   UInt32
)
ENGINE = MergeTree()
ORDER BY (symbol, timestamp)
PARTITION BY toYYYYMM(timestamp)
TTL timestamp + INTERVAL 7 DAY;

CREATE TABLE IF NOT EXISTS hft_dashboard.market_ohlc
(
    symbol      String,
    candle_time DateTime64(6),
    open        Decimal64(8),
    high        Decimal64(8),
    low         Decimal64(8),
    close       Decimal64(8),
    volume      Decimal64(8)
)
ENGINE = ReplacingMergeTree()
ORDER BY (symbol, candle_time)
PARTITION BY toYYYYMM(candle_time)
TTL candle_time + INTERVAL 30 DAY;