use crate::models::*;
use chrono::Utc;
use rand::Rng;
use rand_distr::{Distribution, LogNormal, Normal};
use rust_decimal::Decimal;
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use std::collections::VecDeque;
use uuid::Uuid;

/// MarketGenerator implements Geometric Brownian Motion (GBM) for realistic
/// price simulation. Each symbol gets its own generator with independent state.
pub struct MarketGenerator {
    pub symbol: String,
    current_price: f64,
    drift: f64,      // μ — slight directional bias
    volatility: f64, // σ — step size variance
    spread: f64,     // bid/ask spread width
    pub change_1h: Option<f64>,
    pub change_24h: Option<f64>,
    last_candle_minute: Option<String>,
    current_candle: Option<OhlcvBar>,
    prev_candle: Option<OhlcvBar>,
    pub tick_history: VecDeque<MarketTick>,
    max_history: usize,
}

impl MarketGenerator {
    pub fn new(
        symbol: &str,
        start_price: f64,
        drift: f64,
        volatility: f64,
        spread: f64,
        max_history: usize,
    ) -> Self {
        Self {
            symbol: symbol.to_string(),
            current_price: start_price,
            drift,
            volatility,
            spread,
            change_1h: None,
            change_24h: None,
            last_candle_minute: None,
            current_candle: None,
            prev_candle: None,
            tick_history: VecDeque::new(),
            max_history,
        }
    }

    /// Advance the random walk by one step using GBM:
    ///   dS = S × (μ·dt + σ·√dt · Z)
    /// where Z ~ N(0,1) sampled from a true Gaussian distribution.
    pub fn advance(&mut self) -> MarketTick {
        let mut rng = rand::thread_rng();
        let dt: f64 = 0.01; // time step

        // True N(0,1) — unbounded tails, correct for rare multi-sigma events
        let normal = Normal::new(0.0_f64, 1.0_f64).unwrap();
        let z: f64 = normal.sample(&mut rng);

        let ds = self.current_price * (self.drift * dt + self.volatility * dt.sqrt() * z);
        self.current_price += ds;

        // Clamp to prevent negative prices
        if self.current_price < 0.01 {
            self.current_price = 0.01;
        }

        let side = if rng.gen_bool(0.5) { "buy" } else { "sell" };
        let amount = rng.gen_range(0.01..500.0);

        let tick = MarketTick {
            symbol: self.symbol.clone(),
            side: side.to_string(),
            price: Decimal::from_f64(self.current_price)
                .unwrap_or(Decimal::ZERO)
                .round_dp(8),
            amount: Decimal::from_f64(amount)
                .unwrap_or(Decimal::ZERO)
                .round_dp(8),
            timestamp: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
            order_id: Uuid::new_v4().to_string(),
            trader_id: rng.gen_range(1..=9999),
        };

        // Store in history ring buffer
        if self.tick_history.len() >= self.max_history {
            self.tick_history.pop_front();
        }
        self.tick_history.push_back(tick.clone());
        self.update_current_candle(&tick);

        tick
    }

    /// Get the latest tick (if any).
    pub fn latest_tick(&self) -> Option<&MarketTick> {
        self.tick_history.back()
    }

    /// Derive a BBO snapshot from the current price using u64 fixed-point,
    /// including a Level 2 depth of 20 levels.
    ///
    /// Level spacing: cumulative geometric steps (~2% per level from mid)
    /// — more realistic than uniform random jitter.
    ///
    /// Level sizes: sampled from LogNormal(μ=4, σ=1.2), giving heavy-tailed
    /// size distribution representative of real limit order books.
    pub fn current_bbo(&self) -> BboSnapshot {
        let mut rng = rand::thread_rng();
        let half_spread = self.spread / 2.0;
        let best_bid_price = self.current_price - half_spread;
        let best_ask_price = self.current_price + half_spread;

        // Log-normal size distribution: median ≈ e^4 ≈ 54 units
        let size_dist = LogNormal::new(4.0_f64, 1.2_f64).unwrap();
        // Tick size scales with spread so BTC and SOL get appropriate granularity
        let tick_size = (self.spread / 20.0).max(0.01);

        let mut bids = Vec::with_capacity(20);
        let mut asks = Vec::with_capacity(20);

        let mut bid_cumulative_offset = 0.0_f64;
        let mut ask_cumulative_offset = 0.0_f64;

        for i in 0..20_i32 {
            // Geometric step: each level is ~2% further from mid than the previous
            let step = tick_size * 1.02_f64.powi(i);
            bid_cumulative_offset += step;
            ask_cumulative_offset += step;

            let bid_p = best_bid_price - bid_cumulative_offset;
            let ask_p = best_ask_price + ask_cumulative_offset;

            bids.push(OrderBookLevel {
                price: to_fixed_point(bid_p.max(0.01)),
                size: to_fixed_point(size_dist.sample(&mut rng)),
            });

            asks.push(OrderBookLevel {
                price: to_fixed_point(ask_p),
                size: to_fixed_point(size_dist.sample(&mut rng)),
            });
        }

        BboSnapshot {
            symbol: self.symbol.clone(),
            best_bid: bids[0].price,
            best_ask: asks[0].price,
            bid_size: bids[0].size,
            ask_size: asks[0].size,
            spread: to_fixed_point(self.spread),
            bids,
            asks,
            timestamp: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
        }
    }

    /// Build OHLCV candles from tick history, grouped by minute.
    pub fn build_ohlcv(&self, minutes: usize) -> Vec<OhlcvBar> {
        if self.tick_history.is_empty() {
            return vec![];
        }

        let mut candles: Vec<OhlcvBar> = Vec::new();
        let mut current: Option<OhlcvBar> = None;

        for tick in &self.tick_history {
            let candle_time = if tick.timestamp.len() >= 16 {
                format!("{}:00.000000Z", &tick.timestamp[..16])
            } else {
                tick.timestamp.clone()
            };

            match &mut current {
                Some(bar) if bar.candle_time == candle_time => {
                    if tick.price > bar.high {
                        bar.high = tick.price;
                    }
                    if tick.price < bar.low {
                        bar.low = tick.price;
                    }
                    bar.close = tick.price;
                    bar.volume += tick.amount;
                }
                _ => {
                    if let Some(completed) = current.take() {
                        candles.push(completed);
                    }
                    current = Some(OhlcvBar {
                        symbol: self.symbol.clone(),
                        candle_time,
                        open: tick.price,
                        high: tick.price,
                        low: tick.price,
                        close: tick.price,
                        volume: tick.amount,
                    });
                }
            }
        }

        if let Some(last) = current {
            candles.push(last);
        }

        let len = candles.len();
        if len > minutes {
            candles.drain(..len - minutes);
        }

        candles
    }

    /// Get the current price as f64.
    pub fn current_price(&self) -> f64 {
        self.current_price
    }

    /// Build a WebSocket message.
    /// `actual_latency_ms` is the measured tick generation time in milliseconds.
    /// `actual_tps` is the nominal throughput derived from the configured tick interval.
    pub fn to_ws_message(&self, actual_latency_ms: f64, actual_tps: u32) -> MarketDataMessage {
        let latest = self.latest_tick();

        let telemetry = SystemTelemetry {
            latency: actual_latency_ms,
            throughput_tps: actual_tps,
            // error_rate is tracked via the hft_db_errors_total Prometheus counter;
            // it is not re-derived here to avoid coupling the hot tick path to metrics reads.
            error_rate: 0.0,
        };

        let ohlc = self.current_candle.clone();

        MarketDataMessage {
            price: self.current_price,
            volume: latest.and_then(|t| t.amount.to_f64()).unwrap_or(0.0),
            symbol: self.symbol.clone(),
            change_1h: self.change_1h,
            change_24h: self.change_24h,
            bbo: Some(self.current_bbo()),
            tick: latest.cloned(),
            ohlc,
            telemetry,
        }
    }

    /// Check if the latest tick closed a minute candle, and return that candle.
    pub fn check_candle_closure(&mut self) -> Option<OhlcvBar> {
        let latest_tick = self.latest_tick()?;
        if latest_tick.timestamp.len() < 16 {
            return None;
        }
        let current_min = &latest_tick.timestamp[..16];

        if let Some(last_min) = &self.last_candle_minute {
            if last_min != current_min {
                let closed_bar = self.prev_candle.clone();
                self.last_candle_minute = Some(current_min.to_string());
                return closed_bar;
            }
        } else {
            self.last_candle_minute = Some(current_min.to_string());
        }

        None
    }

    fn update_current_candle(&mut self, tick: &MarketTick) {
        let tick_min = &tick.timestamp[..16.min(tick.timestamp.len())];
        let new_bar = || OhlcvBar {
            symbol: self.symbol.clone(),
            candle_time: tick_min.to_string() + ":00.000000Z",
            open: tick.price,
            high: tick.price,
            low: tick.price,
            close: tick.price,
            volume: tick.amount,
        };

        match &mut self.current_candle {
            Some(bar) if bar.candle_time.starts_with(tick_min) => {
                if tick.price > bar.high {
                    bar.high = tick.price;
                }
                if tick.price < bar.low {
                    bar.low = tick.price;
                }
                bar.close = tick.price;
                bar.volume += tick.amount;
            }
            _ => {
                self.prev_candle = self.current_candle.take();
                self.current_candle = Some(new_bar());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_generator() -> MarketGenerator {
        MarketGenerator::new("SOL/USDC", 150.0, 0.0001, 0.002, 0.01, 1_200)
    }

    #[test]
    fn advance_price_stays_positive() {
        let mut gen = make_generator();
        for _ in 0..10_000 {
            let tick = gen.advance();
            assert!(
                tick.price > Decimal::ZERO,
                "price went non-positive: {}",
                tick.price
            );
        }
    }

    #[test]
    fn bbo_best_ask_greater_than_best_bid() {
        let gen = make_generator();
        let bbo = gen.current_bbo();
        assert!(
            bbo.best_ask > bbo.best_bid,
            "best_ask {} must be > best_bid {}",
            bbo.best_ask,
            bbo.best_bid
        );
    }

    #[test]
    fn bbo_has_20_levels() {
        let gen = make_generator();
        let bbo = gen.current_bbo();
        assert_eq!(bbo.bids.len(), 20);
        assert_eq!(bbo.asks.len(), 20);
    }

    #[test]
    fn build_ohlcv_limits_candles() {
        let mut gen = make_generator();
        for _ in 0..200 {
            gen.advance();
        }
        // build_ohlcv(1) must return at most 1 candle
        let candles = gen.build_ohlcv(1);
        assert!(candles.len() <= 1, "expected at most 1 candle, got {}", candles.len());
    }

    #[test]
    fn tick_history_respects_max() {
        let mut gen = MarketGenerator::new("TEST/USD", 100.0, 0.0, 0.001, 0.01, 10);
        for _ in 0..50 {
            gen.advance();
        }
        assert_eq!(gen.tick_history.len(), 10);
    }

    #[test]
    fn ohlcv_high_gte_low() {
        let mut gen = make_generator();
        for _ in 0..300 {
            gen.advance();
        }
        for bar in gen.build_ohlcv(10) {
            assert!(
                bar.high >= bar.low,
                "high {} < low {} in candle {}",
                bar.high,
                bar.low,
                bar.candle_time
            );
        }
    }
}
