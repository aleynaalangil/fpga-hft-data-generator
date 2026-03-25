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
/// GET /v1/feed?rate_ms=N — WebSocket endpoint for live market data streaming.
/// Sends a JSON `MarketDataMessage` for each symbol on every tick.
pub async fn feed(
    req: HttpRequest,
    stream: web::Payload,
    broadcast_tx: web::Data<broadcast::Sender<String>>,
) -> Result<HttpResponse, actix_web::Error> {
    // 1. Properly handle the WebSocket upgrade
    let (response, mut session, mut msg_stream) = actix_ws::handle(&req, stream)?;
    let mut rx = broadcast_tx.subscribe();

    actix_web::rt::spawn(async move {
        let mut last_heartbeat = Instant::now();
        let mut interval = tokio::time::interval(Duration::from_secs(5));

        loop {
            tokio::select! {
                // BRANCH 1: Heartbeat Timer
                _ = interval.tick() => {
                    // Send ping every 20s to prevent stale connections
                    if Instant::now().duration_since(last_heartbeat) > Duration::from_secs(20) {
                        if session.ping(b"").await.is_err() {
                            break;
                        }
                        last_heartbeat = Instant::now();
                    }
                }

                // BRANCH 2: Handle Incoming Client Messages (Pongs/Close)
                Some(Ok(msg)) = msg_stream.next() => {
                    match msg {
                        Message::Pong(_) => last_heartbeat = Instant::now(),
                        Message::Close(_) => break,
                        _ => (),
                    }
                }

                // BRANCH 3: Real-time Market Data Stream
                // This now fires IMMEDIATELY when a tick is broadcast
                res = rx.recv() => {
                    match res {
                        Ok(msg_json) => {
                            if session.text(msg_json).await.is_err() {
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(count)) => {
                            warn!("Client lagged by {} messages", count);
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            break;
                        }
                    }
                }
            }
        }
    });

    Ok(response)
}
