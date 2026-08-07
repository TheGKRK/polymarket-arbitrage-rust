//! Shared trading data models (mirrors Python's `polymarket_client/models.py`).
//!
//! Kalshi's models depend on these same types in the Python codebase, so they
//! stay in one shared module here too.

use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OrderSide {
    Buy,
    Sell,
}

impl OrderSide {
    pub fn as_str(&self) -> &'static str {
        match self {
            OrderSide::Buy => "buy",
            OrderSide::Sell => "sell",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    Pending,
    Open,
    PartiallyFilled,
    Filled,
    Cancelled,
    Expired,
    Rejected,
}

impl Default for OrderStatus {
    fn default() -> Self {
        OrderStatus::Pending
    }
}

impl OrderStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            OrderStatus::Pending => "pending",
            OrderStatus::Open => "open",
            OrderStatus::PartiallyFilled => "partially_filled",
            OrderStatus::Filled => "filled",
            OrderStatus::Cancelled => "cancelled",
            OrderStatus::Expired => "expired",
            OrderStatus::Rejected => "rejected",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TokenType {
    Yes,
    No,
}

impl TokenType {
    pub fn as_str(&self) -> &'static str {
        match self {
            TokenType::Yes => "yes",
            TokenType::No => "no",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OpportunityType {
    BundleLong,
    BundleShort,
    MmBid,
    MmAsk,
}

impl OpportunityType {
    pub fn as_str(&self) -> &'static str {
        match self {
            OpportunityType::BundleLong => "bundle_long",
            OpportunityType::BundleShort => "bundle_short",
            OpportunityType::MmBid => "mm_bid",
            OpportunityType::MmAsk => "mm_ask",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct PriceLevel {
    pub price: f64,
    pub size: f64,
}

impl PriceLevel {
    pub fn new(price: f64, size: f64) -> Self {
        Self { price, size }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct OrderBookSide {
    pub levels: Vec<PriceLevel>,
}

impl OrderBookSide {
    pub fn new(levels: Vec<PriceLevel>) -> Self {
        Self { levels }
    }

    pub fn best_price(&self) -> Option<f64> {
        self.levels.first().map(|l| l.price)
    }

    pub fn best_size(&self) -> Option<f64> {
        self.levels.first().map(|l| l.size)
    }

    pub fn get_depth(&self, levels: usize) -> &[PriceLevel] {
        &self.levels[..self.levels.len().min(levels)]
    }

    pub fn total_size(&self, levels: usize) -> f64 {
        self.levels.iter().take(levels).map(|l| l.size).sum()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TokenOrderBook {
    pub token_type: TokenType,
    pub bids: OrderBookSide,
    pub asks: OrderBookSide,
    pub last_update: DateTime<Utc>,
}

impl TokenOrderBook {
    pub fn new(token_type: TokenType) -> Self {
        Self {
            token_type,
            bids: OrderBookSide::default(),
            asks: OrderBookSide::default(),
            last_update: Utc::now(),
        }
    }

    pub fn best_bid(&self) -> Option<f64> {
        self.bids.best_price()
    }

    pub fn best_ask(&self) -> Option<f64> {
        self.asks.best_price()
    }

    pub fn best_bid_size(&self) -> Option<f64> {
        self.bids.best_size()
    }

    pub fn best_ask_size(&self) -> Option<f64> {
        self.asks.best_size()
    }

    pub fn spread(&self) -> Option<f64> {
        Some(self.best_ask()? - self.best_bid()?)
    }

    pub fn mid_price(&self) -> Option<f64> {
        Some((self.best_bid()? + self.best_ask()?) / 2.0)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct OrderBook {
    pub market_id: String,
    pub yes: TokenOrderBook,
    pub no: TokenOrderBook,
    pub timestamp: DateTime<Utc>,
}

impl OrderBook {
    pub fn new(market_id: impl Into<String>) -> Self {
        Self {
            market_id: market_id.into(),
            yes: TokenOrderBook::new(TokenType::Yes),
            no: TokenOrderBook::new(TokenType::No),
            timestamp: Utc::now(),
        }
    }

    pub fn best_bid_yes(&self) -> Option<f64> {
        self.yes.best_bid()
    }

    pub fn best_ask_yes(&self) -> Option<f64> {
        self.yes.best_ask()
    }

    pub fn best_bid_no(&self) -> Option<f64> {
        self.no.best_bid()
    }

    pub fn best_ask_no(&self) -> Option<f64> {
        self.no.best_ask()
    }

    pub fn total_ask(&self) -> Option<f64> {
        Some(self.best_ask_yes()? + self.best_ask_no()?)
    }

    pub fn total_bid(&self) -> Option<f64> {
        Some(self.best_bid_yes()? + self.best_bid_no()?)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Market {
    pub market_id: String,
    pub condition_id: String,
    pub question: String,
    pub description: String,
    pub yes_token_id: String,
    pub no_token_id: String,
    pub active: bool,
    pub closed: bool,
    pub resolved: bool,
    pub resolution: Option<String>,
    pub volume_24h: f64,
    pub liquidity: f64,
    pub created_at: Option<DateTime<Utc>>,
    pub end_date: Option<DateTime<Utc>>,
    pub category: String,
    pub tags: Vec<String>,
}

impl Market {
    /// Mirrors `Market(market_id=..., condition_id=..., question=...)` with
    /// every other field left at its dataclass default.
    pub fn new(market_id: impl Into<String>, condition_id: impl Into<String>, question: impl Into<String>) -> Self {
        Self {
            market_id: market_id.into(),
            condition_id: condition_id.into(),
            question: question.into(),
            description: String::new(),
            yes_token_id: String::new(),
            no_token_id: String::new(),
            active: true,
            closed: false,
            resolved: false,
            resolution: None,
            volume_24h: 0.0,
            liquidity: 0.0,
            created_at: None,
            end_date: None,
            category: String::new(),
            tags: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Order {
    pub order_id: String,
    pub market_id: String,
    pub token_type: TokenType,
    pub side: OrderSide,
    pub price: f64,
    pub size: f64,
    pub filled_size: f64,
    pub status: OrderStatus,
    pub strategy_tag: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Order {
    pub fn new(
        order_id: impl Into<String>,
        market_id: impl Into<String>,
        token_type: TokenType,
        side: OrderSide,
        price: f64,
        size: f64,
    ) -> Self {
        let now = Utc::now();
        Self {
            order_id: order_id.into(),
            market_id: market_id.into(),
            token_type,
            side,
            price,
            size,
            filled_size: 0.0,
            status: OrderStatus::default(),
            strategy_tag: String::new(),
            created_at: now,
            updated_at: now,
        }
    }

    pub fn remaining_size(&self) -> f64 {
        self.size - self.filled_size
    }

    pub fn is_filled(&self) -> bool {
        matches!(self.status, OrderStatus::Filled)
    }

    pub fn is_open(&self) -> bool {
        matches!(
            self.status,
            OrderStatus::Open | OrderStatus::PartiallyFilled | OrderStatus::Pending
        )
    }

    pub fn notional(&self) -> f64 {
        self.price * self.size
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Position {
    pub market_id: String,
    pub token_type: TokenType,
    pub size: f64,
    pub avg_entry_price: f64,
    pub realized_pnl: f64,
}

impl Position {
    pub fn new(market_id: impl Into<String>, token_type: TokenType, size: f64) -> Self {
        Self {
            market_id: market_id.into(),
            token_type,
            size,
            avg_entry_price: 0.0,
            realized_pnl: 0.0,
        }
    }

    pub fn notional(&self) -> f64 {
        self.size.abs() * self.avg_entry_price
    }

    pub fn is_long(&self) -> bool {
        self.size > 0.0
    }

    pub fn is_short(&self) -> bool {
        self.size < 0.0
    }

    pub fn unrealized_pnl(&self, current_price: f64) -> f64 {
        if self.size == 0.0 {
            0.0
        } else {
            self.size * (current_price - self.avg_entry_price)
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Trade {
    pub trade_id: String,
    pub order_id: String,
    pub market_id: String,
    pub token_type: TokenType,
    pub side: OrderSide,
    pub price: f64,
    pub size: f64,
    pub fee: f64,
    pub timestamp: DateTime<Utc>,
}

impl Trade {
    pub fn notional(&self) -> f64 {
        self.price * self.size
    }

    pub fn net_cost(&self) -> f64 {
        self.notional() + self.fee
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Opportunity {
    pub opportunity_id: String,
    pub opportunity_type: OpportunityType,
    pub market_id: String,
    pub edge: f64,
    pub best_bid_yes: Option<f64>,
    pub best_ask_yes: Option<f64>,
    pub best_bid_no: Option<f64>,
    pub best_ask_no: Option<f64>,
    pub suggested_size: f64,
    pub max_size: f64,
    pub detected_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub acted_upon: bool,
}

impl Opportunity {
    pub fn new(opportunity_id: impl Into<String>, opportunity_type: OpportunityType, market_id: impl Into<String>, edge: f64) -> Self {
        Self {
            opportunity_id: opportunity_id.into(),
            opportunity_type,
            market_id: market_id.into(),
            edge,
            best_bid_yes: None,
            best_ask_yes: None,
            best_bid_no: None,
            best_ask_no: None,
            suggested_size: 0.0,
            max_size: 0.0,
            detected_at: Utc::now(),
            expires_at: None,
            acted_upon: false,
        }
    }

    pub fn is_bundle_arb(&self) -> bool {
        matches!(self.opportunity_type, OpportunityType::BundleLong | OpportunityType::BundleShort)
    }

    pub fn is_market_making(&self) -> bool {
        matches!(self.opportunity_type, OpportunityType::MmBid | OpportunityType::MmAsk)
    }
}

/// A desired order spec attached to a `Signal`. Mirrors the plain `dict`
/// order specs built in Python's `ArbEngine`.
#[derive(Debug, Clone, Serialize)]
pub struct OrderSpec {
    pub token_type: TokenType,
    pub side: OrderSide,
    pub price: f64,
    pub size: f64,
    pub strategy_tag: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Signal {
    pub signal_id: String,
    pub action: String,
    pub market_id: String,
    pub opportunity: Option<Opportunity>,
    pub orders: Vec<OrderSpec>,
    pub cancel_order_ids: Vec<String>,
    pub priority: i32,
    pub created_at: DateTime<Utc>,
}

impl Signal {
    pub fn new_place_orders(
        signal_id: impl Into<String>,
        market_id: impl Into<String>,
        opportunity: Option<Opportunity>,
        orders: Vec<OrderSpec>,
        priority: i32,
    ) -> Self {
        Self {
            signal_id: signal_id.into(),
            action: "place_orders".to_string(),
            market_id: market_id.into(),
            opportunity,
            orders,
            cancel_order_ids: Vec::new(),
            priority,
            created_at: Utc::now(),
        }
    }

    pub fn is_place(&self) -> bool {
        self.action == "place_orders"
    }

    pub fn is_cancel(&self) -> bool {
        self.action == "cancel_orders"
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MarketState {
    pub market: Market,
    pub order_book: OrderBook,
    pub positions: HashMap<TokenType, Position>,
    pub open_orders: Vec<Order>,
    pub timestamp: DateTime<Utc>,
}

impl MarketState {
    pub fn new(market: Market, order_book: OrderBook) -> Self {
        Self {
            market,
            order_book,
            positions: HashMap::new(),
            open_orders: Vec::new(),
            timestamp: Utc::now(),
        }
    }

    pub fn yes_position(&self) -> Option<&Position> {
        self.positions.get(&TokenType::Yes)
    }

    pub fn no_position(&self) -> Option<&Position> {
        self.positions.get(&TokenType::No)
    }

    pub fn net_exposure(&self) -> f64 {
        let yes_notional = self.yes_position().map(|p| p.notional()).unwrap_or(0.0);
        let no_notional = self.no_position().map(|p| p.notional()).unwrap_or(0.0);
        yes_notional + no_notional
    }
}
