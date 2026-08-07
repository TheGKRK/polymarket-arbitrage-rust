//! Trading bot with integrated web dashboard (mirrors `run_with_dashboard.py`).
//! Supports cross-platform arbitrage between Polymarket and Kalshi.
//!
//! Usage:
//!   bot-dashboard              Dry run mode
//!   bot-dashboard --live       Live mode
//!   bot-dashboard --port 8080  Custom port

use clap::Parser;
use polymarket_arb_bot::config::{self, BotConfig};
use polymarket_arb_bot::core::arb_engine::{ArbConfig, ArbEngine};
use polymarket_arb_bot::core::cross_platform_arb::CrossPlatformArbEngine;
use polymarket_arb_bot::core::data_feed::DataFeed;
use polymarket_arb_bot::core::execution::{ExecutionConfig, ExecutionEngine};
use polymarket_arb_bot::core::portfolio::Portfolio;
use polymarket_arb_bot::core::risk_manager::{RiskConfig, RiskManager};
use polymarket_arb_bot::dashboard::integration::DashboardIntegration;
use polymarket_arb_bot::dashboard::server::build_router;
use polymarket_arb_bot::dashboard::state::Dashboard;
use polymarket_arb_bot::kalshi_client::KalshiClient;
use polymarket_arb_bot::logging_setup;
use polymarket_arb_bot::models::{Market, MarketState};
use polymarket_arb_bot::polymarket_client::{PolymarketClient, PolymarketClientOptions};
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::Mutex as AsyncMutex;
use tracing::{error, info};

#[derive(Parser)]
#[command(name = "bot-dashboard", about = "Polymarket Arbitrage Bot with Live Dashboard")]
struct Args {
    /// Path to configuration file. Defaults to the repo's shared
    /// `config.yaml` (one level above this crate), resolved at compile
    /// time from `CARGO_MANIFEST_DIR` so it's independent of whatever
    /// directory you happen to run this from.
    #[arg(short = 'c', long, default_value = concat!(env!("CARGO_MANIFEST_DIR"), "/../config.yaml"))]
    config: String,

    #[arg(long, default_value_t = 8888)]
    port: u16,

    #[arg(long)]
    live: bool,

    #[arg(long = "dry-run")]
    dry_run: bool,

    #[arg(short = 'v', long)]
    verbose: bool,
}

// `arb_engine`/`risk_manager` are otherwise only reached through clones
// held by the on_update closure and the dashboard integration; kept here
// too so this struct owns an `Arc` for the bot's whole lifetime, matching
// Python's `self.arb_engine = ...` instance attributes.
#[allow(dead_code)]
struct TradingBotWithDashboard {
    config: BotConfig,
    port: u16,
    running: Arc<AtomicBool>,
    client: Arc<PolymarketClient>,
    kalshi_client: Option<Arc<KalshiClient>>,
    cross_platform_engine: Option<Arc<AsyncMutex<CrossPlatformArbEngine>>>,
    data_feed: Arc<DataFeed>,
    arb_engine: Arc<Mutex<ArbEngine>>,
    execution_engine: Arc<ExecutionEngine>,
    risk_manager: Arc<Mutex<RiskManager>>,
    portfolio: Arc<Mutex<Portfolio>>,
    dashboard: Arc<Dashboard>,
    dashboard_integration: Arc<DashboardIntegration>,
}

impl TradingBotWithDashboard {
    async fn start(config: BotConfig, port: u16) -> Arc<Self> {
        info!("{}", "=".repeat(60));
        info!("Polymarket + Kalshi Arbitrage Bot");
        info!("{}", "=".repeat(60));
        info!(mode = if config.is_dry_run() { "DRY RUN" } else { "LIVE" });
        info!(cross_platform = config.mode.cross_platform_enabled);
        info!(dashboard_url = format!("http://localhost:{port}"));
        info!("{}", "=".repeat(60));

        let running = Arc::new(AtomicBool::new(true));

        let client = Arc::new(PolymarketClient::new(PolymarketClientOptions {
            rest_url: config.api.polymarket_rest_url.clone(),
            ws_url: config.api.polymarket_ws_url.clone(),
            gamma_url: config.api.gamma_api_url.clone(),
            api_key: non_empty(&config.api.api_key),
            private_key: non_empty(&config.api.private_key),
            timeout: config.api.timeout_seconds,
            dry_run: config.is_dry_run(),
            ..Default::default()
        }));
        client.connect().await;

        let dashboard = Dashboard::new();

        let mut kalshi_client = None;
        let mut cross_platform_engine = None;

        if config.mode.cross_platform_enabled && config.mode.kalshi_enabled {
            info!("Initializing Kalshi client for cross-platform arbitrage...");
            let kc = Arc::new(KalshiClient::new(config.api.timeout_seconds, config.api.max_retries, config.is_dry_run()));
            kc.connect().await;
            kalshi_client = Some(Arc::clone(&kc));

            let cp_engine = Arc::new(AsyncMutex::new(CrossPlatformArbEngine::new(config.trading.min_edge, 0.015, 0.01, 0.02)));
            cross_platform_engine = Some(Arc::clone(&cp_engine));

            dashboard.with_state(|s| s.cross_platform.enabled = true).await;
        }

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
            ..Default::default()
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

        let dashboard_for_cb = Arc::clone(&dashboard);
        let on_update_arb = Arc::clone(&arb_engine);
        let on_update_risk = Arc::clone(&risk_manager);
        let on_update_exec = Arc::clone(&execution_engine);
        let running_cb = Arc::clone(&running);
        let on_update: polymarket_arb_bot::core::data_feed::OnUpdate = Arc::new(move |_market_id: &str, state: &MarketState| {
            if !running_cb.load(Ordering::SeqCst) {
                return;
            }
            if !on_update_risk.lock().unwrap().within_global_limits() {
                return;
            }

            let signals = on_update_arb.lock().unwrap().analyze(state);
            for signal in signals {
                if let Some(opp) = &signal.opportunity {
                    let dashboard = Arc::clone(&dashboard_for_cb);
                    let extra = json!({"suggested_size": opp.suggested_size});
                    let opp_type = opp.opportunity_type.as_str().to_string();
                    let market_id = signal.market_id.clone();
                    let edge = opp.edge;
                    tokio::spawn(async move {
                        dashboard
                            .add_opportunity(json!({"type": opp_type, "market_id": market_id, "edge": edge, "suggested_size": extra["suggested_size"]}))
                            .await;
                    });
                }
                {
                    let dashboard = Arc::clone(&dashboard_for_cb);
                    let action = signal.action.clone();
                    let market_id = signal.market_id.clone();
                    tokio::spawn(async move {
                        dashboard.add_signal(json!({"action": action, "market_id": market_id})).await;
                    });
                }

                let exec = Arc::clone(&on_update_exec);
                tokio::spawn(async move { exec.submit_signal(signal).await });
            }
        });

        let data_feed = DataFeed::new(Arc::clone(&client), config.trading.markets.clone(), 5.0, Some(on_update), config.use_simulation());
        data_feed.start().await;

        let dashboard_integration = DashboardIntegration::new(
            Arc::clone(&dashboard),
            Some(Arc::clone(&data_feed)),
            Some(Arc::clone(&arb_engine)),
            Some(Arc::clone(&execution_engine)),
            Some(Arc::clone(&risk_manager)),
            Some(Arc::clone(&portfolio)),
            if config.is_dry_run() { "dry_run" } else { "live" },
        )
        .await;
        dashboard_integration.start(1.0).await;

        let bot = Arc::new(Self {
            config,
            port,
            running,
            client,
            kalshi_client,
            cross_platform_engine,
            data_feed,
            arb_engine,
            execution_engine,
            risk_manager,
            portfolio,
            dashboard,
            dashboard_integration,
        });

        if bot.config.is_dry_run() && bot.config.mode.simulate_fills {
            let sim = Arc::clone(&bot);
            tokio::spawn(async move { sim.simulate_fills().await });
        }

        if bot.kalshi_client.is_some() {
            let kalshi_bot = Arc::clone(&bot);
            tokio::spawn(async move { kalshi_bot.start_kalshi_monitoring().await });
        }

        let router = build_router(Arc::clone(&bot.dashboard));
        let addr = format!("0.0.0.0:{}", bot.port);
        tokio::spawn(async move {
            let listener = tokio::net::TcpListener::bind(&addr).await.expect("failed to bind dashboard address");
            if let Err(e) = axum::serve(listener, router).await {
                error!(error = %e, "Dashboard server error");
            }
        });

        info!("Bot and dashboard started successfully!");
        info!(url = format!("http://localhost:{}", bot.port), "Open in your browser");

        bot
    }

    async fn simulate_fills(self: Arc<Self>) {
        use rand::Rng;
        while self.running.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;

            let orders = self.execution_engine.get_open_orders(None).await;
            for order in orders {
                let fill_probability = self.config.mode.fill_probability.clamp(0.0, 1.0);
                if rand::thread_rng().gen_bool(fill_probability) {
                    if let Some(trade) = self.client.simulate_fill(&order.order_id, None).await {
                        self.execution_engine.handle_fill(&trade).await;
                        self.dashboard_integration.add_trade(trade.side.as_str(), trade.price, trade.size, json!({"market_id": trade.market_id})).await;
                    }
                }
            }
        }
    }

    async fn start_kalshi_monitoring(self: Arc<Self>) {
        let Some(kalshi_client) = &self.kalshi_client else { return };
        let Some(cross_platform_engine) = &self.cross_platform_engine else { return };

        info!("Starting Kalshi market monitoring...");

        self.dashboard.with_state(|s| s.cross_platform.matching_status = "loading".to_string()).await;

        info!("Fetching Kalshi markets...");
        let dashboard_progress = Arc::clone(&self.dashboard);
        let kalshi_markets = kalshi_client
            .list_all_markets(
                "open",
                5000,
                Some(&mut |count| {
                    let dashboard = Arc::clone(&dashboard_progress);
                    tokio::spawn(async move { dashboard.with_state(|s| s.cross_platform.kalshi_markets = count).await });
                }),
            )
            .await;
        info!(count = kalshi_markets.len(), "Loaded Kalshi markets");

        self.dashboard.with_state(|s| s.cross_platform.kalshi_markets = kalshi_markets.len()).await;

        info!("Waiting for Polymarket markets...");
        let mut poly_count = 0;
        for i in 0..30 {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            poly_count = self.data_feed.market_ids().await.len();
            self.dashboard.with_state(|s| s.cross_platform.polymarket_markets = poly_count).await;

            if poly_count >= 50 {
                info!(poly_count, "Got Polymarket markets - starting matching!");
                break;
            }
            if i % 5 == 0 {
                info!(poly_count, "Polymarket markets loaded...");
            }
        }

        if poly_count == 0 || kalshi_markets.is_empty() {
            return;
        }

        let polymarket_markets: Vec<Market> = {
            let mut markets = Vec::new();
            for market_id in self.data_feed.market_ids().await {
                if let Some(m) = self.data_feed.get_market(&market_id).await {
                    markets.push(m);
                }
            }
            markets
        };

        info!(poly = polymarket_markets.len(), kalshi = kalshi_markets.len(), "Starting background matching");
        self.dashboard.with_state(|s| s.cross_platform.matching_status = "matching".to_string()).await;

        let total = polymarket_markets.len() * kalshi_markets.len();
        self.dashboard.with_state(|s| s.cross_platform.matching_total = total).await;

        let dashboard_for_progress = Arc::clone(&self.dashboard);
        let matches = {
            let mut engine = cross_platform_engine.lock().await;
            engine
                .matcher
                .find_matches(
                    &polymarket_markets,
                    &kalshi_markets,
                    Some(&mut |checked, total, matches_found| {
                        let dashboard = Arc::clone(&dashboard_for_progress);
                        tokio::spawn(async move {
                            dashboard
                                .with_state(|s| {
                                    s.cross_platform.matching_checked = checked;
                                    s.cross_platform.matching_progress = if total > 0 { (checked * 100 / total) as u32 } else { 0 };
                                    s.cross_platform.matched_pairs = matches_found;
                                })
                                .await;
                        });
                    }),
                )
                .await
        };

        info!(pairs = matches.len(), "Matching complete");

        let matched_pairs_display: Vec<serde_json::Value> = matches
            .iter()
            .take(50)
            .map(|pair| json!({"poly_question": pair.polymarket_question, "kalshi_title": pair.kalshi_title, "similarity": pair.similarity_score, "category": pair.category}))
            .collect();

        self.dashboard
            .with_state(|s| {
                s.cross_platform.matching_status = "complete".to_string();
                s.cross_platform.matching_progress = 100;
                s.cross_platform.matched_pairs = matches.len();
                s.cross_platform.matched_pairs_data = matched_pairs_display;
            })
            .await;
    }

    async fn stop(&self) {
        info!("Shutting down...");
        self.running.store(false, Ordering::SeqCst);

        self.dashboard_integration.stop().await;
        self.data_feed.stop().await;
        self.execution_engine.stop().await;
        self.client.disconnect().await;

        let summary = self.portfolio.lock().unwrap().get_summary();
        info!("{}", "=".repeat(60));
        info!("Final Summary");
        info!("{}", "=".repeat(60));
        info!(total_pnl = summary.pnl.total_pnl, total_trades = summary.total_trades, win_rate = summary.win_rate);

        if let Some(cp) = &self.cross_platform_engine {
            let stats = cp.lock().await.get_stats();
            info!(cross_platform_opportunities = stats.total_opportunities, matched_pairs = stats.matched_pairs);
        }

        info!("Shutdown complete");
    }
}

fn non_empty(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
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

    let bot = TradingBotWithDashboard::start(config, args.port).await;

    let _ = tokio::signal::ctrl_c().await;
    info!("Shutdown signal received");
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
