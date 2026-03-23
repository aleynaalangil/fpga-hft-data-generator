use crate::models::*;
use chrono::Utc;
use rand::Rng;
use rust_decimal::Decimal;
use rust_decimal::prelude::FromPrimitive;
use std::collections::VecDeque;
use uuid::Uuid;

/// MarketGenerator implements Geometric Brownian Motion (GBM) for realistic
/// price simulation. Each symbol gets its own generator with independent state.
pub struct MarketGenerator {
    pub symbol: String,
    current_price: f64,
    drift: f64,       // μ — slight directional bias
    volatility: f64,  // σ — step size variance
    spread: f64,      // bid/ask spread width
    tick_history: VecDeque<MarketTick>,
    max_history: usize,
}

impl MarketGenerator {
    pub fn new(
        symbol: &str,
        start_price: f64,
        drift: f64,
        volatility: f64,
        spread: f64,
    ) -> Self {
        Self {
            symbol: symbol.to_string(),
            current_price: start_price,
            drift,
            volatility,
            spread,
            tick_history: VecDeque::with_capacity(10_000),
            max_history: 10_000,
        }
    }

    /// Advance the random walk by one step using GBM:
    ///   dS = S × (μ·dt + σ·√dt · Z)
    /// where Z ~ N(0,1) approximated via uniform distribution for speed.
    pub fn advance(&mut self) -> MarketTick {
        let mut rng = rand::thread_rng();
        let dt: f64 = 0.01; // time step

        // Box-Muller approximation: sum of 12 uniform - 6 ≈ N(0,1)
        let z: f64 = (0..12)
            .map(|_| rng.r#gen::<f64>())
            .sum::<f64>()
            - 6.0;

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
            timestamp: Utc::now().format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string(),
            order_id: Uuid::new_v4().to_string(),
            trader_id: rng.gen_range(1..=9999),
        };

        // Store in history ring buffer
        if self.tick_history.len() >= self.max_history {
            self.tick_history.pop_front();
        }
        self.tick_history.push_back(tick.clone());

        tick
    }

    /// Get the latest tick (if any).
    pub fn latest_tick(&self) -> Option<&MarketTick> {
        self.tick_history.back()
    }

    /// Derive a BBO snapshot from the current price using u64 fixed-point.
    pub fn current_bbo(&self) -> BboSnapshot {
        let mut rng_bbo = rand::thread_rng();
        let half_spread = self.spread / 2.0;
        let bid = self.current_price - half_spread;
        let ask = self.current_price + half_spread;

        BboSnapshot {
            symbol: self.symbol.clone(),
            best_bid: to_fixed_point(bid),
            best_ask: to_fixed_point(ask),
            bid_size: to_fixed_point(rng_bbo.gen_range(1.0..1000.0)),
            ask_size: to_fixed_point(rng_bbo.gen_range(1.0..1000.0)),
            spread: to_fixed_point(self.spread),
            timestamp: Utc::now().format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string(),
        }
    }

    /// Build OHLCV candles from tick history, grouped into `interval_secs` buckets.
    pub fn build_ohlcv(&self, minutes: usize) -> Vec<OhlcvBar> {
        if self.tick_history.is_empty() {
            return vec![];
        }

        // Group ticks by minute bucket
        let mut buckets: std::collections::BTreeMap<String, Vec<&MarketTick>> =
            std::collections::BTreeMap::new();

        for tick in &self.tick_history {
            // Truncate timestamp to start of minute
            let candle_time = if tick.timestamp.len() >= 16 {
                format!("{}:00.000000Z", &tick.timestamp[..16])
            } else {
                tick.timestamp.clone()
            };
            buckets.entry(candle_time).or_default().push(tick);
        }

        let mut candles: Vec<OhlcvBar> = buckets
            .into_iter()
            .map(|(candle_time, ticks)| {
                let open = ticks.first().unwrap().price;
                let close = ticks.last().unwrap().price;
                let high = ticks.iter().map(|t| t.price).max().unwrap_or(open);
                let low = ticks.iter().map(|t| t.price).min().unwrap_or(open);
                let volume: Decimal = ticks.iter().map(|t| t.amount).sum();

                OhlcvBar {
                    symbol: self.symbol.clone(),
                    candle_time,
                    open,
                    high,
                    low,
                    close,
                    volume,
                }
            })
            .collect();

        // Return only the last N minutes
        let len = candles.len();
        if len > minutes {
            candles = candles.split_off(len - minutes);
        }

        candles
    }

    /// Get the current price as f64 (for WebSocket messages).
    pub fn current_price(&self) -> f64 {
        self.current_price
    }

    /// Build a WebSocket message matching the frontend store shape.
    pub fn to_ws_message(&self) -> MarketDataMessage {
        let latest = self.latest_tick();
        MarketDataMessage {
            price: self.current_price,
            volume: latest
                .and_then(|t| t.amount.to_string().parse::<f64>().ok())
                .unwrap_or(0.0),
            symbol: self.symbol.clone(),
        }
    }
}
