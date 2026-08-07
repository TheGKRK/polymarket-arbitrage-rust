//! Detects arbitrage opportunities between Polymarket and Kalshi for
//! matched market pairs (mirrors the `CrossPlatformArbEngine` class in
//! `core/cross_platform_arb.py`).

use crate::core::market_matcher::{MarketMatcher, MarketPair};
use crate::models::OrderBook;
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::fmt;
use tracing::info;

#[derive(Debug, Clone)]
pub struct CrossPlatformOpportunity {
    pub opportunity_id: String,
    pub market_pair: MarketPair,
    pub buy_platform: String,
    pub sell_platform: String,
    pub token: String,
    pub buy_price: f64,
    pub sell_price: f64,
    pub gross_edge: f64,
    pub net_edge: f64,
    pub edge_pct: f64,
    pub suggested_size: f64,
    pub max_size: f64,
    pub buy_liquidity: f64,
    pub sell_liquidity: f64,
    pub detected_at: DateTime<Utc>,
}

impl fmt::Display for CrossPlatformOpportunity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "CrossPlatformArb: Buy {} on {} @ ${:.3}, Sell on {} @ ${:.3} | Net Edge: {:.2}%",
            self.token,
            self.buy_platform,
            self.buy_price,
            self.sell_platform,
            self.sell_price,
            self.edge_pct * 100.0
        )
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CrossPlatformStats {
    pub total_opportunities: usize,
    pub matched_pairs: usize,
    pub avg_edge: f64,
}

pub struct CrossPlatformArbEngine {
    pub min_edge: f64,
    pub polymarket_taker_fee: f64,
    pub kalshi_taker_fee: f64,
    pub gas_cost: f64,
    pub matcher: MarketMatcher,
    opportunities: Vec<CrossPlatformOpportunity>,
    opportunity_count: u64,
}

struct Direction<'a> {
    buy_platform: &'a str,
    sell_platform: &'a str,
    token: &'a str,
    buy_price: f64,
    sell_price: f64,
    buy_liquidity: f64,
    sell_liquidity: f64,
    buy_fee_rate: f64,
    sell_fee_rate: f64,
}

impl CrossPlatformArbEngine {
    pub fn new(min_edge: f64, polymarket_taker_fee: f64, kalshi_taker_fee: f64, gas_cost: f64) -> Self {
        Self {
            min_edge,
            polymarket_taker_fee,
            kalshi_taker_fee,
            gas_cost,
            matcher: MarketMatcher::new(0.5),
            opportunities: Vec::new(),
            opportunity_count: 0,
        }
    }

    /// Check for an arbitrage opportunity between a matched market pair,
    /// evaluating all four buy/sell/token directions and keeping the best.
    pub fn check_arbitrage(&mut self, market_pair: &MarketPair, polymarket_ob: &OrderBook, kalshi_ob: &OrderBook) -> Option<CrossPlatformOpportunity> {
        let poly_yes_ask = polymarket_ob.best_ask_yes();
        let poly_yes_bid = polymarket_ob.best_bid_yes();
        let poly_no_ask = polymarket_ob.best_ask_no();
        let poly_no_bid = polymarket_ob.best_bid_no();

        let kalshi_yes_ask = kalshi_ob.best_ask_yes();
        let kalshi_yes_bid = kalshi_ob.best_bid_yes();
        let kalshi_no_ask = kalshi_ob.best_ask_no();
        let kalshi_no_bid = kalshi_ob.best_bid_no();

        if poly_yes_ask.is_none() || poly_yes_bid.is_none() || kalshi_yes_ask.is_none() || kalshi_yes_bid.is_none() {
            return None;
        }

        let mut directions = Vec::new();

        if let (Some(buy), Some(sell)) = (poly_yes_ask, kalshi_yes_bid) {
            directions.push(Direction {
                buy_platform: "polymarket",
                sell_platform: "kalshi",
                token: "YES",
                buy_price: buy,
                sell_price: sell,
                buy_liquidity: polymarket_ob.yes.asks.best_size().unwrap_or(0.0),
                sell_liquidity: kalshi_ob.yes.bids.best_size().unwrap_or(0.0),
                buy_fee_rate: self.polymarket_taker_fee,
                sell_fee_rate: self.kalshi_taker_fee,
            });
        }
        if let (Some(buy), Some(sell)) = (kalshi_yes_ask, poly_yes_bid) {
            directions.push(Direction {
                buy_platform: "kalshi",
                sell_platform: "polymarket",
                token: "YES",
                buy_price: buy,
                sell_price: sell,
                buy_liquidity: kalshi_ob.yes.asks.best_size().unwrap_or(0.0),
                sell_liquidity: polymarket_ob.yes.bids.best_size().unwrap_or(0.0),
                buy_fee_rate: self.kalshi_taker_fee,
                sell_fee_rate: self.polymarket_taker_fee,
            });
        }
        if let (Some(buy), Some(sell)) = (poly_no_ask, kalshi_no_bid) {
            directions.push(Direction {
                buy_platform: "polymarket",
                sell_platform: "kalshi",
                token: "NO",
                buy_price: buy,
                sell_price: sell,
                buy_liquidity: polymarket_ob.no.asks.best_size().unwrap_or(0.0),
                sell_liquidity: kalshi_ob.no.bids.best_size().unwrap_or(0.0),
                buy_fee_rate: self.polymarket_taker_fee,
                sell_fee_rate: self.kalshi_taker_fee,
            });
        }
        if let (Some(buy), Some(sell)) = (kalshi_no_ask, poly_no_bid) {
            directions.push(Direction {
                buy_platform: "kalshi",
                sell_platform: "polymarket",
                token: "NO",
                buy_price: buy,
                sell_price: sell,
                buy_liquidity: kalshi_ob.no.asks.best_size().unwrap_or(0.0),
                sell_liquidity: polymarket_ob.no.bids.best_size().unwrap_or(0.0),
                buy_fee_rate: self.kalshi_taker_fee,
                sell_fee_rate: self.polymarket_taker_fee,
            });
        }

        let mut best_opp: Option<CrossPlatformOpportunity> = None;
        let mut best_net_edge = 0.0;

        for d in directions {
            let gross = d.sell_price - d.buy_price;
            let fees = d.buy_price * d.buy_fee_rate + d.sell_price * d.sell_fee_rate + self.gas_cost * 2.0;
            let net = gross - fees;

            if net > best_net_edge && net >= self.min_edge {
                best_net_edge = net;
                best_opp = Some(self.create_opportunity(market_pair, d.buy_platform, d.sell_platform, d.token, d.buy_price, d.sell_price, gross, net, d.buy_liquidity, d.sell_liquidity));
            }
        }

        if let Some(opp) = &best_opp {
            info!(%opp, "CROSS-PLATFORM ARB");
            self.opportunities.push(opp.clone());
        }

        best_opp
    }

    #[allow(clippy::too_many_arguments)]
    fn create_opportunity(
        &mut self,
        market_pair: &MarketPair,
        buy_platform: &str,
        sell_platform: &str,
        token: &str,
        buy_price: f64,
        sell_price: f64,
        gross_edge: f64,
        net_edge: f64,
        buy_liquidity: f64,
        sell_liquidity: f64,
    ) -> CrossPlatformOpportunity {
        self.opportunity_count += 1;

        let max_size = buy_liquidity.min(sell_liquidity);
        let suggested_size = max_size.min(100.0);

        CrossPlatformOpportunity {
            opportunity_id: format!("xplat_{}", self.opportunity_count),
            market_pair: market_pair.clone(),
            buy_platform: buy_platform.to_string(),
            sell_platform: sell_platform.to_string(),
            token: token.to_string(),
            buy_price,
            sell_price,
            gross_edge,
            net_edge,
            edge_pct: if buy_price > 0.0 { net_edge / buy_price } else { 0.0 },
            suggested_size,
            max_size,
            buy_liquidity,
            sell_liquidity,
            detected_at: Utc::now(),
        }
    }

    pub fn get_recent_opportunities(&self, limit: usize) -> &[CrossPlatformOpportunity] {
        let start = self.opportunities.len().saturating_sub(limit);
        &self.opportunities[start..]
    }

    pub fn get_stats(&self) -> CrossPlatformStats {
        let avg_edge = if self.opportunities.is_empty() { 0.0 } else { self.opportunities.iter().map(|o| o.net_edge).sum::<f64>() / self.opportunities.len() as f64 };
        CrossPlatformStats { total_opportunities: self.opportunities.len(), matched_pairs: self.matcher.get_cached_pairs().len(), avg_edge }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{OrderBookSide, PriceLevel};

    fn pair() -> MarketPair {
        MarketPair {
            polymarket_id: "p1".into(),
            kalshi_ticker: "K1".into(),
            polymarket_question: "Q".into(),
            kalshi_title: "T".into(),
            similarity_score: 0.9,
            category: "politics".into(),
            matched_at: Utc::now(),
        }
    }

    fn book(bid_yes: f64, ask_yes: f64) -> OrderBook {
        let mut ob = OrderBook::new("m");
        ob.yes.bids = OrderBookSide::new(vec![PriceLevel::new(bid_yes, 500.0)]);
        ob.yes.asks = OrderBookSide::new(vec![PriceLevel::new(ask_yes, 500.0)]);
        ob.no.bids = OrderBookSide::new(vec![PriceLevel::new(1.0 - ask_yes, 500.0)]);
        ob.no.asks = OrderBookSide::new(vec![PriceLevel::new(1.0 - bid_yes, 500.0)]);
        ob
    }

    #[test]
    fn detects_cross_platform_edge_when_prices_diverge() {
        let mut engine = CrossPlatformArbEngine::new(0.02, 0.015, 0.01, 0.02);
        let poly = book(0.40, 0.42); // Cheap on Polymarket.
        let kalshi = book(0.55, 0.57); // Expensive on Kalshi.

        let opp = engine.check_arbitrage(&pair(), &poly, &kalshi);
        assert!(opp.is_some());
        let opp = opp.unwrap();
        assert_eq!(opp.buy_platform, "polymarket");
        assert_eq!(opp.sell_platform, "kalshi");
    }

    #[test]
    fn no_opportunity_when_prices_aligned() {
        let mut engine = CrossPlatformArbEngine::new(0.02, 0.015, 0.01, 0.02);
        let poly = book(0.49, 0.50);
        let kalshi = book(0.49, 0.50);
        assert!(engine.check_arbitrage(&pair(), &poly, &kalshi).is_none());
    }
}
