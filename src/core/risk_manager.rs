//! Position limits, loss limits, and other risk constraints (mirrors
//! `core/risk_manager.py`). Distinct from `crate::config::RiskConfig`
//! (the YAML section), exactly as the Python module keeps its own
//! `RiskConfig` separate from `utils.config_loader.RiskConfig`.

use crate::models::{Order, OrderSide, Trade};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::HashMap;
use tracing::{info, warn};

#[derive(Debug, Clone)]
pub struct RiskConfig {
    pub max_position_per_market: f64,
    pub max_global_exposure: f64,
    pub max_daily_loss: f64,
    pub max_drawdown_pct: f64,
    pub trade_only_high_volume: bool,
    pub min_24h_volume: f64,
    pub whitelist: Vec<String>,
    pub blacklist: Vec<String>,
    pub kill_switch_enabled: bool,
    pub auto_unwind_on_breach: bool,
}

impl Default for RiskConfig {
    fn default() -> Self {
        Self {
            max_position_per_market: 200.0,
            max_global_exposure: 5000.0,
            max_daily_loss: 500.0,
            max_drawdown_pct: 0.10,
            trade_only_high_volume: true,
            min_24h_volume: 10000.0,
            whitelist: Vec::new(),
            blacklist: Vec::new(),
            kill_switch_enabled: true,
            auto_unwind_on_breach: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RiskState {
    pub daily_pnl: f64,
    pub peak_pnl: f64,
    pub current_drawdown: f64,
    pub global_exposure: f64,
    pub kill_switch_triggered: bool,
    pub kill_switch_reason: String,
    pub last_check: DateTime<Utc>,
}

impl Default for RiskState {
    fn default() -> Self {
        Self {
            daily_pnl: 0.0,
            peak_pnl: 0.0,
            current_drawdown: 0.0,
            global_exposure: 0.0,
            kill_switch_triggered: false,
            kill_switch_reason: String::new(),
            last_check: Utc::now(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RiskSummary {
    pub global_exposure: f64,
    pub max_global_exposure: f64,
    pub utilization_pct: f64,
    pub daily_pnl: f64,
    pub max_daily_loss: f64,
    pub peak_pnl: f64,
    pub current_drawdown_pct: f64,
    pub max_drawdown_pct: f64,
    pub kill_switch_triggered: bool,
    pub kill_switch_reason: String,
    pub markets_with_exposure: usize,
    pub session_trade_count: usize,
    pub within_limits: bool,
}

pub struct RiskManager {
    pub config: RiskConfig,
    pub state: RiskState,
    market_exposure: HashMap<String, f64>,
    market_volumes: HashMap<String, f64>,
    session_trades: Vec<Trade>,
}

impl RiskManager {
    pub fn new(config: RiskConfig) -> Self {
        info!(
            max_per_market = config.max_position_per_market,
            max_global = config.max_global_exposure,
            max_daily_loss = config.max_daily_loss,
            "RiskManager initialized"
        );
        Self {
            config,
            state: RiskState::default(),
            market_exposure: HashMap::new(),
            market_volumes: HashMap::new(),
            session_trades: Vec::new(),
        }
    }

    /// Check if an order passes all risk checks.
    pub fn check_order(&mut self, order: &Order) -> bool {
        if self.state.kill_switch_triggered {
            warn!(reason = %self.state.kill_switch_reason, "Order rejected: kill switch triggered");
            return false;
        }

        if self.config.blacklist.contains(&order.market_id) {
            warn!(market_id = %order.market_id, "Order rejected: market is blacklisted");
            return false;
        }

        if !self.config.whitelist.is_empty() && !self.config.whitelist.contains(&order.market_id) {
            warn!(market_id = %order.market_id, "Order rejected: market not in whitelist");
            return false;
        }

        if self.config.trade_only_high_volume {
            let market_volume = self.market_volumes.get(&order.market_id).copied().unwrap_or(0.0);
            if market_volume < self.config.min_24h_volume {
                warn!(market_id = %order.market_id, market_volume, min = self.config.min_24h_volume, "Order rejected: volume below minimum");
                return false;
            }
        }

        let current_market_exposure = self.market_exposure.get(&order.market_id).copied().unwrap_or(0.0);
        let new_exposure = if order.side == OrderSide::Buy { order.notional() } else { -order.notional() };
        let projected_exposure = (current_market_exposure + new_exposure).abs();

        if projected_exposure > self.config.max_position_per_market {
            warn!(
                market_id = %order.market_id,
                current_market_exposure,
                new_exposure,
                projected_exposure,
                limit = self.config.max_position_per_market,
                "Order rejected: would exceed market limit"
            );
            return false;
        }

        let projected_global = self.state.global_exposure + new_exposure.abs();
        if projected_global > self.config.max_global_exposure {
            warn!(
                current = self.state.global_exposure,
                order = new_exposure.abs(),
                projected_global,
                limit = self.config.max_global_exposure,
                "Order rejected: would exceed global limit"
            );
            return false;
        }

        if self.state.daily_pnl < -self.config.max_daily_loss {
            warn!(daily_pnl = self.state.daily_pnl, limit = self.config.max_daily_loss, "Order rejected: daily loss limit exceeded");
            if self.config.kill_switch_enabled {
                self.trigger_kill_switch("Daily loss limit exceeded");
            }
            return false;
        }

        if self.state.current_drawdown > self.config.max_drawdown_pct {
            warn!(drawdown = self.state.current_drawdown, limit = self.config.max_drawdown_pct, "Order rejected: drawdown limit exceeded");
            if self.config.kill_switch_enabled {
                self.trigger_kill_switch("Drawdown limit exceeded");
            }
            return false;
        }

        true
    }

    pub fn update_position(&mut self, market_id: &str, size_delta: f64, price: f64) {
        let notional_change = (size_delta * price).abs();
        let exposure = self.market_exposure.entry(market_id.to_string()).or_insert(0.0);

        if size_delta > 0.0 {
            *exposure += notional_change;
            self.state.global_exposure += notional_change;
        } else {
            *exposure -= notional_change;
            self.state.global_exposure -= notional_change;
        }

        *exposure = exposure.max(0.0);
        self.state.global_exposure = self.state.global_exposure.max(0.0);
        self.state.last_check = Utc::now();
    }

    pub fn update_from_fill(&mut self, trade: &Trade) {
        let size_delta = if trade.side == OrderSide::Buy { trade.size } else { -trade.size };
        self.update_position(&trade.market_id, size_delta, trade.price);
        self.session_trades.push(trade.clone());
    }

    pub fn update_pnl(&mut self, realized_pnl: f64, unrealized_pnl: f64) {
        let total_pnl = realized_pnl + unrealized_pnl;
        self.state.daily_pnl = total_pnl;

        if total_pnl > self.state.peak_pnl {
            self.state.peak_pnl = total_pnl;
        }

        self.state.current_drawdown = if self.state.peak_pnl > 0.0 { (self.state.peak_pnl - total_pnl) / self.state.peak_pnl } else { 0.0 };

        if total_pnl < -self.config.max_daily_loss && self.config.kill_switch_enabled && !self.state.kill_switch_triggered {
            self.trigger_kill_switch("Daily loss limit exceeded");
        }

        if self.state.current_drawdown > self.config.max_drawdown_pct && self.config.kill_switch_enabled && !self.state.kill_switch_triggered {
            self.trigger_kill_switch("Drawdown limit exceeded");
        }
    }

    pub fn update_market_volume(&mut self, market_id: &str, volume_24h: f64) {
        self.market_volumes.insert(market_id.to_string(), volume_24h);
    }

    pub fn set_market_volumes(&mut self, volumes: HashMap<String, f64>) {
        self.market_volumes.extend(volumes);
    }

    fn trigger_kill_switch(&mut self, reason: &str) {
        self.state.kill_switch_triggered = true;
        self.state.kill_switch_reason = reason.to_string();
        tracing::error!(reason, "KILL SWITCH TRIGGERED");
    }

    pub fn reset_kill_switch(&mut self) {
        self.state.kill_switch_triggered = false;
        self.state.kill_switch_reason.clear();
        warn!("Kill switch reset");
    }

    pub fn within_global_limits(&self) -> bool {
        if self.state.kill_switch_triggered {
            return false;
        }
        if self.state.daily_pnl < -self.config.max_daily_loss {
            return false;
        }
        if self.state.current_drawdown > self.config.max_drawdown_pct {
            return false;
        }
        if self.state.global_exposure > self.config.max_global_exposure {
            return false;
        }
        true
    }

    pub fn get_market_exposure(&self, market_id: &str) -> f64 {
        self.market_exposure.get(market_id).copied().unwrap_or(0.0)
    }

    pub fn get_available_exposure(&self, market_id: &str) -> f64 {
        let current = self.market_exposure.get(market_id).copied().unwrap_or(0.0);
        (self.config.max_position_per_market - current).max(0.0)
    }

    pub fn get_global_available(&self) -> f64 {
        (self.config.max_global_exposure - self.state.global_exposure).max(0.0)
    }

    pub fn reset_daily_stats(&mut self) {
        self.state.daily_pnl = 0.0;
        self.state.peak_pnl = 0.0;
        self.state.current_drawdown = 0.0;
        self.session_trades.clear();
        info!("Daily stats reset");
    }

    pub fn get_summary(&self) -> RiskSummary {
        RiskSummary {
            global_exposure: self.state.global_exposure,
            max_global_exposure: self.config.max_global_exposure,
            utilization_pct: if self.config.max_global_exposure > 0.0 {
                self.state.global_exposure / self.config.max_global_exposure * 100.0
            } else {
                0.0
            },
            daily_pnl: self.state.daily_pnl,
            max_daily_loss: self.config.max_daily_loss,
            peak_pnl: self.state.peak_pnl,
            current_drawdown_pct: self.state.current_drawdown * 100.0,
            max_drawdown_pct: self.config.max_drawdown_pct * 100.0,
            kill_switch_triggered: self.state.kill_switch_triggered,
            kill_switch_reason: self.state.kill_switch_reason.clone(),
            markets_with_exposure: self.market_exposure.values().filter(|&&e| e > 0.0).count(),
            session_trade_count: self.session_trades.len(),
            within_limits: self.within_global_limits(),
        }
    }

    pub fn add_to_blacklist(&mut self, market_id: &str) {
        if !self.config.blacklist.iter().any(|m| m == market_id) {
            self.config.blacklist.push(market_id.to_string());
            info!(market_id, "Market added to blacklist");
        }
    }

    pub fn remove_from_blacklist(&mut self, market_id: &str) {
        if let Some(pos) = self.config.blacklist.iter().position(|m| m == market_id) {
            self.config.blacklist.remove(pos);
            info!(market_id, "Market removed from blacklist");
        }
    }
}

/// Ported from `tests/test_risk_manager.py`: same fixtures, same cases.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::TokenType;

    /// Mirrors the `risk_config`/`risk_manager` pytest fixtures.
    fn risk_manager() -> RiskManager {
        let mut rm = RiskManager::new(RiskConfig {
            max_position_per_market: 200.0,
            max_global_exposure: 1000.0,
            max_daily_loss: 100.0,
            max_drawdown_pct: 0.10,
            trade_only_high_volume: true,
            min_24h_volume: 10000.0,
            whitelist: Vec::new(),
            blacklist: vec!["blocked_market".to_string()],
            kill_switch_enabled: true,
            auto_unwind_on_breach: false,
        });
        rm.set_market_volumes(HashMap::from([("test_market".to_string(), 50000.0), ("low_volume_market".to_string(), 1000.0)]));
        rm
    }

    /// Mirrors the `create_order` helper (defaults: test_market, BUY, 0.50, 100.0).
    fn create_order(market_id: &str, side: OrderSide, price: f64, size: f64) -> Order {
        Order::new("test_order", market_id, TokenType::Yes, side, price, size)
    }

    mod order_validation {
        use super::*;

        #[test]
        fn valid_order_passes() {
            let mut rm = risk_manager();
            let order = create_order("test_market", OrderSide::Buy, 0.50, 100.0); // $50 notional
            assert!(rm.check_order(&order));
        }

        #[test]
        fn reject_blacklisted_market() {
            let mut rm = risk_manager();
            let order = create_order("blocked_market", OrderSide::Buy, 0.50, 100.0);
            assert!(!rm.check_order(&order));
        }

        #[test]
        fn reject_low_volume_market() {
            let mut rm = risk_manager();
            let order = create_order("low_volume_market", OrderSide::Buy, 0.50, 100.0);
            assert!(!rm.check_order(&order));
        }

        #[test]
        fn reject_exceeds_market_limit() {
            let mut rm = risk_manager();
            rm.update_position("test_market", 350.0, 0.50);
            let order = create_order("test_market", OrderSide::Buy, 0.50, 100.0); // Additional $50.
            assert!(!rm.check_order(&order));
        }

        #[test]
        fn reject_exceeds_global_limit() {
            let mut rm = risk_manager();
            rm.update_position("market_1", 800.0, 1.0); // $800
            rm.update_position("market_2", 150.0, 1.0); // $150
            let order = create_order("test_market", OrderSide::Buy, 0.50, 200.0); // Additional $100.
            assert!(!rm.check_order(&order));
        }
    }

    mod kill_switch {
        use super::*;

        #[test]
        fn on_daily_loss() {
            let mut rm = risk_manager();
            rm.update_pnl(-150.0, 0.0); // $150 loss > $100 limit.
            let order = create_order("test_market", OrderSide::Buy, 0.50, 100.0);
            assert!(!rm.check_order(&order));
            assert!(rm.state.kill_switch_triggered);
        }

        #[test]
        fn on_drawdown() {
            let mut rm = risk_manager();
            rm.update_pnl(1000.0, 0.0); // Peak at $1000.
            rm.update_pnl(800.0, 0.0); // Now at $800 = 20% drawdown.
            let order = create_order("test_market", OrderSide::Buy, 0.50, 100.0);
            assert!(!rm.check_order(&order));
            assert!(rm.state.kill_switch_triggered);
        }

        #[test]
        fn reset() {
            let mut rm = risk_manager();
            rm.update_pnl(-150.0, 0.0);
            assert!(rm.state.kill_switch_triggered);
            rm.reset_kill_switch();
            assert!(!rm.state.kill_switch_triggered);
        }
    }

    mod exposure_tracking {
        use super::*;

        #[test]
        fn market_exposure_tracking() {
            let mut rm = risk_manager();
            rm.update_position("market_1", 100.0, 0.50);
            assert_eq!(rm.get_market_exposure("market_1"), 50.0);
            assert_eq!(rm.get_market_exposure("market_2"), 0.0);
        }

        #[test]
        fn global_exposure_tracking() {
            let mut rm = risk_manager();
            rm.update_position("market_1", 100.0, 0.50);
            rm.update_position("market_2", 200.0, 0.25);
            assert_eq!(rm.state.global_exposure, 100.0); // 50 + 50
        }

        #[test]
        fn available_exposure() {
            let mut rm = risk_manager();
            rm.update_position("test_market", 100.0, 1.0);
            assert_eq!(rm.get_available_exposure("test_market"), 100.0); // 200 limit - 100 used.
        }
    }

    mod risk_summary {
        use super::*;

        #[test]
        fn within_limits_check() {
            let mut rm = risk_manager();
            assert!(rm.within_global_limits());
            rm.update_pnl(-150.0, 0.0);
            assert!(!rm.within_global_limits());
        }
    }

    mod blacklist_management {
        use super::*;

        #[test]
        fn add_to_blacklist() {
            let mut rm = risk_manager();
            rm.add_to_blacklist("new_blocked_market");
            let order = create_order("new_blocked_market", OrderSide::Buy, 0.50, 100.0);
            assert!(!rm.check_order(&order));
        }

        #[test]
        fn remove_from_blacklist() {
            let mut rm = risk_manager();
            rm.remove_from_blacklist("blocked_market");
            rm.set_market_volumes(HashMap::from([("blocked_market".to_string(), 50000.0)]));
            let order = create_order("blocked_market", OrderSide::Buy, 0.50, 100.0);
            assert!(rm.check_order(&order));
        }
    }
}
