//! Inventory, positions, and PnL tracking (mirrors `core/portfolio.py`).

use crate::models::{OrderSide, TokenType, Trade};
use serde::Serialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize)]
pub struct PortfolioPosition {
    pub market_id: String,
    pub token_type: TokenType,
    pub size: f64,
    pub avg_entry_price: f64,
    pub realized_pnl: f64,
    pub cost_basis: f64,
    pub total_bought: f64,
    pub total_sold: f64,
    pub trade_count: u32,
}

impl PortfolioPosition {
    fn new(market_id: impl Into<String>, token_type: TokenType) -> Self {
        Self {
            market_id: market_id.into(),
            token_type,
            size: 0.0,
            avg_entry_price: 0.0,
            realized_pnl: 0.0,
            cost_basis: 0.0,
            total_bought: 0.0,
            total_sold: 0.0,
            trade_count: 0,
        }
    }

    pub fn unrealized_pnl(&self, current_price: f64) -> f64 {
        if self.size == 0.0 {
            0.0
        } else {
            self.size * (current_price - self.avg_entry_price)
        }
    }

    pub fn total_pnl(&self, current_price: f64) -> f64 {
        self.realized_pnl + self.unrealized_pnl(current_price)
    }

    pub fn notional(&self) -> f64 {
        self.size.abs() * self.avg_entry_price
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PortfolioStats {
    pub total_realized_pnl: f64,
    pub total_unrealized_pnl: f64,
    pub total_fees_paid: f64,
    pub total_trades: u32,
    pub winning_trades: u32,
    pub losing_trades: u32,
    pub total_volume: f64,
}

impl PortfolioStats {
    pub fn total_pnl(&self) -> f64 {
        self.total_realized_pnl + self.total_unrealized_pnl
    }

    pub fn win_rate(&self) -> f64 {
        let total = self.winning_trades + self.losing_trades;
        if total == 0 {
            0.0
        } else {
            self.winning_trades as f64 / total as f64
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Exposure {
    pub yes_size: f64,
    pub no_size: f64,
    pub yes_notional: f64,
    pub no_notional: f64,
    pub total_notional: f64,
    pub net_position: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PnlBreakdown {
    pub realized_pnl: f64,
    pub unrealized_pnl: f64,
    pub total_pnl: f64,
    pub fees_paid: f64,
    pub net_pnl: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PortfolioSummary {
    pub initial_balance: f64,
    pub cash_balance: f64,
    pub total_exposure: f64,
    pub pnl: PnlBreakdown,
    pub total_trades: u32,
    pub win_rate: f64,
    pub total_volume: f64,
    pub positions_count: usize,
    pub markets_traded: usize,
}

pub struct Portfolio {
    pub initial_balance: f64,
    pub cash_balance: f64,
    positions: HashMap<String, HashMap<TokenType, PortfolioPosition>>,
    trades: Vec<Trade>,
    pub stats: PortfolioStats,
    current_prices: HashMap<String, HashMap<TokenType, f64>>,
}

impl Portfolio {
    pub fn new(initial_balance: f64) -> Self {
        Self {
            initial_balance,
            cash_balance: initial_balance,
            positions: HashMap::new(),
            trades: Vec::new(),
            stats: PortfolioStats::default(),
            current_prices: HashMap::new(),
        }
    }

    pub fn update_from_fill(&mut self, trade: &Trade) {
        let position = self
            .positions
            .entry(trade.market_id.clone())
            .or_default()
            .entry(trade.token_type)
            .or_insert_with(|| PortfolioPosition::new(trade.market_id.clone(), trade.token_type));

        match trade.side {
            OrderSide::Buy => Self::process_buy(position, trade, &mut self.stats),
            OrderSide::Sell => Self::process_sell(position, trade, &mut self.stats),
        }

        position.trade_count += 1;

        match trade.side {
            OrderSide::Buy => self.cash_balance -= trade.net_cost(),
            OrderSide::Sell => self.cash_balance += trade.notional() - trade.fee,
        }

        self.trades.push(trade.clone());
        self.stats.total_trades += 1;
        self.stats.total_fees_paid += trade.fee;
        self.stats.total_volume += trade.notional();
    }

    fn process_buy(position: &mut PortfolioPosition, trade: &Trade, stats: &mut PortfolioStats) {
        let new_size = position.size + trade.size;

        if position.size >= 0.0 {
            // Adding to long position.
            let total_cost = position.avg_entry_price * position.size + trade.price * trade.size;
            position.avg_entry_price = if new_size > 0.0 { total_cost / new_size } else { 0.0 };
            position.cost_basis += trade.net_cost();
        } else if trade.size <= position.size.abs() {
            // Partial cover of a short position.
            let realized = (position.avg_entry_price - trade.price) * trade.size;
            position.realized_pnl += realized;
            stats.total_realized_pnl += realized;
            if realized > 0.0 {
                stats.winning_trades += 1;
            } else {
                stats.losing_trades += 1;
            }
        } else {
            // Full cover of the short, then go long with the remainder.
            let short_size = position.size.abs();
            let realized = (position.avg_entry_price - trade.price) * short_size;
            position.realized_pnl += realized;
            stats.total_realized_pnl += realized;

            let long_size = trade.size - short_size;
            position.avg_entry_price = trade.price;
            position.cost_basis = long_size * trade.price;

            if realized > 0.0 {
                stats.winning_trades += 1;
            } else {
                stats.losing_trades += 1;
            }
        }

        position.size = new_size;
        position.total_bought += trade.size;
    }

    fn process_sell(position: &mut PortfolioPosition, trade: &Trade, stats: &mut PortfolioStats) {
        let new_size = position.size - trade.size;

        if position.size > 0.0 {
            if trade.size <= position.size {
                // Partial sell of a long position.
                let realized = (trade.price - position.avg_entry_price) * trade.size;
                position.realized_pnl += realized;
                stats.total_realized_pnl += realized;
                if realized > 0.0 {
                    stats.winning_trades += 1;
                } else {
                    stats.losing_trades += 1;
                }
            } else {
                // Full sell of the long, then go short with the remainder.
                let long_size = position.size;
                let realized = (trade.price - position.avg_entry_price) * long_size;
                position.realized_pnl += realized;
                stats.total_realized_pnl += realized;

                let short_size = trade.size - long_size;
                position.avg_entry_price = trade.price;
                position.cost_basis = short_size * trade.price;

                if realized > 0.0 {
                    stats.winning_trades += 1;
                } else {
                    stats.losing_trades += 1;
                }
            }
        } else {
            // Adding to a short position.
            let total_value = position.avg_entry_price * position.size.abs() + trade.price * trade.size;
            let new_short_size = new_size.abs();
            position.avg_entry_price = if new_short_size > 0.0 { total_value / new_short_size } else { 0.0 };
            position.cost_basis += trade.notional();
        }

        position.size = new_size;
        position.total_sold += trade.size;
    }

    pub fn update_prices(&mut self, market_id: &str, yes_price: f64, no_price: f64) {
        let entry = self.current_prices.entry(market_id.to_string()).or_default();
        entry.insert(TokenType::Yes, yes_price);
        entry.insert(TokenType::No, no_price);
        self.recalculate_unrealized_pnl();
    }

    fn recalculate_unrealized_pnl(&mut self) {
        let mut total = 0.0;
        for (market_id, tokens) in &self.positions {
            let Some(prices) = self.current_prices.get(market_id) else { continue };
            for (token_type, position) in tokens {
                if let Some(&current_price) = prices.get(token_type) {
                    total += position.unrealized_pnl(current_price);
                }
            }
        }
        self.stats.total_unrealized_pnl = total;
    }

    pub fn get_position(&self, market_id: &str, token_type: TokenType) -> Option<&PortfolioPosition> {
        self.positions.get(market_id)?.get(&token_type)
    }

    pub fn get_exposure(&self, market_id: &str) -> Exposure {
        let Some(tokens) = self.positions.get(market_id) else {
            return Exposure { yes_size: 0.0, no_size: 0.0, yes_notional: 0.0, no_notional: 0.0, total_notional: 0.0, net_position: 0.0 };
        };

        let yes_pos = tokens.get(&TokenType::Yes);
        let no_pos = tokens.get(&TokenType::No);

        let yes_size = yes_pos.map(|p| p.size).unwrap_or(0.0);
        let no_size = no_pos.map(|p| p.size).unwrap_or(0.0);
        let yes_notional = yes_pos.map(|p| p.notional()).unwrap_or(0.0);
        let no_notional = no_pos.map(|p| p.notional()).unwrap_or(0.0);

        Exposure {
            yes_size,
            no_size,
            yes_notional,
            no_notional,
            total_notional: yes_notional + no_notional,
            net_position: yes_size - no_size,
        }
    }

    pub fn get_total_exposure(&self) -> f64 {
        self.positions.values().flat_map(|tokens| tokens.values()).map(|p| p.notional()).sum()
    }

    pub fn get_pnl(&self) -> PnlBreakdown {
        PnlBreakdown {
            realized_pnl: self.stats.total_realized_pnl,
            unrealized_pnl: self.stats.total_unrealized_pnl,
            total_pnl: self.stats.total_pnl(),
            fees_paid: self.stats.total_fees_paid,
            net_pnl: self.stats.total_pnl() - self.stats.total_fees_paid,
        }
    }

    pub fn get_summary(&self) -> PortfolioSummary {
        PortfolioSummary {
            initial_balance: self.initial_balance,
            cash_balance: self.cash_balance,
            total_exposure: self.get_total_exposure(),
            pnl: self.get_pnl(),
            total_trades: self.stats.total_trades,
            win_rate: self.stats.win_rate(),
            total_volume: self.stats.total_volume,
            positions_count: self.positions.values().map(|t| t.len()).sum(),
            markets_traded: self.positions.len(),
        }
    }

    pub fn get_all_positions(&self) -> &HashMap<String, HashMap<TokenType, PortfolioPosition>> {
        &self.positions
    }

    pub fn get_recent_trades(&self, limit: usize) -> &[Trade] {
        let start = self.trades.len().saturating_sub(limit);
        &self.trades[start..]
    }

    pub fn reset(&mut self) {
        self.positions.clear();
        self.trades.clear();
        self.cash_balance = self.initial_balance;
        self.stats = PortfolioStats::default();
        self.current_prices.clear();
    }
}

/// Ported from `tests/test_portfolio.py`: same fixtures, same cases.
#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn portfolio() -> Portfolio {
        Portfolio::new(10000.0)
    }

    /// Mirrors the `create_trade` helper (defaults: test_market, YES, BUY, 0.50, 100.0, fee 0.0).
    #[allow(clippy::too_many_arguments)]
    fn create_trade(market_id: &str, token_type: TokenType, side: OrderSide, price: f64, size: f64, fee: f64, trade_id: &str, order_id: &str) -> Trade {
        Trade { trade_id: trade_id.into(), order_id: order_id.into(), market_id: market_id.into(), token_type, side, price, size, fee, timestamp: Utc::now() }
    }

    fn default_trade(side: OrderSide, price: f64, size: f64) -> Trade {
        create_trade("test_market", TokenType::Yes, side, price, size, 0.0, "trade_1", "order_1")
    }

    mod position_tracking {
        use super::*;

        #[test]
        fn initial_state() {
            let p = portfolio();
            assert_eq!(p.cash_balance, 10000.0);
            assert_eq!(p.get_total_exposure(), 0.0);
            assert_eq!(p.stats.total_trades, 0);
        }

        #[test]
        fn buy_creates_position() {
            let mut p = portfolio();
            p.update_from_fill(&default_trade(OrderSide::Buy, 0.50, 100.0));

            let position = p.get_position("test_market", TokenType::Yes).unwrap();
            assert_eq!(position.size, 100.0);
            assert_eq!(position.avg_entry_price, 0.50);
        }

        #[test]
        fn sell_reduces_position() {
            let mut p = portfolio();
            p.update_from_fill(&default_trade(OrderSide::Buy, 0.50, 100.0));
            p.update_from_fill(&create_trade("test_market", TokenType::Yes, OrderSide::Sell, 0.60, 50.0, 0.0, "trade_2", "order_2"));

            let position = p.get_position("test_market", TokenType::Yes).unwrap();
            assert_eq!(position.size, 50.0);
        }

        #[test]
        fn average_price_calculation() {
            let mut p = portfolio();
            p.update_from_fill(&default_trade(OrderSide::Buy, 0.50, 100.0));
            p.update_from_fill(&create_trade("test_market", TokenType::Yes, OrderSide::Buy, 0.60, 100.0, 0.0, "trade_2", "order_2"));

            let position = p.get_position("test_market", TokenType::Yes).unwrap();
            assert_eq!(position.size, 200.0);
            assert_eq!(position.avg_entry_price, 0.55); // (100*0.50 + 100*0.60) / 200
        }
    }

    mod pnl_calculation {
        use super::*;

        #[test]
        fn realized_pnl_on_profitable_trade() {
            let mut p = portfolio();
            p.update_from_fill(&default_trade(OrderSide::Buy, 0.50, 100.0));
            p.update_from_fill(&create_trade("test_market", TokenType::Yes, OrderSide::Sell, 0.60, 100.0, 0.0, "trade_2", "order_2"));

            assert!((p.stats.total_realized_pnl - 10.0).abs() < 0.01);
            assert_eq!(p.stats.winning_trades, 1);
        }

        #[test]
        fn realized_pnl_on_losing_trade() {
            let mut p = portfolio();
            p.update_from_fill(&default_trade(OrderSide::Buy, 0.60, 100.0));
            p.update_from_fill(&create_trade("test_market", TokenType::Yes, OrderSide::Sell, 0.50, 100.0, 0.0, "trade_2", "order_2"));

            assert!((p.stats.total_realized_pnl - (-10.0)).abs() < 0.01);
            assert_eq!(p.stats.losing_trades, 1);
        }

        #[test]
        fn unrealized_pnl() {
            let mut p = portfolio();
            p.update_from_fill(&default_trade(OrderSide::Buy, 0.50, 100.0));
            p.update_prices("test_market", 0.60, 0.40);

            let position = p.get_position("test_market", TokenType::Yes).unwrap();
            assert!((position.unrealized_pnl(0.60) - 10.0).abs() < 0.01); // 100 * (0.60 - 0.50)
        }

        #[test]
        fn fee_tracking() {
            let mut p = portfolio();
            p.update_from_fill(&create_trade("test_market", TokenType::Yes, OrderSide::Buy, 0.50, 100.0, 0.50, "trade_1", "order_1"));
            assert_eq!(p.stats.total_fees_paid, 0.50);
        }
    }

    mod exposure {
        use super::*;

        #[test]
        fn market_exposure() {
            let mut p = portfolio();
            p.update_from_fill(&create_trade("test_market", TokenType::Yes, OrderSide::Buy, 0.50, 100.0, 0.0, "trade_1", "order_1"));
            p.update_from_fill(&create_trade("test_market", TokenType::No, OrderSide::Buy, 0.40, 100.0, 0.0, "trade_2", "order_2"));

            let exposure = p.get_exposure("test_market");
            assert_eq!(exposure.yes_size, 100.0);
            assert_eq!(exposure.no_size, 100.0);
            assert_eq!(exposure.total_notional, 90.0); // 50 + 40
        }

        #[test]
        fn total_exposure() {
            let mut p = portfolio();
            p.update_from_fill(&create_trade("market_1", TokenType::Yes, OrderSide::Buy, 0.50, 100.0, 0.0, "trade_1", "order_1"));
            p.update_from_fill(&create_trade("market_2", TokenType::Yes, OrderSide::Buy, 0.40, 100.0, 0.0, "trade_2", "order_2"));

            assert_eq!(p.get_total_exposure(), 90.0); // 50 + 40
        }
    }

    mod win_rate {
        use super::*;

        #[test]
        fn win_rate_calculation() {
            let mut p = portfolio();
            p.update_from_fill(&create_trade("test_market", TokenType::Yes, OrderSide::Buy, 0.50, 100.0, 0.0, "t1", "o1"));
            p.update_from_fill(&create_trade("test_market", TokenType::Yes, OrderSide::Sell, 0.60, 100.0, 0.0, "t2", "o2"));
            p.update_from_fill(&create_trade("test_market", TokenType::Yes, OrderSide::Buy, 0.60, 100.0, 0.0, "t3", "o3"));
            p.update_from_fill(&create_trade("test_market", TokenType::Yes, OrderSide::Sell, 0.50, 100.0, 0.0, "t4", "o4"));

            assert_eq!(p.stats.winning_trades, 1);
            assert_eq!(p.stats.losing_trades, 1);
            assert_eq!(p.stats.win_rate(), 0.5);
        }
    }

    mod summary {
        use super::*;

        #[test]
        fn reset_clears_state() {
            let mut p = portfolio();
            p.update_from_fill(&default_trade(OrderSide::Buy, 0.50, 100.0));

            p.reset();

            assert_eq!(p.stats.total_trades, 0);
            assert_eq!(p.get_total_exposure(), 0.0);
            assert_eq!(p.cash_balance, 10000.0);
        }
    }

    #[test]
    fn flipping_long_to_short_splits_realized_and_new_basis() {
        let mut p = portfolio();
        p.update_from_fill(&default_trade(OrderSide::Buy, 0.40, 50.0));
        // Sell more than the long position -> realize on the long, then open a short at 0.60.
        p.update_from_fill(&create_trade("test_market", TokenType::Yes, OrderSide::Sell, 0.60, 80.0, 0.0, "trade_2", "order_2"));

        let pos = p.get_position("test_market", TokenType::Yes).unwrap();
        assert!((pos.size - (-30.0)).abs() < 1e-9);
        assert!((pos.avg_entry_price - 0.60).abs() < 1e-9);
    }
}
