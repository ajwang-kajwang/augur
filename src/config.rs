use dotenv::dotenv;
use std::env;

pub struct Config {
    pub api_key: String,
    pub secret_key: String,
    pub passphrase: String,
    pub is_paper_trading: bool,
    pub trading_enabled: bool,
    pub account_balance: f64,
    /// When true, every incoming trade tick is recorded to Parquet files.
    /// This is independent of trading_enabled — you typically want
    /// persistence ON even in dry-run, to build a backtesting dataset.
    pub persistence_enabled: bool,
    /// Output directory for Parquet files. Ignored if persistence_enabled
    /// is false. On the Jetson, this should point at an external SSD
    /// mount — writing tick data to the boot MicroSD will destroy it.
    pub persistence_path: String,
}

impl Config {
    pub fn load() -> Result<Self, String> {
        dotenv().ok();

        let api_key = env::var("OKX_API_KEY").map_err(|_| "Missing OKX_API_KEY")?;
        let secret_key = env::var("OKX_SECRET_KEY").map_err(|_| "Missing OKX_SECRET_KEY")?;
        let passphrase = env::var("OKX_PASSPHRASE").map_err(|_| "Missing OKX_PASSPHRASE")?;

        Ok(Config {
            api_key,
            secret_key,
            passphrase,
            is_paper_trading: true,
            trading_enabled: env::var("AUGUR_TRADING_ENABLED")
                .unwrap_or_else(|_| "0".to_string()) == "1",
            account_balance: env::var("AUGUR_ACCOUNT_BALANCE")
                .unwrap_or_else(|_| "10000.0".to_string())
                .parse::<f64>()
                .unwrap_or(10000.0),
            persistence_enabled: env::var("AUGUR_PERSISTENCE_ENABLED")
                .unwrap_or_else(|_| "1".to_string()) == "1",
            persistence_path: env::var("AUGUR_PERSISTENCE_PATH")
                .unwrap_or_else(|_| "/mnt/usb_ssd/augur_data".to_string()),
        })
    }
}
