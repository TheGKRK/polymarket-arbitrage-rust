//! Axum web server for the trading dashboard (mirrors the FastAPI routes in
//! `dashboard/server.py`): the same 6 JSON endpoints plus `/ws`, and the
//! same single-page HTML app served at `/`.

use super::state::Dashboard;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::Router;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tracing::error;

/// The dashboard's frontend: a single static HTML/CSS/JS page extracted
/// verbatim from Python's `get_embedded_html()`. It only talks to the
/// generic JSON endpoints and `/ws` below, so it needed no changes.
const INDEX_HTML: &str = include_str!("index.html");

pub fn build_router(dashboard: Arc<Dashboard>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/state", get(get_state))
        .route("/api/markets", get(get_markets))
        .route("/api/opportunities", get(get_opportunities))
        .route("/api/portfolio", get(get_portfolio))
        .route("/api/risk", get(get_risk))
        .route("/api/timing", get(get_timing))
        .route("/ws", get(ws_handler))
        .with_state(dashboard)
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn get_state(State(dashboard): State<Arc<Dashboard>>) -> axum::Json<Value> {
    axum::Json(dashboard.to_json().await)
}

async fn get_markets(State(dashboard): State<Arc<Dashboard>>) -> axum::Json<Value> {
    let markets = dashboard.read_state(|s| json!(s.markets)).await;
    axum::Json(json!({ "markets": markets }))
}

async fn get_opportunities(State(dashboard): State<Arc<Dashboard>>) -> axum::Json<Value> {
    let opportunities = dashboard.read_state(|s| s.opportunities[s.opportunities.len().saturating_sub(50)..].to_vec()).await;
    axum::Json(json!({ "opportunities": opportunities }))
}

async fn get_portfolio(State(dashboard): State<Arc<Dashboard>>) -> axum::Json<Value> {
    axum::Json(dashboard.read_state(|s| s.portfolio.clone()).await)
}

async fn get_risk(State(dashboard): State<Arc<Dashboard>>) -> axum::Json<Value> {
    axum::Json(dashboard.read_state(|s| s.risk.clone()).await)
}

async fn get_timing(State(dashboard): State<Arc<Dashboard>>) -> axum::Json<Value> {
    axum::Json(dashboard.read_state(|s| s.timing.clone()).await)
}

async fn ws_handler(ws: WebSocketUpgrade, State(dashboard): State<Arc<Dashboard>>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, dashboard))
}

async fn handle_socket(mut socket: WebSocket, dashboard: Arc<Dashboard>) {
    let initial = json!({"type": "initial", "data": dashboard.to_json().await});
    if socket.send(Message::Text(initial.to_string())).await.is_err() {
        return;
    }

    let mut rx = dashboard.subscribe();

    loop {
        tokio::select! {
            broadcasted = rx.recv() => {
                match broadcasted {
                    Ok(text) => {
                        if socket.send(Message::Text(text)).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
            incoming = tokio::time::timeout(Duration::from_secs(30), socket.recv()) => {
                match incoming {
                    Ok(Some(Ok(Message::Text(text)))) => {
                        if let Ok(msg) = serde_json::from_str::<Value>(&text) {
                            if msg.get("type").and_then(Value::as_str) == Some("ping") {
                                if socket.send(Message::Text(json!({"type": "pong"}).to_string())).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    Ok(Some(Ok(Message::Close(_)))) | Ok(None) => break,
                    Ok(Some(Err(e))) => {
                        error!(error = %e, "WebSocket error");
                        break;
                    }
                    Ok(Some(Ok(_))) => {} // Ignore binary/ping/pong control frames.
                    Err(_) => {
                        // 30s of client silence -> heartbeat, matching Python's behavior.
                        if socket.send(Message::Text(json!({"type": "heartbeat"}).to_string())).await.is_err() {
                            break;
                        }
                    }
                }
            }
        }
    }
}
