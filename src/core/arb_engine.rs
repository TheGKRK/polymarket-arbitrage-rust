//! Bundle-mispricing and market-making opportunity detection (mirrors
//! `core/arb_engine.py`). The fee-aware edge math here is
//! dollar-correctness-critical and is ported 1:1 from the Python formulas.

use crate::models::{MarketState, Opportunity, OpportunityType, OrderBook, OrderSide, OrderSpec, Signal, TokenOrderBook, TokenType};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::Serialize;
use std::collections::HashMap;
use tracing::info;

#[derive(Debug, Clone)]
pub struct ArbConfig {
    pub min_edge: f64,
    pub bundle_arb_enabled: bool,
    pub min_spread: f64,
    pub mm_enabled: bool,
    pub tick_size: f64,
    pub default_order_size: f64,
    pub min_order_size: f64,
    pub max_order_size: f64,
    pub signal_expiry_seconds: f64,
    pub maker_fee_bps: f64,
    pub taker_fee_bps: f64,
    pub gas_cost_per_order: f64,
}

impl Default for ArbConfig {
    fn default() -> Self {
        Self {
            min_edge: 0.01,
            bundle_arb_enabled: true,
            min_spread: 0.05,
            mm_enabled: true,
            tick_size: 0.01,
            default_order_size: 50.0,
            min_order_size: 5.0,
            max_order_size: 200.0,
            signal_expiry_seconds: 5.0,
            maker_fee_bps: 0.0,
            taker_fee_bps: 150.0,
            gas_cost_per_order: 0.02,
        }
    }
}

#[derive(Debug, Clone)]
pub struct OpportunityTiming {
    pub opportunity_id: String,
    pub market_id: String,
    pub opportunity_type: String,
    pub detected_at: DateTime<Utc>,
    pub edge: f64,
    pub expired_at: Option<DateTime<Utc>>,
    pub duration_ms: Option<f64>,
    pub was_executed: bool,
}

impl OpportunityTiming {
    pub fn mark_expired(&mut self, executed: bool) {
        let now = Utc::now();
        self.expired_at = Some(now);
        self.duration_ms = Some((now - self.detected_at).num_microseconds().unwrap_or(0) as f64 / 1000.0);
        self.was_executed = executed;
    }
}

#[derive(Debug, Clone)]
pub struct ArbStats {
    pub bundle_opportunities_detected: u32,
    pub mm_opportunities_detected: u32,
    pub signals_generated: u32,
    pub last_opportunity_time: Option<DateTime<Utc>>,
    pub total_opportunities_tracked: u32,
    pub avg_opportunity_duration_ms: f64,
    pub min_opportunity_duration_ms: f64,
    pub max_opportunity_duration_ms: f64,
    pub opportunities_under_100ms: u32,
    pub opportunities_under_500ms: u32,
    pub opportunities_under_1s: u32,
    pub opportunities_over_1s: u32,
}

impl Default for ArbStats {
    fn default() -> Self {
        Self {
            bundle_opportunities_detected: 0,
            mm_opportunities_detected: 0,
            signals_generated: 0,
            last_opportunity_time: None,
            total_opportunities_tracked: 0,
            avg_opportunity_duration_ms: 0.0,
            min_opportunity_duration_ms: f64::INFINITY,
            max_opportunity_duration_ms: 0.0,
            opportunities_under_100ms: 0,
            opportunities_under_500ms: 0,
            opportunities_under_1s: 0,
            opportunities_over_1s: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RecentDuration {
    #[serde(rename = "type")]
    pub kind: String,
    pub duration_ms: f64,
    pub edge: f64,
    pub executed: bool,
    pub time: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TimingStats {
    pub total_tracked: u32,
    pub avg_duration_ms: f64,
    pub min_duration_ms: Option<f64>,
    pub max_duration_ms: f64,
    pub under_100ms: u32,
    pub under_500ms: u32,
    pub under_1s: u32,
    pub over_1s: u32,
    pub active_opportunities: usize,
    pub recent_durations: Vec<RecentDuration>,
}

pub struct ArbEngine {
    pub config: ArbConfig,
    pub stats: ArbStats,
    recent_opportunities: HashMap<String, Opportunity>,
    opportunity_cooldown: HashMap<String, DateTime<Utc>>,
    active_opportunities: HashMap<String, OpportunityTiming>,
    opportunity_history: Vec<OpportunityTiming>,
}

fn short_id(prefix: &str, len: usize) -> String {
    format!("{prefix}_{}", &uuid::Uuid::new_v4().simple().to_string()[..len])
}

impl ArbEngine {
    pub fn new(config: ArbConfig) -> Self {
        info!(min_edge = config.min_edge, min_spread = config.min_spread, "ArbEngine initialized");
        Self {
            config,
            stats: ArbStats::default(),
            recent_opportunities: HashMap::new(),
            opportunity_cooldown: HashMap::new(),
            active_opportunities: HashMap::new(),
            opportunity_history: Vec::new(),
        }
    }

    /// Analyze a market state and generate trading signals.
    pub fn analyze(&mut self, market_state: &MarketState) -> Vec<Signal> {
        let mut signals = Vec::new();
        let order_book = &market_state.order_book;
        let market_id = market_state.market.market_id.clone();

        self.check_expired_opportunities(&market_id, order_book);

        if self.config.bundle_arb_enabled {
            if let Some(signal) = self.check_bundle_arbitrage(&market_id, order_book) {
                signals.push(signal);
            }
        }

        if self.config.mm_enabled {
            signals.extend(self.check_market_making(&market_id, order_book));
        }

        signals
    }

    fn check_expired_opportunities(&mut self, market_id: &str, order_book: &OrderBook) {
        let now = Utc::now();
        let mut expired_keys = Vec::new();

        for (key, timing) in self.active_opportunities.iter_mut() {
            if timing.market_id != market_id {
                continue;
            }

            let mut still_valid = false;

            if timing.opportunity_type.contains("bundle_long") {
                if let (Some(ask_yes), Some(ask_no)) = (order_book.best_ask_yes(), order_book.best_ask_no()) {
                    let total_ask = ask_yes + ask_no;
                    if 1.0 - total_ask >= self.config.min_edge * 0.5 {
                        still_valid = true;
                    }
                }
            } else if timing.opportunity_type.contains("bundle_short") {
                if let (Some(bid_yes), Some(bid_no)) = (order_book.best_bid_yes(), order_book.best_bid_no()) {
                    let total_bid = bid_yes + bid_no;
                    if total_bid - 1.0 >= self.config.min_edge * 0.5 {
                        still_valid = true;
                    }
                }
            }

            let age_seconds = (now - timing.detected_at).num_milliseconds() as f64 / 1000.0;
            if age_seconds > 10.0 {
                still_valid = false;
            }

            if !still_valid {
                timing.mark_expired(false);
                expired_keys.push(key.clone());
            }
        }

        for key in expired_keys {
            if let Some(timing) = self.active_opportunities.remove(&key) {
                self.record_opportunity_duration(timing);
            }
        }
    }

    fn record_opportunity_duration(&mut self, timing: OpportunityTiming) {
        let Some(duration_ms) = timing.duration_ms else { return };

        self.opportunity_history.push(timing.clone());
        if self.opportunity_history.len() > 1000 {
            let drain_to = self.opportunity_history.len() - 500;
            self.opportunity_history.drain(..drain_to);
        }

        self.stats.total_opportunities_tracked += 1;

        if duration_ms < self.stats.min_opportunity_duration_ms {
            self.stats.min_opportunity_duration_ms = duration_ms;
        }
        if duration_ms > self.stats.max_opportunity_duration_ms {
            self.stats.max_opportunity_duration_ms = duration_ms;
        }

        let n = self.stats.total_opportunities_tracked as f64;
        let old_avg = self.stats.avg_opportunity_duration_ms;
        self.stats.avg_opportunity_duration_ms = old_avg + (duration_ms - old_avg) / n;

        if duration_ms < 100.0 {
            self.stats.opportunities_under_100ms += 1;
        } else if duration_ms < 500.0 {
            self.stats.opportunities_under_500ms += 1;
        } else if duration_ms < 1000.0 {
            self.stats.opportunities_under_1s += 1;
        } else {
            self.stats.opportunities_over_1s += 1;
        }

        info!(
            opportunity_type = %timing.opportunity_type,
            duration_ms,
            edge = timing.edge,
            market_id = %timing.market_id,
            "Opportunity EXPIRED"
        );
    }

    fn start_tracking_opportunity(&mut self, opportunity: &Opportunity) {
        let key = format!("{}_{}", opportunity.market_id, opportunity.opportunity_type.as_str());
        if self.active_opportunities.contains_key(&key) {
            return;
        }
        self.active_opportunities.insert(
            key,
            OpportunityTiming {
                opportunity_id: opportunity.opportunity_id.clone(),
                market_id: opportunity.market_id.clone(),
                opportunity_type: opportunity.opportunity_type.as_str().to_string(),
                detected_at: Utc::now(),
                edge: opportunity.edge,
                expired_at: None,
                duration_ms: None,
                was_executed: false,
            },
        );
    }

    pub fn mark_opportunity_executed(&mut self, market_id: &str, opportunity_type: &str) {
        let key = format!("{market_id}_{opportunity_type}");
        if let Some(mut timing) = self.active_opportunities.remove(&key) {
            timing.mark_expired(true);
            self.record_opportunity_duration(timing);
        }
    }

    pub fn get_timing_stats(&self) -> TimingStats {
        let recent_history: &[OpportunityTiming] = {
            let len = self.opportunity_history.len();
            &self.opportunity_history[len.saturating_sub(100)..]
        };
        let tail_start = recent_history.len().saturating_sub(20);

        TimingStats {
            total_tracked: self.stats.total_opportunities_tracked,
            avg_duration_ms: round1(self.stats.avg_opportunity_duration_ms),
            min_duration_ms: if self.stats.min_opportunity_duration_ms.is_finite() { Some(round1(self.stats.min_opportunity_duration_ms)) } else { None },
            max_duration_ms: round1(self.stats.max_opportunity_duration_ms),
            under_100ms: self.stats.opportunities_under_100ms,
            under_500ms: self.stats.opportunities_under_500ms,
            under_1s: self.stats.opportunities_under_1s,
            over_1s: self.stats.opportunities_over_1s,
            active_opportunities: self.active_opportunities.len(),
            recent_durations: recent_history[tail_start..]
                .iter()
                .map(|t| RecentDuration {
                    kind: t.opportunity_type.clone(),
                    duration_ms: t.duration_ms.map(round1).unwrap_or(0.0),
                    edge: round4(t.edge),
                    executed: t.was_executed,
                    time: t.detected_at.to_rfc3339(),
                })
                .collect(),
        }
    }

    /// Bundle Long: buy YES + NO when total_ask < 1 - min_edge - fees.
    /// Bundle Short: sell YES + NO when total_bid > 1 + min_edge + fees.
    /// Fees are factored in to ensure net profitability.
    fn check_bundle_arbitrage(&mut self, market_id: &str, order_book: &OrderBook) -> Option<Signal> {
        let (best_ask_yes, best_ask_no, best_bid_yes, best_bid_no) =
            (order_book.best_ask_yes(), order_book.best_ask_no(), order_book.best_bid_yes(), order_book.best_bid_no());

        let (best_ask_yes, best_ask_no, best_bid_yes, best_bid_no) = match (best_ask_yes, best_ask_no, best_bid_yes, best_bid_no) {
            (Some(a), Some(b), Some(c), Some(d)) => (a, b, c, d),
            _ => return None,
        };

        let total_ask = best_ask_yes + best_ask_no;
        let total_bid = best_bid_yes + best_bid_no;

        let taker_fee_pct = self.config.taker_fee_bps / 10000.0;
        let gas_cost = self.config.gas_cost_per_order * 2.0;

        let fee_cost_long = taker_fee_pct * total_ask;
        let fee_cost_short = taker_fee_pct * total_bid;

        let mut opportunity: Option<Opportunity> = None;

        let gross_edge_long = 1.0 - total_ask;
        let net_edge_long = gross_edge_long - fee_cost_long - gas_cost;

        if net_edge_long >= self.config.min_edge {
            let edge = net_edge_long;
            let yes_ask_size = order_book.yes.best_ask_size().unwrap_or(0.0);
            let no_ask_size = order_book.no.best_ask_size().unwrap_or(0.0);
            let max_size = yes_ask_size.min(no_ask_size);

            let mut suggested_size = (self.config.default_order_size / best_ask_yes.max(best_ask_no)).min(max_size);
            suggested_size = suggested_size.max(self.config.min_order_size);

            let opp = Opportunity {
                opportunity_id: short_id("bundle_long", 8),
                opportunity_type: OpportunityType::BundleLong,
                market_id: market_id.to_string(),
                edge,
                best_bid_yes: Some(best_bid_yes),
                best_ask_yes: Some(best_ask_yes),
                best_bid_no: Some(best_bid_no),
                best_ask_no: Some(best_ask_no),
                suggested_size,
                max_size,
                detected_at: Utc::now(),
                expires_at: Some(Utc::now() + ChronoDuration::milliseconds((self.config.signal_expiry_seconds * 1000.0) as i64)),
                acted_upon: false,
            };

            self.stats.bundle_opportunities_detected += 1;
            info!(market_id, total_ask, gross_edge_long, fee_cost_long, edge, suggested_size, "Bundle LONG opportunity");
            opportunity = Some(opp);
        }

        let gross_edge_short = total_bid - 1.0;
        let net_edge_short = gross_edge_short - fee_cost_short - gas_cost;

        if opportunity.is_none() && net_edge_short >= self.config.min_edge {
            let edge = net_edge_short;
            let yes_bid_size = order_book.yes.best_bid_size().unwrap_or(0.0);
            let no_bid_size = order_book.no.best_bid_size().unwrap_or(0.0);
            let max_size = yes_bid_size.min(no_bid_size);

            let mut suggested_size = (self.config.default_order_size / best_bid_yes.max(best_bid_no)).min(max_size);
            suggested_size = suggested_size.max(self.config.min_order_size);

            let opp = Opportunity {
                opportunity_id: short_id("bundle_short", 8),
                opportunity_type: OpportunityType::BundleShort,
                market_id: market_id.to_string(),
                edge,
                best_bid_yes: Some(best_bid_yes),
                best_ask_yes: Some(best_ask_yes),
                best_bid_no: Some(best_bid_no),
                best_ask_no: Some(best_ask_no),
                suggested_size,
                max_size,
                detected_at: Utc::now(),
                expires_at: Some(Utc::now() + ChronoDuration::milliseconds((self.config.signal_expiry_seconds * 1000.0) as i64)),
                acted_upon: false,
            };

            self.stats.bundle_opportunities_detected += 1;
            info!(market_id, total_bid, gross_edge_short, fee_cost_short, edge, suggested_size, "Bundle SHORT opportunity");
            opportunity = Some(opp);
        }

        let opportunity = opportunity?;

        let cooldown_key = format!("{market_id}_{}", opportunity.opportunity_type.as_str());
        if let Some(until) = self.opportunity_cooldown.get(&cooldown_key) {
            if Utc::now() < *until {
                return None;
            }
        }

        self.opportunity_cooldown.insert(cooldown_key, Utc::now() + ChronoDuration::seconds(2));
        self.recent_opportunities.insert(opportunity.opportunity_id.clone(), opportunity.clone());
        self.stats.last_opportunity_time = Some(Utc::now());

        self.start_tracking_opportunity(&opportunity);

        Some(self.create_bundle_signal(opportunity))
    }

    fn create_bundle_signal(&mut self, opportunity: Opportunity) -> Signal {
        let orders = if opportunity.opportunity_type == OpportunityType::BundleLong {
            vec![
                OrderSpec { token_type: TokenType::Yes, side: OrderSide::Buy, price: opportunity.best_ask_yes.unwrap(), size: opportunity.suggested_size, strategy_tag: "bundle_arb".to_string() },
                OrderSpec { token_type: TokenType::No, side: OrderSide::Buy, price: opportunity.best_ask_no.unwrap(), size: opportunity.suggested_size, strategy_tag: "bundle_arb".to_string() },
            ]
        } else {
            vec![
                OrderSpec { token_type: TokenType::Yes, side: OrderSide::Sell, price: opportunity.best_bid_yes.unwrap(), size: opportunity.suggested_size, strategy_tag: "bundle_arb".to_string() },
                OrderSpec { token_type: TokenType::No, side: OrderSide::Sell, price: opportunity.best_bid_no.unwrap(), size: opportunity.suggested_size, strategy_tag: "bundle_arb".to_string() },
            ]
        };

        let signal = Signal::new_place_orders(short_id("sig", 12), opportunity.market_id.clone(), Some(opportunity), orders, 10);
        self.stats.signals_generated += 1;
        signal
    }

    fn check_market_making(&mut self, market_id: &str, order_book: &OrderBook) -> Vec<Signal> {
        let mut signals = Vec::new();

        if let Some(signal) = self.check_mm_token(market_id, &order_book.yes, TokenType::Yes) {
            signals.push(signal);
        }
        if let Some(signal) = self.check_mm_token(market_id, &order_book.no, TokenType::No) {
            signals.push(signal);
        }

        signals
    }

    fn check_mm_token(&mut self, market_id: &str, token_book: &TokenOrderBook, token_type: TokenType) -> Option<Signal> {
        let (best_bid, best_ask, spread) = (token_book.best_bid(), token_book.best_ask(), token_book.spread());
        let (best_bid, best_ask, spread) = match (best_bid, best_ask, spread) {
            (Some(b), Some(a), Some(s)) => (b, a, s),
            _ => return None,
        };

        if spread < self.config.min_spread {
            return None;
        }

        let cooldown_key = format!("mm_{market_id}_{}", token_type.as_str());
        if let Some(until) = self.opportunity_cooldown.get(&cooldown_key) {
            if Utc::now() < *until {
                return None;
            }
        }
        self.opportunity_cooldown.insert(cooldown_key, Utc::now() + ChronoDuration::seconds(5));

        let our_bid = best_bid + self.config.tick_size;
        let our_ask = best_ask - self.config.tick_size;

        if our_ask <= our_bid {
            return None;
        }

        let our_spread = our_ask - our_bid;
        if our_spread < self.config.tick_size * 2.0 {
            return None;
        }

        let mut order_size = self.config.default_order_size / ((our_bid + our_ask) / 2.0);
        order_size = order_size.min(self.config.max_order_size);
        order_size = order_size.max(self.config.min_order_size);

        let opportunity = Opportunity {
            opportunity_id: short_id(&format!("mm_{}", token_type.as_str()), 8),
            opportunity_type: if token_type == TokenType::Yes { OpportunityType::MmBid } else { OpportunityType::MmAsk },
            market_id: market_id.to_string(),
            edge: our_spread / 2.0,
            best_bid_yes: None,
            best_ask_yes: None,
            best_bid_no: None,
            best_ask_no: None,
            suggested_size: order_size,
            max_size: order_size * 2.0,
            detected_at: Utc::now(),
            expires_at: None,
            acted_upon: false,
        };

        self.stats.mm_opportunities_detected += 1;
        self.stats.last_opportunity_time = Some(Utc::now());

        info!(market_id, token = token_type.as_str(), spread, our_spread, order_size, "MM opportunity");

        let orders = vec![
            OrderSpec { token_type, side: OrderSide::Buy, price: our_bid, size: order_size, strategy_tag: "market_making".to_string() },
            OrderSpec { token_type, side: OrderSide::Sell, price: our_ask, size: order_size, strategy_tag: "market_making".to_string() },
        ];

        let signal = Signal::new_place_orders(short_id("sig", 12), market_id, Some(opportunity), orders, 5);
        self.stats.signals_generated += 1;
        Some(signal)
    }

    pub fn get_recent_opportunities(&self, max_age_seconds: f64) -> Vec<&Opportunity> {
        let cutoff = Utc::now() - ChronoDuration::milliseconds((max_age_seconds * 1000.0) as i64);
        self.recent_opportunities.values().filter(|opp| opp.detected_at > cutoff).collect()
    }

    pub fn clear_expired_opportunities(&mut self) -> usize {
        let now = Utc::now();
        let expired: Vec<String> = self
            .recent_opportunities
            .iter()
            .filter(|(_, opp)| opp.expires_at.map(|e| e < now).unwrap_or(false))
            .map(|(id, _)| id.clone())
            .collect();
        for id in &expired {
            self.recent_opportunities.remove(id);
        }
        expired.len()
    }

    pub fn get_stats(&self) -> &ArbStats {
        &self.stats
    }
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

fn round4(v: f64) -> f64 {
    (v * 10000.0).round() / 10000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Market, OrderBookSide, PriceLevel};

    fn book_with(bid_yes: f64, ask_yes: f64, bid_no: f64, ask_no: f64) -> OrderBook {
        let mut ob = OrderBook::new("m1");
        ob.yes.bids = OrderBookSide::new(vec![PriceLevel::new(bid_yes, 100.0)]);
        ob.yes.asks = OrderBookSide::new(vec![PriceLevel::new(ask_yes, 100.0)]);
        ob.no.bids = OrderBookSide::new(vec![PriceLevel::new(bid_no, 100.0)]);
        ob.no.asks = OrderBookSide::new(vec![PriceLevel::new(ask_no, 100.0)]);
        ob
    }

    fn state_with(ob: OrderBook) -> MarketState {
        MarketState::new(Market::new("m1", "c1", "Q?"), ob)
    }

    #[test]
    fn detects_bundle_long_when_underpriced_after_fees() {
        // total_ask = 0.90, fees = 1.5%*0.90 + 0.04 = 0.0535, net edge = 0.10-0.0535=0.0465 > 1% min_edge
        let mut engine = ArbEngine::new(ArbConfig { min_edge: 0.01, ..Default::default() });
        let ob = book_with(0.44, 0.46, 0.42, 0.44);
        let signals = engine.analyze(&state_with(ob));

        assert_eq!(signals.len(), 1);
        let opp = signals[0].opportunity.as_ref().unwrap();
        assert_eq!(opp.opportunity_type, OpportunityType::BundleLong);
    }

    #[test]
    fn no_bundle_signal_when_within_fair_value() {
        let mut engine = ArbEngine::new(ArbConfig::default());
        let ob = book_with(0.48, 0.50, 0.48, 0.50); // total_ask = 1.00, no edge
        let signals = engine.analyze(&state_with(ob));
        assert!(signals.iter().all(|s| s.opportunity.as_ref().map(|o| !o.is_bundle_arb()).unwrap_or(true)));
    }

    #[test]
    fn detects_market_making_when_spread_wide_enough() {
        let mut engine = ArbEngine::new(ArbConfig { min_spread: 0.05, bundle_arb_enabled: false, ..Default::default() });
        let ob = book_with(0.40, 0.50, 0.40, 0.50); // 10c spread on both sides
        let signals = engine.analyze(&state_with(ob));
        assert!(signals.iter().any(|s| s.opportunity.as_ref().map(|o| o.is_market_making()).unwrap_or(false)));
    }

    /// Everything below is ported from `tests/test_arb_engine.py`: same
    /// `arb_config`/`arb_engine` fixture (fees zeroed for easy edge-math
    /// verification), same helper functions, same cases.
    fn test_engine() -> ArbEngine {
        ArbEngine::new(ArbConfig {
            min_edge: 0.01,
            bundle_arb_enabled: true,
            min_spread: 0.05,
            mm_enabled: true,
            tick_size: 0.01,
            default_order_size: 50.0,
            maker_fee_bps: 0.0,
            taker_fee_bps: 0.0,
            gas_cost_per_order: 0.0,
            ..Default::default()
        })
    }

    fn market_state(yes_bid: f64, yes_ask: f64, no_bid: f64, no_ask: f64) -> MarketState {
        let mut market = Market::new("test_market", "test_market", "Test Market");
        market.volume_24h = 50000.0;
        MarketState::new(market, book_with(yes_bid, yes_ask, no_bid, no_ask))
    }

    mod bundle_arbitrage {
        use super::*;

        #[test]
        fn detect_bundle_long_opportunity() {
            // YES ask = 0.45, NO ask = 0.50 -> total = 0.95 (5% edge).
            let mut engine = test_engine();
            let signals = engine.analyze(&market_state(0.43, 0.45, 0.48, 0.50));

            let bundle_signals: Vec<_> = signals.iter().filter(|s| s.opportunity.as_ref().map(|o| o.is_bundle_arb()).unwrap_or(false)).collect();
            assert_eq!(bundle_signals.len(), 1);

            let opp = bundle_signals[0].opportunity.as_ref().unwrap();
            assert_eq!(opp.opportunity_type, OpportunityType::BundleLong);
            assert!(opp.edge >= 0.04); // At least 4% edge.
            assert_eq!(bundle_signals[0].orders.len(), 2); // Both YES and NO orders.
        }

        #[test]
        fn detect_bundle_short_opportunity() {
            // YES bid = 0.55, NO bid = 0.50 -> total = 1.05 (5% edge).
            let mut engine = test_engine();
            let signals = engine.analyze(&market_state(0.55, 0.57, 0.50, 0.52));

            let bundle_signals: Vec<_> = signals.iter().filter(|s| s.opportunity.as_ref().map(|o| o.is_bundle_arb()).unwrap_or(false)).collect();
            assert_eq!(bundle_signals.len(), 1);

            let opp = bundle_signals[0].opportunity.as_ref().unwrap();
            assert_eq!(opp.opportunity_type, OpportunityType::BundleShort);
            assert!(opp.edge >= 0.04);
        }

        #[test]
        fn no_opportunity_when_fair() {
            // YES ask = 0.50, NO ask = 0.50 -> total = 1.00 (no edge).
            let mut engine = test_engine();
            let signals = engine.analyze(&market_state(0.48, 0.50, 0.48, 0.50));
            assert!(signals.iter().all(|s| s.opportunity.as_ref().map(|o| !o.is_bundle_arb()).unwrap_or(true)));
        }

        #[test]
        fn edge_below_threshold() {
            // Total ask = 0.995 -> only 0.5% edge, below the 1% threshold.
            let mut engine = test_engine();
            let signals = engine.analyze(&market_state(0.48, 0.50, 0.48, 0.495));
            assert!(signals.iter().all(|s| s.opportunity.as_ref().map(|o| !o.is_bundle_arb()).unwrap_or(true)));
        }
    }

    mod market_making {
        use super::*;

        #[test]
        fn detect_mm_opportunity_wide_spread() {
            // YES spread = 0.10 (10%) - above the 5% min_spread.
            let mut engine = test_engine();
            let signals = engine.analyze(&market_state(0.45, 0.55, 0.40, 0.50));
            assert!(signals.iter().any(|s| s.opportunity.as_ref().map(|o| o.is_market_making()).unwrap_or(false)));
        }

        #[test]
        fn no_mm_opportunity_tight_spread() {
            // YES spread = 0.02 (2%) - below the 5% min_spread.
            let mut engine = test_engine();
            let signals = engine.analyze(&market_state(0.49, 0.51, 0.48, 0.50));
            assert!(signals.iter().all(|s| s.opportunity.as_ref().map(|o| !o.is_market_making()).unwrap_or(true)));
        }
    }

    mod signal_generation {
        use super::*;

        #[test]
        fn signal_contains_correct_orders() {
            let mut engine = test_engine();
            let signals = engine.analyze(&market_state(0.43, 0.45, 0.48, 0.50));

            for signal in &signals {
                assert!(signal.is_place() || signal.is_cancel());
                assert_eq!(signal.market_id, "test_market");
                for order in &signal.orders {
                    // `OrderSpec` is a typed struct here (Python used a loose
                    // dict); simply constructing it is the parity check.
                    let _ = (order.token_type, order.side, order.price, order.size);
                }
            }
        }

        #[test]
        fn statistics_tracking() {
            let mut engine = test_engine();
            assert_eq!(engine.get_stats().bundle_opportunities_detected, 0);

            engine.analyze(&market_state(0.43, 0.45, 0.48, 0.50));

            let stats = engine.get_stats();
            assert!(stats.bundle_opportunities_detected >= 1);
            assert!(stats.signals_generated >= 1);
        }
    }

    mod edge_cases {
        use super::*;

        #[test]
        fn missing_prices() {
            let mut engine = test_engine();
            let state = MarketState::new(Market::new("test_market", "test_market", "Test Market"), OrderBook::new("test_market"));
            let signals = engine.analyze(&state);
            assert!(signals.iter().all(|s| s.opportunity.as_ref().map(|o| !o.is_bundle_arb()).unwrap_or(true)));
        }

        #[test]
        fn extreme_prices_do_not_panic() {
            let mut engine = test_engine();
            let signals = engine.analyze(&market_state(0.01, 0.02, 0.01, 0.02));
            let _ = signals; // Should not panic.
        }
    }
}
