use crate::models::*;
use chrono::Utc;
use rand::Rng;
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
    tick_history: VecDeque<MarketTick>,
    max_history: usize,
}

impl MarketGenerator {
    pub fn new(symbol: &str, start_price: f64, drift: f64, volatility: f64, spread: f64) -> Self {
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
            max_history: 1_200,
        }
    }

    /// Advance the random walk by one step using GBM:
    ///   dS = S × (μ·dt + σ·√dt · Z)
    /// where Z ~ N(0,1) approximated via uniform distribution for speed.
    pub fn advance(&mut self) -> MarketTick {
        let mut rng = rand::thread_rng();
        let dt: f64 = 0.01; // time step

        // Box-Muller approximation: sum of 12 uniform - 6 ≈ N(0,1)
        let z: f64 = (0..12).map(|_| rng.r#gen::<f64>()).sum::<f64>() - 6.0;

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
    /// including a simulated Level 2 depth (20 levels).
    pub fn current_bbo(&self) -> BboSnapshot {
        let mut rng_bbo = rand::thread_rng();
        let half_spread = self.spread / 2.0;
        let best_bid_price = self.current_price - half_spread;
        let best_ask_price = self.current_price + half_spread;

        let mut bids = Vec::with_capacity(20);
        let mut asks = Vec::with_capacity(20);

        let tick_size = 0.01; // Mock tick size for ladder steps

        for i in 0..20 {
            let bid_p = best_bid_price - (i as f64 * tick_size * (rng_bbo.gen_range(0.8..1.2)));
            let ask_p = best_ask_price + (i as f64 * tick_size * (rng_bbo.gen_range(0.8..1.2)));

            bids.push(OrderBookLevel {
                price: to_fixed_point(bid_p),
                size: to_fixed_point(rng_bbo.gen_range(1.0..500.0) * (1.0 + i as f64 * 0.1)),
            });

            asks.push(OrderBookLevel {
                price: to_fixed_point(ask_p),
                size: to_fixed_point(rng_bbo.gen_range(1.0..500.0) * (1.0 + i as f64 * 0.1)),
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

    /// Build OHLCV candles from tick history, grouped into `interval_secs` buckets.
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
                tick.timestamp[..tick.timestamp.len()].to_string()
            };

            match &mut current {
                Some(bar) if bar.candle_time == candle_time => {
                    // Same minute — just update running OHLCV, zero allocations
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
                    // Minute boundary — push completed bar, open new one
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

        // Don't forget the last in-progress candle
        if let Some(last) = current {
            candles.push(last);
        }

        // Return only the last N minutes
        let len = candles.len();
        if len > minutes {
            candles.drain(..len - minutes); // drain is cheaper than split_off
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

        let mut rng = rand::thread_rng();
        // Simulate a latency spike > 50ms roughly 5% of the time
        let latency = if rng.gen_bool(0.05) {
            rng.gen_range(51.0..120.0)
        } else {
            rng.gen_range(5.0..20.0)
        };

        let telemetry = SystemTelemetry {
            latency,
            throughput_tps: rng.gen_range(8000..12000),
            error_rate: rng.gen_range(0.0001..0.001),
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
                // Minute has changed! prev_candle holds the just-completed bar.
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
                // Minute rolled over — save completed candle, open new one
                self.prev_candle = self.current_candle.take();
                self.current_candle = Some(new_bar());
            }
        }
    }
}
