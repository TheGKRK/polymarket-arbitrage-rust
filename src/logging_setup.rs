//! Logging setup (mirrors `utils/logging_utils.py`).
//!
//! Python routes to three files via three named loggers (root ->
//! `bot.log`, `"trades"` -> `trades.log`, `"opportunities"` ->
//! `opportunities.log`) plus a colored console handler. The `tracing`
//! equivalent of a dotted logger name is an event's `target`, so the
//! `trades`/`opportunities` file layers below filter on `target() ==
//! "trades"` / `"opportunities"` - call sites use `target: "trades"` (see
//! `TradeLogger` below) exactly where Python called
//! `logging.getLogger("trades")`.
//!
//! Python's custom `TRADE`/`OPPORTUNITY` numeric levels (between INFO and
//! WARNING) don't carry any filtering behavior beyond routing to their
//! dedicated file, which the `target`-based layers already reproduce, so
//! they're not ported as distinct levels - these events are emitted at
//! `info!` with the matching `target`.
//!
//! Python's `RotatingFileHandler` rotates by file size; `tracing-appender`
//! only supports time-based rotation. This uses daily rotation instead, an
//! intentional, disclosed deviation (see migration plan).

use crate::config::LoggingConfig;
use tracing::level_filters::LevelFilter;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::Rotation;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{filter, fmt, Layer};

/// Must be kept alive for the process lifetime - dropping a guard stops its
/// writer from flushing.
pub struct LoggingGuards(#[allow(dead_code)] Vec<WorkerGuard>);

pub fn setup_logging(config: &LoggingConfig) -> LoggingGuards {
    std::fs::create_dir_all(&config.log_dir).expect("failed to create log directory");

    let console_level = parse_level(&config.console_level);
    let file_level = parse_level(&config.file_level);

    let mut guards = Vec::new();

    let console_layer = fmt::layer().with_target(true).with_filter(LevelFilter::from_level(console_level));

    let (main_writer, main_guard) = tracing_appender::non_blocking(tracing_appender::rolling::RollingFileAppender::new(Rotation::DAILY, &config.log_dir, &config.main_log_file));
    guards.push(main_guard);
    let main_layer = fmt::layer().with_writer(main_writer).with_ansi(false).with_filter(LevelFilter::from_level(file_level));

    let (trades_writer, trades_guard) = tracing_appender::non_blocking(tracing_appender::rolling::RollingFileAppender::new(Rotation::DAILY, &config.log_dir, &config.trades_log_file));
    guards.push(trades_guard);
    let trades_layer = fmt::layer().with_writer(trades_writer).with_ansi(false).with_filter(filter::filter_fn(|meta| meta.target() == "trades"));

    let (opps_writer, opps_guard) = tracing_appender::non_blocking(tracing_appender::rolling::RollingFileAppender::new(Rotation::DAILY, &config.log_dir, &config.opportunities_log_file));
    guards.push(opps_guard);
    let opportunities_layer = fmt::layer().with_writer(opps_writer).with_ansi(false).with_filter(filter::filter_fn(|meta| meta.target() == "opportunities"));

    // Permissive by default (matches Python's DEBUG-level root logger),
    // muting only known-noisy HTTP/WS dependencies - matching Python's
    // explicit `logging.getLogger("httpx").setLevel(WARNING)` calls. An
    // earlier version of this allow-listed just the `polymarket_arb_bot`
    // library target, which silently muted every `info!` call made
    // directly in the `bot`/`bot_dashboard` binary crates (a different
    // target than the library) - default-permissive avoids that trap.
    // The floor here must stay at or below the most permissive per-layer
    // filter (file_level defaults to DEBUG, console goes to DEBUG under
    // `--verbose`) - this top-level filter gates every layer before any
    // per-layer filter even sees the event.
    let noise_filter = tracing_subscriber::EnvFilter::new("debug,reqwest=warn,hyper=warn,tokio_tungstenite=warn");

    tracing_subscriber::registry()
        .with(noise_filter)
        .with(console_layer)
        .with(main_layer)
        .with(trades_layer)
        .with(opportunities_layer)
        .init();

    tracing::info!(console = %config.console_level, file = %config.file_level, dir = %config.log_dir, "Logging initialized");

    LoggingGuards(guards)
}

fn parse_level(level: &str) -> tracing::Level {
    match level.to_uppercase().as_str() {
        "DEBUG" => tracing::Level::DEBUG,
        "WARNING" | "WARN" => tracing::Level::WARN,
        "ERROR" => tracing::Level::ERROR,
        "CRITICAL" => tracing::Level::ERROR,
        _ => tracing::Level::INFO,
    }
}

/// Mirrors `TradeLogger` - logs to the `trades` target/file.
pub struct TradeLogger;

impl TradeLogger {
    pub fn log_order_placed(&self, order_id: &str, market_id: &str, side: &str, token: &str, price: f64, size: f64, strategy: &str) {
        tracing::info!(target: "trades", "ORDER_PLACED | id={order_id} | market={market_id} | {side} {size:.4} {token} @ {price:.4} | strategy={strategy}");
    }

    pub fn log_order_filled(&self, trade_id: &str, order_id: &str, market_id: &str, side: &str, token: &str, price: f64, size: f64, fee: f64) {
        tracing::info!(target: "trades", "ORDER_FILLED | trade={trade_id} | order={order_id} | market={market_id} | {side} {size:.4} {token} @ {price:.4} | fee={fee:.4}");
    }

    pub fn log_order_cancelled(&self, order_id: &str, reason: &str) {
        tracing::info!(target: "trades", "ORDER_CANCELLED | id={order_id} | reason={reason}");
    }
}

/// Mirrors `OpportunityLogger` - logs to the `opportunities` target/file.
pub struct OpportunityLogger;

impl OpportunityLogger {
    pub fn log_bundle_opportunity(&self, opportunity_id: &str, market_id: &str, opportunity_type: &str, edge: f64, total_price: f64, suggested_size: f64) {
        tracing::info!(target: "opportunities", "BUNDLE_ARB | id={opportunity_id} | market={market_id} | type={opportunity_type} | edge={edge:.4} | total={total_price:.4} | size={suggested_size:.2}");
    }

    pub fn log_mm_opportunity(&self, opportunity_id: &str, market_id: &str, token: &str, spread: f64, bid: f64, ask: f64, suggested_size: f64) {
        tracing::info!(target: "opportunities", "MM_SPREAD | id={opportunity_id} | market={market_id} | token={token} | spread={spread:.4} | bid={bid:.4} | ask={ask:.4} | size={suggested_size:.2}");
    }
}

/// Mirrors `PerformanceLogger`.
pub struct PerformanceLogger;

impl PerformanceLogger {
    pub fn log_snapshot(&self, pnl: &crate::core::portfolio::PnlBreakdown, exposure: f64, positions: usize, open_orders: usize) {
        tracing::info!(
            "SNAPSHOT | realized={:.2} | unrealized={:.2} | total={:.2} | exposure={:.2} | positions={} | orders={}",
            pnl.realized_pnl,
            pnl.unrealized_pnl,
            pnl.total_pnl,
            exposure,
            positions,
            open_orders
        );
    }

    pub fn log_latency(&self, operation: &str, latency_ms: f64) {
        tracing::debug!("LATENCY | {operation} | {latency_ms:.2}ms");
    }
}

pub const TRADE_LOGGER: TradeLogger = TradeLogger;
pub const OPPORTUNITY_LOGGER: OpportunityLogger = OpportunityLogger;
pub const PERFORMANCE_LOGGER: PerformanceLogger = PerformanceLogger;
