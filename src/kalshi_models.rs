//! Kalshi-specific data structures that map onto the shared trading models
//! (mirrors `kalshi_client/models.py`).

use crate::models::{OrderBook, OrderBookSide, PriceLevel, TokenOrderBook, TokenType};
use chrono::{DateTime, Utc};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct KalshiMarket {
    pub ticker: String,
    pub event_ticker: String,
    pub series_ticker: String,
    pub title: String,
    pub subtitle: String,
    pub yes_price: f64,
    pub no_price: f64,
    pub status: String,
    pub result: Option<String>,
    pub volume: i64,
    pub open_interest: i64,
    pub close_time: Option<DateTime<Utc>>,
    pub expiration_time: Option<DateTime<Utc>>,
    pub category: String,
}

impl KalshiMarket {
    pub fn is_active(&self) -> bool {
        self.status == "open" || self.status == "active"
    }

    pub fn to_unified_market_id(&self) -> String {
        format!("kalshi:{}", self.ticker)
    }
}

/// Kalshi only returns bids in their API. For a binary market: YES bids are
/// what people will pay to buy YES; NO bids are what people will pay to buy
/// NO. The ask price is derived: `ask_yes = 1.0 - best_bid_no`.
#[derive(Debug, Clone, Serialize)]
pub struct KalshiOrderBook {
    pub ticker: String,
    pub yes_bids: Vec<PriceLevel>,
    pub no_bids: Vec<PriceLevel>,
    pub timestamp: DateTime<Utc>,
}

impl KalshiOrderBook {
    pub fn best_bid_yes(&self) -> Option<f64> {
        self.yes_bids.first().map(|l| l.price)
    }

    pub fn best_bid_no(&self) -> Option<f64> {
        self.no_bids.first().map(|l| l.price)
    }

    /// If someone bids X for NO, they're implicitly offering YES at (1.0 - X).
    pub fn best_ask_yes(&self) -> Option<f64> {
        Some(1.0 - self.no_bids.first()?.price)
    }

    /// If someone bids X for YES, they're implicitly offering NO at (1.0 - X).
    pub fn best_ask_no(&self) -> Option<f64> {
        Some(1.0 - self.yes_bids.first()?.price)
    }

    /// Convert to the unified `OrderBook` format used for cross-platform arbitrage.
    pub fn to_unified_orderbook(&self) -> OrderBook {
        let mut yes_token_ob = TokenOrderBook::new(TokenType::Yes);
        let mut no_token_ob = TokenOrderBook::new(TokenType::No);

        yes_token_ob.bids = OrderBookSide::new(self.yes_bids.clone());
        if !self.no_bids.is_empty() {
            let mut derived_yes_asks: Vec<PriceLevel> = self
                .no_bids
                .iter()
                .map(|bid| PriceLevel::new(1.0 - bid.price, bid.size))
                .collect();
            derived_yes_asks.sort_by(|a, b| a.price.partial_cmp(&b.price).unwrap());
            yes_token_ob.asks = OrderBookSide::new(derived_yes_asks);
        }

        no_token_ob.bids = OrderBookSide::new(self.no_bids.clone());
        if !self.yes_bids.is_empty() {
            let mut derived_no_asks: Vec<PriceLevel> = self
                .yes_bids
                .iter()
                .map(|bid| PriceLevel::new(1.0 - bid.price, bid.size))
                .collect();
            derived_no_asks.sort_by(|a, b| a.price.partial_cmp(&b.price).unwrap());
            no_token_ob.asks = OrderBookSide::new(derived_no_asks);
        }

        OrderBook {
            market_id: format!("kalshi:{}", self.ticker),
            yes: yes_token_ob,
            no: no_token_ob,
            timestamp: self.timestamp,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct KalshiEvent {
    pub event_ticker: String,
    pub series_ticker: String,
    pub title: String,
    pub category: String,
    pub markets: Vec<KalshiMarket>,
}

impl KalshiEvent {
    pub fn market_count(&self) -> usize {
        self.markets.len()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct KalshiSeries {
    pub ticker: String,
    pub title: String,
    pub frequency: String,
    pub category: String,
}
