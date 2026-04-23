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
            client: Client::builder()
                .default_headers(headers)
                .build()
                .expect("reqwest client build"),
            config,
            base_url: "https://www.okx.com".to_string(),
        }
    }

    pub fn config(&self) -> &Config { &self.config }

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

    pub async fn get_ticker(&self, inst_id: &str) -> Result<serde_json::Value, Box<dyn Error>> {
        let path = format!("/api/v5/market/ticker?instId={}", inst_id);
        let url = format!("{}{}", self.base_url, path);
        let resp = self.client.get(&url).send().await?.json().await?;
        Ok(resp)
    }

    pub async fn place_order(&self, inst_id: &str, side: &str, sz: &str)
        -> Result<serde_json::Value, Box<dyn Error>>
    {
        let payload = serde_json::json!({
            "instId": inst_id,
            "tdMode": "cross",
            "side": side,
            "ordType": "market",
            "sz": sz,
        });
        self.place_order_payload(&payload).await
    }

    pub async fn place_order_payload(&self, payload: &serde_json::Value)
        -> Result<serde_json::Value, Box<dyn Error>>
    {
        let path = "/api/v5/trade/order";
        let url = format!("{}{}", self.base_url, path);
        let body_str = payload.to_string();
        let headers = self.sign_request("POST", path, &body_str)?;

        let resp = self.client.post(&url)
            .headers(headers)
            .body(body_str)
            .send()
            .await?;
        Ok(resp.json::<serde_json::Value>().await?)
    }

    pub async fn cancel_order(&self, inst_id: &str, ord_id: &str)
        -> Result<serde_json::Value, Box<dyn Error>>
    {
        let path = "/api/v5/trade/cancel-order";
        let url = format!("{}{}", self.base_url, path);
        let payload = serde_json::json!({ "instId": inst_id, "ordId": ord_id });
        let body_str = payload.to_string();
        let headers = self.sign_request("POST", path, &body_str)?;

        let resp = self.client.post(&url)
            .headers(headers)
            .body(body_str)
            .send()
            .await?;
        Ok(resp.json::<serde_json::Value>().await?)
    }

    /// GET /api/v5/trade/orders-history — Phase 2F (v0.12).
    /// Returns the most recent orders (last 7 days, instrument-scoped).
    /// Used by the reconciliation pass to backfill missed events from
    /// any private WS disconnect window.
    ///
    /// `inst_type` is "SWAP" for perpetuals. `inst_id` filters to one
    /// instrument; pass empty string for all (we always pass a specific
    /// inst_id to keep responses small).
    ///
    /// OKX paginates this via `before`/`after` query params keyed on
    /// `ordId`. For our use we typically only need the most recent ~50
    /// orders (way more than the FMG strategy submits in a day), so we
    /// take the default page (~100 entries) and don't paginate.
    ///
    /// Returns a `Send + Sync` error so the caller can `await` this
    /// from inside a spawned task (tokio::spawn requires Send futures).
    pub async fn get_orders_history(&self, inst_type: &str, inst_id: &str)
        -> Result<serde_json::Value, Box<dyn Error + Send + Sync>>
    {
        let path = format!("/api/v5/trade/orders-history?instType={}&instId={}",
                           inst_type, inst_id);
        let url = format!("{}{}", self.base_url, path);
        // GETs with query params: the body in the signature is empty.
        let headers = self.sign_request_send(&path, "")?;

        let resp = self.client.get(&url)
            .headers(headers)
            .send()
            .await?;
        Ok(resp.json::<serde_json::Value>().await?)
    }

    /// Send-safe variant of sign_request for use in spawned tasks.
    /// Returns the same error type as the Send variant of the callers.
    fn sign_request_send(&self, path: &str, body: &str)
        -> Result<HeaderMap, Box<dyn Error + Send + Sync>>
    {
        let timestamp = Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
        let message = format!("{}{}{}{}", timestamp, "GET", path, body);

        let mut mac = HmacSha256::new_from_slice(self.config.secret_key.as_bytes())
            .map_err(|e| Box::<dyn Error + Send + Sync>::from(e.to_string()))?;
        mac.update(message.as_bytes());
        let signature = base64::encode(mac.finalize().into_bytes());

        let mut headers = HeaderMap::new();
        headers.insert("OK-ACCESS-KEY", HeaderValue::from_str(&self.config.api_key)
            .map_err(|e| Box::<dyn Error + Send + Sync>::from(e.to_string()))?);
        headers.insert("OK-ACCESS-SIGN", HeaderValue::from_str(&signature)
            .map_err(|e| Box::<dyn Error + Send + Sync>::from(e.to_string()))?);
        headers.insert("OK-ACCESS-TIMESTAMP", HeaderValue::from_str(&timestamp)
            .map_err(|e| Box::<dyn Error + Send + Sync>::from(e.to_string()))?);
        headers.insert("OK-ACCESS-PASSPHRASE", HeaderValue::from_str(&self.config.passphrase)
            .map_err(|e| Box::<dyn Error + Send + Sync>::from(e.to_string()))?);
        Ok(headers)
    }
}
