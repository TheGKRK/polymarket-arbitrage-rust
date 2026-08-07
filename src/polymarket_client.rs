//! Polymarket API client (mirrors `polymarket_client/api.py`).
//!
//! This is a straight port of the Python client, including the fact that
//! real order placement was never implemented upstream: `place_order`'s
//! non-dry-run path is still a stub that posts an empty `token_id`, exactly
//! as in the Python source. No EIP-712/wallet signing exists to port.

use crate::models::{
    Market, Order, OrderBook, OrderBookSide, OrderSide, OrderStatus, Position, PriceLevel,
    TokenOrderBook, TokenType, Trade,
};
use async_stream::stream;
use chrono::Utc;
use futures_util::Stream;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};
use reqwest::Method;
use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("HTTP {status}: {body}")]
    Status { status: reqwest::StatusCode, body: String },
    #[error(transparent)]
    Request(#[from] reqwest::Error),
    #[error("{0}")]
    Other(String),
}

#[derive(Debug, Clone)]
pub struct PolymarketClientOptions {
    pub rest_url: String,
    pub ws_url: String,
    pub gamma_url: String,
    pub api_key: Option<String>,
    pub api_secret: Option<String>,
    pub passphrase: Option<String>,
    pub private_key: Option<String>,
    pub timeout: f64,
    pub max_retries: u32,
    pub retry_delay: f64,
    pub dry_run: bool,
}

impl Default for PolymarketClientOptions {
    fn default() -> Self {
        Self {
            rest_url: "https://clob.polymarket.com".to_string(),
            ws_url: "wss://ws-subscriptions-clob.polymarket.com/ws/market".to_string(),
            gamma_url: "https://gamma-api.polymarket.com".to_string(),
            api_key: None,
            api_secret: None,
            passphrase: None,
            private_key: None,
            timeout: 30.0,
            max_retries: 3,
            retry_delay: 1.0,
            dry_run: true,
        }
    }
}

pub struct PolymarketClient {
    pub rest_url: String,
    pub ws_url: String,
    pub gamma_url: String,
    pub api_key: Option<String>,
    pub api_secret: Option<String>,
    pub passphrase: Option<String>,
    pub private_key: Option<String>,
    pub max_retries: u32,
    pub retry_delay: f64,
    pub dry_run: bool,
    http: reqwest::Client,
    markets_cache: RwLock<HashMap<String, Market>>,
    simulated_orders: RwLock<HashMap<String, Order>>,
    simulated_positions: RwLock<HashMap<String, HashMap<TokenType, Position>>>,
    simulated_trades: RwLock<Vec<Trade>>,
}

impl PolymarketClient {
    pub fn new(opts: PolymarketClientOptions) -> Self {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("Content-Type", "application/json".parse().unwrap());
        headers.insert("Accept", "application/json".parse().unwrap());
        if let Some(key) = &opts.api_key {
            // TODO: Implement proper CLOB API authentication.
            // Polymarket uses L1/L2 authentication with signatures.
            if let Ok(v) = key.parse() {
                headers.insert("POLY_API_KEY", v);
            }
        }

        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs_f64(opts.timeout))
            .default_headers(headers)
            .build()
            .expect("failed to build reqwest client");

        Self {
            rest_url: opts.rest_url.trim_end_matches('/').to_string(),
            ws_url: opts.ws_url,
            gamma_url: opts.gamma_url.trim_end_matches('/').to_string(),
            api_key: opts.api_key,
            api_secret: opts.api_secret,
            passphrase: opts.passphrase,
            private_key: opts.private_key,
            max_retries: opts.max_retries,
            retry_delay: opts.retry_delay,
            dry_run: opts.dry_run,
            http,
            markets_cache: RwLock::new(HashMap::new()),
            simulated_orders: RwLock::new(HashMap::new()),
            simulated_positions: RwLock::new(HashMap::new()),
            simulated_trades: RwLock::new(Vec::new()),
        }
    }

    pub async fn connect(&self) {
        info!(dry_run = self.dry_run, "Polymarket client connected");
    }

    pub async fn disconnect(&self) {
        info!("Polymarket client disconnected");
    }

    async fn request(
        &self,
        method: Method,
        endpoint: &str,
        params: Option<&HashMap<String, String>>,
        json_data: Option<&Value>,
        base_url: Option<&str>,
    ) -> Result<Value, ApiError> {
        let url = format!("{}{}", base_url.unwrap_or(&self.rest_url), endpoint);

        for attempt in 0..self.max_retries {
            let mut req = self.http.request(method.clone(), &url);
            if let Some(p) = params {
                req = req.query(p);
            }
            if let Some(body) = json_data {
                req = req.json(body);
            }

            match req.send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        return resp.json::<Value>().await.map_err(ApiError::Request);
                    }
                    let body = resp.text().await.unwrap_or_default();
                    warn!(%status, %url, "HTTP error");
                    if status.is_server_error() && attempt + 1 < self.max_retries {
                        tokio::time::sleep(Duration::from_secs_f64(self.retry_delay * (attempt as f64 + 1.0))).await;
                        continue;
                    }
                    return Err(ApiError::Status { status, body });
                }
                Err(e) => {
                    warn!(%url, error = %e, "Request error");
                    if attempt + 1 < self.max_retries {
                        tokio::time::sleep(Duration::from_secs_f64(self.retry_delay * (attempt as f64 + 1.0))).await;
                        continue;
                    }
                    return Err(ApiError::Request(e));
                }
            }
        }
        unreachable!("retry loop always returns")
    }

    /// Fetch list of available markets from the Gamma API, paginating to get
    /// ALL active markets across all categories.
    pub async fn list_markets(&self, filters: Option<HashMap<String, String>>) -> Result<Vec<Market>, ApiError> {
        let mut params = filters.unwrap_or_default();
        params.entry("closed".to_string()).or_insert_with(|| "false".to_string());
        params.entry("order".to_string()).or_insert_with(|| "volume24hr".to_string());
        params.entry("ascending".to_string()).or_insert_with(|| "false".to_string());

        let mut all_markets = Vec::new();
        let mut offset: u64 = 0;
        let limit: u64 = 100;
        let max_markets = 5000;

        info!("Fetching ALL available markets from Polymarket...");

        loop {
            params.insert("limit".to_string(), limit.to_string());
            params.insert("offset".to_string(), offset.to_string());

            let data = match self.request(Method::GET, "/markets", Some(&params), None, Some(&self.gamma_url)).await {
                Ok(v) => v,
                Err(ApiError::Status { status, .. }) if status.as_u16() == 422 => {
                    info!(offset, "Reached Gamma API offset pagination limit");
                    break;
                }
                Err(e) => return Err(e),
            };

            let items = data.as_array().cloned().unwrap_or_default();
            if items.is_empty() {
                break;
            }

            let mut batch_valid = 0;
            for item in &items {
                if let Some(market) = self.parse_market(item) {
                    if !market.yes_token_id.is_empty() && !market.no_token_id.is_empty() {
                        self.markets_cache.write().await.insert(market.market_id.clone(), market.clone());
                        all_markets.push(market);
                        batch_valid += 1;
                    }
                }
            }

            info!(offset, fetched = items.len(), valid = batch_valid, "Fetched batch");

            if (items.len() as u64) < limit {
                break;
            }
            offset += limit;

            tokio::time::sleep(Duration::from_millis(150)).await;

            if all_markets.len() >= max_markets {
                info!(max_markets, "Reached market cap");
                break;
            }
        }

        info!(total = all_markets.len(), "TOTAL active markets with valid tokens");
        Ok(all_markets)
    }

    /// Fetch events (which contain markets) from the Gamma API.
    pub async fn list_events(&self, filters: Option<HashMap<String, String>>) -> Vec<Value> {
        let mut params = filters.unwrap_or_default();
        params.entry("closed".to_string()).or_insert_with(|| "false".to_string());
        params.entry("limit".to_string()).or_insert_with(|| "50".to_string());
        params.entry("order".to_string()).or_insert_with(|| "id".to_string());
        params.entry("ascending".to_string()).or_insert_with(|| "false".to_string());

        match self.request(Method::GET, "/events", Some(&params), None, Some(&self.gamma_url)).await {
            Ok(data) => data.as_array().cloned().unwrap_or_default(),
            Err(e) => {
                warn!(error = %e, "Failed to fetch events");
                Vec::new()
            }
        }
    }

    fn parse_market(&self, data: &Value) -> Option<Market> {
        let market_id = value_to_id_string(data.get("id")?);
        if market_id.is_empty() {
            return None;
        }
        let condition_id = data.get("conditionId").and_then(Value::as_str).unwrap_or("").to_string();

        let clob_token_ids_raw = data.get("clobTokenIds").and_then(Value::as_str).unwrap_or("");
        let (yes_token_id, no_token_id) = parse_clob_token_ids(clob_token_ids_raw);

        Some(Market {
            market_id,
            condition_id,
            question: data.get("question").and_then(Value::as_str).unwrap_or("").to_string(),
            description: data.get("description").and_then(Value::as_str).unwrap_or("").to_string(),
            yes_token_id,
            no_token_id,
            active: data.get("active").and_then(Value::as_bool).unwrap_or(true),
            closed: data.get("closed").and_then(Value::as_bool).unwrap_or(false),
            resolved: data.get("umaResolutionStatus").and_then(Value::as_str) == Some("resolved"),
            resolution: None,
            volume_24h: data
                .get("volume24hr")
                .and_then(Value::as_f64)
                .or_else(|| data.get("volume24hrClob").and_then(Value::as_f64))
                .unwrap_or(0.0),
            liquidity: data
                .get("liquidityNum")
                .and_then(Value::as_f64)
                .or_else(|| data.get("liquidityClob").and_then(Value::as_f64))
                .unwrap_or(0.0),
            created_at: None,
            end_date: None,
            category: data.get("category").and_then(Value::as_str).unwrap_or("").to_string(),
            tags: Vec::new(),
        })
    }

    pub async fn get_market(&self, market_id: &str) -> Result<Market, ApiError> {
        let result = self.request(Method::GET, &format!("/markets/{market_id}"), None, None, Some(&self.gamma_url)).await;
        match result.and_then(|data| self.parse_market(&data).ok_or_else(|| ApiError::Other("Failed to parse market".to_string()))) {
            Ok(market) => Ok(market),
            Err(e) => {
                warn!(market_id, error = %e, "Failed to fetch market");
                if self.dry_run {
                    Ok(Market::new(market_id, market_id, format!("Market {market_id}")))
                } else {
                    Err(e)
                }
            }
        }
    }

    pub async fn get_market_by_slug(&self, slug: &str) -> Result<Market, ApiError> {
        let data = self.request(Method::GET, &format!("/markets/slug/{slug}"), None, None, Some(&self.gamma_url)).await?;
        self.parse_market(&data).ok_or_else(|| ApiError::Other("Failed to parse market".to_string()))
    }

    pub async fn get_event_by_slug(&self, slug: &str) -> Result<Value, ApiError> {
        self.request(Method::GET, &format!("/events/slug/{slug}"), None, None, Some(&self.gamma_url)).await
    }

    /// Fetch the current order book for a market via the CLOB API.
    pub async fn get_orderbook(&self, market_id: &str) -> Result<OrderBook, ApiError> {
        let market = self.get_market(market_id).await?;

        if market.yes_token_id.is_empty() || market.no_token_id.is_empty() {
            warn!(market_id, "No token IDs for market");
            return Ok(OrderBook::new(market_id));
        }

        let yes_book = self.fetch_token_orderbook(&market.yes_token_id, TokenType::Yes).await;
        let no_book = self.fetch_token_orderbook(&market.no_token_id, TokenType::No).await;

        Ok(OrderBook {
            market_id: market_id.to_string(),
            yes: yes_book,
            no: no_book,
            timestamp: Utc::now(),
        })
    }

    async fn fetch_token_orderbook(&self, token_id: &str, token_type: TokenType) -> TokenOrderBook {
        let mut params = HashMap::new();
        params.insert("token_id".to_string(), token_id.to_string());

        match self.request(Method::GET, "/book", Some(&params), None, Some(&self.rest_url)).await {
            Ok(data) => {
                let bids = parse_price_levels(data.get("bids"), 10);
                let asks = parse_price_levels(data.get("asks"), 10);
                TokenOrderBook {
                    token_type,
                    bids: OrderBookSide::new(bids),
                    asks: OrderBookSide::new(asks),
                    last_update: Utc::now(),
                }
            }
            Err(e) => {
                warn!(token_id, error = %e, "Failed to fetch orderbook for token");
                TokenOrderBook::new(token_type)
            }
        }
    }

    fn generate_simulated_orderbook(&self, market_id: &str) -> OrderBook {
        // `StdRng` (not `thread_rng()`) so this stays `Send` across the
        // `.await` points inside the `stream!` generator body below.
        let mut rng = StdRng::from_entropy();

        let mut yes_mid: f64 = 0.50 + rng.gen_range(-0.30..0.30);
        let inefficiency = if rng.gen_bool(0.20) {
            rng.gen_range(-0.08..0.08)
        } else {
            rng.gen_range(-0.02..0.02)
        };
        let mut no_mid = 1.0 - yes_mid + inefficiency;
        yes_mid = yes_mid.clamp(0.01, 0.99);
        no_mid = no_mid.clamp(0.01, 0.99);

        let spread = rng.gen_range(0.02..0.06);

        fn generate_levels(rng: &mut impl Rng, mid: f64, spread: f64, is_bid: bool, count: usize) -> Vec<PriceLevel> {
            let mut levels = Vec::with_capacity(count);
            for i in 0..count {
                let offset = (i as f64 + 1.0) * 0.01;
                let price = if is_bid {
                    (mid - spread / 2.0 - offset).max(0.01)
                } else {
                    (mid + spread / 2.0 + offset).min(0.99)
                };
                let size = rng.gen_range(100.0..1000.0);
                levels.push(PriceLevel::new(round2(price), round2(size)));
            }
            levels
        }

        let yes_book = TokenOrderBook {
            token_type: TokenType::Yes,
            bids: OrderBookSide::new(generate_levels(&mut rng, yes_mid, spread, true, 5)),
            asks: OrderBookSide::new(generate_levels(&mut rng, yes_mid, spread, false, 5)),
            last_update: Utc::now(),
        };
        let no_book = TokenOrderBook {
            token_type: TokenType::No,
            bids: OrderBookSide::new(generate_levels(&mut rng, no_mid, spread, true, 5)),
            asks: OrderBookSide::new(generate_levels(&mut rng, no_mid, spread, false, 5)),
            last_update: Utc::now(),
        };

        OrderBook {
            market_id: market_id.to_string(),
            yes: yes_book,
            no: no_book,
            timestamp: Utc::now(),
        }
    }

    /// Stream order book updates. If `use_simulation` is true, generates
    /// simulated data with occasional arbitrage opportunities; otherwise
    /// fetches REAL data from the Polymarket CLOB API by rotating through
    /// batches of markets.
    pub fn stream_orderbook<'a>(&'a self, market_ids: Vec<String>, use_simulation: bool) -> impl Stream<Item = (String, OrderBook)> + 'a {
        stream! {
            if use_simulation {
                for await item in self.stream_simulated_orderbooks(market_ids) {
                    yield item;
                }
                return;
            }

            info!(count = market_ids.len(), "Starting REAL orderbook stream");

            let mut market_tokens: Vec<(String, String, String)> = Vec::new();
            {
                let cache = self.markets_cache.read().await;
                for market_id in &market_ids {
                    if let Some(market) = cache.get(market_id) {
                        if !market.yes_token_id.is_empty() && !market.no_token_id.is_empty() {
                            market_tokens.push((market_id.clone(), market.yes_token_id.clone(), market.no_token_id.clone()));
                        }
                    }
                }
            }

            info!(count = market_tokens.len(), "Have token IDs (from cache)");

            if market_tokens.is_empty() {
                warn!("No markets with valid token IDs found!");
                return;
            }

            let active_batch_size = 500usize;
            let markets_per_request_batch = 20usize;
            let total_markets = market_tokens.len();
            let mut current_offset = 0usize;

            info!(total_markets, active_batch_size, "Will rotate through markets");

            loop {
                let end_offset = (current_offset + active_batch_size).min(total_markets);
                let active_markets = &market_tokens[current_offset..end_offset];

                info!(from = current_offset + 1, to = end_offset, total_markets, "Processing markets");

                for request_batch in active_markets.chunks(markets_per_request_batch) {
                    for (market_id, yes_token, no_token) in request_batch {
                        let yes_book = self.fetch_token_orderbook(yes_token, TokenType::Yes).await;
                        let no_book = self.fetch_token_orderbook(no_token, TokenType::No).await;

                        let orderbook = OrderBook {
                            market_id: market_id.clone(),
                            yes: yes_book,
                            no: no_book,
                            timestamp: Utc::now(),
                        };

                        yield (market_id.clone(), orderbook);
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }

                current_offset = end_offset;
                if current_offset >= total_markets {
                    current_offset = 0;
                    info!("Completed full market cycle, starting over...");
                }

                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }

    fn stream_simulated_orderbooks<'a>(&'a self, market_ids: Vec<String>) -> impl Stream<Item = (String, OrderBook)> + 'a {
        stream! {
            info!(count = market_ids.len(), "Starting SIMULATED orderbook stream");

            let active_markets: Vec<String> = if market_ids.len() > 100 {
                market_ids[..100].to_vec()
            } else {
                market_ids
            };

            loop {
                // `StdRng` (not `thread_rng()`) so this stays `Send` across the
        // `.await` points inside the `stream!` generator body below.
        let mut rng = StdRng::from_entropy();
                let batch_size = 15.min(active_markets.len());
                let batch: Vec<&String> = active_markets.choose_multiple(&mut rng, batch_size).collect();
                drop(rng);

                for market_id in batch {
                    let orderbook = self.generate_simulated_orderbook(market_id);
                    yield (market_id.clone(), orderbook);
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }

                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }

    pub async fn get_positions(&self) -> HashMap<String, HashMap<TokenType, Position>> {
        if self.dry_run {
            return self.simulated_positions.read().await.clone();
        }

        match self.request(Method::GET, "/positions", None, None, None).await {
            Ok(data) => {
                let mut positions: HashMap<String, HashMap<TokenType, Position>> = HashMap::new();
                if let Some(items) = data.as_array() {
                    for item in items {
                        let market_id = item.get("market_id").and_then(Value::as_str).unwrap_or("").to_string();
                        let token_type = if item.get("outcome").and_then(Value::as_str) == Some("Yes") {
                            TokenType::Yes
                        } else {
                            TokenType::No
                        };
                        let position = Position {
                            market_id: market_id.clone(),
                            token_type,
                            size: item.get("size").and_then(Value::as_f64).unwrap_or(0.0),
                            avg_entry_price: item.get("avg_price").and_then(Value::as_f64).unwrap_or(0.0),
                            realized_pnl: item.get("realized_pnl").and_then(Value::as_f64).unwrap_or(0.0),
                        };
                        positions.entry(market_id).or_default().insert(token_type, position);
                    }
                }
                positions
            }
            Err(e) => {
                warn!(error = %e, "Failed to fetch positions");
                HashMap::new()
            }
        }
    }

    /// Place a limit order.
    ///
    /// TODO (ported from Python as-is): implement actual Polymarket CLOB
    /// order placement (`POST /order`). The non-dry-run path below sends a
    /// stub payload with an empty `token_id` and no signature, exactly as
    /// the original Python client did.
    pub async fn place_order(
        &self,
        market_id: &str,
        token_type: TokenType,
        side: OrderSide,
        price: f64,
        size: f64,
        strategy_tag: &str,
    ) -> Result<Order, ApiError> {
        let order_id = format!("order_{}", short_uuid());
        let mut order = Order::new(order_id.clone(), market_id, token_type, side, price, size);
        order.status = OrderStatus::Open;
        order.strategy_tag = strategy_tag.to_string();

        if self.dry_run {
            info!(order_id = %order.order_id, "[DRY RUN] Placing order");
            self.simulated_orders.write().await.insert(order_id, order.clone());
            return Ok(order);
        }

        let payload = serde_json::json!({
            "market_id": market_id,
            "token_id": "",
            "side": side.as_str(),
            "price": price.to_string(),
            "size": size.to_string(),
        });

        match self.request(Method::POST, "/order", None, Some(&payload), None).await {
            Ok(data) => {
                order.order_id = data.get("order_id").and_then(Value::as_str).unwrap_or(&order.order_id).to_string();
                order.status = OrderStatus::Open;
                info!(order_id = %order.order_id, "Order placed");
                Ok(order)
            }
            Err(e) => {
                error!(error = %e, "Failed to place order");
                Err(e)
            }
        }
    }

    pub async fn cancel_order(&self, order_id: &str) -> Result<(), ApiError> {
        if self.dry_run {
            if let Some(order) = self.simulated_orders.write().await.get_mut(order_id) {
                order.status = OrderStatus::Cancelled;
                info!(order_id, "[DRY RUN] Cancelled order");
            }
            return Ok(());
        }

        self.request(Method::DELETE, &format!("/order/{order_id}"), None, None, None).await?;
        info!(order_id, "Order cancelled");
        Ok(())
    }

    pub async fn cancel_all_orders(&self, market_id: Option<&str>) -> u32 {
        let orders = self.get_open_orders(market_id).await;
        let mut cancelled = 0;
        for order in orders {
            if self.cancel_order(&order.order_id).await.is_ok() {
                cancelled += 1;
            }
        }
        cancelled
    }

    pub async fn get_open_orders(&self, market_id: Option<&str>) -> Vec<Order> {
        if self.dry_run {
            return self
                .simulated_orders
                .read()
                .await
                .values()
                .filter(|o| o.is_open() && market_id.map(|m| m == o.market_id).unwrap_or(true))
                .cloned()
                .collect();
        }

        let params = market_id.map(|m| HashMap::from([("market_id".to_string(), m.to_string())]));
        match self.request(Method::GET, "/orders", params.as_ref(), None, None).await {
            Ok(data) => data
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .filter_map(|item| {
                    let side = if item.get("side").and_then(Value::as_str) == Some("sell") { OrderSide::Sell } else { OrderSide::Buy };
                    let mut order = Order::new(
                        item.get("order_id").and_then(Value::as_str)?.to_string(),
                        item.get("market_id").and_then(Value::as_str)?.to_string(),
                        if item.get("outcome").and_then(Value::as_str) == Some("Yes") { TokenType::Yes } else { TokenType::No },
                        side,
                        item.get("price").and_then(Value::as_f64)?,
                        item.get("size").and_then(Value::as_f64)?,
                    );
                    order.filled_size = item.get("filled_size").and_then(Value::as_f64).unwrap_or(0.0);
                    Some(order)
                })
                .collect(),
            Err(e) => {
                warn!(error = %e, "Failed to fetch open orders");
                Vec::new()
            }
        }
    }

    pub async fn get_trades(&self, market_id: Option<&str>, limit: usize) -> Vec<Trade> {
        if self.dry_run {
            let trades = self.simulated_trades.read().await;
            let start = trades.len().saturating_sub(limit);
            return trades[start..]
                .iter()
                .filter(|t| market_id.map(|m| m == t.market_id).unwrap_or(true))
                .cloned()
                .collect();
        }

        let mut params = HashMap::new();
        params.insert("limit".to_string(), limit.to_string());
        if let Some(m) = market_id {
            params.insert("market_id".to_string(), m.to_string());
        }

        match self.request(Method::GET, "/trades", Some(&params), None, None).await {
            Ok(data) => data
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .filter_map(|item| {
                    let side = if item.get("side").and_then(Value::as_str) == Some("sell") { OrderSide::Sell } else { OrderSide::Buy };
                    Some(Trade {
                        trade_id: item.get("trade_id").and_then(Value::as_str)?.to_string(),
                        order_id: item.get("order_id").and_then(Value::as_str)?.to_string(),
                        market_id: item.get("market_id").and_then(Value::as_str)?.to_string(),
                        token_type: if item.get("outcome").and_then(Value::as_str) == Some("Yes") { TokenType::Yes } else { TokenType::No },
                        side,
                        price: item.get("price").and_then(Value::as_f64)?,
                        size: item.get("size").and_then(Value::as_f64)?,
                        fee: item.get("fee").and_then(Value::as_f64).unwrap_or(0.0),
                        timestamp: item
                            .get("timestamp")
                            .and_then(Value::as_str)
                            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                            .map(|dt| dt.with_timezone(&Utc))
                            .unwrap_or_else(Utc::now),
                    })
                })
                .collect(),
            Err(e) => {
                warn!(error = %e, "Failed to fetch trades");
                Vec::new()
            }
        }
    }

    /// Simulate an order fill (for dry run mode). Returns the generated
    /// trade if successful.
    pub async fn simulate_fill(&self, order_id: &str, fill_size: Option<f64>) -> Option<Trade> {
        let mut orders = self.simulated_orders.write().await;
        let order = orders.get_mut(order_id)?;
        if !order.is_open() {
            return None;
        }

        let fill_size = fill_size.unwrap_or_else(|| order.remaining_size()).min(order.remaining_size());

        let notional = fill_size * order.price;
        let fee_rate = 0.015; // 1.5% taker fee, realistic Polymarket rate.
        let fee = notional * fee_rate;

        let trade = Trade {
            trade_id: format!("trade_{}", short_uuid()),
            order_id: order_id.to_string(),
            market_id: order.market_id.clone(),
            token_type: order.token_type,
            side: order.side,
            price: order.price,
            size: fill_size,
            fee,
            timestamp: Utc::now(),
        };

        order.filled_size += fill_size;
        order.updated_at = Utc::now();
        order.status = if order.remaining_size() <= 0.0 { OrderStatus::Filled } else { OrderStatus::PartiallyFilled };
        drop(orders);

        self.update_simulated_position(&trade).await;
        self.simulated_trades.write().await.push(trade.clone());

        info!(trade_id = %trade.trade_id, "[DRY RUN] Simulated fill");
        Some(trade)
    }

    async fn update_simulated_position(&self, trade: &Trade) {
        let mut positions = self.simulated_positions.write().await;
        let market_positions = positions.entry(trade.market_id.clone()).or_default();
        let pos = market_positions
            .entry(trade.token_type)
            .or_insert_with(|| Position::new(trade.market_id.clone(), trade.token_type, 0.0));

        if trade.side == OrderSide::Buy {
            let new_size = pos.size + trade.size;
            if new_size > 0.0 {
                pos.avg_entry_price = (pos.avg_entry_price * pos.size + trade.price * trade.size) / new_size;
            }
            pos.size = new_size;
        } else {
            if pos.size > 0.0 {
                let realized = (trade.price - pos.avg_entry_price) * trade.size;
                pos.realized_pnl += realized;
            }
            pos.size -= trade.size;
        }
    }
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

fn short_uuid() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_string()
}

fn value_to_id_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

/// Parse `clobTokenIds`, a JSON array string like `["tokenId1","tokenId2"]`,
/// falling back to comma-separated parsing on malformed input.
fn parse_clob_token_ids(raw: &str) -> (String, String) {
    if raw.is_empty() {
        return (String::new(), String::new());
    }
    if let Ok(Value::Array(items)) = serde_json::from_str::<Value>(raw) {
        let yes = items.first().and_then(Value::as_str).unwrap_or("").trim().to_string();
        let no = items.get(1).and_then(Value::as_str).unwrap_or("").trim().to_string();
        return (yes, no);
    }
    let parts: Vec<&str> = raw.split(',').collect();
    let yes = parts.first().map(|s| s.trim().to_string()).unwrap_or_default();
    let no = parts.get(1).map(|s| s.trim().to_string()).unwrap_or_default();
    (yes, no)
}

fn parse_price_levels(levels: Option<&Value>, max: usize) -> Vec<PriceLevel> {
    levels
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .take(max)
                .map(|lvl| {
                    let price = lvl.get("price").and_then(value_as_f64).unwrap_or(0.0);
                    let size = lvl.get("size").and_then(value_as_f64).unwrap_or(0.0);
                    PriceLevel::new(price, size)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn value_as_f64(v: &Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}
