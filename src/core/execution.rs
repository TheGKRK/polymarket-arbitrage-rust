//! Order placement, cancellation, and management (mirrors `core/execution.py`).
//! Consumes signals from the `ArbEngine` and interfaces with the API client.

use crate::core::portfolio::Portfolio;
use crate::core::risk_manager::RiskManager;
use crate::models::{Order, OrderSide, OrderStatus, Signal, TokenType, Trade};
use crate::polymarket_client::PolymarketClient;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as SyncMutex};
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

#[derive(Debug, Clone)]
pub struct ExecutionConfig {
    pub slippage_tolerance: f64,
    pub order_timeout_seconds: f64,
    pub max_retries: u32,
    pub retry_delay: f64,
    pub enable_slippage_check: bool,
    pub dry_run: bool,
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            slippage_tolerance: 0.02,
            order_timeout_seconds: 60.0,
            max_retries: 3,
            retry_delay: 0.5,
            enable_slippage_check: true,
            dry_run: true,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ExecutionStats {
    pub orders_placed: u32,
    pub orders_filled: u32,
    pub orders_cancelled: u32,
    pub orders_rejected: u32,
    pub total_notional: f64,
    pub signals_processed: u32,
    pub signals_rejected: u32,
    pub slippage_rejections: u32,
}

#[derive(Default)]
struct Tracking {
    stats: ExecutionStats,
    open_orders: HashMap<String, Order>,
    order_timestamps: HashMap<String, DateTime<Utc>>,
    orders_by_market: HashMap<String, Vec<String>>,
    orders_by_strategy: HashMap<String, Vec<String>>,
}

pub struct ExecutionEngine {
    client: Arc<PolymarketClient>,
    risk_manager: Arc<SyncMutex<RiskManager>>,
    portfolio: Arc<SyncMutex<Portfolio>>,
    config: ExecutionConfig,
    tracking: Mutex<Tracking>,
    running: Arc<AtomicBool>,
    signal_tx: mpsc::Sender<Signal>,
    signal_rx: Mutex<Option<mpsc::Receiver<Signal>>>,
    processing_task: Mutex<Option<JoinHandle<()>>>,
    timeout_task: Mutex<Option<JoinHandle<()>>>,
}

impl ExecutionEngine {
    pub fn new(client: Arc<PolymarketClient>, risk_manager: Arc<SyncMutex<RiskManager>>, portfolio: Arc<SyncMutex<Portfolio>>, config: ExecutionConfig) -> Arc<Self> {
        let (tx, rx) = mpsc::channel(1024);
        info!(dry_run = config.dry_run, "ExecutionEngine initialized");
        Arc::new(Self {
            client,
            risk_manager,
            portfolio,
            config,
            tracking: Mutex::new(Tracking::default()),
            running: Arc::new(AtomicBool::new(false)),
            signal_tx: tx,
            signal_rx: Mutex::new(Some(rx)),
            processing_task: Mutex::new(None),
            timeout_task: Mutex::new(None),
        })
    }

    pub async fn start(self: &Arc<Self>) {
        if self.running.swap(true, Ordering::SeqCst) {
            return;
        }

        let Some(rx) = self.signal_rx.lock().await.take() else { return };

        let this = Arc::clone(self);
        let processing = tokio::spawn(async move { this.process_signals(rx).await });
        *self.processing_task.lock().await = Some(processing);

        let this = Arc::clone(self);
        let timeout_monitor = tokio::spawn(async move { this.monitor_order_timeouts().await });
        *self.timeout_task.lock().await = Some(timeout_monitor);

        info!("ExecutionEngine started");
    }

    pub async fn stop(&self) {
        if !self.running.swap(false, Ordering::SeqCst) {
            return;
        }

        if let Some(handle) = self.processing_task.lock().await.take() {
            handle.abort();
        }

        self.cancel_all_orders(None).await;

        info!("ExecutionEngine stopped");
    }

    pub async fn submit_signal(&self, signal: Signal) {
        let signal_id = signal.signal_id.clone();
        if self.signal_tx.send(signal).await.is_ok() {
            tracing::debug!(signal_id, "Signal queued");
        }
    }

    async fn process_signals(self: Arc<Self>, mut rx: mpsc::Receiver<Signal>) {
        while self.running.load(Ordering::SeqCst) {
            match tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv()).await {
                Ok(Some(signal)) => {
                    self.execute_signal(&signal).await;
                    self.tracking.lock().await.stats.signals_processed += 1;
                }
                Ok(None) => break, // Sender dropped.
                Err(_) => continue, // Timed out waiting; loop to re-check `running`.
            }
        }
    }

    async fn execute_signal(&self, signal: &Signal) {
        info!(signal_id = %signal.signal_id, action = %signal.action, "Executing signal");

        if signal.is_place() {
            self.handle_place_orders(signal).await;
        } else if signal.is_cancel() {
            self.handle_cancel_orders(signal).await;
        } else {
            warn!(action = %signal.action, "Unknown signal action");
        }
    }

    async fn handle_place_orders(&self, signal: &Signal) {
        for order_spec in &signal.orders {
            if self.config.enable_slippage_check {
                if let Some(opportunity) = &signal.opportunity {
                    if !Self::check_slippage(opportunity, order_spec, self.config.slippage_tolerance) {
                        self.tracking.lock().await.stats.slippage_rejections += 1;
                        warn!(?order_spec.token_type, ?order_spec.side, "Order rejected due to slippage");
                        continue;
                    }
                }
            }

            let proposed_order = Order::new("temp", &signal.market_id, order_spec.token_type, order_spec.side, order_spec.price, order_spec.size);
            let allowed = self.risk_manager.lock().unwrap().check_order(&proposed_order);
            if !allowed {
                self.tracking.lock().await.stats.signals_rejected += 1;
                warn!(market_id = %signal.market_id, "Order rejected by risk manager");
                continue;
            }

            match self.place_order_with_retry(&signal.market_id, order_spec.token_type, order_spec.side, order_spec.price, order_spec.size, &order_spec.strategy_tag).await {
                Some(order) => {
                    let notional = order.notional();
                    self.track_order(order).await;
                    let mut t = self.tracking.lock().await;
                    t.stats.orders_placed += 1;
                    t.stats.total_notional += notional;
                }
                None => {
                    self.tracking.lock().await.stats.orders_rejected += 1;
                }
            }
        }
    }

    async fn handle_cancel_orders(&self, signal: &Signal) {
        for order_id in &signal.cancel_order_ids {
            if let Err(e) = self.cancel_order(order_id).await {
                error!(order_id, error = %e, "Failed to cancel order");
            }
        }
    }

    fn check_slippage(opportunity: &crate::models::Opportunity, order_spec: &crate::models::OrderSpec, tolerance: f64) -> bool {
        let (snapshot_bid, snapshot_ask) = if order_spec.token_type == TokenType::Yes {
            (opportunity.best_bid_yes, opportunity.best_ask_yes)
        } else {
            (opportunity.best_bid_no, opportunity.best_ask_no)
        };

        let (Some(snapshot_bid), Some(snapshot_ask)) = (snapshot_bid, snapshot_ask) else {
            return true; // Can't check, allow.
        };

        let slippage = if order_spec.side == OrderSide::Buy {
            if snapshot_ask > 0.0 { (order_spec.price - snapshot_ask) / snapshot_ask } else { 0.0 }
        } else if snapshot_bid > 0.0 {
            (snapshot_bid - order_spec.price) / snapshot_bid
        } else {
            0.0
        };

        slippage.abs() <= tolerance
    }

    async fn place_order_with_retry(&self, market_id: &str, token_type: TokenType, side: OrderSide, price: f64, size: f64, strategy_tag: &str) -> Option<Order> {
        let mut last_error = None;

        for attempt in 0..self.config.max_retries {
            match self.client.place_order(market_id, token_type, side, price, size, strategy_tag).await {
                Ok(order) => {
                    info!(order_id = %order.order_id, side = side.as_str(), size, token = token_type.as_str(), price, "Order placed");
                    return Some(order);
                }
                Err(e) => {
                    warn!(attempt = attempt + 1, error = %e, "Order placement attempt failed");
                    last_error = Some(e);
                    if attempt + 1 < self.config.max_retries {
                        tokio::time::sleep(std::time::Duration::from_secs_f64(self.config.retry_delay)).await;
                    }
                }
            }
        }

        error!(retries = self.config.max_retries, error = ?last_error, "Order placement failed after retries");
        None
    }

    async fn track_order(&self, order: Order) {
        let mut t = self.tracking.lock().await;
        t.order_timestamps.insert(order.order_id.clone(), Utc::now());
        t.orders_by_market.entry(order.market_id.clone()).or_default().push(order.order_id.clone());
        if !order.strategy_tag.is_empty() {
            t.orders_by_strategy.entry(order.strategy_tag.clone()).or_default().push(order.order_id.clone());
        }
        t.open_orders.insert(order.order_id.clone(), order);
    }

    async fn untrack_order(&self, order_id: &str) {
        let mut t = self.tracking.lock().await;
        if let Some(order) = t.open_orders.remove(order_id) {
            t.order_timestamps.remove(order_id);
            if let Some(ids) = t.orders_by_market.get_mut(&order.market_id) {
                ids.retain(|id| id != order_id);
            }
            if !order.strategy_tag.is_empty() {
                if let Some(ids) = t.orders_by_strategy.get_mut(&order.strategy_tag) {
                    ids.retain(|id| id != order_id);
                }
            }
        }
    }

    pub async fn cancel_order(&self, order_id: &str) -> Result<bool, crate::polymarket_client::ApiError> {
        match self.client.cancel_order(order_id).await {
            Ok(()) => {
                self.untrack_order(order_id).await;
                self.tracking.lock().await.stats.orders_cancelled += 1;
                info!(order_id, "Order cancelled");
                Ok(true)
            }
            Err(e) => {
                error!(order_id, error = %e, "Failed to cancel order");
                Err(e)
            }
        }
    }

    pub async fn cancel_all_orders(&self, market_id: Option<&str>) -> u32 {
        let order_ids: Vec<String> = {
            let t = self.tracking.lock().await;
            match market_id {
                Some(m) => t.orders_by_market.get(m).cloned().unwrap_or_default(),
                None => t.open_orders.keys().cloned().collect(),
            }
        };

        let mut cancelled = 0;
        for order_id in order_ids {
            if self.cancel_order(&order_id).await.unwrap_or(false) {
                cancelled += 1;
            }
        }

        info!(cancelled, "Cancelled orders");
        cancelled
    }

    pub async fn cancel_orders_by_strategy(&self, strategy_tag: &str) -> u32 {
        let order_ids: Vec<String> = self.tracking.lock().await.orders_by_strategy.get(strategy_tag).cloned().unwrap_or_default();
        let mut cancelled = 0;
        for order_id in order_ids {
            if self.cancel_order(&order_id).await.unwrap_or(false) {
                cancelled += 1;
            }
        }
        cancelled
    }

    async fn monitor_order_timeouts(self: Arc<Self>) {
        while self.running.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;

            let now = Utc::now();
            let timeout_delta = ChronoDuration::milliseconds((self.config.order_timeout_seconds * 1000.0) as i64);

            let timed_out: Vec<String> = {
                let t = self.tracking.lock().await;
                t.order_timestamps.iter().filter(|(_, ts)| now - **ts > timeout_delta).map(|(id, _)| id.clone()).collect()
            };

            for order_id in timed_out {
                info!(order_id, "Order timed out");
                let _ = self.cancel_order(&order_id).await;
            }
        }
    }

    /// Handle a trade fill notification.
    pub async fn handle_fill(&self, trade: &Trade) {
        {
            let mut t = self.tracking.lock().await;
            if let Some(order) = t.open_orders.get_mut(&trade.order_id) {
                order.filled_size += trade.size;
                order.updated_at = Utc::now();

                if order.remaining_size() <= 0.0 {
                    order.status = OrderStatus::Filled;
                    drop(t);
                    self.untrack_order(&trade.order_id).await;
                    self.tracking.lock().await.stats.orders_filled += 1;
                } else {
                    order.status = OrderStatus::PartiallyFilled;
                }
            }
        }

        self.portfolio.lock().unwrap().update_from_fill(trade);
        self.risk_manager.lock().unwrap().update_from_fill(trade);

        info!(trade_id = %trade.trade_id, side = trade.side.as_str(), size = trade.size, token = trade.token_type.as_str(), price = trade.price, "Fill");
    }

    pub async fn get_open_orders(&self, market_id: Option<&str>) -> Vec<Order> {
        let t = self.tracking.lock().await;
        match market_id {
            Some(m) => t.orders_by_market.get(m).map(|ids| ids.iter().filter_map(|id| t.open_orders.get(id).cloned()).collect()).unwrap_or_default(),
            None => t.open_orders.values().cloned().collect(),
        }
    }

    pub async fn get_stats(&self) -> ExecutionStats {
        self.tracking.lock().await.stats.clone()
    }

    pub async fn open_order_count(&self) -> usize {
        self.tracking.lock().await.open_orders.len()
    }
}
