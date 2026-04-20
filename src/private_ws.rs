// src/private_ws.rs
//
// ============================================================================
// WHY THIS MODULE EXISTS (Phase 2E)
// ============================================================================
//
// The public WS client (ws_client.rs) streams anonymous market data —
// trades and order books. The `orders` and `positions` channels that
// drive our position state machine live on OKX's AUTHENTICATED private
// WebSocket endpoint. It's a separate connection with a separate URL
// and requires an HMAC-signed login before any subscription.
//
// ============================================================================
// LOGIN PROTOCOL
// ============================================================================
//
// 1. Open WS to wss://wspap.okx.com:8443/ws/v5/private?brokerId=9999 (paper)
//    or wss://ws.okx.com:8443/ws/v5/private (live).
//
// 2. Send a login frame:
//      {"op": "login", "args": [{
//         "apiKey":    "...",
//         "passphrase":"...",
//         "timestamp": "<unix_seconds>",
//         "sign":      base64(HMAC-SHA256(secret, timestamp + "GET" + "/users/self/verify"))
//      }]}
//
// 3. Wait for {"event": "login", "code": "0"} — if code != "0", abort.
//
// 4. Subscribe to channels:
//      {"op": "subscribe", "args": [
//         {"channel": "orders",    "instType": "SWAP"},
//         {"channel": "positions", "instType": "SWAP"}
//      ]}
//
// 5. From then on, each order state transition on the account pushes
//    a message with channel="orders" and data carrying state, clOrdId,
//    fills, and timestamps.
//
// ============================================================================
// RECONNECTION
// ============================================================================
//
// Private WS is wrapped in the same exponential-backoff loop as public
// (see reconnect.rs). On disconnect, we log in afresh and re-subscribe.
// Because the Positions registry lives on the main task and survives
// across reconnects, the state we care about is preserved — we only
// lose events that happened DURING the disconnect. This is a known
// gap and would be addressed in a later phase by calling the REST
// `/api/v5/trade/orders-history` endpoint on reconnect to reconcile.

use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{self, Duration};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use tracing::{info, warn, error, debug};
use crate::config::Config;
use crate::position::{Positions, OrderUpdate};

type HmacSha256 = Hmac<Sha256>;

pub struct PrivateWsConfig {
    pub url: String,
    pub api_key: String,
    pub secret_key: String,
    pub passphrase: String,
    pub instruments: Vec<String>,
}

impl PrivateWsConfig {
    pub fn from_app_config(config: &Config, instruments: Vec<String>) -> Self {
        let url = if config.is_paper_trading {
            "wss://wspap.okx.com:8443/ws/v5/private?brokerId=9999".to_string()
        } else {
            "wss://ws.okx.com:8443/ws/v5/private".to_string()
        };
        PrivateWsConfig {
            url,
            api_key: config.api_key.clone(),
            secret_key: config.secret_key.clone(),
            passphrase: config.passphrase.clone(),
            instruments,
        }
    }
}

/// Run the private WS task with exponential-backoff reconnection.
/// This function runs forever; spawn it with `tokio::spawn`.
pub async fn run(config: PrivateWsConfig, positions: Arc<Mutex<Positions>>) {
    let mut backoff_secs = 1u64;
    const BACKOFF_MAX: u64 = 30;

    loop {
        match connect_and_run(&config, &positions).await {
            Ok(()) => {
                // Normal close — reset backoff and reconnect promptly.
                info!("[private] session ended cleanly; reconnecting...");
                backoff_secs = 1;
            }
            Err(e) => {
                error!("[private] session failed: {}; reconnecting in {}s", e, backoff_secs);
                time::sleep(Duration::from_secs(backoff_secs)).await;
                backoff_secs = (backoff_secs * 2).min(BACKOFF_MAX);
            }
        }
    }
}

/// One connection lifecycle: connect → login → subscribe → drive events.
/// Returns Ok on graceful close, Err on any failure (caller backs off).
async fn connect_and_run(
    config: &PrivateWsConfig,
    positions: &Arc<Mutex<Positions>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!("[private] connecting to {}", config.url);
    let (ws_stream, _) = connect_async(&config.url).await?;
    let (mut write, mut read) = ws_stream.split();

    // --- Login ---
    let login_msg = build_login_message(config)?;
    write.send(Message::Text(login_msg)).await?;

    // Wait for login response (first text frame with event="login").
    let login_ok = loop {
        match read.next().await {
            Some(Ok(Message::Text(text))) => {
                let parsed: serde_json::Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if parsed.get("event").and_then(|v| v.as_str()) == Some("login") {
                    let code = parsed.get("code").and_then(|v| v.as_str()).unwrap_or("");
                    let msg = parsed.get("msg").and_then(|v| v.as_str()).unwrap_or("");
                    if code == "0" {
                        info!("[private] login OK");
                        break true;
                    } else {
                        error!("[private] login FAILED code={} msg={}", code, msg);
                        break false;
                    }
                }
                // Non-login event before login response — keep reading.
            }
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(Box::new(e)),
            None => return Err("connection closed during login".into()),
        }
    };
    if !login_ok {
        return Err("login rejected".into());
    }

    // --- Subscribe ---
    let sub_msg = build_subscribe_message();
    debug!("[private] subscribing: {}", sub_msg);
    write.send(Message::Text(sub_msg)).await?;

    // --- Main read loop with keepalive ---
    let mut ping_interval = time::interval(Duration::from_secs(20));
    ping_interval.tick().await; // fire immediately then every 20s

    loop {
        tokio::select! {
            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if text == "pong" { continue; }
                        handle_message(&text, positions).await;
                    }
                    Some(Ok(Message::Close(_))) => {
                        warn!("[private] server sent close frame");
                        return Ok(());
                    }
                    Some(Ok(_)) => continue,
                    Some(Err(e)) => return Err(Box::new(e)),
                    None => return Ok(()),
                }
            }
            _ = ping_interval.tick() => {
                if let Err(e) = write.send(Message::Text("ping".into())).await {
                    return Err(Box::new(e));
                }
            }
        }
    }
}

fn build_login_message(
    config: &PrivateWsConfig,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let ts = chrono::Utc::now().timestamp().to_string();
    let prehash = format!("{}GET/users/self/verify", ts);

    let mut mac = HmacSha256::new_from_slice(config.secret_key.as_bytes())?;
    mac.update(prehash.as_bytes());
    let sign = base64::encode(mac.finalize().into_bytes());

    Ok(serde_json::json!({
        "op": "login",
        "args": [{
            "apiKey": config.api_key,
            "passphrase": config.passphrase,
            "timestamp": ts,
            "sign": sign,
        }]
    }).to_string())
}

fn build_subscribe_message() -> String {
    // Subscribe to both order events and position events at the SWAP level.
    // instType="SWAP" covers all perpetual swaps on the account — we'll
    // filter to BTC-USDT-SWAP in the handler.
    serde_json::json!({
        "op": "subscribe",
        "args": [
            {"channel": "orders",    "instType": "SWAP"},
            {"channel": "positions", "instType": "SWAP"}
        ]
    }).to_string()
}

async fn handle_message(raw: &str, positions: &Arc<Mutex<Positions>>) {
    let v: serde_json::Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => {
            debug!("[private] unparseable message: {}", e);
            return;
        }
    };

    // Event messages (subscribe confirmations, errors) — log and move on.
    if let Some(event) = v.get("event").and_then(|e| e.as_str()) {
        if event != "subscribe" {
            warn!("[private] event: {} ({})", event, raw);
        } else {
            info!("[private] subscribed: {:?}", v.get("arg"));
        }
        return;
    }

    let channel = v.get("arg")
        .and_then(|a| a.get("channel"))
        .and_then(|c| c.as_str())
        .unwrap_or("");
    let data = match v.get("data").and_then(|d| d.as_array()) {
        Some(a) => a,
        None => return,
    };

    match channel {
        "orders"    => handle_orders(data, positions).await,
        "positions" => handle_positions(data, positions).await,
        _           => debug!("[private] ignoring channel: {}", channel),
    }
}

async fn handle_orders(data: &[serde_json::Value], positions: &Arc<Mutex<Positions>>) {
    for entry in data {
        let client_ord_id = entry.get("clOrdId").and_then(|s| s.as_str()).unwrap_or("").to_string();
        let ord_id = entry.get("ordId").and_then(|s| s.as_str()).unwrap_or("").to_string();
        let state = entry.get("state").and_then(|s| s.as_str()).unwrap_or("").to_string();

        let fill_price = entry.get("fillPx")
            .and_then(|s| s.as_str())
            .and_then(|s| s.parse::<f64>().ok());
        let acc_fill_size = entry.get("accFillSz")
            .and_then(|s| s.as_str())
            .and_then(|s| s.parse::<f64>().ok())
            .map(|f| f as u64);
        let ts = entry.get("uTime")
            .and_then(|s| s.as_str())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);

        // OCO attached algo orders arrive with clOrdId ending in "_oco".
        // A fill on those signals position CLOSE on the parent.
        if state == "filled" && client_ord_id.ends_with("_oco") {
            let parent = client_ord_id.trim_end_matches("_oco").to_string();
            positions.lock().await.mark_closed_by_parent(&parent, ts);
            continue;
        }

        let update = OrderUpdate {
            client_ord_id, ord_id, state,
            fill_price, acc_fill_size, timestamp_ms: ts,
        };
        positions.lock().await.apply_order_update(&update);
    }
}

async fn handle_positions(data: &[serde_json::Value], _positions: &Arc<Mutex<Positions>>) {
    // v0.7 uses orders-channel events as the source of truth for state
    // transitions. The positions channel is logged for observability
    // and will become authoritative for P&L tracking in Phase 2F.
    for entry in data {
        let inst = entry.get("instId").and_then(|s| s.as_str()).unwrap_or("?");
        let pos_side = entry.get("posSide").and_then(|s| s.as_str()).unwrap_or("?");
        let pos = entry.get("pos").and_then(|s| s.as_str()).unwrap_or("0");
        let avg_px = entry.get("avgPx").and_then(|s| s.as_str()).unwrap_or("0");
        debug!("[private] position snapshot: {} {} pos={} avg={}", inst, pos_side, pos, avg_px);
    }
}
