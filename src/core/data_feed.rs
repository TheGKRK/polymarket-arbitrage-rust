//! Real-time in-memory order book / position state for monitored markets
//! (mirrors `core/data_feed.py`).

use crate::models::{Market, MarketState, OrderBook, Position, TokenType};
use crate::polymarket_client::PolymarketClient;
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

pub type OnUpdate = Arc<dyn Fn(&str, &MarketState) + Send + Sync>;

pub struct DataFeed {
    client: Arc<PolymarketClient>,
    market_ids: Mutex<Vec<String>>,
    position_refresh_interval: f64,
    on_update: Option<OnUpdate>,
    use_simulation: bool,
    markets: RwLock<HashMap<String, Market>>,
    order_books: RwLock<HashMap<String, OrderBook>>,
    positions: RwLock<HashMap<String, HashMap<TokenType, Position>>>,
    market_states: RwLock<HashMap<String, MarketState>>,
    running: Arc<AtomicBool>,
    orderbook_task: Mutex<Option<JoinHandle<()>>>,
    position_task: Mutex<Option<JoinHandle<()>>>,
    update_count: AtomicU64,
    last_update: RwLock<HashMap<String, DateTime<Utc>>>,
}

impl DataFeed {
    pub fn new(client: Arc<PolymarketClient>, market_ids: Vec<String>, position_refresh_interval: f64, on_update: Option<OnUpdate>, use_simulation: bool) -> Arc<Self> {
        Arc::new(Self {
            client,
            market_ids: Mutex::new(market_ids),
            position_refresh_interval,
            on_update,
            use_simulation,
            markets: RwLock::new(HashMap::new()),
            order_books: RwLock::new(HashMap::new()),
            positions: RwLock::new(HashMap::new()),
            market_states: RwLock::new(HashMap::new()),
            running: Arc::new(AtomicBool::new(false)),
            orderbook_task: Mutex::new(None),
            position_task: Mutex::new(None),
            update_count: AtomicU64::new(0),
            last_update: RwLock::new(HashMap::new()),
        })
    }

    pub async fn start(self: &Arc<Self>) {
        if self.running.swap(true, Ordering::SeqCst) {
            warn!("DataFeed already running");
            return;
        }

        info!(count = self.market_ids.lock().await.len(), "Starting DataFeed");

        if let Err(e) = self.fetch_markets().await {
            error!(error = %e, "Failed to fetch markets");
            self.running.store(false, Ordering::SeqCst);
            return;
        }

        self.refresh_positions().await;

        let this = Arc::clone(self);
        *self.orderbook_task.lock().await = Some(tokio::spawn(async move { this.stream_orderbooks().await }));

        let this = Arc::clone(self);
        *self.position_task.lock().await = Some(tokio::spawn(async move { this.position_refresh_loop().await }));

        info!("DataFeed started successfully");
    }

    pub async fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);

        if let Some(h) = self.orderbook_task.lock().await.take() {
            h.abort();
        }
        if let Some(h) = self.position_task.lock().await.take() {
            h.abort();
        }

        info!("DataFeed stopped");
    }

    async fn fetch_markets(&self) -> Result<(), crate::polymarket_client::ApiError> {
        let mut ids = self.market_ids.lock().await;

        if ids.is_empty() {
            let markets = self.client.list_markets(None).await?;
            let mut cache = self.markets.write().await;
            for market in &markets {
                cache.insert(market.market_id.clone(), market.clone());
            }
            *ids = markets.iter().map(|m| m.market_id.clone()).collect();
            info!(count = ids.len(), "Discovered and loaded active markets (no re-fetch needed!)");
        } else {
            for market_id in ids.iter() {
                let market = self.client.get_market(market_id).await?;
                self.markets.write().await.insert(market_id.clone(), market);
            }
        }
        Ok(())
    }

    async fn stream_orderbooks(self: Arc<Self>) {
        let market_ids: Vec<String> = self.market_ids.lock().await.clone();
        let use_simulation = self.use_simulation;

        while self.running.load(Ordering::SeqCst) {
            let client = Arc::clone(&self.client);
            let ids = market_ids.clone();
            let stream = client.stream_orderbook(ids, use_simulation);
            tokio::pin!(stream);

            loop {
                match stream.next().await {
                    Some((market_id, orderbook)) => {
                        if !self.running.load(Ordering::SeqCst) {
                            break;
                        }
                        self.order_books.write().await.insert(market_id.clone(), orderbook);
                        self.last_update.write().await.insert(market_id.clone(), Utc::now());
                        self.update_count.fetch_add(1, Ordering::SeqCst);
                        self.update_market_state(&market_id).await;
                    }
                    None => break,
                }
            }

            if self.running.load(Ordering::SeqCst) {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
    }

    async fn position_refresh_loop(self: Arc<Self>) {
        while self.running.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_secs_f64(self.position_refresh_interval)).await;
            self.refresh_positions().await;
        }
    }

    async fn refresh_positions(&self) {
        let positions = self.client.get_positions().await;
        let market_ids: Vec<String> = positions.keys().cloned().collect();
        *self.positions.write().await = positions;

        let order_books = self.order_books.read().await;
        let have_books: Vec<String> = market_ids.into_iter().filter(|id| order_books.contains_key(id)).collect();
        drop(order_books);

        for market_id in have_books {
            self.update_market_state(&market_id).await;
        }
    }

    async fn update_market_state(&self, market_id: &str) {
        let markets = self.markets.read().await;
        let Some(market) = markets.get(market_id).cloned() else { return };
        drop(markets);

        let order_book = self.order_books.read().await.get(market_id).cloned().unwrap_or_else(|| OrderBook::new(market_id));
        let positions = self.positions.read().await.get(market_id).cloned().unwrap_or_default();

        let mut state = MarketState::new(market, order_book);
        state.positions = positions;

        self.market_states.write().await.insert(market_id.to_string(), state.clone());

        if let Some(cb) = &self.on_update {
            cb(market_id, &state);
        }
    }

    pub async fn get_market_state(&self, market_id: &str) -> Option<MarketState> {
        self.market_states.read().await.get(market_id).cloned()
    }

    pub async fn get_all_market_states(&self) -> HashMap<String, MarketState> {
        self.market_states.read().await.clone()
    }

    pub async fn get_order_book(&self, market_id: &str) -> Option<OrderBook> {
        self.order_books.read().await.get(market_id).cloned()
    }

    pub async fn get_positions(&self, market_id: &str) -> HashMap<TokenType, Position> {
        self.positions.read().await.get(market_id).cloned().unwrap_or_default()
    }

    pub async fn get_market(&self, market_id: &str) -> Option<Market> {
        self.markets.read().await.get(market_id).cloned()
    }

    pub async fn market_ids(&self) -> Vec<String> {
        self.market_ids.lock().await.clone()
    }

    pub fn update_count(&self) -> u64 {
        self.update_count.load(Ordering::SeqCst)
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub async fn get_staleness(&self, market_id: &str) -> Option<f64> {
        let ts = *self.last_update.read().await.get(market_id)?;
        Some((Utc::now() - ts).num_milliseconds() as f64 / 1000.0)
    }

    /// Wait until data is available for all markets. Returns `true` if data
    /// is available, `false` on timeout.
    pub async fn wait_for_data(&self, timeout: f64) -> bool {
        let start = Utc::now();
        let ids = self.market_ids.lock().await.clone();

        while (Utc::now() - start).num_milliseconds() as f64 / 1000.0 < timeout {
            let order_books = self.order_books.read().await;
            if ids.iter().all(|id| order_books.contains_key(id)) {
                return true;
            }
            drop(order_books);
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        false
    }
}
