//! Dashboard state container and WebSocket fan-out (mirrors `DashboardState`
//! in `dashboard/server.py`).
//!
//! Python broadcasts by reaching into a raw `list[WebSocket]` from whichever
//! task calls `broadcast()`, relying on asyncio's single-threaded scheduler
//! to avoid send races. Rust can't safely share a `WebSocket` sink across
//! tasks like that, so this uses a `tokio::sync::broadcast` channel instead:
//! each connection's own task owns its socket and forwards broadcasted
//! messages to it. Same effect for clients, cleaner ownership in Rust.

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

#[derive(Debug, Clone, Serialize)]
pub struct CrossPlatformState {
    pub enabled: bool,
    pub kalshi_markets: usize,
    pub polymarket_markets: usize,
    pub matched_pairs: usize,
    pub kalshi_orderbooks: usize,
    pub cross_opportunities: Vec<Value>,
    pub matched_pairs_data: Vec<Value>,
    pub matching_progress: u32,
    pub matching_checked: usize,
    pub matching_total: usize,
    pub matching_status: String,
}

impl Default for CrossPlatformState {
    fn default() -> Self {
        Self {
            enabled: false,
            kalshi_markets: 0,
            polymarket_markets: 0,
            matched_pairs: 0,
            kalshi_orderbooks: 0,
            cross_opportunities: Vec::new(),
            matched_pairs_data: Vec::new(),
            matching_progress: 0,
            matching_checked: 0,
            matching_total: 0,
            matching_status: "idle".to_string(),
        }
    }
}

pub struct DashboardState {
    pub markets: HashMap<String, Value>,
    pub opportunities: Vec<Value>,
    pub signals: Vec<Value>,
    pub orders: Vec<Value>,
    pub trades: Vec<Value>,
    pub portfolio: Value,
    pub risk: Value,
    pub stats: Value,
    pub timing: Value,
    pub operational: Value,
    pub cross_platform: CrossPlatformState,
    pub is_running: bool,
    pub mode: String,
    pub last_update: DateTime<Utc>,
    pub started_at: DateTime<Utc>,
}

impl Default for DashboardState {
    fn default() -> Self {
        let now = Utc::now();
        Self {
            markets: HashMap::new(),
            opportunities: Vec::new(),
            signals: Vec::new(),
            orders: Vec::new(),
            trades: Vec::new(),
            portfolio: json!({}),
            risk: json!({}),
            stats: json!({}),
            timing: json!({}),
            operational: json!({}),
            cross_platform: CrossPlatformState::default(),
            is_running: false,
            mode: "dry_run".to_string(),
            last_update: now,
            started_at: now,
        }
    }
}

impl DashboardState {
    fn to_json(&self) -> Value {
        let uptime = (Utc::now() - self.started_at).num_milliseconds() as f64 / 1000.0;
        let tail = |v: &[Value], n: usize| -> Vec<Value> { v[v.len().saturating_sub(n)..].to_vec() };

        json!({
            "markets": self.markets,
            "opportunities": tail(&self.opportunities, 50),
            "signals": tail(&self.signals, 50),
            "orders": self.orders,
            "trades": tail(&self.trades, 100),
            "portfolio": self.portfolio,
            "risk": self.risk,
            "stats": self.stats,
            "timing": self.timing,
            "operational": self.operational,
            "cross_platform": self.cross_platform,
            "is_running": self.is_running,
            "mode": self.mode,
            "last_update": self.last_update.to_rfc3339(),
            "started_at": self.started_at.to_rfc3339(),
            "uptime_seconds": uptime,
        })
    }

    fn add_opportunity(&mut self, mut opportunity: Value) {
        if let Value::Object(map) = &mut opportunity {
            map.insert("timestamp".to_string(), json!(Utc::now().to_rfc3339()));
        }
        self.opportunities.push(opportunity);
        if self.opportunities.len() > 200 {
            let drain_to = self.opportunities.len() - 100;
            self.opportunities.drain(..drain_to);
        }
    }

    fn add_signal(&mut self, mut signal: Value) {
        if let Value::Object(map) = &mut signal {
            map.insert("timestamp".to_string(), json!(Utc::now().to_rfc3339()));
        }
        self.signals.push(signal);
        if self.signals.len() > 200 {
            let drain_to = self.signals.len() - 100;
            self.signals.drain(..drain_to);
        }
    }

    fn add_trade(&mut self, mut trade: Value) {
        if let Value::Object(map) = &mut trade {
            map.insert("timestamp".to_string(), json!(Utc::now().to_rfc3339()));
        }
        self.trades.push(trade);
        if self.trades.len() > 500 {
            let drain_to = self.trades.len() - 250;
            self.trades.drain(..drain_to);
        }
    }
}

/// The dashboard's shared, lockable state plus its WebSocket broadcast
/// channel. One instance is shared (via `Arc`) between the axum routes and
/// `DashboardIntegration`.
pub struct Dashboard {
    state: RwLock<DashboardState>,
    tx: broadcast::Sender<String>,
}

impl Dashboard {
    pub fn new() -> Arc<Self> {
        let (tx, _rx) = broadcast::channel(1024);
        Arc::new(Self { state: RwLock::new(DashboardState::default()), tx })
    }

    pub async fn to_json(&self) -> Value {
        self.state.read().await.to_json()
    }

    pub async fn with_state<R>(&self, f: impl FnOnce(&mut DashboardState) -> R) -> R {
        f(&mut *self.state.write().await)
    }

    pub async fn read_state<R>(&self, f: impl FnOnce(&DashboardState) -> R) -> R {
        f(&*self.state.read().await)
    }

    pub async fn add_opportunity(&self, opportunity: Value) {
        self.state.write().await.add_opportunity(opportunity);
    }

    pub async fn add_signal(&self, signal: Value) {
        self.state.write().await.add_signal(signal);
    }

    pub async fn add_trade(&self, trade: Value) {
        self.state.write().await.add_trade(trade);
    }

    /// Broadcast a JSON message to all connected WebSocket clients. A no-op
    /// (like Python's early-return on an empty connection list) if nobody's
    /// listening.
    pub fn broadcast(&self, data: &Value) {
        let _ = self.tx.send(data.to_string());
    }

    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.tx.subscribe()
    }
}
