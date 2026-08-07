//! Configuration loading and validation (mirrors `utils/config_loader.py`).

use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("Configuration file not found: {0}")]
    NotFound(String),
    #[error("Invalid YAML in config file: {0}")]
    InvalidYaml(#[from] serde_yaml::Error),
    #[error("Failed to read configuration file: {0}")]
    Io(#[from] std::io::Error),
    #[error("Configuration validation failed:\n{0}")]
    Validation(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ApiConfig {
    pub polymarket_rest_url: String,
    pub polymarket_ws_url: String,
    pub gamma_api_url: String,
    pub kalshi_api_url: String,
    pub api_key: String,
    pub api_secret: String,
    pub passphrase: String,
    pub private_key: String,
    pub timeout_seconds: f64,
    pub max_retries: u32,
    pub retry_delay_seconds: f64,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            polymarket_rest_url: "https://clob.polymarket.com".to_string(),
            polymarket_ws_url: "wss://ws-subscriptions-clob.polymarket.com/ws/market".to_string(),
            gamma_api_url: "https://gamma-api.polymarket.com".to_string(),
            kalshi_api_url: "https://api.elections.kalshi.com/trade-api/v2".to_string(),
            api_key: String::new(),
            api_secret: String::new(),
            passphrase: String::new(),
            private_key: String::new(),
            timeout_seconds: 30.0,
            max_retries: 3,
            retry_delay_seconds: 1.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TradingConfig {
    pub markets: Vec<String>,
    pub min_edge: f64,
    pub bundle_arb_enabled: bool,
    pub min_spread: f64,
    pub tick_size: f64,
    pub mm_enabled: bool,
    pub default_order_size: f64,
    pub min_order_size: f64,
    pub max_order_size: f64,
    pub slippage_tolerance: f64,
    pub order_timeout_seconds: f64,
    pub maker_fee_bps: f64,
    pub taker_fee_bps: f64,
    pub estimated_gas_per_order: f64,
}

impl Default for TradingConfig {
    fn default() -> Self {
        Self {
            markets: Vec::new(),
            min_edge: 0.01,
            bundle_arb_enabled: true,
            min_spread: 0.05,
            tick_size: 0.01,
            mm_enabled: true,
            default_order_size: 50.0,
            min_order_size: 5.0,
            max_order_size: 200.0,
            slippage_tolerance: 0.02,
            order_timeout_seconds: 60.0,
            maker_fee_bps: 0.0,
            taker_fee_bps: 150.0,
            estimated_gas_per_order: 0.02,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ModeConfig {
    pub trading_mode: String,
    pub data_mode: String,
    pub cross_platform_enabled: bool,
    pub kalshi_enabled: bool,
    pub min_match_similarity: f64,
    pub dry_run_initial_balance: f64,
    pub simulate_fills: bool,
    pub fill_probability: f64,
}

impl Default for ModeConfig {
    fn default() -> Self {
        Self {
            trading_mode: "dry_run".to_string(),
            data_mode: "real".to_string(),
            cross_platform_enabled: true,
            kalshi_enabled: true,
            min_match_similarity: 0.6,
            dry_run_initial_balance: 10000.0,
            simulate_fills: true,
            fill_probability: 0.8,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LoggingConfig {
    pub console_level: String,
    pub file_level: String,
    pub log_dir: String,
    pub main_log_file: String,
    pub trades_log_file: String,
    pub opportunities_log_file: String,
    pub max_log_size_mb: u64,
    pub backup_count: u32,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            console_level: "INFO".to_string(),
            file_level: "DEBUG".to_string(),
            log_dir: "logs".to_string(),
            main_log_file: "bot.log".to_string(),
            trades_log_file: "trades.log".to_string(),
            opportunities_log_file: "opportunities.log".to_string(),
            max_log_size_mb: 50,
            backup_count: 5,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MonitoringConfig {
    pub snapshot_interval: f64,
    pub heartbeat_interval: f64,
    pub track_latency: bool,
    pub track_fill_rates: bool,
}

impl Default for MonitoringConfig {
    fn default() -> Self {
        Self {
            snapshot_interval: 60.0,
            heartbeat_interval: 30.0,
            track_latency: true,
            track_fill_rates: true,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BotConfig {
    pub api: ApiConfig,
    pub trading: TradingConfig,
    pub risk: RiskConfig,
    pub mode: ModeConfig,
    pub logging: LoggingConfig,
    pub monitoring: MonitoringConfig,
}

impl BotConfig {
    pub fn is_dry_run(&self) -> bool {
        self.mode.trading_mode.to_lowercase() == "dry_run"
    }

    pub fn is_live(&self) -> bool {
        self.mode.trading_mode.to_lowercase() == "live"
    }

    pub fn use_simulation(&self) -> bool {
        self.mode.data_mode.to_lowercase() == "simulation"
    }
}

/// Load configuration from a YAML file, applying `POLYMARKET_*` env var
/// overrides and validating the result. Mirrors `load_config` in
/// `utils/config_loader.py`.
pub fn load_config(config_path: impl AsRef<Path>) -> Result<BotConfig, ConfigError> {
    let path = config_path.as_ref();

    if !path.exists() {
        return Err(ConfigError::NotFound(path.display().to_string()));
    }

    let raw = std::fs::read_to_string(path)?;
    let mut config: BotConfig = serde_yaml::from_str(&raw)?;

    apply_env_overrides(&mut config.api);
    validate_config(&config)?;

    Ok(config)
}

fn apply_env_overrides(api: &mut ApiConfig) {
    if let Ok(v) = std::env::var("POLYMARKET_API_KEY") {
        if !v.is_empty() {
            api.api_key = v;
        }
    }
    if let Ok(v) = std::env::var("POLYMARKET_API_SECRET") {
        if !v.is_empty() {
            api.api_secret = v;
        }
    }
    if let Ok(v) = std::env::var("POLYMARKET_PASSPHRASE") {
        if !v.is_empty() {
            api.passphrase = v;
        }
    }
    if let Ok(v) = std::env::var("POLYMARKET_PRIVATE_KEY") {
        if !v.is_empty() {
            api.private_key = v;
        }
    }
}

fn validate_config(config: &BotConfig) -> Result<(), ConfigError> {
    let mut errors: Vec<String> = Vec::new();

    if config.trading.min_edge < 0.0 || config.trading.min_edge > 1.0 {
        errors.push("trading.min_edge must be between 0 and 1".to_string());
    }
    if config.trading.min_spread < 0.0 || config.trading.min_spread > 1.0 {
        errors.push("trading.min_spread must be between 0 and 1".to_string());
    }
    if config.trading.tick_size <= 0.0 {
        errors.push("trading.tick_size must be positive".to_string());
    }
    if config.trading.default_order_size <= 0.0 {
        errors.push("trading.default_order_size must be positive".to_string());
    }

    if config.risk.max_position_per_market <= 0.0 {
        errors.push("risk.max_position_per_market must be positive".to_string());
    }
    if config.risk.max_global_exposure <= 0.0 {
        errors.push("risk.max_global_exposure must be positive".to_string());
    }
    if config.risk.max_daily_loss < 0.0 {
        errors.push("risk.max_daily_loss must be non-negative".to_string());
    }
    if config.risk.max_drawdown_pct < 0.0 || config.risk.max_drawdown_pct > 1.0 {
        errors.push("risk.max_drawdown_pct must be between 0 and 1".to_string());
    }

    let mode_lower = config.mode.trading_mode.to_lowercase();
    if mode_lower != "live" && mode_lower != "dry_run" {
        errors.push("mode.trading_mode must be 'live' or 'dry_run'".to_string());
    }

    if config.is_live() {
        if config.api.api_key.is_empty() || config.api.api_key == "YOUR_API_KEY_HERE" {
            errors.push("api.api_key is required for live trading".to_string());
        }
        if config.api.private_key.is_empty() || config.api.private_key == "YOUR_PRIVATE_KEY_HERE" {
            errors.push("api.private_key is required for live trading".to_string());
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        let joined = errors.iter().map(|e| format!("  - {e}")).collect::<Vec<_>>().join("\n");
        Err(ConfigError::Validation(joined))
    }
}

/// Save configuration to a YAML file. Mirrors `save_config`.
pub fn save_config(config: &BotConfig, config_path: impl AsRef<Path>) -> Result<(), ConfigError> {
    let yaml = serde_yaml::to_string(config)?;
    std::fs::write(config_path, yaml)?;
    Ok(())
}

pub fn get_default_config() -> BotConfig {
    BotConfig::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_python_dataclass_defaults() {
        let config = BotConfig::default();
        assert_eq!(config.api.polymarket_rest_url, "https://clob.polymarket.com");
        assert_eq!(config.trading.min_edge, 0.01);
        assert_eq!(config.risk.max_global_exposure, 5000.0);
        assert!(config.is_dry_run());
        assert!(!config.is_live());
    }

    #[test]
    fn loads_existing_repo_config_yaml() {
        // The real config.yaml lives one directory up from this crate.
        let path = Path::new("../config.yaml");
        if !path.exists() {
            return; // Skip if run outside the monorepo layout.
        }
        let config = load_config(path).expect("config.yaml should load and validate");
        assert!(config.is_dry_run());
        assert_eq!(config.risk.max_global_exposure, 50.0);
    }
}
