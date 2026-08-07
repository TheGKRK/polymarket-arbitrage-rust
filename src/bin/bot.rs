//! Polymarket Arbitrage Trading Bot - main entry point (mirrors `main.py`).
//!
//! Usage:
//!   bot                        Run in dry-run mode (default)
//!   bot --live                 Run in live mode
//!   bot --backtest             Run backtest
//!   bot --config my.yaml       Use a custom config file

use clap::Parser;
use polymarket_arb_bot::backtest::{self, BacktestConfig};
use polymarket_arb_bot::config::{self, BotConfig};
use polymarket_arb_bot::core::arb_engine::{ArbConfig, ArbEngine};
use polymarket_arb_bot::core::data_feed::DataFeed;
use polymarket_arb_bot::core::execution::{ExecutionConfig, ExecutionEngine};
use polymarket_arb_bot::core::portfolio::Portfolio;
use polymarket_arb_bot::core::risk_manager::{RiskConfig, RiskManager};
use polymarket_arb_bot::logging_setup::{self, PERFORMANCE_LOGGER};
use polymarket_arb_bot::models::MarketState;
use polymarket_arb_bot::polymarket_client::{PolymarketClient, PolymarketClientOptions};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tracing::{error, info};

#[derive(Parser)]
#[command(name = "bot", about = "Polymarket Arbitrage Trading Bot")]
struct Args {
    /// Path to configuration file. Defaults to the repo's shared
    /// `config.yaml` (one level above this crate), resolved at compile
    /// time from `CARGO_MANIFEST_DIR` so it's independent of whatever
    /// directory you happen to run this from.
    #[arg(short = 'c', long, default_value = concat!(env!("CARGO_MANIFEST_DIR"), "/../config.yaml"))]
    config: String,

    /// Run in live trading mode
    #[arg(long)]
    live: bool,

    /// Run in dry-run mode (default)
    #[arg(long = "dry-run")]
    dry_run: bool,

    /// Run backtest simulation
    #[arg(long)]
    backtest: bool,

    /// Backtest duration in simulated seconds
    #[arg(long = "backtest-duration", default_value_t = 300.0)]
    backtest_duration: f64,

    /// Enable verbose logging
    #[arg(short = 'v', long)]
    verbose: bool,
}

struct TradingBot {
    config: BotConfig,
    running: Arc<AtomicBool>,
    client: Arc<PolymarketClient>,
    data_feed: Arc<DataFeed>,
    arb_engine: Arc<Mutex<ArbEngine>>,
    execution_engine: Arc<ExecutionEngine>,
    risk_manager: Arc<Mutex<RiskManager>>,
    portfolio: Arc<Mutex<Portfolio>>,
}

impl TradingBot {
    async fn start(config: BotConfig) -> Arc<Self> {
        info!("{}", "=".repeat(60));
        info!("Polymarket Arbitrage Bot Starting");
        info!("{}", "=".repeat(60));
        info!(mode = if config.is_dry_run() { "DRY RUN" } else { "LIVE" });
        info!(markets = ?config.trading.markets, "Markets (empty = auto-discover)");

        let client = Arc::new(PolymarketClient::new(PolymarketClientOptions {
            rest_url: config.api.polymarket_rest_url.clone(),
            ws_url: config.api.polymarket_ws_url.clone(),
            gamma_url: config.api.gamma_api_url.clone(),
            api_key: non_empty(&config.api.api_key),
            api_secret: non_empty(&config.api.api_secret),
            passphrase: non_empty(&config.api.passphrase),
            private_key: non_empty(&config.api.private_key),
            timeout: config.api.timeout_seconds,
            max_retries: config.api.max_retries,
            retry_delay: config.api.retry_delay_seconds,
            dry_run: config.is_dry_run(),
        }));
        client.connect().await;

        let initial_balance = if config.is_dry_run() { config.mode.dry_run_initial_balance } else { 0.0 };
        let portfolio = Arc::new(Mutex::new(Portfolio::new(initial_balance)));

        let risk_manager = Arc::new(Mutex::new(RiskManager::new(RiskConfig {
            max_position_per_market: config.risk.max_position_per_market,
            max_global_exposure: config.risk.max_global_exposure,
            max_daily_loss: config.risk.max_daily_loss,
            max_drawdown_pct: config.risk.max_drawdown_pct,
            trade_only_high_volume: config.risk.trade_only_high_volume,
            min_24h_volume: config.risk.min_24h_volume,
            whitelist: config.risk.whitelist.clone(),
            blacklist: config.risk.blacklist.clone(),
            kill_switch_enabled: config.risk.kill_switch_enabled,
            auto_unwind_on_breach: config.risk.auto_unwind_on_breach,
        })));

        let execution_engine = ExecutionEngine::new(
            Arc::clone(&client),
            Arc::clone(&risk_manager),
            Arc::clone(&portfolio),
            ExecutionConfig { slippage_tolerance: config.trading.slippage_tolerance, order_timeout_seconds: config.trading.order_timeout_seconds, dry_run: config.is_dry_run(), ..Default::default() },
        );
        execution_engine.start().await;

        let arb_engine = Arc::new(Mutex::new(ArbEngine::new(ArbConfig {
            min_edge: config.trading.min_edge,
            bundle_arb_enabled: config.trading.bundle_arb_enabled,
            min_spread: config.trading.min_spread,
            mm_enabled: config.trading.mm_enabled,
            tick_size: config.trading.tick_size,
            default_order_size: config.trading.default_order_size,
            min_order_size: config.trading.min_order_size,
            max_order_size: config.trading.max_order_size,
            ..Default::default()
        })));

        let running = Arc::new(AtomicBool::new(true));

        let on_update_arb = Arc::clone(&arb_engine);
        let on_update_risk = Arc::clone(&risk_manager);
        let on_update_exec = Arc::clone(&execution_engine);
        let on_update: polymarket_arb_bot::core::data_feed::OnUpdate = Arc::new(move |_market_id: &str, state: &MarketState| {
            if !on_update_risk.lock().unwrap().within_global_limits() {
                return;
            }
            let signals = on_update_arb.lock().unwrap().analyze(state);
            for signal in signals {
                let exec = Arc::clone(&on_update_exec);
                tokio::spawn(async move { exec.submit_signal(signal).await });
            }
        });

        let data_feed = DataFeed::new(Arc::clone(&client), config.trading.markets.clone(), 5.0, Some(on_update), config.use_simulation());
        data_feed.start().await;

        info!("Waiting for market data...");
        if !data_feed.wait_for_data(30.0).await {
            tracing::warn!("Timeout waiting for initial data, proceeding anyway");
        }

        info!("Bot started successfully!");
        info!("{}", "-".repeat(60));

        let bot = Arc::new(Self { config, running, client, data_feed, arb_engine, execution_engine, risk_manager, portfolio });

        let monitoring = Arc::clone(&bot);
        tokio::spawn(async move { monitoring.monitoring_loop().await });

        if bot.config.is_dry_run() && bot.config.mode.simulate_fills {
            let sim = Arc::clone(&bot);
            tokio::spawn(async move { sim.simulate_fills().await });
        }

        bot
    }

    async fn monitoring_loop(self: Arc<Self>) {
        let interval = self.config.monitoring.snapshot_interval;

        while self.running.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_secs_f64(interval)).await;

            let (pnl, exposure, positions, open_orders) = {
                let p = self.portfolio.lock().unwrap();
                (p.get_pnl(), p.get_total_exposure(), p.get_all_positions().len(), 0usize)
            };
            let open_orders = self.execution_engine.open_order_count().await.max(open_orders);

            PERFORMANCE_LOGGER.log_snapshot(&pnl, exposure, positions, open_orders);

            self.risk_manager.lock().unwrap().update_pnl(pnl.realized_pnl, pnl.unrealized_pnl);

            let exec_stats = self.execution_engine.get_stats().await;
            info!(
                orders_placed = exec_stats.orders_placed,
                orders_filled = exec_stats.orders_filled,
                total_pnl = pnl.total_pnl,
                "Stats"
            );

            if self.risk_manager.lock().unwrap().get_summary().kill_switch_triggered {
                error!("KILL SWITCH ACTIVE - Trading halted");
            }
        }
    }

    async fn simulate_fills(self: Arc<Self>) {
        use rand::Rng;
        while self.running.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;

            let orders = self.execution_engine.get_open_orders(None).await;
            for order in orders {
                let fill_probability = self.config.mode.fill_probability;
                if rand::thread_rng().gen_bool(fill_probability.clamp(0.0, 1.0)) {
                    if let Some(trade) = self.client.simulate_fill(&order.order_id, None).await {
                        self.execution_engine.handle_fill(&trade).await;
                    }
                }
            }
        }
    }

    async fn stop(&self) {
        info!("Shutting down...");
        self.running.store(false, Ordering::SeqCst);

        self.data_feed.stop().await;
        self.execution_engine.stop().await;
        self.client.disconnect().await;

        let summary = self.portfolio.lock().unwrap().get_summary();
        info!("{}", "=".repeat(60));
        info!("Final Portfolio Summary");
        info!("{}", "=".repeat(60));
        info!(total_pnl = summary.pnl.total_pnl, realized = summary.pnl.realized_pnl, unrealized = summary.pnl.unrealized_pnl);
        info!(total_trades = summary.total_trades, win_rate = summary.win_rate, total_volume = summary.total_volume);

        let stats = self.arb_engine.lock().unwrap().get_stats().clone();
        info!("{}", "-".repeat(60));
        info!(bundle_opportunities = stats.bundle_opportunities_detected, mm_opportunities = stats.mm_opportunities_detected, signals_generated = stats.signals_generated);

        info!("{}", "=".repeat(60));
        info!("Bot stopped");
    }
}

fn non_empty(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

async fn run_backtest_mode(config: BotConfig, duration: f64) {
    info!("Starting backtest mode...");

    let portfolio = Arc::new(Mutex::new(Portfolio::new(config.mode.dry_run_initial_balance)));
    let risk_manager = Arc::new(Mutex::new(RiskManager::new(RiskConfig {
        max_position_per_market: config.risk.max_position_per_market,
        max_global_exposure: config.risk.max_global_exposure,
        max_daily_loss: config.risk.max_daily_loss,
        max_drawdown_pct: config.risk.max_drawdown_pct,
        ..Default::default()
    })));
    let arb_engine = Mutex::new(ArbEngine::new(ArbConfig {
        min_edge: config.trading.min_edge,
        bundle_arb_enabled: config.trading.bundle_arb_enabled,
        min_spread: config.trading.min_spread,
        mm_enabled: config.trading.mm_enabled,
        tick_size: config.trading.tick_size,
        default_order_size: config.trading.default_order_size,
        ..Default::default()
    }));

    let client = Arc::new(PolymarketClient::new(PolymarketClientOptions { dry_run: true, ..Default::default() }));
    client.connect().await;

    let execution_engine = ExecutionEngine::new(Arc::clone(&client), Arc::clone(&risk_manager), Arc::clone(&portfolio), ExecutionConfig { dry_run: true, ..Default::default() });
    execution_engine.start().await;

    let backtest_config = BacktestConfig { initial_balance: config.mode.dry_run_initial_balance, simulate_fills: true, fill_probability: config.mode.fill_probability, ..Default::default() };
    let market_ids = if config.trading.markets.is_empty() { (0..3).map(|i| format!("market_{i}")).collect() } else { config.trading.markets.clone() };

    backtest::run_backtest(backtest_config, market_ids, &arb_engine, &execution_engine, &portfolio, duration).await;

    execution_engine.stop().await;
    client.disconnect().await;
}

async fn main_async(args: Args) {
    let mut config = match config::load_config(&args.config) {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, "Failed to load config");
            std::process::exit(1);
        }
    };

    if args.live {
        config.mode.trading_mode = "live".to_string();
    } else if args.dry_run {
        config.mode.trading_mode = "dry_run".to_string();
    }

    if args.backtest {
        run_backtest_mode(config, args.backtest_duration).await;
        return;
    }

    let bot = TradingBot::start(config).await;

    let shutdown = Arc::new(tokio::sync::Notify::new());
    let shutdown_signal = Arc::clone(&shutdown);
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        info!("Received shutdown signal");
        shutdown_signal.notify_one();
    });

    shutdown.notified().await;
    bot.stop().await;
}

fn main() {
    let args = Args::parse();

    let mut logging_config = polymarket_arb_bot::config::get_default_config().logging;
    if args.verbose {
        logging_config.console_level = "DEBUG".to_string();
    }
    let _guards = logging_setup::setup_logging(&logging_config);

    let runtime = tokio::runtime::Runtime::new().expect("failed to build tokio runtime");
    runtime.block_on(main_async(args));
}
