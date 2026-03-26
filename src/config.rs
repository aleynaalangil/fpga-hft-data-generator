use serde::Deserialize;
use tracing::info;

/// Per-symbol GBM parameters. All fields are required when defined in config.toml.
#[derive(Debug, Clone, Deserialize)]
pub struct SymbolConfig {
    pub name: String,
    pub initial_price: f64,
    pub drift: f64,
    pub volatility: f64,
    pub spread: f64,
}

/// Top-level application configuration.
/// Load via `AppConfig::load()`. All fields have defaults so the service
/// works with no config file present.
#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    /// HTTP listen port. Overridden by env var PORT.
    pub port: u16,
    /// Tick generation interval in milliseconds.
    pub tick_interval_ms: u64,
    /// How often the DB inserter flushes on a timer (seconds).
    pub db_flush_interval_secs: u64,
    /// Maximum trades buffered before a forced flush.
    pub db_insert_buffer_size: usize,
    /// Ring-buffer depth per symbol (number of raw ticks kept in memory).
    pub tick_history_size: usize,
    /// Broadcast channel capacity for WebSocket fan-out.
    pub ws_broadcast_capacity: usize,
    /// How often to poll ClickHouse for 1h/24h price change (seconds).
    pub change_poll_interval_secs: u64,
    /// Seconds since last Pong before a WebSocket client is forcibly closed.
    pub ws_heartbeat_timeout_secs: u64,
    /// Seconds between Ping frames sent to idle WebSocket clients.
    pub ws_ping_interval_secs: u64,
    /// REST API: sustained requests per second per IP.
    pub rate_limit_per_second: u64,
    /// REST API: burst allowance above the sustained rate.
    pub rate_limit_burst: u32,
    /// Trading pairs to simulate.
    pub symbols: Vec<SymbolConfig>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            port: 8080,
            tick_interval_ms: 10,
            db_flush_interval_secs: 1,
            db_insert_buffer_size: 1_000,
            tick_history_size: 1_200,
            ws_broadcast_capacity: 128,
            change_poll_interval_secs: 5,
            ws_heartbeat_timeout_secs: 60,
            ws_ping_interval_secs: 20,
            rate_limit_per_second: 100,
            rate_limit_burst: 50,
            symbols: vec![
                SymbolConfig {
                    name: "SOL/USDC".to_string(),
                    initial_price: 150.0,
                    drift: 0.0001,
                    volatility: 0.002,
                    spread: 0.01,
                },
                SymbolConfig {
                    name: "BTC/USDC".to_string(),
                    initial_price: 65_000.0,
                    drift: 0.0001,
                    volatility: 0.001,
                    spread: 10.0,
                },
            ],
        }
    }
}

impl AppConfig {
    /// Load configuration from `config.toml` if present, then override
    /// `port` from the `PORT` environment variable.
    ///
    /// Falls back to compiled-in defaults when no file exists, so the
    /// service starts correctly with zero configuration.
    pub fn load() -> Self {
        let mut cfg = match std::fs::read_to_string("config.toml") {
            Ok(contents) => match toml::from_str::<AppConfig>(&contents) {
                Ok(parsed) => {
                    info!("Loaded configuration from config.toml");
                    parsed
                }
                Err(e) => {
                    tracing::warn!("Failed to parse config.toml ({}), using defaults", e);
                    AppConfig::default()
                }
            },
            Err(_) => {
                info!("No config.toml found, using built-in defaults");
                AppConfig::default()
            }
        };

        // Environment variable overrides (only PORT for now — add more as needed)
        if let Ok(port_str) = std::env::var("PORT") {
            if let Ok(port) = port_str.parse::<u16>() {
                cfg.port = port;
            }
        }

        cfg
    }
}
