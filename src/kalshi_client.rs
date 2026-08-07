//! Kalshi API client (mirrors `kalshi_client/api.py`).
//!
//! Only hits public, unauthenticated market-data endpoints - no signing or
//! credentials involved.
//!
//! Note on error handling: the Python client raises on most non-429/404
//! HTTP errors, but nearly every call site already treats a failed or
//! missing response the same as an empty one (via `asyncio.gather(...,
//! return_exceptions=True)` or a broad `except Exception`). This port
//! collapses that distinction deliberately: any unrecoverable failure here
//! just degrades to an empty result, which has the same practical effect.

use crate::kalshi_models::{KalshiEvent, KalshiMarket, KalshiOrderBook, KalshiSeries};
use crate::models::{OrderBook, PriceLevel};
use async_stream::stream;
use chrono::Utc;
use futures_util::{future::join_all, Stream};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::RwLock;
use std::time::Duration;
use tracing::{debug, info, warn};

const BASE_URL: &str = "https://api.elections.kalshi.com/trade-api/v2";

pub struct KalshiClient {
    pub timeout: f64,
    pub max_retries: u32,
    pub dry_run: bool,
    http: reqwest::Client,
    markets_cache: RwLock<HashMap<String, KalshiMarket>>,
}

impl KalshiClient {
    pub fn new(timeout: f64, max_retries: u32, dry_run: bool) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs_f64(timeout))
            .build()
            .expect("failed to build reqwest client");

        Self {
            timeout,
            max_retries,
            dry_run,
            http,
            markets_cache: RwLock::new(HashMap::new()),
        }
    }

    pub async fn connect(&self) {}

    pub async fn disconnect(&self) {}

    async fn get(&self, endpoint: &str, params: Option<&HashMap<String, String>>) -> Value {
        let url = format!("{BASE_URL}{endpoint}");

        for attempt in 0..self.max_retries {
            let mut req = self.http.get(&url);
            if let Some(p) = params {
                req = req.query(p);
            }

            match req.send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if status.as_u16() == 429 {
                        let wait = 2u64.pow(attempt);
                        warn!(wait, "Rate limited, waiting before retry");
                        tokio::time::sleep(Duration::from_secs(wait)).await;
                        continue;
                    } else if status.as_u16() == 404 {
                        debug!(endpoint, "Not found");
                        return serde_json::json!({});
                    } else if !status.is_success() {
                        warn!(%status, endpoint, "HTTP error");
                        return serde_json::json!({});
                    }
                    return resp.json::<Value>().await.unwrap_or_else(|_| serde_json::json!({}));
                }
                Err(e) => {
                    warn!(attempt, error = %e, "Request error");
                    if attempt + 1 < self.max_retries {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                    return serde_json::json!({});
                }
            }
        }
        serde_json::json!({})
    }

    pub async fn get_series(&self, series_ticker: &str) -> Option<KalshiSeries> {
        let data = self.get(&format!("/series/{series_ticker}"), None).await;
        let s = data.get("series")?;
        Some(KalshiSeries {
            ticker: s.get("ticker").and_then(Value::as_str).unwrap_or(series_ticker).to_string(),
            title: s.get("title").and_then(Value::as_str).unwrap_or("").to_string(),
            frequency: s.get("frequency").and_then(Value::as_str).unwrap_or("").to_string(),
            category: s.get("category").and_then(Value::as_str).unwrap_or("").to_string(),
        })
    }

    pub async fn get_event(&self, event_ticker: &str) -> Option<KalshiEvent> {
        let data = self.get(&format!("/events/{event_ticker}"), None).await;
        let e = data.get("event")?;
        Some(KalshiEvent {
            event_ticker: e.get("ticker").and_then(Value::as_str).unwrap_or(event_ticker).to_string(),
            series_ticker: e.get("series_ticker").and_then(Value::as_str).unwrap_or("").to_string(),
            title: e.get("title").and_then(Value::as_str).unwrap_or("").to_string(),
            category: e.get("category").and_then(Value::as_str).unwrap_or("").to_string(),
            markets: Vec::new(),
        })
    }

    pub async fn list_markets(
        &self,
        status: &str,
        series_ticker: Option<&str>,
        event_ticker: Option<&str>,
        limit: u32,
        cursor: Option<&str>,
    ) -> (Vec<KalshiMarket>, Option<String>) {
        let mut params = HashMap::new();
        params.insert("status".to_string(), status.to_string());
        params.insert("limit".to_string(), limit.min(1000).to_string());
        if let Some(s) = series_ticker {
            params.insert("series_ticker".to_string(), s.to_string());
        }
        if let Some(e) = event_ticker {
            params.insert("event_ticker".to_string(), e.to_string());
        }
        if let Some(c) = cursor {
            params.insert("cursor".to_string(), c.to_string());
        }

        let data = self.get("/markets", Some(&params)).await;
        let Some(items) = data.get("markets").and_then(Value::as_array) else {
            return (Vec::new(), None);
        };

        let mut markets = Vec::new();
        for m in items {
            if let Some(market) = self.parse_market(m) {
                self.markets_cache.write().unwrap().insert(market.ticker.clone(), market.clone());
                markets.push(market);
            }
        }

        let next_cursor = data.get("cursor").and_then(Value::as_str).map(String::from);
        (markets, next_cursor)
    }

    pub async fn list_all_markets(&self, status: &str, max_markets: usize, mut on_progress: Option<&mut (dyn FnMut(usize) + Send)>) -> Vec<KalshiMarket> {
        let mut all_markets = Vec::new();
        let mut cursor: Option<String> = None;

        while all_markets.len() < max_markets {
            let (markets, next_cursor) = self.list_markets(status, None, None, 1000, cursor.as_deref()).await;

            if markets.is_empty() {
                break;
            }

            all_markets.extend(markets);
            info!(count = all_markets.len(), "Kalshi: markets loaded...");

            if let Some(cb) = on_progress.as_mut() {
                cb(all_markets.len());
            }

            match next_cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }

            tokio::time::sleep(Duration::from_millis(200)).await;
        }

        info!(count = all_markets.len(), "Kalshi: total markets loaded");
        all_markets.truncate(max_markets);
        all_markets
    }

    pub async fn get_market(&self, ticker: &str) -> Option<KalshiMarket> {
        if let Some(cached) = self.markets_cache.read().unwrap().get(ticker) {
            return Some(cached.clone());
        }

        let data = self.get(&format!("/markets/{ticker}"), None).await;
        let market = self.parse_market(data.get("market")?)?;
        self.markets_cache.write().unwrap().insert(ticker.to_string(), market.clone());
        Some(market)
    }

    fn parse_market(&self, data: &Value) -> Option<KalshiMarket> {
        let mut yes_price = data.get("yes_price").and_then(Value::as_f64).map(|c| c / 100.0).unwrap_or(0.0);
        let mut no_price = data.get("no_price").and_then(Value::as_f64).map(|c| c / 100.0).unwrap_or(0.0);
        if no_price == 0.0 && yes_price > 0.0 {
            no_price = 1.0 - yes_price;
        }
        if yes_price == 0.0 && no_price == 0.0 {
            yes_price = 0.0;
        }

        let close_time = data
            .get("close_time")
            .and_then(Value::as_str)
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc));

        Some(KalshiMarket {
            ticker: data.get("ticker").and_then(Value::as_str).unwrap_or("").to_string(),
            event_ticker: data.get("event_ticker").and_then(Value::as_str).unwrap_or("").to_string(),
            series_ticker: data.get("series_ticker").and_then(Value::as_str).unwrap_or("").to_string(),
            title: data.get("title").and_then(Value::as_str).unwrap_or("").to_string(),
            subtitle: data.get("subtitle").and_then(Value::as_str).unwrap_or("").to_string(),
            yes_price,
            no_price,
            status: data.get("status").and_then(Value::as_str).unwrap_or("").to_string(),
            result: data.get("result").and_then(Value::as_str).map(String::from),
            volume: data.get("volume").and_then(Value::as_i64).unwrap_or(0),
            open_interest: data.get("open_interest").and_then(Value::as_i64).unwrap_or(0),
            close_time,
            expiration_time: None,
            category: data.get("category").and_then(Value::as_str).unwrap_or("").to_string(),
        })
    }

    pub async fn get_orderbook(&self, ticker: &str) -> Option<KalshiOrderBook> {
        let data = self.get(&format!("/markets/{ticker}/orderbook"), None).await;
        let ob = data.get("orderbook")?;

        let mut yes_bids = parse_cents_levels(ob.get("yes"));
        let mut no_bids = parse_cents_levels(ob.get("no"));
        yes_bids.sort_by(|a, b| b.price.partial_cmp(&a.price).unwrap());
        no_bids.sort_by(|a, b| b.price.partial_cmp(&a.price).unwrap());

        Some(KalshiOrderBook {
            ticker: ticker.to_string(),
            yes_bids,
            no_bids,
            timestamp: Utc::now(),
        })
    }

    pub async fn get_orderbook_unified(&self, ticker: &str) -> Option<OrderBook> {
        Some(self.get_orderbook(ticker).await?.to_unified_orderbook())
    }

    /// Stream order books for multiple markets using polling (Kalshi's
    /// public API has no push-based streaming).
    pub fn stream_orderbooks<'a>(&'a self, tickers: Vec<String>, batch_size: usize, rotation_delay: f64) -> impl Stream<Item = (String, OrderBook)> + 'a {
        stream! {
            info!(count = tickers.len(), "Starting Kalshi orderbook stream");

            loop {
                for batch in tickers.chunks(batch_size.max(1)) {
                    let futures = batch.iter().map(|ticker| self.get_orderbook_unified(ticker));
                    let results = join_all(futures).await;

                    for (ticker, result) in batch.iter().zip(results) {
                        if let Some(ob) = result {
                            yield (ticker.clone(), ob);
                        } else {
                            debug!(ticker, "Failed to get Kalshi orderbook");
                        }
                    }

                    tokio::time::sleep(Duration::from_secs_f64(rotation_delay)).await;
                }
            }
        }
    }

    pub async fn get_markets_by_category(&self, category: &str) -> Vec<KalshiMarket> {
        let all_markets = self.list_all_markets("open", 10_000, None).await;
        all_markets.into_iter().filter(|m| m.category.to_lowercase() == category.to_lowercase()).collect()
    }

    pub async fn search_markets(&self, query: &str) -> Vec<KalshiMarket> {
        let all_markets = self.list_all_markets("open", 10_000, None).await;
        let query_lower = query.to_lowercase();
        all_markets
            .into_iter()
            .filter(|m| m.title.to_lowercase().contains(&query_lower) || m.subtitle.to_lowercase().contains(&query_lower))
            .collect()
    }
}

fn parse_cents_levels(levels: Option<&Value>) -> Vec<PriceLevel> {
    levels
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|level| {
                    let pair = level.as_array()?;
                    if pair.len() < 2 {
                        return None;
                    }
                    let price_cents = pair[0].as_f64()?;
                    let quantity = pair[1].as_f64()?;
                    Some(PriceLevel::new(price_cents / 100.0, quantity))
                })
                .collect()
        })
        .unwrap_or_default()
}
