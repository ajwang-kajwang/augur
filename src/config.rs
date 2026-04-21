use dotenv::dotenv;
use std::env;

pub struct Config {
    pub api_key: String,
    pub secret_key: String,
    pub passphrase: String,
    pub is_paper_trading: bool,
    pub trading_enabled: bool,
    pub account_balance: f64,
    pub persistence_enabled: bool,
    pub persistence_path: String,
}

impl Config {
    pub fn load() -> Result<Self, String> {
        dotenv().ok();

        let api_key = env::var("OKX_API_KEY").map_err(|_| "Missing OKX_API_KEY")?;
        let secret_key = env::var("OKX_SECRET_KEY").map_err(|_| "Missing OKX_SECRET_KEY")?;
        let passphrase = env::var("OKX_PASSPHRASE").map_err(|_| "Missing OKX_PASSPHRASE")?;
        let persistence_enabled = std::env::var("AUGUR_PERSISTENCE_ENABLED")
            .unwrap_or_else(|_| "0".to_string()) == "1";
            
        let persistence_path = std::env::var("AUGUR_PERSISTENCE_PATH")
            .unwrap_or_else(|_| "/mnt/usb_ssd/augur_data".to_string());

        Ok(Config {
            api_key,
            secret_key,
            passphrase,
            persistence_enabled,
            persistence_path,
            is_paper_trading: true,
            trading_enabled: env::var("AUGUR_TRADING_ENABLED")
                .unwrap_or_else(|_| "0".to_string()) == "1",
            account_balance: env::var("AUGUR_ACCOUNT_BALANCE")
                .unwrap_or_else(|_| "10000.0".to_string())
                .parse::<f64>()
                .unwrap_or(10000.0),
        })
    }
}
