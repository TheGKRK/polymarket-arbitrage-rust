//! Backtesting using simulated market data (mirrors `utils/backtest.py`).
//!
//! Note ported faithfully from the Python source: `run_backtest` detects
//! opportunities and submits signals to the execution engine, which places
//! simulated dry-run orders - but nothing in the backtest path ever calls
//! `simulate_fill` on those orders (unlike the live dry-run bot, which runs
//! a separate `_simulate_fills` background task). So `portfolio` stays at
//! zero trades/PnL for the whole backtest today; that's an existing gap in
//! the Python feature, not something introduced here. Preserved as-is per
//! the migration's "port faithfully, don't add new functionality" scope -
//! worth flagging to a human, not silently "fixing" via this migration.

use crate::core::arb_engine::ArbEngine;
use crate::core::execution::ExecutionEngine;
use crate::core::portfolio::Portfolio;
use crate::models::{Market, MarketState, OrderBook, OrderBookSide, PriceLevel, TokenOrderBook, TokenType};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use rand_distr::{Distribution, Normal};
use std::collections::HashMap;
use std::sync::Mutex;
use tracing::info;

#[derive(Debug, Clone)]
pub struct BacktestConfig {
    pub start_time: DateTime<Utc>,
    pub end_time: Option<DateTime<Utc>>,
    pub time_step_seconds: f64,
    pub initial_balance: f64,
    pub simulate_fills: bool,
    pub fill_probability: f64,
    pub partial_fill_probability: f64,
    pub price_volatility: f64,
    pub spread_range: (f64, f64),
    pub mispricing_probability: f64,
    pub mispricing_magnitude: f64,
    pub base_liquidity: f64,
    pub liquidity_variance: f64,
}

impl Default for BacktestConfig {
    fn default() -> Self {
        Self {
            start_time: Utc::now(),
            end_time: None,
            time_step_seconds: 1.0,
            initial_balance: 10000.0,
            simulate_fills: true,
            fill_probability: 0.8,
            partial_fill_probability: 0.3,
            price_volatility: 0.01,
            spread_range: (0.02, 0.10),
            mispricing_probability: 0.05,
            mispricing_magnitude: 0.03,
            base_liquidity: 1000.0,
            liquidity_variance: 0.5,
        }
    }
}

#[derive(Debug, Clone)]
pub struct BacktestResult {
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    pub duration_seconds: f64,
    pub initial_balance: f64,
    pub final_balance: f64,
    pub total_pnl: f64,
    pub realized_pnl: f64,
    pub unrealized_pnl: f64,
    pub total_trades: u32,
    pub winning_trades: u32,
    pub losing_trades: u32,
    pub win_rate: f64,
    pub bundle_opportunities: u32,
    pub mm_opportunities: u32,
    pub opportunities_acted_on: u64,
    pub max_drawdown: f64,
    pub max_exposure: f64,
    pub sharpe_ratio: Option<f64>,
}

impl BacktestResult {
    pub fn summary(&self) -> String {
        format!(
            "\n=== Backtest Results ===\n\
            Duration: {:.1} seconds\n\
            PnL: ${:.2} ({:.1}%)\n  \
            Realized: ${:.2}\n  \
            Unrealized: ${:.2}\n\n\
            Trading:\n  \
            Total Trades: {}\n  \
            Win Rate: {:.1}%\n  \n\
            Opportunities:\n  \
            Bundle Arb: {}\n  \
            Market Making: {}\n  \
            Acted Upon: {}\n\n\
            Risk:\n  \
            Max Drawdown: {:.1}%\n  \
            Max Exposure: ${:.2}\n",
            self.duration_seconds,
            self.total_pnl,
            self.total_pnl / self.initial_balance * 100.0,
            self.realized_pnl,
            self.unrealized_pnl,
            self.total_trades,
            self.win_rate * 100.0,
            self.bundle_opportunities,
            self.mm_opportunities,
            self.opportunities_acted_on,
            self.max_drawdown * 100.0,
            self.max_exposure,
        )
    }
}

struct SimulatedOrderBook {
    market_id: String,
    yes_price: f64,
    volatility: f64,
    spread_range: (f64, f64),
    base_liquidity: f64,
}

impl SimulatedOrderBook {
    fn new(market_id: impl Into<String>, initial_yes_price: f64, volatility: f64, spread_range: (f64, f64), base_liquidity: f64) -> Self {
        Self { market_id: market_id.into(), yes_price: initial_yes_price, volatility, spread_range, base_liquidity }
    }

    fn step(&mut self, rng: &mut StdRng, introduce_mispricing: bool, mispricing_mag: f64) -> OrderBook {
        let normal = Normal::new(0.0, self.volatility).unwrap();
        self.yes_price += normal.sample(rng);
        self.yes_price = self.yes_price.clamp(0.05, 0.95);

        let mut no_price = 1.0 - self.yes_price;

        if introduce_mispricing {
            let adjustment = rng.gen_range(0.5..1.0) * mispricing_mag;
            if rng.gen_bool(0.5) {
                self.yes_price -= adjustment / 2.0;
                no_price -= adjustment / 2.0;
            } else {
                self.yes_price += adjustment / 2.0;
                no_price += adjustment / 2.0;
            }
        }
        no_price = no_price.clamp(0.05, 0.95);

        let yes_spread = rng.gen_range(self.spread_range.0..self.spread_range.1);
        let no_spread = rng.gen_range(self.spread_range.0..self.spread_range.1);

        let yes_book = self.generate_token_book(rng, self.yes_price, yes_spread, TokenType::Yes);
        let no_book = self.generate_token_book(rng, no_price, no_spread, TokenType::No);

        OrderBook { market_id: self.market_id.clone(), yes: yes_book, no: no_book, timestamp: Utc::now() }
    }

    fn generate_token_book(&self, rng: &mut StdRng, mid_price: f64, spread: f64, token_type: TokenType) -> TokenOrderBook {
        let mut bids = Vec::with_capacity(5);
        let mut asks = Vec::with_capacity(5);

        let best_bid = mid_price - spread / 2.0;
        let best_ask = mid_price + spread / 2.0;

        for i in 0..5 {
            let bid_price = (best_bid - i as f64 * 0.01).max(0.01);
            let ask_price = (best_ask + i as f64 * 0.01).min(0.99);

            let liquidity_factor = 1.0 / (1.0 + i as f64 * 0.3);
            let bid_size = self.base_liquidity * liquidity_factor * rng.gen_range(0.5..1.5);
            let ask_size = self.base_liquidity * liquidity_factor * rng.gen_range(0.5..1.5);

            bids.push(PriceLevel::new(round2(bid_price), round2(bid_size)));
            asks.push(PriceLevel::new(round2(ask_price), round2(ask_size)));
        }

        TokenOrderBook { token_type, bids: OrderBookSide::new(bids), asks: OrderBookSide::new(asks), last_update: Utc::now() }
    }
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

pub struct BacktestEngine {
    config: BacktestConfig,
    markets: HashMap<String, Market>,
    order_books: HashMap<String, SimulatedOrderBook>,
    current_time: DateTime<Utc>,
    pnl_history: Vec<(DateTime<Utc>, f64)>,
    exposure_history: Vec<(DateTime<Utc>, f64)>,
    bundle_opportunity_count: u32,
    mm_opportunity_count: u32,
    trade_count: u64,
}

impl BacktestEngine {
    pub fn new(config: BacktestConfig) -> Self {
        info!("BacktestEngine initialized");
        let current_time = config.start_time;
        Self {
            config,
            markets: HashMap::new(),
            order_books: HashMap::new(),
            current_time,
            pnl_history: Vec::new(),
            exposure_history: Vec::new(),
            bundle_opportunity_count: 0,
            mm_opportunity_count: 0,
            trade_count: 0,
        }
    }

    pub fn add_market(&mut self, market_id: &str, question: &str, initial_yes_price: f64, rng: &mut StdRng) {
        let question = if question.is_empty() { format!("Simulated Market {market_id}") } else { question.to_string() };
        let mut market = Market::new(market_id, market_id, question);
        market.volume_24h = rng.gen_range(10000.0..100000.0);
        self.markets.insert(market_id.to_string(), market);

        self.order_books.insert(
            market_id.to_string(),
            SimulatedOrderBook::new(market_id, initial_yes_price, self.config.price_volatility, self.config.spread_range, self.config.base_liquidity),
        );

        info!(market_id, "Added simulated market");
    }

    pub fn get_market(&self, market_id: &str) -> Option<&Market> {
        self.markets.get(market_id)
    }

    pub fn initial_balance(&self) -> f64 {
        self.config.initial_balance
    }

    /// Advance one tick: yield an updated order book for every simulated
    /// market (in insertion order isn't guaranteed like Python's dict, but
    /// order doesn't affect the aggregate results this produces), then
    /// advance simulated time. Returns `(updates, should_stop)`.
    pub fn step_all(&mut self, rng: &mut StdRng) -> (Vec<(String, OrderBook)>, bool) {
        let mut updates = Vec::with_capacity(self.order_books.len());

        for (market_id, sim_book) in self.order_books.iter_mut() {
            let introduce_mispricing = rng.gen_bool(self.config.mispricing_probability);
            let order_book = sim_book.step(rng, introduce_mispricing, self.config.mispricing_magnitude);
            updates.push((market_id.clone(), order_book));
        }

        self.current_time += ChronoDuration::milliseconds((self.config.time_step_seconds * 1000.0) as i64);

        let should_stop = self.config.end_time.map(|end| self.current_time >= end).unwrap_or(false);
        (updates, should_stop)
    }

    pub fn record_opportunity(&mut self, opportunity_type: &str) {
        if opportunity_type == "bundle_long" || opportunity_type == "bundle_short" {
            self.bundle_opportunity_count += 1;
        } else {
            self.mm_opportunity_count += 1;
        }
    }

    pub fn record_pnl(&mut self, pnl: f64) {
        self.pnl_history.push((self.current_time, pnl));
    }

    pub fn record_exposure(&mut self, exposure: f64) {
        self.exposure_history.push((self.current_time, exposure));
    }

    pub fn get_result(&self, final_balance: f64, realized_pnl: f64, unrealized_pnl: f64, winning_trades: u32, losing_trades: u32) -> BacktestResult {
        let mut max_drawdown = 0.0f64;
        let mut peak = self.config.initial_balance;
        for &(_, pnl) in &self.pnl_history {
            let equity = self.config.initial_balance + pnl;
            if equity > peak {
                peak = equity;
            }
            let drawdown = if peak > 0.0 { (peak - equity) / peak } else { 0.0 };
            max_drawdown = max_drawdown.max(drawdown);
        }

        let max_exposure = self.exposure_history.iter().map(|&(_, e)| e).fold(0.0, f64::max);
        let total_trades = winning_trades + losing_trades;

        BacktestResult {
            start_time: self.config.start_time,
            end_time: self.current_time,
            duration_seconds: (self.current_time - self.config.start_time).num_milliseconds() as f64 / 1000.0,
            initial_balance: self.config.initial_balance,
            final_balance,
            total_pnl: realized_pnl + unrealized_pnl,
            realized_pnl,
            unrealized_pnl,
            total_trades,
            winning_trades,
            losing_trades,
            win_rate: if total_trades > 0 { winning_trades as f64 / total_trades as f64 } else { 0.0 },
            bundle_opportunities: self.bundle_opportunity_count,
            mm_opportunities: self.mm_opportunity_count,
            opportunities_acted_on: self.trade_count,
            max_drawdown,
            max_exposure,
            sharpe_ratio: None,
        }
    }
}

/// High-level function that sets up and runs a complete backtest.
///
/// `risk_manager` is intentionally not a parameter here: Python's
/// `run_backtest` accepted one but never referenced it in the function
/// body, so there's nothing to port.
pub async fn run_backtest(mut config: BacktestConfig, market_ids: Vec<String>, arb_engine: &Mutex<ArbEngine>, execution_engine: &ExecutionEngine, portfolio: &Mutex<Portfolio>, duration_seconds: f64) -> BacktestResult {
    info!(duration_seconds, "Starting backtest");

    config.end_time = Some(config.start_time + ChronoDuration::milliseconds((duration_seconds * 1000.0) as i64));

    let mut rng = StdRng::from_entropy();
    let mut engine = BacktestEngine::new(config);

    for market_id in &market_ids {
        let initial_price = rng.gen_range(0.3..0.7);
        engine.add_market(market_id, "", initial_price, &mut rng);
    }

    let mut update_count = 0u64;

    loop {
        let (updates, should_stop) = engine.step_all(&mut rng);

        for (market_id, order_book) in updates {
            update_count += 1;

            let Some(market) = engine.get_market(&market_id).cloned() else { continue };
            let state = MarketState::new(market, order_book);

            let signals = arb_engine.lock().unwrap().analyze(&state);
            for signal in signals {
                if let Some(opp) = &signal.opportunity {
                    engine.record_opportunity(opp.opportunity_type.as_str());
                }
                execution_engine.submit_signal(signal).await;
            }

            let total_pnl = portfolio.lock().unwrap().stats.total_pnl();
            engine.record_pnl(total_pnl);
            let exposure = portfolio.lock().unwrap().get_total_exposure();
            engine.record_exposure(exposure);

            if update_count % 100 == 0 {
                info!(update_count, "Backtest progress");
            }
        }

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        if should_stop {
            break;
        }
    }

    let (final_pnl, realized, unrealized, winning, losing) = {
        let p = portfolio.lock().unwrap();
        (p.stats.total_pnl(), p.stats.total_realized_pnl, p.stats.total_unrealized_pnl, p.stats.winning_trades, p.stats.losing_trades)
    };

    let result = engine.get_result(engine.initial_balance() + final_pnl, realized, unrealized, winning, losing);

    info!("Backtest completed");
    println!("{}", result.summary());

    result
}
