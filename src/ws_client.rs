// src/ws_client.rs
//
// THE BIG PICTURE:
// ================
// Your old architecture:  main() calls instrument.update() → REST GET → wait → parse → return
// Your new architecture:  WsClient connects once → OKX pushes data continuously → parsed events
//                         flow through channels → your strategy reacts instantly
//
// The control flow is INVERTED. Instead of you asking "what's the price?",
// the exchange tells you "the price just changed" the millisecond it happens.
//
// ASYNC RUST CRASH COURSE (read this first):
// ===========================================
// 1. `async fn` — declares a function that CAN pause. It returns a Future,
//    not the actual value. Nothing happens until you `.await` it.
//
// 2. `.await` — "pause here until this operation completes, but let other
//    tasks run while I wait." This is how one thread handles thousands of
//    connections without blocking.
//
// 3. `tokio::spawn(async { ... })` — launches a new concurrent task. Think
//    of it like spawning a thread, but far cheaper. Thousands of these can
//    run on a single OS thread.
//
// 4. `tokio::sync::mpsc` — a multi-producer, single-consumer channel.
//    Task A sends data, Task B receives it. This is how the WS read loop
//    (which runs forever) communicates with your strategy loop safely.
//
// 5. `tokio::select!` — waits on MULTIPLE async operations simultaneously.
//    Whichever completes first gets handled. This is your multiplexer.

use crate::ws_types::*;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio::time::{self, Duration};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{info, warn, error};
use url::Url;

/// Configuration for the WebSocket connection.
pub struct WsConfig {
    /// OKX public WebSocket endpoint.
    /// Paper trading: wss://wspap.okx.com:8443/ws/v5/public?brokerId=9999
    /// Live:          wss://ws.okx.com:8443/ws/v5/public
    pub url: String,
    /// Instruments to subscribe to, e.g. ["BTC-USDT-SWAP", "ETH-USDT-SWAP"]
    pub instruments: Vec<String>,
    /// Channel buffer size. 1024 means the channel can hold 1024 unprocessed
    /// messages before backpressure kicks in. If your consumer can't keep up
    /// with 1024 queued messages, you have bigger problems.
    pub channel_buffer: usize,
}

impl WsConfig {
    /// Sane defaults for paper trading with BTC.
    pub fn paper_trading(instruments: Vec<String>) -> Self {
        WsConfig {
            url: "wss://wspap.okx.com:8443/ws/v5/public?brokerId=9999".to_string(),
            instruments,
            channel_buffer: 1024,
        }
    }

    pub fn live(instruments: Vec<String>) -> Self {
        WsConfig {
            url: "wss://ws.okx.com:8443/ws/v5/public".to_string(),
            instruments,
            channel_buffer: 1024,
        }
    }
}

/// The WebSocket client. Call `connect()` to get a receiver channel
/// that streams parsed events into your strategy loop.
pub struct WsClient {
    config: WsConfig,
}

impl WsClient {
    pub fn new(config: WsConfig) -> Self {
        WsClient { config }
    }

    /// Connects to OKX, subscribes to channels, and returns a receiver
    /// that yields `StreamEvent`s.
    ///
    /// WHY A CHANNEL?
    /// The WebSocket read loop must run forever in the background. Your
    /// strategy loop also runs forever. They can't both own the same thread.
    /// The channel decouples them: the WS loop sends, your strategy receives.
    ///
    /// Returns: mpsc::Receiver<StreamEvent> — your single intake pipe for
    /// all market data across all instruments and channels.
    pub async fn connect(self) -> Result<mpsc::Receiver<StreamEvent>, Box<dyn std::error::Error>> {
        let url = Url::parse(&self.config.url)?;
        info!("Connecting to {}", url);

        // --- Step 1: TCP + TLS + WebSocket handshake (one await, all handled) ---
        let (ws_stream, _response) = connect_async(url).await?;
        info!("WebSocket connected");

        // Split the WebSocket into a write half (Sink) and read half (Stream).
        // WHY SPLIT?
        // We need to write (send subscriptions, pings) AND read (receive data)
        // concurrently. Splitting lets two different tasks hold each half.
        let (mut write, mut read) = ws_stream.split();

        // --- Step 2: Subscribe to channels ---
        let subscribe_msg = self.build_subscribe_message();
        info!("Subscribing: {}", subscribe_msg);
        write.send(Message::Text(subscribe_msg)).await?;

        // --- Step 3: Create the channel that connects WS loop → strategy loop ---
        let (tx, rx) = mpsc::channel::<StreamEvent>(self.config.channel_buffer);

        // --- Step 4: Spawn the read loop as a background task ---
        // This task runs FOREVER (until the connection drops or we shut down).
        // It reads raw WS messages, parses them, and pushes StreamEvents
        // through `tx`. Your strategy loop reads from `rx`.
        tokio::spawn(async move {
            // Ping interval: OKX drops connections that go silent for 30s.
            // We send "ping" every 15s to keep the connection alive.
            let mut ping_interval = time::interval(Duration::from_secs(15));

            loop {
                // `select!` is the multiplexer. It simultaneously waits for:
                //   - the next WebSocket message (from the exchange)
                //   - the next ping timer tick (from our keepalive clock)
                // Whichever fires first gets handled. Then we loop back.
                tokio::select! {
                    // Branch 1: Incoming WebSocket message
                    msg = read.next() => {
                        match msg {
                            Some(Ok(Message::Text(text))) => {
                                // OKX responds to our "ping" with "pong" — just ignore it.
                                if text == "pong" {
                                    continue;
                                }
                                // Parse and dispatch the message.
                                if let Some(event) = parse_message(&text) {
                                    // If the channel is full (consumer is overwhelmed),
                                    // try_send drops the message instead of blocking.
                                    // In HFT, stale data is worse than no data.
                                    if tx.try_send(event).is_err() {
                                        warn!("Channel full — dropping message (consumer too slow)");
                                    }
                                }
                            }
                            Some(Ok(Message::Close(_))) => {
                                warn!("Server sent close frame");
                                break;
                            }
                            Some(Err(e)) => {
                                error!("WebSocket error: {}", e);
                                break;
                            }
                            None => {
                                // Stream ended (connection dropped).
                                error!("WebSocket stream ended");
                                break;
                            }
                            _ => {} // Binary, Ping, Pong frames — ignore
                        }
                    }

                    // Branch 2: Time to send a keepalive ping
                    _ = ping_interval.tick() => {
                        if write.send(Message::Text("ping".to_string())).await.is_err() {
                            error!("Failed to send ping — connection likely dead");
                            break;
                        }
                    }
                }
            }

            warn!("Read loop exited — connection lost");
            // When this task ends, `tx` is dropped, which causes `rx.recv()`
            // in the strategy loop to return `None`, signaling shutdown.
        });

        Ok(rx)
    }

    /// Builds the OKX subscription payload for all instruments.
    /// Subscribes to both "trades" and "books5" for each instrument.
    fn build_subscribe_message(&self) -> String {
        let mut args = Vec::new();
        for inst in &self.config.instruments {
            // trades: every executed trade, tick by tick
            args.push(serde_json::json!({
                "channel": "trades",
                "instId": inst
            }));
            // books5: top 5 levels of the order book, pushed on every change
            args.push(serde_json::json!({
                "channel": "books5",
                "instId": inst
            }));
        }
        serde_json::json!({
            "op": "subscribe",
            "args": args
        })
        .to_string()
    }
}

// ============================================================================
// MESSAGE PARSER
// ============================================================================
// Converts raw JSON into typed StreamEvents.
// This is the "membrane" between the messy outside world and your clean
// internal types. If OKX changes their JSON schema, ONLY this code breaks.

fn parse_message(raw: &str) -> Option<StreamEvent> {
    let envelope: WsEnvelope = serde_json::from_str(raw).ok()?;

    // Subscription confirmations: { "event": "subscribe" }
    // Not data — log and skip.
    if let Some(event) = &envelope.event {
        info!("WS event: {}", event);
        return None;
    }

    let arg = envelope.arg.as_ref()?;
    let data = envelope.data.as_ref()?;
    let channel = arg.channel.as_str();

    match channel {
        "trades" => parse_trades(data),
        "books5" => parse_book(data, arg.inst_id.as_deref()?),
        _ => {
            warn!("Unknown channel: {}", channel);
            None
        }
    }
}

fn parse_trades(data: &serde_json::Value) -> Option<StreamEvent> {
    let trades: Vec<TradeData> = serde_json::from_value(data.clone()).ok()?;
    let t = trades.first()?;

    Some(StreamEvent::Trade(TradeUpdate {
        inst_id: t.inst_id.clone(),
        price: t.px.parse().ok()?,
        size: t.sz.parse().ok()?,
        side: t.side.clone(),
        timestamp_ms: t.ts.parse().ok()?,
    }))
}

fn parse_book(data: &serde_json::Value, inst_id: &str) -> Option<StreamEvent> {
    let books: Vec<BookData> = serde_json::from_value(data.clone()).ok()?;
    let b = books.first()?;

    let parse_levels = |levels: &[Vec<String>]| -> Vec<BookLevel> {
        levels
            .iter()
            .filter_map(|level| {
                Some(BookLevel {
                    price: level.first()?.parse().ok()?,
                    size: level.get(1)?.parse().ok()?,
                })
            })
            .collect()
    };

    Some(StreamEvent::Book(OrderBookUpdate {
        inst_id: inst_id.to_string(),
        asks: parse_levels(&b.asks),
        bids: parse_levels(&b.bids),
        timestamp_ms: b.ts.parse().ok()?,
    }))
}
