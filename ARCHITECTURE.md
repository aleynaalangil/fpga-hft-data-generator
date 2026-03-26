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
        |   /api/v1/ohlcv/{symbol}?minutes=N&interval=1m|5m|15m|1h|1d
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

**Historical queries:**
- `poll_1h_change` / `poll_24h_change` — oldest price in the last 1h / 24h from
  `historical_trades`. Returns `None` if no data yet.
- `get_historical_ohlc(client, symbol, minutes, interval)` — routes to the correct
  ClickHouse data source based on `interval`:
  - `1m` → `market_ohlc` (pre-aggregated 1m candles) → fallback: `toStartOfMinute` GROUP BY on raw trades
  - `5m` → `historical_trades_mv_5m` (AggregatingMergeTree) → fallback: `toStartOfInterval(..., INTERVAL 5 MINUTE)` GROUP BY
  - `15m` → `historical_trades_mv_15m` → fallback: `toStartOfInterval(..., INTERVAL 15 MINUTE)` GROUP BY
  - `1h` → `historical_trades_mv_1h` → fallback: `toStartOfHour` GROUP BY
  - `1d` → `historical_trades_mv_1d` → fallback: `toStartOfDay` GROUP BY

  MV queries use `*Merge` aggregate functions (`argMinMerge`, `maxMerge`, `minMerge`, `argMaxMerge`, `sumMerge`) to finalise the stored `*State` values. The fallback path re-aggregates from `historical_trades` on-the-fly using the matching truncation function — this covers the period before the MVs were created or backfilled.
- `backfill_missing_candles` — on startup, fills `market_ohlc` for any minute
  in the last 2 hours not already present.

**ClickHouse connection** — configured via env vars. A startup warning is logged
when default credentials (`inserter_user` / `inserter_pass`) are detected.

---

### `api.rs` — REST Handlers

All handlers receive an `Arc`-wrapped `DashMap` and a ClickHouse `Client` via
Actix's `web::Data` extractor.

**Symbol URL format** — URL uses dashes (SOL-USDC), internal map uses slashes
(SOL/USDC). `raw_symbol_to_symbol()` converts between them.

**OHLCV query parameters:**

| Parameter | Default | Constraint | Description |
|---|---|---|---|
| `minutes` | 60 | clamped [1, 10080] | Time window to return |
| `interval` | `1m` | `1m` / `5m` / `15m` / `1h` / `1d` | Candle resolution — routes to the appropriate ClickHouse MV |

**OHLCV routing** — `interval` is forwarded to `get_historical_ohlc` in `db.rs`
which selects the correct data source. Unrecognised interval values fall back to
the `1m` path.

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

### Real-Time Broadcast Model & Data Consistency

**End-to-end tick pipeline:**

```
GBM advance() — every 10ms per symbol
  └─ MarketTick { price, bbo, ohlc, telemetry, change_1h, change_24h }
       └─ to_ws_message() → JSON string  (serialised ONCE)
            └─ broadcast::Sender<String>.send(json)
                 └─ tokio broadcast channel (capacity: ws_broadcast_capacity)
                      └─ each WS handler's select! branch receives the same Arc<str>
                           └─ session.text(json) → WebSocket frame to client
```

**Why pre-serialise the JSON once.** The broadcast channel carries `String` (the serialised payload), not `MarketDataMessage`. If each handler serialised independently, N clients would each call `serde_json::to_string` on the same data — O(N) CPU work per tick. Pre-serialising in the tick loop makes broadcast O(1) for serialisation regardless of client count, with O(N) only for the frame copy into each client's socket buffer.

**Backpressure: lagged client eviction.** The `tokio::sync::broadcast` channel has a fixed capacity (`ws_broadcast_capacity`, default 128). When a slow client falls 64 messages behind (half capacity), its `recv()` returns `Lagged(count)`. The handler evicts the client rather than blocking the channel or letting it accumulate unbounded lag. This prevents one slow client from causing `send()` to block, which would delay ticks for all other clients.

**Heartbeat-based dead client detection.** The TCP stack does not reliably signal when a client silently disappears (e.g. network cut, browser tab killed). The heartbeat loop sends `Ping` frames every `ping_interval_secs` and tracks the last `Pong` receipt time. If `elapsed > heartbeat_timeout_secs` the session is forcibly closed and the `WS_CLIENT_GAUGE` is decremented. Without this, dead connections would accumulate, consume memory, and eventually exhaust the broadcast channel capacity.

**Message ordering guarantee.** `tokio::sync::broadcast` is a MPMC channel — all receivers see messages in insertion order. Since ticks are generated in a single loop (not parallel tasks), the tick sequence received by any client is the same total order as generated. There is no possibility of client A receiving tick N before tick N-1.

**What is not guaranteed:**
- **Exactly-once delivery** — a client that reconnects does not receive ticks missed during the disconnect. The 100ms client-side buffer and latest-wins semantics handle this gracefully: the first tick after reconnect overwrites the stale value.
- **Synchronisation across clients** — two clients may be at different positions in the broadcast channel at any moment. This is acceptable for a display dashboard; it would not be acceptable for a matching engine.
- **Consistency with ClickHouse** — ticks are broadcast before the corresponding `TradeRow` is flushed to ClickHouse (the lazy inserter has up to 1s delay). A client could see a price in the WebSocket stream that has not yet appeared in the OHLCV history endpoint. This gap closes within 1s under normal conditions.

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

## Security

### Authentication & Access Control

**This service has no user authentication.** The `/v1/feed` WebSocket endpoint and all REST endpoints (`/api/v1/tick`, `/api/v1/ohlcv`, `/api/v1/bbo`) are publicly accessible. This is an intentional design decision for a synthetic market data feed:

- The data is entirely synthetic — no real financial data, no user data, no account information is served
- The intended consumers are the exchange-sim backend (internal) and the frontend dashboard (via Vercel rewrite)
- In production, access should be restricted by network policy rather than application-level authentication (see hardening section below)

**Rate limiting** is the only access control mechanism implemented. `actix-governor` enforces a token bucket per source IP:

| Parameter | Default | Config key |
|---|---|---|
| Sustained rate | 100 req/s | `rate_limit_per_second` |
| Burst allowance | 50 requests | `rate_limit_burst` |

The rate limiter applies to all REST endpoints. WebSocket connections are rate-limited only at the initial HTTP upgrade request — subsequent frames are not rate-limited once the connection is established. This prevents rapid reconnect storms but does not throttle a connected client's frame consumption rate.

---

### ClickHouse Credential Security

**Principle of least privilege:** The `inserter_user` ClickHouse account has only `INSERT` and `SELECT` on `hft_dashboard.*`. It cannot `DROP` tables, modify schemas, or read system tables. If the application credentials are compromised, the blast radius is limited to the `hft_dashboard` database.

**Credential management:**

| Variable | Default | Risk if default used |
|---|---|---|
| `CLICKHOUSE_USER` | `inserter_user` | None (low-privilege account) |
| `CLICKHOUSE_PASSWORD` | `inserter_pass` | Known default, must be changed before network exposure |
| `CLICKHOUSE_URL` | `http://localhost:8123` | Plain HTTP; credentials travel in cleartext |

A startup warning is logged when the default password is detected:
```
WARN Default ClickHouse credentials in use. Set CLICKHOUSE_USER and
     CLICKHOUSE_PASSWORD before deploying outside localhost.
```

**ClickHouse connection:** Plain HTTP on `localhost` (Docker Compose internal network). Credentials are sent as HTTP Basic auth headers. Acceptable for single-host local deployment; requires TLS or a Unix socket for any network-exposed setup.

---

### Sensitive Data Exposure

**No user data served.** All data generated and served by this service is synthetic market data (GBM-generated prices, simulated order IDs, random trader IDs). There are no real user accounts, no real financial positions, and no PII anywhere in the data pipeline.

**Prometheus metrics endpoint** (`/api/v1/metrics`) exposes operational metrics (tick counts, latency histograms, WS client counts, DB error rates). These do not contain user data but do reveal service internals (throughput, error rates). In production, this endpoint should be restricted to the internal monitoring network.

---

### Secure Communication

**Frontend → fpga-hft-data-generator (REST):** In production, the frontend calls `/api/v1/ohlcv/*` which Vercel rewrites to this service over HTTPS. The service itself can run plain HTTP behind Vercel's TLS termination.

**Frontend → fpga-hft-data-generator (WebSocket):** The WS connection from the browser is direct (`wss://`). The service must present a valid TLS certificate for the browser to allow a `wss://` upgrade. This is the one endpoint that is not proxied by Vercel and requires independent TLS setup.

**exchange-sim → fpga-hft-data-generator:** Internal WebSocket connection on Docker Compose private network. No TLS required when both services are on the same host. On a multi-host deployment, the connection should run within a private VPC or use mutual TLS.

---

### Security Gaps (Production Hardening Required)

| Gap | Risk | Recommended fix |
|---|---|---|
| No authentication on API | Anyone with network access can read market data and metrics | Restrict by network policy (VPC, private subnet) or add API key check |
| CORS fully open (`allow_any_origin`) | Any web origin can make cross-origin API requests | Restrict to Vercel app domain and internal origins |
| Plain HTTP to ClickHouse | Credentials in cleartext on network | Enable ClickHouse TLS; or use Unix socket on single-host setups |
| Metrics endpoint publicly accessible | Exposes operational internals | Move behind internal network or add IP allowlist middleware |
| No WS authentication | Any client can connect and consume the full tick stream | Add token-based WS handshake query param or subprotocol auth |

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

## Technology Choices

### Rust

**Performance:** Rust compiles to native machine code with no garbage collector. GC pauses are the main source of unpredictable latency in Go, JVM, and Node.js services. At 100 ticks/sec per symbol, a 10ms GC pause doubles the tick interval for its duration and produces visible chart gaps. Rust's ownership model eliminates GC entirely — latency is bounded by I/O, not the runtime.

**Scalability:** Zero-cost abstractions mean adding features (new metrics, more symbols, additional WS clients) does not introduce hidden runtime overhead. `async/await` on top of `tokio` scales to thousands of concurrent WebSocket connections on a single thread pool with no per-connection stack allocation.

**Maintainability:** The borrow checker prevents data races at compile time — a critical property for a service that shares price state across a tick generator task, multiple REST handler threads, and WebSocket broadcast. Race conditions that would only appear under load in Go or C++ are compile errors in Rust.

**Alternatives considered:** Go — simpler concurrency model, faster compile times, GC pauses are a drawback. Node.js — single-threaded event loop, GC, not suitable for CPU-bound GBM computation at this frequency. Python — too slow for 100 ticks/sec without native extensions.

---

### actix-web

**Performance:** Consistently ranks in the top 3 on TechEmpower Framework Benchmarks across plaintext and JSON serialisation categories. Its actor model dispatches requests to a configurable number of OS threads (`HttpServer::workers`), each running a `tokio` event loop — full CPU utilisation with no lock contention on the hot path.

**Scalability:** The `web::Data<T>` extractor injects shared state (DashMap, ClickHouse client) into handlers without copying. Middleware (CORS, rate limiting via `actix-governor`) composes without modifying handler signatures.

**Maintainability:** Handler functions are plain `async fn` returning `impl Responder`. No macros, no trait boilerplate. Error handling through `AppError`-style enums maps cleanly to HTTP status codes. The route registration in `main.rs` is a single readable block.

**Alternatives considered:** Axum — similar performance, slightly more ergonomic extractor system, but less mature ecosystem at project start. Warp — combinator style is harder to read for complex route trees. Rocket — async support was added later and is less battle-tested.

---

### tokio

**Performance:** Multi-threaded work-stealing scheduler maximises CPU utilisation across all cores. `tokio::select!` allows a single task to wait on multiple futures simultaneously — the WebSocket handler, tick timer, and DB flush timer all run in one `select!` loop with no thread-per-connection overhead.

**Scalability:** The async model handles thousands of concurrent WebSocket clients on a fixed thread pool. `tokio::sync::broadcast` provides efficient one-to-many fan-out without per-subscriber locking.

**Maintainability:** `tokio_util::CancellationToken` propagates shutdown intent to all tasks cleanly. `mpsc` channels decouple the tick generator from the DB inserter — each evolves independently. `tokio::time::interval` provides precise periodic timers without busy-waiting.

**Alternatives considered:** `async-std` — compatible API but smaller ecosystem and fewer production deployments. Blocking threads — simpler but cannot handle 1000+ concurrent WS clients without exhausting the thread pool.

---

### ClickHouse

**Performance:** Columnar storage reads only the columns referenced in a query — a `SELECT price, timestamp FROM historical_trades WHERE symbol = ?` scan reads two columns, not all seven. ZSTD + DoubleDelta compression reduces I/O by 80–90% for time-series data. Vectorised query execution processes data in 8192-row granules using SIMD instructions.

**Scalability:** Designed for insert throughput at millions of rows/sec. The lazy inserter pattern (batch flush every 1s or 1000 rows) aligns with ClickHouse's preferred write pattern. `AggregatingMergeTree` materialized views incrementally maintain OHLCV aggregates as data arrives, eliminating full-table GROUP BY scans at query time.

**Maintainability:** The schema is defined in a single SQL file and applied once. `MergeTree` table semantics (immutable inserts, background merges, TTL) are well-documented and predictable. Adding a new timeframe MV requires one `CREATE MATERIALIZED VIEW` statement.

**Alternatives considered:** PostgreSQL with TimescaleDB — MVCC row storage is inefficient for append-only time-series; TimescaleDB improves this but cannot match ClickHouse columnar compression. InfluxDB — purpose-built time-series but lacks SQL and JOIN support needed for cross-table OHLCV queries. Redis — in-memory only, not suitable for 30-day historical retention.

---

### DashMap

**Performance:** Shard-based concurrent HashMap. Reads from the tick generator, REST handlers, and WebSocket broadcast path happen simultaneously — a `Mutex<HashMap>` would serialize all of these. DashMap shards the lock space so concurrent reads to different symbol buckets never contend.

**Scalability:** At 2 symbols the sharding is overkill, but the design is correct for 50+ symbols with no code change. Lock-free reads are sub-microsecond — critical on the WebSocket broadcast path where latency adds directly to client-observed tick delay.

**Maintainability:** The API mirrors `std::collections::HashMap`. Switching from `Mutex<HashMap>` to `DashMap` requires changing only the type declaration — no changes to call sites.

**Alternatives considered:** `RwLock<HashMap>` — allows concurrent reads but blocks all readers during a write. At 100 writes/sec (one per tick) with many concurrent REST handlers this creates measurable read latency spikes. `Arc<Mutex<HashMap>>` — fully serializes all access.

---

### rust_decimal

**Performance:** Fixed-point arithmetic is faster than arbitrary-precision libraries for the decimal scales used here (8 decimal places). Operations stay within 128-bit integers — no heap allocation, no big-integer arithmetic.

**Scalability:** `Decimal64(8)` in ClickHouse maps directly to Rust `Decimal` with scale 8. The custom `ch_decimal` serde module serializes to/from `i64` (ClickHouse binary wire format) without intermediate string conversion.

**Maintainability:** Using `Decimal` throughout the tick generator, DB layer, and BBO builder eliminates the class of bugs where float arithmetic produces non-round numbers in price displays. `Decimal::new(val, 8)` is explicit about scale — there is no implicit precision loss.

**Alternatives considered:** `f64` — fast, but 64-bit IEEE 754 cannot represent many decimal fractions exactly. Price drift accumulates rounding error across thousands of GBM steps. `num_bigint` — arbitrary precision but slower and more complex than needed for 8 decimal places.

---

### Prometheus + lazy_static metrics

**Performance:** Prometheus counters and gauges are atomic integers/floats — increment is a single `fetch_add` instruction. Histograms use pre-bucketed atomic arrays. Scraping the `/api/v1/metrics` endpoint encodes the current values without locking the metrics themselves.

**Scalability:** The pull model (Prometheus scrapes on its own schedule) decouples metric collection from the application write path. Adding new metrics is a single `lazy_static!` declaration and registration call.

**Maintainability:** Standard exposition format is compatible with Grafana, Alertmanager, and any Prometheus-compatible monitoring stack. The `DB_ERROR_COUNTER` with `operation` label allows per-operation error rate dashboards without changing the metric schema.

**Alternatives considered:** Custom JSON `/metrics` endpoint — no ecosystem integration. StatsD/Datadog — push model, requires a sidecar agent. OpenTelemetry — more powerful but significantly more complex to configure for a single-service deployment.

---

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

---

## ClickHouse Database Design

### Overview

Two ClickHouse databases serve the full system. `hft_dashboard` is owned by `fpga-hft-data-generator` and holds all market data. `exchange` is owned by `exchange-sim` and holds all account state. `exchange-sim` also holds a cross-database `SELECT` grant on `hft_dashboard` so it can read live prices directly.

The authoritative schema is in `clickhouse-config/init.sql`. The `fpga-hft-data-generator/schema/schema.sql` file is a developer-facing subset used for standalone bringup without Docker Compose.

---

### Databases and Ownership

| Database | Owner service | Tables |
|---|---|---|
| `hft_dashboard` | `fpga-hft-data-generator` | `historical_trades`, `market_ohlc`, 5 materialized views |
| `exchange` | `exchange-sim` | `users`, `orders`, `positions` |

ClickHouse users:
- `inserter_user` — INSERT + SELECT on `hft_dashboard.*`
- `exchange_user` — INSERT + SELECT on `exchange.*`, SELECT on `hft_dashboard.*`

---

### Table Reference

#### `hft_dashboard.historical_trades`

Raw trade ticks written by `fpga-hft-data-generator` at up to 100 ticks/sec per symbol.

```sql
ENGINE = MergeTree()
ORDER BY (symbol, timestamp)
PARTITION BY toYYYYMM(timestamp)
TTL toDateTime(timestamp) + INTERVAL 2 HOUR DELETE
```

| Column | Type | Codec | Notes |
|---|---|---|---|
| `symbol` | String | ZSTD(3) | Trading pair, e.g. `SOL/USDC` |
| `side` | Int8 | — | 1 = buy, 2 = sell |
| `price` | Decimal64(8) | ZSTD(3) | 8 decimal places |
| `amount` | Decimal64(8) | ZSTD(3) | 8 decimal places |
| `timestamp` | DateTime64(6) | DoubleDelta + ZSTD(1) | Microsecond UTC |
| `order_id` | String | ZSTD(3) | UUID |
| `trader_id` | UInt32 | ZSTD(1) | Simulated trader identity |

**Design rationale:**
- `ORDER BY (symbol, timestamp)` — all time-range queries filter on `symbol` first. ClickHouse physically sorts data by this key, so a `WHERE symbol = 'SOL/USDC' AND timestamp >= ...` scan skips all granules that don't match — no full table scan.
- `PARTITION BY toYYYYMM(timestamp)` — monthly partitions bound the scan surface for time-bounded queries and allow `ALTER TABLE DROP PARTITION` for bulk data removal without rewriting the table.
- `DoubleDelta` codec on `timestamp` — optimal for monotonically increasing integers with regular spacing (HFT tick data). Compresses sequential microsecond timestamps by ~80% vs raw storage.
- `ZSTD(3)` on price/amount/symbol — ZSTD at level 3 favours compression ratio; suitable for columns written in large batches rather than read on the critical path.
- TTL 2 hours — intentionally short for this synthetic simulator. At 100 ticks/sec × 2 symbols the table would grow ~60 million rows/hour; the 2-hour window keeps storage bounded while still supporting 1m chart backfill.

---

#### `hft_dashboard.market_ohlc`

Pre-aggregated 1-minute OHLCV candles written by `fpga-hft-data-generator` on candle close.

```sql
ENGINE = ReplacingMergeTree()
ORDER BY (symbol, candle_time)
PARTITION BY toYYYYMM(candle_time)
TTL toDateTime(candle_time) + INTERVAL 90 DAY DELETE
```

| Column | Type | Codec | Notes |
|---|---|---|---|
| `symbol` | String | ZSTD(3) | |
| `candle_time` | DateTime64(6) | DoubleDelta + ZSTD(1) | Minute boundary (UTC) |
| `open` | Decimal64(8) | ZSTD(3) | First price in candle |
| `high` | Decimal64(8) | ZSTD(3) | |
| `low` | Decimal64(8) | ZSTD(3) | |
| `close` | Decimal64(8) | ZSTD(3) | Last price in candle |
| `volume` | Decimal64(8) | ZSTD(3) | Sum of `amount` in candle |
| `change_1h` | Decimal64(8) | ZSTD(3) | % change vs price 1h ago |
| `change_24h` | Decimal64(8) | ZSTD(3) | % change vs price 24h ago |

**Design rationale:**
- `ReplacingMergeTree()` — ClickHouse has no UPDATE. Candle values for an open (in-progress) minute change with every tick. Re-inserting a new row with the same `(symbol, candle_time)` key and reading with `FINAL` forces deduplication, providing upsert semantics without a transactional database.
- 90-day TTL — chart data beyond 3 months is rarely needed for a trading dashboard. Longer retention would require either larger monthly partitions or a separate cold-storage table.

---

#### `hft_dashboard.historical_trades_mv_*` (Materialized Views)

Five `AggregatingMergeTree` views that incrementally maintain OHLCV aggregates at each resolution. Populated automatically on every INSERT to `historical_trades`.

| View | Interval function | Typical query window |
|---|---|---|
| `historical_trades_mv_1m` | `toStartOfMinute` | 24 hours |
| `historical_trades_mv_5m` | `toStartOfInterval(..., INTERVAL 5 MINUTE)` | 5 days |
| `historical_trades_mv_15m` | `toStartOfInterval(..., INTERVAL 15 MINUTE)` | 15 days |
| `historical_trades_mv_1h` | `toStartOfHour` | 30 days |
| `historical_trades_mv_1d` | `toStartOfDay` | 3 months |

Each view stores partial aggregate states:

| Aggregate state | Resolves to | Query function |
|---|---|---|
| `argMinState(price, timestamp)` | First price (open) | `argMinMerge(open)` |
| `maxState(price)` | Highest price (high) | `maxMerge(high)` |
| `minState(price)` | Lowest price (low) | `minMerge(low)` |
| `argMaxState(price, timestamp)` | Last price (close) | `argMaxMerge(close)` |
| `sumState(amount)` | Total volume | `sumMerge(volume)` |

**Design rationale:**
- Without MVs, every chart load would `GROUP BY toStartOfInterval(timestamp, ...)` across potentially millions of raw tick rows. At 100 ticks/sec × 2 symbols the 1h view alone covers ~720,000 raw rows. The MV reduces this to a `GROUP BY` across ~60 pre-merged candles.
- `AggregatingMergeTree` stores binary intermediate state (not final values). Multiple inserts in the same candle window are merged in the background — the `*Merge` functions at query time finalise the state into usable values.
- Backfill: MVs only capture new inserts from creation time. The SQL file (`db/queries/materialized_views_multi.sql`) includes commented INSERT...SELECT backfill queries that re-aggregate existing `historical_trades` data — run these once on first deploy.

---

#### `exchange.users`

```sql
ENGINE = ReplacingMergeTree(created_at)
ORDER BY id
```

| Column | Type | Notes |
|---|---|---|
| `id` | String | UUID |
| `username` | String | Uniqueness enforced in-process (Mutex), not at DB level |
| `password_hash` | String | bcrypt |
| `role` | String | `'admin'` / `'trader'` |
| `balance_usdc` | String | Stored as decimal string to avoid float precision loss |
| `created_at` | DateTime64(6) | Version column — higher value wins on merge |
| `is_active` | UInt8 | Soft-delete flag |

**Design rationale:**
- `ReplacingMergeTree(created_at)` — balance updates re-insert a new row with a later `created_at`. The higher timestamp wins at merge time. All reads use `FINAL` to force dedup before background merges complete.
- `balance_usdc` as String — ClickHouse stores Decimal, but the exchange-sim serialises balances as decimal strings to avoid f64 rounding. Rust `rust_decimal::Decimal` parses the string at read time.
- No partition — the users table will have at most thousands of rows. Partitioning at this scale adds overhead with no benefit.

---

#### `exchange.orders`

```sql
ENGINE = ReplacingMergeTree(updated_at)
ORDER BY (user_id, id)
PARTITION BY toYYYYMM(created_at)
```

| Column | Type | Notes |
|---|---|---|
| `id` | String | UUID |
| `user_id` | String | FK to `exchange.users.id` |
| `symbol` | String | `'SOL/USDC'` etc. |
| `side` | String | `'buy'` / `'sell'` |
| `order_type` | String | `'market'` / `'limit'` |
| `price` | String | Fill price; `'0'` for pending limit orders |
| `amount` | String | Quantity in base asset |
| `total_usdc` | String | `price × amount` |
| `limit_price` | String | Empty for market orders |
| `status` | String | `'filled'` / `'rejected'` / `'pending'` / `'canceled'` |
| `reject_reason` | String | Empty when not rejected |
| `realized_pnl` | String | Empty for buys and pending orders |
| `created_at` | DateTime64(6) | Immutable — partition key |
| `updated_at` | DateTime64(6) | Version column |

**Design rationale:**
- `ReplacingMergeTree(updated_at)` — limit orders transition through states (`pending` → `filled` or `canceled`). Each transition re-inserts the row with a newer `updated_at`; `FINAL` at read time returns only the latest state.
- `ORDER BY (user_id, id)` — the most common read pattern is `WHERE user_id = ?`. Placing `user_id` first in the sort key makes per-user order history scans read a contiguous range of sorted data.
- `PARTITION BY toYYYYMM(created_at)` — monthly partitions bound the scan for time-filtered admin queries and enable partition-level archiving.

---

#### `exchange.positions`

```sql
ENGINE = ReplacingMergeTree(updated_at)
ORDER BY (user_id, symbol)
```

| Column | Type | Notes |
|---|---|---|
| `user_id` | String | |
| `symbol` | String | |
| `quantity` | String | Base asset held; `'0'` means no position |
| `avg_buy_price` | String | Weighted average cost basis |
| `updated_at` | DateTime64(6) | Version column |

**Design rationale:**
- `ORDER BY (user_id, symbol)` — position queries always filter on `user_id`. Positions are few per user (one per symbol) so no partition is needed.
- Non-zero filter: reads use `WHERE toFloat64OrZero(quantity) > 0` to exclude closed (zeroed) positions from the result set without physically deleting rows.

---

### DB Interactions — Cross-Service Summary

| Operation | Service | Table | Pattern |
|---|---|---|---|
| Tick INSERT (batched) | fpga-hft-data-generator | `hft_dashboard.historical_trades` | Buffered, flush every 1s or 1000 rows |
| Candle INSERT | fpga-hft-data-generator | `hft_dashboard.market_ohlc` | On minute boundary, per symbol |
| MV population | ClickHouse (automatic) | `historical_trades_mv_*` | Triggered by every INSERT to `historical_trades` |
| 1h/24h change poll | fpga-hft-data-generator | `hft_dashboard.historical_trades` | `SELECT price ... ORDER BY timestamp ASC LIMIT 1` every 5s |
| OHLCV read | fpga-hft-data-generator | `hft_dashboard.market_ohlc` / MVs | On `GET /api/v1/ohlcv/{symbol}?interval=` |
| Candle backfill | fpga-hft-data-generator | `hft_dashboard.market_ohlc` | INSERT...SELECT on startup |
| User register | exchange-sim | `exchange.users` | INSERT new row |
| User login | exchange-sim | `exchange.users` | SELECT FINAL WHERE username = ? |
| Balance update | exchange-sim | `exchange.users` | Re-INSERT with new `balance_usdc` + later `created_at` |
| Order place | exchange-sim | `exchange.orders` | INSERT with `status='filled'` (market) or `'pending'` (limit) |
| Order fill (limit) | exchange-sim | `exchange.orders` | Re-INSERT with `status='filled'`, exec price, `updated_at=now()` |
| Order cancel | exchange-sim | `exchange.orders` | Re-INSERT with `status='canceled'` |
| Order history | exchange-sim | `exchange.orders` | SELECT FINAL WHERE user_id = ? ORDER BY created_at DESC LIMIT N |
| Position upsert | exchange-sim | `exchange.positions` | Re-INSERT with new quantity/avg_price |
| Position read | exchange-sim | `exchange.positions` | SELECT FINAL WHERE user_id = ? AND quantity > 0 |

---

### Query Performance Notes

#### Design choices that prevent slow queries at scale

**1. ORDER BY prefix alignment**
Every production query filters on the leading column(s) of the ORDER BY key before applying any other predicate. Violating this (e.g. `WHERE symbol = ?` on a table whose key starts with `timestamp`) forces a full-table scan across all granules.

| Table | ORDER BY | Common filter — aligned? |
|---|---|---|
| `historical_trades` | `(symbol, timestamp)` | `WHERE symbol = ? AND timestamp >= ?` — yes |
| `market_ohlc` | `(symbol, candle_time)` | `WHERE symbol = ? AND candle_time >= ?` — yes |
| `exchange.orders` | `(user_id, id)` | `WHERE user_id = ?` — yes |
| `exchange.positions` | `(user_id, symbol)` | `WHERE user_id = ?` — yes |

**2. FINAL keyword — cost and when to use it**
`FINAL` forces read-time deduplication on `ReplacingMergeTree` tables. Without it, stale duplicate rows may be returned before background merges run.

Cost: ClickHouse reads all versions of each row, compares version columns, and discards losers. On a table with millions of rows and high re-insert rate this can be 2–5× slower than a plain SELECT.

Mitigation strategies for large deployments:
- `OPTIMIZE TABLE exchange.orders FINAL` — forces a merge, making subsequent SELECTs cheaper. Run during low-traffic windows.
- Point lookups (`WHERE id = ? LIMIT 1`) have low FINAL overhead because only a few granules are read.
- For admin full-table scans (`list_all_orders`) on large datasets, consider adding a `created_at` range filter to limit the partition scan before FINAL deduplication runs.

**3. Materialized views eliminate GROUP BY on raw ticks**
The 5 OHLCV MVs replace on-the-fly aggregation. Approximate query cost comparison at scale (2 symbols × 30 days × 100 ticks/sec):

| Approach | Rows scanned for 1h of 5m candles |
|---|---|
| GROUP BY on `historical_trades` | ~720,000 raw tick rows |
| SELECT from `historical_trades_mv_5m` | ~24 pre-merged candle rows |

The MV fallback (on-the-fly GROUP BY) is used only when the MV is empty — during the first minutes after deployment before backfill runs.

**4. Batched inserts**
`fpga-hft-data-generator` buffers trade rows and flushes in batches (1,000 rows or 1 second, whichever comes first). ClickHouse is optimised for bulk inserts; single-row inserts at 100/sec would create ~360,000 small parts per hour, overwhelming the background merger and eventually causing `Too many parts` errors.

**5. Partition pruning**
Monthly partitions on `historical_trades` and `exchange.orders` mean queries with a `timestamp >= now() - INTERVAL N MINUTE` condition only open the current (and possibly previous) month's partition files. For the TTL-bounded `historical_trades` table (2-hour window), the entire active dataset fits in one partition.

**6. Potential optimizations for future scale**

| Scenario | Optimization |
|---|---|
| `exchange.users FINAL` slow with many balance updates | Add `OPTIMIZE TABLE exchange.users FINAL` to a nightly cron |
| `list_all_orders` admin scan slow | Add `WHERE created_at >= ?` filter; expose `from_date` query param |
| `historical_trades` hot reads | Add a `skip index` on `price` (`minmax`, granularity 4) for range queries |
| `positions` query slow under many symbols | Already ORDER BY `(user_id, symbol)` — add PREWHERE on `user_id` |
| MV query slow after high-volume insert burst | Run `OPTIMIZE TABLE historical_trades_mv_5m FINAL` to force merge of pending parts |
| Tick volume grows beyond 100/sec | Switch `historical_trades` to `PARTITION BY toYYYYMMDD(timestamp)` for finer partition pruning |
