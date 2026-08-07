//! Core trading engine (mirrors the Python `core/` package): data feed,
//! arbitrage detection, order execution, risk limits, and portfolio tracking.

pub mod arb_engine;
pub mod cross_platform_arb;
pub mod data_feed;
pub mod execution;
pub mod market_matcher;
pub mod portfolio;
pub mod risk_manager;
pub mod sequence_ratio;
