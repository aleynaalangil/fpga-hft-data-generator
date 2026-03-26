use actix_web::{HttpRequest, HttpResponse, error, http::StatusCode, web};
use actix_ws::Message;
use futures_util::StreamExt;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tracing::warn;

#[derive(thiserror::Error, Debug)]
pub enum HftError {
    #[error("ClickHouse connection failed")]
    DbError(#[from] clickhouse::error::Error),
    #[error("Symbol not found: {0}")]
    SymbolNotFound(String),
}

impl error::ResponseError for HftError {
    fn status_code(&self) -> StatusCode {
        match *self {
            HftError::SymbolNotFound(_) => StatusCode::NOT_FOUND,
            HftError::DbError(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn error_response(&self) -> HttpResponse {
        HttpResponse::build(self.status_code())
            .json(serde_json::json!({ "error": self.to_string() }))
    }
}

/// Configuration passed into the WebSocket handler.
#[derive(Clone)]
pub struct WsConfig {
    /// Seconds since last Pong before the connection is forcibly closed.
    pub heartbeat_timeout_secs: u64,
    /// Seconds between outgoing Ping frames.
    pub ping_interval_secs: u64,
}

impl Default for WsConfig {
    fn default() -> Self {
        Self {
            heartbeat_timeout_secs: 60,
            ping_interval_secs: 20,
        }
    }
}

/// GET /v1/feed — WebSocket endpoint for live market data streaming.
/// Sends a JSON `MarketDataMessage` for each symbol on every tick.
pub async fn feed(
    req: HttpRequest,
    stream: web::Payload,
    broadcast_tx: web::Data<broadcast::Sender<String>>,
    ws_cfg: web::Data<WsConfig>,
) -> Result<HttpResponse, actix_web::Error> {
    let (response, mut session, mut msg_stream) = actix_ws::handle(&req, stream)?;
    let mut rx = broadcast_tx.subscribe();

    let heartbeat_timeout = Duration::from_secs(ws_cfg.heartbeat_timeout_secs);
    let ping_interval_dur = Duration::from_secs(ws_cfg.ping_interval_secs);
    // Check heartbeat more frequently than the timeout so we react promptly
    let check_interval = Duration::from_secs(ws_cfg.ping_interval_secs / 2).max(Duration::from_secs(5));

    // Track this connection in Prometheus
    crate::db::WS_CLIENT_GAUGE.inc();

    actix_web::rt::spawn(async move {
        let mut last_heartbeat = Instant::now();
        let mut interval = tokio::time::interval(check_interval);

        'conn: loop {
            tokio::select! {
                // BRANCH 1: Heartbeat timer
                _ = interval.tick() => {
                    let elapsed = Instant::now().duration_since(last_heartbeat);

                    // Forcibly close stale connections that never sent a Pong
                    if elapsed > heartbeat_timeout {
                        warn!("WebSocket client timed out after {:?}, closing", elapsed);
                        let _ = session.close(None).await;
                        break 'conn;
                    }

                    // Send a Ping when we approach the ping interval
                    // last_heartbeat is only reset on Pong receipt, not here,
                    // so a silent client will eventually hit the timeout above.
                    if elapsed > ping_interval_dur {
                        if session.ping(b"").await.is_err() {
                            break 'conn;
                        }
                    }
                }

                // BRANCH 2: Handle incoming client messages (Pong / Close)
                Some(Ok(msg)) = msg_stream.next() => {
                    match msg {
                        Message::Pong(_) => {
                            // Reset heartbeat timer only on an actual Pong response
                            last_heartbeat = Instant::now();
                        }
                        Message::Close(_) => break 'conn,
                        _ => (),
                    }
                }

                // BRANCH 3: Real-time market data stream
                res = rx.recv() => {
                    match res {
                        Ok(msg_json) => {
                            if session.text(msg_json).await.is_err() {
                                break 'conn;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(count)) => {
                            warn!("WebSocket client lagged by {} messages", count);
                            crate::db::WS_LAGGED_COUNTER.inc();
                            // Evict persistently slow clients (lag > half channel capacity)
                            if count > 64 {
                                warn!("Evicting lagged WebSocket client (lag={})", count);
                                let _ = session.close(None).await;
                                break 'conn;
                            }
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            break 'conn;
                        }
                    }
                }
            }
        }

        // Decrement client count regardless of how we exited the loop
        crate::db::WS_CLIENT_GAUGE.dec();
    });

    Ok(response)
}
