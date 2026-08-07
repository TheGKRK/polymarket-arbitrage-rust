//! Bridges the trading bot's live components into the dashboard's state
//! (mirrors `DashboardIntegration` in `dashboard/integration.py`).

use super::state::Dashboard;
use crate::core::arb_engine::ArbEngine;
use crate::core::data_feed::DataFeed;
use crate::core::execution::ExecutionEngine;
use crate::core::portfolio::Portfolio;
use crate::core::risk_manager::RiskManager;
use chrono::Utc;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as SyncMutex};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

pub struct DashboardIntegration {
    dashboard: Arc<Dashboard>,
    data_feed: Option<Arc<DataFeed>>,
    arb_engine: Option<Arc<SyncMutex<ArbEngine>>>,
    execution_engine: Option<Arc<ExecutionEngine>>,
    risk_manager: Option<Arc<SyncMutex<RiskManager>>>,
    portfolio: Option<Arc<SyncMutex<Portfolio>>>,
    running: Arc<AtomicBool>,
    update_task: Mutex<Option<JoinHandle<()>>>,
}

#[allow(clippy::too_many_arguments)]
impl DashboardIntegration {
    pub async fn new(
        dashboard: Arc<Dashboard>,
        data_feed: Option<Arc<DataFeed>>,
        arb_engine: Option<Arc<SyncMutex<ArbEngine>>>,
        execution_engine: Option<Arc<ExecutionEngine>>,
        risk_manager: Option<Arc<SyncMutex<RiskManager>>>,
        portfolio: Option<Arc<SyncMutex<Portfolio>>>,
        mode: &str,
    ) -> Arc<Self> {
        dashboard
            .with_state(|s| {
                s.mode = mode.to_string();
                s.is_running = false;
            })
            .await;

        Arc::new(Self {
            dashboard,
            data_feed,
            arb_engine,
            execution_engine,
            risk_manager,
            portfolio,
            running: Arc::new(AtomicBool::new(false)),
            update_task: Mutex::new(None),
        })
    }

    pub async fn start(self: &Arc<Self>, update_interval: f64) {
        self.running.store(true, Ordering::SeqCst);
        self.dashboard.with_state(|s| s.is_running = true).await;

        let this = Arc::clone(self);
        *self.update_task.lock().await = Some(tokio::spawn(async move { this.update_loop(update_interval).await }));

        tracing::info!("Dashboard integration started");
    }

    pub async fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
        self.dashboard.with_state(|s| s.is_running = false).await;

        if let Some(h) = self.update_task.lock().await.take() {
            h.abort();
        }

        tracing::info!("Dashboard integration stopped");
    }

    async fn update_loop(self: Arc<Self>, interval: f64) {
        while self.running.load(Ordering::SeqCst) {
            self.update_state().await;
            self.broadcast_update().await;
            tokio::time::sleep(std::time::Duration::from_secs_f64(interval)).await;
        }
    }

    async fn update_state(&self) {
        if let Some(data_feed) = &self.data_feed {
            let mut markets = std::collections::HashMap::new();
            for (market_id, state) in data_feed.get_all_market_states().await {
                let ob = &state.order_book;
                let question = if state.market.question.is_empty() { market_id.clone() } else { state.market.question.chars().take(80).collect() };
                markets.insert(
                    market_id.clone(),
                    json!({
                        "market_id": market_id,
                        "question": question,
                        "best_bid_yes": ob.best_bid_yes(),
                        "best_ask_yes": ob.best_ask_yes(),
                        "best_bid_no": ob.best_bid_no(),
                        "best_ask_no": ob.best_ask_no(),
                        "total_ask": ob.total_ask(),
                        "total_bid": ob.total_bid(),
                        "spread_yes": ob.yes.spread(),
                        "spread_no": ob.no.spread(),
                    }),
                );
            }
            let markets_with_data = markets.values().filter(|m| m.get("best_bid_yes").map(|v| !v.is_null()).unwrap_or(false) || m.get("best_ask_yes").map(|v| !v.is_null()).unwrap_or(false)).count();
            let markets_count = markets.len();

            self.dashboard.with_state(|s| s.markets = markets).await;

            let operational = json!({
                "total_markets": data_feed.market_ids().await.len(),
                "markets_with_orderbooks": markets_count,
                "markets_with_prices": markets_with_data,
                "orderbook_updates": data_feed.update_count(),
                "is_streaming": data_feed.is_running(),
            });
            self.dashboard.with_state(|s| s.operational = operational).await;
        }

        if let Some(portfolio) = &self.portfolio {
            let summary = json!(portfolio.lock().unwrap().get_summary());
            self.dashboard.with_state(|s| s.portfolio = summary).await;
        }

        if let Some(risk_manager) = &self.risk_manager {
            let summary = json!(risk_manager.lock().unwrap().get_summary());
            self.dashboard.with_state(|s| s.risk = summary).await;
        }

        if let Some(execution_engine) = &self.execution_engine {
            let orders = execution_engine.get_open_orders(None).await;
            let orders_json: Vec<Value> = orders
                .iter()
                .map(|o| {
                    json!({
                        "order_id": o.order_id,
                        "market_id": o.market_id,
                        "side": o.side.as_str(),
                        "token_type": o.token_type.as_str(),
                        "price": o.price,
                        "size": o.size,
                        "filled_size": o.filled_size,
                        "status": o.status.as_str(),
                    })
                })
                .collect();
            self.dashboard.with_state(|s| s.orders = orders_json).await;

            let stats = execution_engine.get_stats().await;
            self.dashboard
                .with_state(|s| {
                    merge_object(
                        &mut s.stats,
                        json!({
                            "orders_placed": stats.orders_placed,
                            "orders_filled": stats.orders_filled,
                            "orders_cancelled": stats.orders_cancelled,
                            "signals_processed": stats.signals_processed,
                        }),
                    );
                })
                .await;
        }

        if let Some(arb_engine) = &self.arb_engine {
            let (arb_stats, timing) = {
                let engine = arb_engine.lock().unwrap();
                (engine.get_stats().clone(), json!(engine.get_timing_stats()))
            };

            self.dashboard
                .with_state(|s| {
                    merge_object(
                        &mut s.stats,
                        json!({
                            "bundle_opportunities": arb_stats.bundle_opportunities_detected,
                            "mm_opportunities": arb_stats.mm_opportunities_detected,
                            "signals_generated": arb_stats.signals_generated,
                        }),
                    );
                    s.timing = timing;
                })
                .await;
        }

        self.dashboard.with_state(|s| s.last_update = Utc::now()).await;
    }

    async fn broadcast_update(&self) {
        let data = self.dashboard.to_json().await;
        self.dashboard.broadcast(&json!({"type": "update", "data": data}));
    }

    pub async fn add_opportunity(&self, opportunity_type: &str, market_id: &str, edge: f64, extra: Value) {
        let mut opp = json!({
            "type": opportunity_type,
            "market_id": market_id,
            "edge": edge,
        });
        merge_object(&mut opp, extra);
        self.dashboard.add_opportunity(opp.clone()).await;
        self.dashboard.broadcast(&json!({"type": "opportunity", "data": opp}));
    }

    pub async fn add_signal(&self, action: &str, market_id: &str, extra: Value) {
        let mut signal = json!({
            "action": action,
            "market_id": market_id,
        });
        merge_object(&mut signal, extra);
        self.dashboard.add_signal(signal.clone()).await;
        self.dashboard.broadcast(&json!({"type": "activity", "data": signal}));
    }

    pub async fn add_trade(&self, side: &str, price: f64, size: f64, extra: Value) {
        let mut trade = json!({
            "side": side,
            "price": price,
            "size": size,
        });
        merge_object(&mut trade, extra);
        self.dashboard.add_trade(trade.clone()).await;
        self.dashboard.broadcast(&json!({"type": "activity", "data": trade}));
    }
}

/// Shallow-merge `patch`'s keys into `base` (both expected to be JSON
/// objects), mirroring Python's `dict.update(**kwargs)` call sites.
fn merge_object(base: &mut Value, patch: Value) {
    if let (Value::Object(base_map), Value::Object(patch_map)) = (base, patch) {
        for (k, v) in patch_map {
            base_map.insert(k, v);
        }
    }
}
