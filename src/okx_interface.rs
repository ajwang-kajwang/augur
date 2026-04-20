// src/okx_interface.rs
//
// Authenticated REST client for OKX. Handles HMAC-SHA256 signing and
// exposes generic order placement so callers can construct complex
// payloads (e.g. limit orders with attached algo orders) without
// bloating this module.

use crate::config::Config;
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE, ACCEPT};
use reqwest::Client;
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

        if config.is_paper_trading {
            headers.insert("x-simulated-trading", HeaderValue::from_static("1"));
        }

        OkxInterface {
            client: Client::new(),
            config,
            base_url: "https://www.okx.com".to_string(),
        }
    }

    fn sign_request(&self, method: &str, path: &str, body: &str)
        -> Result<HeaderMap, Box<dyn Error>>
    {
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

    pub fn get_ticker(&self, inst_id: &str) -> Result<serde_json::Value, Box<dyn Error>> {
        let path = format!("/api/v5/market/ticker?instId={}", inst_id);
        let url = format!("{}{}", self.base_url, path);
        let resp = self.client.get(&url).send()?.json()?;
        Ok(resp)
    }

    /// Place a market order with a simple side+size payload.
    /// Preserved for v0.1 compatibility and quick manual testing.
    pub fn place_order(&self, inst_id: &str, side: &str, sz: &str)
        -> Result<serde_json::Value, Box<dyn Error>>
    {
        let payload = serde_json::json!({
            "instId": inst_id,
            "tdMode": "cross",
            "side": side,
            "ordType": "market",
            "sz": sz,
        });
        self.place_order_payload(&payload)
    }

    /// Place an order with an arbitrary payload. Used by the order manager
    /// to construct limit orders with attached algo orders (OCO exit pairs).
    ///
    /// The payload must match OKX's /api/v5/trade/order schema.
    pub fn place_order_payload(&self, payload: &serde_json::Value)
        -> Result<serde_json::Value, Box<dyn Error>>
    {
        let path = "/api/v5/trade/order";
        let url = format!("{}{}", self.base_url, path);
        let body_str = payload.to_string();
        let headers = self.sign_request("POST", path, &body_str)?;

        let resp = self.client.post(&url)
            .headers(headers)
            .body(body_str)
            .send()?
            .await?;
        let json: serde_json::Value = resp.json().await?;
        Ok(json)
    }

    /// Cancel an active order by instrument and order ID.
    /// Returned by the order manager when entry limits need to be pulled
    /// (e.g. the pattern has invalidated before fill).
    pub fn cancel_order(&self, inst_id: &str, ord_id: &str)
        -> Result<serde_json::Value, Box<dyn Error>>
    {
        let path = "/api/v5/trade/cancel-order";
        let url = format!("{}{}", self.base_url, path);
        let payload = serde_json::json!({
            "instId": inst_id,
            "ordId": ord_id,
        });
        let body_str = payload.to_string();
        let headers = self.sign_request("POST", path, &body_str)?;

        let resp = self.client.post(&url)
            .headers(headers)
            .body(body_str)
            .send()?
            .await?;
        let json: serde_json::Value = resp.json().await?;
        Ok(json)
    }
}
