use crate::config::Config;
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE, ACCEPT};
use reqwest::blocking::Client;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use chrono::Utc;
use std::error::Error;

type HmacSha256 = Hmac<Sha256>;

pub struct OkxInterface {
    client: Client,
    config: Config,
    base_url: String,
}

impl OkxInterface {
    pub fn new(config: Config) -> Self {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        
        // Critical for Demo Trading
        if config.is_paper_trading {
            headers.insert("x-simulated-trading", HeaderValue::from_static("1"));
        }

        OkxInterface {
            client: Client::builder().default_headers(headers).build().unwrap(),
            config,
            base_url: "https://www.okx.com".to_string(),
        }
    }

    // --- Signing Engine ---
    fn sign_request(&self, method: &str, path: &str, body: &str) -> Result<HeaderMap, Box<dyn Error>> {
        let timestamp = Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
        let message = format!("{}{}{}{}", timestamp, method, path, body);

        let mut mac = HmacSha256::new_from_slice(self.config.secret_key.as_bytes())?;
        mac.update(message.as_bytes());
        let signature = base64::encode(mac.finalize().into_bytes());

        let mut headers = HeaderMap::new();
        headers.insert("OK-ACCESS-KEY", HeaderValue::from_str(&self.config.api_key)?);
        headers.insert("OK-ACCESS-SIGN", HeaderValue::from_str(&signature)?);
        headers.insert("OK-ACCESS-TIMESTAMP", HeaderValue::from_str(&timestamp)?);
        headers.insert("OK-ACCESS-PASSPHRASE", HeaderValue::from_str(&self.config.passphrase)?);

        Ok(headers)
    }

    // --- Public API Methods ---

    pub fn get_ticker(&self, inst_id: &str) -> Result<serde_json::Value, Box<dyn Error>> {
        let path = format!("/api/v5/market/ticker?instId={}", inst_id);
        let url = format!("{}{}", self.base_url, path);
        
        let resp = self.client.get(&url).send()?.json()?;
        Ok(resp)
    }

    pub fn place_order(&self, inst_id: &str, side: &str, sz: &str) -> Result<serde_json::Value, Box<dyn Error>> {
        let path = "/api/v5/trade/order";
        let url = format!("{}{}", self.base_url, path);

        // Payload
        let payload = serde_json::json!({
            "instId": inst_id,
            "tdMode": "cross",
            "side": side,
            "ordType": "market",
            "sz": sz
        });
        let body_str = payload.to_string();

        // Sign request
        let headers = self.sign_request("POST", path, &body_str)?;

        // Execute
        let resp = self.client.post(&url)
            .headers(headers)
            .body(body_str)
            .send()?
            .json()?;
            
        Ok(resp)
    }
}