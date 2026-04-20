// src/reconnect.rs
//
// Exponential-backoff reconnection helper for the public WebSocket.
//
// The v0.6 debug pass added a basic retry loop (fixed 3s), which handled
// initial connection failures. This module generalizes it: successive
// failures double the delay (1s → 2s → 4s → ... up to 30s), and every
// successful connection resets the counter.
//
// The private WS has its own copy of this logic baked into `private_ws.rs`
// because it has to retry the login + subscribe dance as a unit. This
// helper is for the public stream only.

use std::time::Duration;
use tokio::sync::mpsc::Receiver;
use tokio::time;
use tracing::{info, error};
use crate::ws_client::{WsClient, WsConfig};
use crate::ws_types::StreamEvent;

pub const BACKOFF_INITIAL_SECS: u64 = 1;
pub const BACKOFF_MAX_SECS: u64 = 30;

/// Open a public WS connection, retrying with exponential backoff
/// until success. Returns the receiver end of the event channel.
pub async fn connect_with_backoff<F>(mut make_config: F) -> Receiver<StreamEvent>
where
    F: FnMut() -> WsConfig,
{
    let mut delay_secs = BACKOFF_INITIAL_SECS;
    loop {
        let config = make_config();
        let client = WsClient::new(config);
        match client.connect().await {
            Ok(rx) => {
                info!("[public] WebSocket connected");
                return rx;
            }
            Err(e) => {
                error!("[public] connection failed: {}; retrying in {}s", e, delay_secs);
                time::sleep(Duration::from_secs(delay_secs)).await;
                delay_secs = (delay_secs * 2).min(BACKOFF_MAX_SECS);
            }
        }
    }
}
