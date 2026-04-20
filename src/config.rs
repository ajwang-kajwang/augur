use dotenv::dotenv;
use std::env;

pub struct Config {
    pub api_key: String,
    pub secret_key: String,
    pub passphrase: String,
    pub is_paper_trading: bool,
    pub trading_enabled: bool,
    pub account_balance: f64,
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
            trading_enabled: std::env::var("AUGUR_TRADING_ENABLED").unwrap_or_else(|_| "0".to_string()) == "1",
            account_balance: std::env::var("AUGUR_ACCOUNT_BALANCE").unwrap_or_else(|_| "10000.0".to_string()).parse::<f64>().unwrap_or(10000.0), 
        })
    }
}
