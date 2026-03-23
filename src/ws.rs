use crate::generator::MarketGenerator;
use actix_web::{web, HttpRequest, HttpResponse};

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::time::{Duration, interval};

type AppState = web::Data<Arc<Mutex<HashMap<String, MarketGenerator>>>>;

#[derive(serde::Deserialize)]
pub struct FeedQuery {
    /// Tick interval in milliseconds (default: 10ms = 100 ticks/sec)
    rate_ms: Option<u64>,
}

/// GET /v1/feed?rate_ms=N — WebSocket endpoint for live market data streaming.
/// Sends a JSON `MarketDataMessage` for each symbol on every tick.
pub async fn feed(
    req: HttpRequest,
    stream: web::Payload,
    state: AppState,
    query: web::Query<FeedQuery>,
) -> Result<HttpResponse, actix_web::Error> {
    let (response, mut session, _msg_stream) = actix_ws::handle(&req, stream)?;

    let rate_ms = query.rate_ms.unwrap_or(10);
    let state = state.clone();

    // Spawn a task that pushes market data to the client at the configured rate
    actix_web::rt::spawn(async move {
        let mut tick_interval = interval(Duration::from_millis(rate_ms));

        loop {
            tick_interval.tick().await;

            let messages: Vec<String> = {
                let gens = match state.lock() {
                    Ok(g) => g,
                    Err(_) => break,
                };
                gens.values()
                    .map(|generator| {
                        let msg = generator.to_ws_message();
                        serde_json::to_string(&msg).unwrap_or_default()
                    })
                    .collect()
            };

            for msg_json in messages {
                if session.text(msg_json).await.is_err() {
                    // Client disconnected
                    return;
                }
            }
        }
    });

    Ok(response)
}
