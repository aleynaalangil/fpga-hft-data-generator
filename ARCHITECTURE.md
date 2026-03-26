# FPGA HFT Data Generator — Architecture & Design Document

## What This Is

A Rust service that generates synthetic high-frequency trading market data using
Geometric Brownian Motion (GBM) price modelling. It serves configurable crypto
pairs (default: SOL/USDC, BTC/USDC) at a configurable tick rate (default: 100/sec),
persists trades and OHLCV candles to ClickHouse, streams real-time data over
WebSocket, and exposes REST endpoints for snapshots and historical queries.

The name "FPGA" is aspirational — nothing in this codebase runs on FPGA hardware.
The naming reflects the intended downstream use: feeding an FPGA-based exchange
simulator with realistic synthetic market data.

---

## High-Level Architecture

```
Clients (REST / WebSocket)
        |
        v
Actix-web HTTP Server (port configurable, default 8080)
Rate limiter: actix-governor (token bucket, per source IP)
        |
        |-- REST endpoints (api.rs)
        |   /api/v1/health
        |   /api/v1/symbols
        |   /api/v1/tick/{symbol}
        |   /api/v1/bbo/{symbol}
        |   /api/v1/ohlcv/{symbol}?minutes=N
        |   /api/v1/metrics
        |
        |-- WebSocket endpoint (ws.rs)
            /v1/feed  (broadcasts ticks in real time, per symbol)
            Heartbeat: Ping every ping_interval_secs, evict after heartbeat_timeout_secs
            Backpressure: evict clients lagged > 64 messages

Background Tasks (tokio::spawn)
        |
        |-- Tick Generator (every tick_interval_ms)
        |   - measures real wall-clock time per tick
        |   - advances GBM price for each symbol
        |   - records TICK_LATENCY_HISTOGRAM
        |   - updates PRICE_GAUGE
        |   - enqueues TradeRow to DB inserter
        |   - broadcasts MarketDataMessage (with real latency/TPS) to WebSocket
        |   - checks for 1-minute candle closures
        |
        |-- 24h Change Poller (every change_poll_interval_secs)
        |   - queries ClickHouse for oldest price in last 1h / 24h
        |   - writes result into MarketGenerator.change_1h / change_24h
        |
        |-- Graceful Shutdown Handler
            - catches Ctrl+C / SIGINT
            - cancels all tasks via CancellationToken
            - waits for DB inserter to drain before exit

Shared State
        DashMap<String, MarketGenerator>   -- lock-free, one entry per symbol
        broadcast::Sender<String>          -- WebSocket fan-out (capacity: ws_broadcast_capacity)
        mpsc::Sender<InserterPayload>      -- DB insert queue (capacity: db_insert_buffer_size)

Persistence (ClickHouse)
        hft_dashboard.historical_trades    -- raw tick rows (MergeTree, 30d TTL)
        hft_dashboard.market_ohlc          -- 1-minute OHLCV (ReplacingMergeTree, 90d TTL)
```

---

## Configuration

All runtime parameters are read from `config.toml` at startup (optional file,
hard-coded defaults apply when absent). The `PORT` environment variable overrides
`config.toml`. ClickHouse credentials are env-var only.

**`config.toml` / `src/config.rs`**

| Key | Default | Description |
|---|---|---|
| `port` | 8080 | HTTP listen port (also env `PORT`) |
| `tick_interval_ms` | 10 | Tick generation interval (10ms = 100 ticks/sec) |
| `db_flush_interval_secs` | 1 | Inserter timer flush interval |
| `db_insert_buffer_size` | 1000 | Trade buffer size before forced flush |
| `tick_history_size` | 1200 | Ring-buffer depth per symbol |
| `ws_broadcast_capacity` | 128 | WebSocket broadcast channel capacity |
| `change_poll_interval_secs` | 5 | 1h/24h change poll interval |
| `ws_heartbeat_timeout_secs` | 60 | Evict WebSocket client after this many idle seconds |
| `ws_ping_interval_secs` | 20 | Send Ping to idle WebSocket clients this often |
| `rate_limit_per_second` | 100 | REST API sustained requests per IP |
| `rate_limit_burst` | 50 | REST API burst allowance |
| `[[symbols]]` | SOL/USDC, BTC/USDC | List of trading pairs with GBM params |

Per-symbol GBM parameters in `config.toml`:

| Symbol | `initial_price` | `drift` | `volatility` | `spread` |
|---|---|---|---|---|
| SOL/USDC | 150.0 | 0.0001 | 0.002 | 0.01 |
| BTC/USDC | 65,000.0 | 0.0001 | 0.001 | 10.0 |

**ClickHouse environment variables:**

| Variable | Default |
|---|---|
| `CLICKHOUSE_URL` | `http://localhost:8123` |
| `CLICKHOUSE_USER` | `inserter_user` |
| `CLICKHOUSE_PASSWORD` | `inserter_pass` |
| `CLICKHOUSE_DB` | `hft_dashboard` |

A warning is logged at startup if the default credentials are detected.

---

## Module Breakdown

### `main.rs` — Bootstrap and Orchestration

Loads `AppConfig`, initialises one `MarketGenerator` per configured symbol,
spawns all background tasks, wires up Actix with CORS, rate limiting, and
two HTTP workers, and manages lifecycle via `CancellationToken`.

**Notable decisions**
- Single tick-generation loop over all symbols rather than one task per symbol.
  Correct for a small symbol count — task-switching cost outweighs parallelism
  gain. A comment in the code flags that 500+ symbols would need a different design.
- Symbol state lives in a `DashMap` (lock-free concurrent HashMap) so REST
  handlers can read state without holding a mutex across await points.
- CORS is fully open (`allow_any_origin`). Appropriate for a local/internal
  simulator, not for internet-facing deployment.
- Real tick latency is measured per-symbol via `Instant` and passed directly
  into `to_ws_message()` and `TICK_LATENCY_HISTOGRAM`.
- Rate limiter (`actix-governor`) wraps the entire App at the middleware level.
  WebSocket connections are subject to the same per-IP bucket for the initial
  upgrade request, but not for subsequent frames.

---

### `config.rs` — Application Configuration

Defines `AppConfig` and `SymbolConfig`. `AppConfig::load()` reads `config.toml`
using the `toml` crate, then applies `PORT` env var override. Falls back to
hard-coded defaults if no file exists.

The `[[symbols]]` array in `config.toml` drives which trading pairs are
initialised — adding a new pair requires only a new TOML block, no code change.

---

### `models.rs` — Data Types

Defines the serialisable structs that cross module boundaries.

**`MarketTick`** — a single trade event with symbol, side, price, amount,
timestamp (RFC3339 microsecond), UUID order id, and random trader id.

**`OrderBookLevel` / `BboSnapshot`** — best bid/offer with a 20-level ladder.
Prices and sizes are stored as fixed-point `u64` scaled by 10^8. This matches
standard HFT practice: integer comparison is faster than floating-point and avoids
rounding drift across aggregations.

**`OhlcvBar`** — 1-minute candle with open/high/low/close/volume, all `Decimal`.

**`MarketDataMessage`** — the WebSocket envelope. Bundles tick, BBO, current
candle, telemetry, and change percentages into one JSON payload. Uses `f64` for
JSON compatibility (JavaScript has no native Decimal type).

**`SystemTelemetry`** — real measured latency (`actual_latency_ms` from the tick
loop) and nominal TPS derived from `tick_interval_ms`. The `error_rate` field is
zero — DB errors are tracked separately via `DB_ERROR_COUNTER` in Prometheus.

---

### `generator.rs` — Price Simulation Engine

**Geometric Brownian Motion** is the standard stochastic model for equity/crypto
prices. The discrete update rule:

```
S_t+1 = S_t × (1 + μ·dt + σ·√dt · Z)

where:
  μ   = drift (configurable per symbol)
  σ   = volatility (configurable per symbol)
  dt  = 0.01 (fixed time step)
  Z   ~ N(0,1) via rand_distr::Normal (true Gaussian, unbounded tails)
```

`rand_distr::Normal` replaced the previous sum-of-12-uniforms approximation.
That method was bounded at ±6σ; the true Gaussian correctly represents
multi-sigma events that occur regularly in crypto markets.

**`advance()`** — computes the next GBM step, appends to the ring buffer
(`VecDeque`, max `tick_history_size` elements), updates the current candle,
returns a `MarketTick`.

**`current_bbo()`** — builds the order book snapshot:
- Level spacing: cumulative geometric steps (~2% per level from mid). Each
  deeper level is slightly further from the best price than the previous,
  producing a realistic staircase rather than random jitter.
- Level sizes: sampled from `LogNormal(μ=4, σ=1.2)`, giving a heavy-tailed
  size distribution representative of real limit order books. The previous
  uniform × depth multiplier was anti-realistic (sizes grew monotonically).
- Tick size scales with spread (`spread / 20`) so BTC and SOL get
  appropriately different granularity.

**`check_candle_closure()`** — detects minute boundary by comparing the first
16 characters ("YYYY-MM-DDTHH:MM") of the current and last tick timestamps.
Returns the completed candle when the minute changes.

**`build_ohlcv(minutes)`** — groups the in-memory ring buffer into 1-minute
candles. Used as fallback when ClickHouse has no data yet.

**`to_ws_message(actual_latency_ms, actual_tps)`** — assembles the WebSocket
payload using real measured values for latency and throughput. No random
simulation of telemetry.

---

### `db.rs` — ClickHouse Integration and Metrics

**Prometheus metrics** — seven singletons registered in a global `Registry`:

| Metric | Type | Labels | What it measures |
|---|---|---|---|
| `hft_ticks_total` | IntCounter | — | Total ticks generated since start |
| `hft_asset_price` | GaugeVec | `symbol` | Current price per symbol |
| `hft_tick_latency_ms` | Histogram | — | Actual tick generation wall time |
| `hft_ws_clients` | Gauge | — | Active WebSocket connections |
| `hft_ws_lagged_total` | IntCounter | — | Broadcast lag events |
| `hft_db_errors_total` | IntCounterVec | `operation` | DB errors by operation |
| `hft_db_flush_duration_ms` | Histogram | — | ClickHouse batch flush time |

**Custom serde modules for ClickHouse binary protocol:**

- `ch_decimal` — serialises `Decimal` to `i64` scaled by 10^8 (matching
  `Decimal64(8)`) and deserialises back. Avoids floating-point loss.
- `ch_datetime` — serialises RFC3339 timestamps to microseconds-since-epoch
  (`i64`). Deserialisation auto-detects seconds vs microseconds based on
  magnitude (values < 1e10 are treated as seconds).

**`run_unified_lazy_inserter(rx, client, flush_secs, buffer_size)`** — receives
`InserterPayload` enums, buffers trade rows up to `buffer_size`, and flushes on
either the buffer limit or a `flush_secs` timer. Both flush functions are now
instrumented with `DB_FLUSH_DURATION` timers and increment `DB_ERROR_COUNTER` on
failure. Buffer sizes and flush intervals come from `AppConfig`.

**Historical queries** — unchanged from initial design:
- `poll_1h_change` / `poll_24h_change` — oldest price in the last 1h / 24h from
  `historical_trades`. Returns `None` if no data yet.
- `get_historical_ohlc` — tries `market_ohlc` first; aggregates from raw trades
  as fallback.
- `backfill_missing_candles` — on startup, fills `market_ohlc` for any minute
  in the last 2 hours not already present.

**ClickHouse connection** — configured via env vars. A startup warning is logged
when default credentials (`inserter_user` / `inserter_pass`) are detected.

---

### `api.rs` — REST Handlers

Unchanged from initial design. All handlers receive an `Arc`-wrapped `DashMap`
and a ClickHouse `Client` via Actix's `web::Data` extractor.

**Symbol URL format** — URL uses dashes (SOL-USDC), internal map uses slashes
(SOL/USDC). `raw_symbol_to_symbol()` converts between them.

**OHLCV fallback** — ClickHouse first; in-memory `build_ohlcv()` if empty.

**`minutes` parameter** — clamped to [1, 10080] (7 days).

**Error model** — `ErrorResponse { error: String }` JSON for all non-200 responses.

---

### `ws.rs` — WebSocket Handler

Uses `actix-ws` for the protocol upgrade. Each client connection receives its own
`tokio::select!` loop over three branches. The handler accepts a `WsConfig`
(heartbeat timeout and ping interval) via Actix's `web::Data`, set from
`AppConfig` at startup.

1. **Heartbeat timer** — ticks every `ping_interval / 2` seconds (capped at 5s
   minimum). On each tick:
   - If `elapsed > heartbeat_timeout_secs`: forcibly close the session and break.
   - If `elapsed > ping_interval_secs`: send a `Ping` frame.
   - `last_heartbeat` is only reset on `Pong` receipt, not on ping-sent. This
     ensures a client that accepts pings but never responds will still be evicted.

2. **Incoming client messages** — handles `Pong` (resets `last_heartbeat`),
   `Close` (clean disconnect), and ignores everything else.

3. **Broadcast channel receiver** — receives pre-serialised JSON strings and
   forwards as `Text` frames. On `Lagged(count)`:
   - Increments `WS_LAGGED_COUNTER`.
   - If `count > 64` (half broadcast channel capacity), evicts the client.

`WS_CLIENT_GAUGE` is incremented at connection start and decremented at every
exit path via a labelled loop (`'conn: loop { ... }` with a single `break 'conn`
point followed by the gauge decrement).

---

### `schema/schema.sql` — ClickHouse Schema

Defines both tables with exact column types matching the Rust row structs:

```sql
historical_trades  -- MergeTree, ORDER BY (symbol, timestamp), TTL 30 days
market_ohlc        -- ReplacingMergeTree, ORDER BY (symbol, candle_time), TTL 90 days
```

`ReplacingMergeTree` on `market_ohlc` makes the backfill `INSERT … SELECT` and
live candle inserts idempotent — duplicate `(symbol, candle_time)` rows are
deduplicated during background merges.

Run once before first service start:
```bash
clickhouse-client < schema/schema.sql
```

---

## Software Methodologies

**Actor-like message passing** — the DB inserter is an isolated task that owns
its buffers and communicates only through channels. This is the Rust idiomatic
approach to shared mutable state: avoid sharing, pass messages instead.

**Cancellation-based shutdown** — `tokio_util::sync::CancellationToken` propagates
shutdown intent to all tasks cleanly. The inserter is additionally signalled by
channel closure and drains its buffer before exiting.

**Layered fallbacks** — OHLCV endpoint: ClickHouse → in-memory ring buffer.
Change poller: real DB value → `None` (zero displayed). Prioritises availability
over strict accuracy.

**Fixed-point arithmetic for prices** — `rust_decimal::Decimal` for business logic,
`u64` fixed-point (scale 10^8) for the order book. Avoids floating-point rounding
in price comparisons.

**Lazy batch insertion** — trades buffered and flushed in batches. ClickHouse is
optimised for bulk inserts; single-row inserts at 100/sec would cause excessive
merge pressure.

**Lock-free shared state** — `DashMap` (shard-based) instead of `Mutex<HashMap>`
allows REST handlers to read symbol state concurrently.

**Measured telemetry** — tick latency is measured per-symbol via `Instant` and
both recorded in Prometheus histograms and forwarded directly in the WebSocket
payload. No simulated values.

**Token-bucket rate limiting** — `actix-governor` wraps the Actix App at the
middleware level, providing per-IP rate limiting with a configurable sustained rate
and burst allowance.

---

## What Was Done Well

- **GBM with true Gaussian tails** — `rand_distr::Normal` gives correct unbounded
  N(0,1) sampling, enabling realistic multi-sigma price moves.
- **Configurable everything** — `config.toml` + `AppConfig` removes all hardcoded
  parameters; new symbols require a TOML block, not a code change.
- **Realistic order book** — log-normal sizes + geometric level spacing replace
  the previous uniform/anti-realistic structure.
- **Real telemetry** — latency and TPS in WebSocket messages are measured, not
  random fiction.
- **Comprehensive Prometheus coverage** — 7 metrics covering tick latency, WS
  client count, lag events, DB errors, and flush duration.
- **Heartbeat enforced correctly** — `last_heartbeat` only resets on Pong; stale
  clients are evicted after a configurable timeout.
- **Lagged client eviction** — clients exceeding half the broadcast channel
  capacity are disconnected rather than silently accumulating lag.
- **Schema shipped** — `schema/schema.sql` removes the undocumented prerequisite
  that previously caused opaque startup failures.
- **12 unit tests** — covering fixed-point precision, decimal round-trips,
  datetime round-trips, GBM price floor, BBO structure, candle logic, and ring
  buffer limits.
- **Separation of concerns** — models, config, generator, DB, API, and WebSocket
  are distinct modules with narrow interfaces.
- **Graceful shutdown** — channel drain prevents data loss on Ctrl+C.
- **Docker multi-stage build** — Debian slim runtime image.
- **Backfill on startup** — candle history survives service restarts.
- **Pre-serialised WebSocket broadcast** — avoids per-client JSON serialisation.

---

## Known Limitations

### BBO is simulated, not microstructure-driven
Log-normal sizes and geometric level spacing are an improvement but not a real limit order book model. A Hawkes process with price-level clustering would be the correct next step.

### No integration tests
Unit tests cover pure functions only. There is no end-to-end test that starts the server, connects a WebSocket client, and asserts on a received message.

### `error_rate` in telemetry is hardcoded to 0.0
`SystemTelemetry.error_rate` is always zero in the WebSocket payload. The real figure is available at `/api/v1/metrics` via `hft_db_errors_total`.

### Backfill window is hardcoded to 2 hours
`backfill_missing_candles` always uses `INTERVAL 2 HOUR`. It should be a `config.toml` parameter.

### CORS is fully open
`allow_any_origin` is fine for local use. Any internet-facing deployment must restrict this to known origins.

---

## Dependency Notes

| Crate | Version | Role |
|---|---|---|
| `actix-web` | 4 | HTTP server and routing |
| `actix-ws` | 0.3 | WebSocket protocol upgrade |
| `actix-cors` | 0.7 | CORS middleware |
| `actix-governor` | 0.5 | Token-bucket rate limiter per source IP |
| `tokio` | 1 (full) | Async runtime, tasks, timers, channels, signal |
| `tokio-util` | 0.7 | `CancellationToken` |
| `futures-util` | 0.3 | Stream combinators for WebSocket |
| `serde` + `serde_json` | 1 | Serialisation |
| `rust_decimal` | 1 | Decimal arithmetic matching ClickHouse Decimal64(8) |
| `clickhouse` | 0.11 | ClickHouse native binary client |
| `dashmap` | 6 | Lock-free concurrent HashMap (stable release) |
| `prometheus` | 0.14.0 | Metrics collection and exposition |
| `lazy_static` | 1.5.0 | Singleton metric registries |
| `tracing` + `tracing-subscriber` | 0.1 / 0.3 | Structured logging |
| `thiserror` | 1.0.69 | Error type derivation |
| `chrono` | 0.4 | DateTime parsing |
| `uuid` | 1 (v4) | Order ID generation |
| `rand` | 0.8 | Uniform random sampling |
| `rand_distr` | 0.4 | True Gaussian (Normal) and LogNormal distributions |
| `toml` | 0.8 | Config file parsing |

---

## Runtime Behaviour Summary

| Property | Default | Configurable |
|---|---|---|
| Tick rate | 100/sec (10ms) | Yes — `tick_interval_ms` |
| Symbols | 2 (SOL/USDC, BTC/USDC) | Yes — `[[symbols]]` in config.toml |
| Tick history per symbol | 1,200 (ring buffer) | Yes — `tick_history_size` |
| Candle resolution | 1 minute | No |
| Order book depth | 20 levels (bid + ask) | No |
| DB flush interval | 1s or 1,000 trades | Yes — `db_flush_interval_secs`, `db_insert_buffer_size` |
| WebSocket broadcast capacity | 128 | Yes — `ws_broadcast_capacity` |
| WS heartbeat timeout | 60s | Yes — `ws_heartbeat_timeout_secs` |
| WS ping interval | 20s | Yes — `ws_ping_interval_secs` |
| REST rate limit | 100 req/s, burst 50 | Yes — `rate_limit_per_second`, `rate_limit_burst` |
| 24h change poll interval | 5s | Yes — `change_poll_interval_secs` |
| HTTP port | 8080 | Yes — `port` / env `PORT` |
| ClickHouse URL | `http://localhost:8123` | Env `CLICKHOUSE_URL` |
| ClickHouse database | `hft_dashboard` | Env `CLICKHOUSE_DB` |
