use dotenv::dotenv;
use std::env;

pub struct Config {
    pub api_key: String,
    pub secret_key: String,
    pub passphrase: String,
    pub is_paper_trading: bool,
}

impl Config {
    pub fn load() -> Result<Self, String> {
        dotenv().ok(); // Load .env file
        
        let api_key = env::var("OKX_API_KEY").map_err(|_| "Missing OKX_API_KEY")?;
        let secret_key = env::var("OKX_SECRET_KEY").map_err(|_| "Missing OKX_SECRET_KEY")?;
        let passphrase = env::var("OKX_PASSPHRASE").map_err(|_| "Missing OKX_PASSPHRASE")?;
        
        Ok(Config {
            api_key,
            secret_key,
            passphrase,
            is_paper_trading: true, 
        })
    }
}
