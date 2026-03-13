// src/main.rs
//
// THE NEW ARCHITECTURE:
// =====================
// Before:  main() → fetch price → place order → exit
// Now:     main() → connect WS → loop forever { react to every market event }
//
// This is the skeleton of an event-driven trading system. Right now, it
// just prints events. In Phase 2, you'll replace the print statements
// with your strategy logic and backtester hooks.

mod config;
mod okx_interface;  // Kept for REST order execution (placing orders is still REST)
mod instrument;
mod order_manager;
mod ws_client;
mod ws_types;

use config::Config;
use ws_client::{WsClient, WsConfig};
use ws_types::StreamEvent;
use tracing::{info, error};

// #[tokio::main] is a macro that:
//   1. Creates a tokio runtime (the async engine)
//   2. Turns your `async fn main()` into a regular fn that blocks on the runtime
//
// Without this, you'd manually write:
//   fn main() { tokio::runtime::Runtime::new().unwrap().block_on(async { ... }) }
//
// The macro does the same thing, just cleaner.
#[tokio::main]
async fn main() {
    // Initialize structured logging. This replaces println! and gives you
    // timestamps, log levels, and filterable output.
    tracing_subscriber::fmt::init();

    info!("=== AUGUR v0.2 — WebSocket Architecture ===");

    // --- 1. Config (unchanged from your v0.1) ---
    let config = Config::load().expect("Failed to load config");
    info!("[1/3] Config loaded. Paper trading: {}", config.is_paper_trading);

    // --- 2. Connect WebSocket ---
    // Define which instruments to stream. Add more here to multiplex.
    let instruments = vec![
        "BTC-USDT-SWAP".to_string(),
        // Uncomment to add more instruments to the same stream:
        // "ETH-USDT-SWAP".to_string(),
    ];

    let ws_config = if config.is_paper_trading {
        WsConfig::paper_trading(instruments)
    } else {
        WsConfig::live(instruments)
    };

    let ws = WsClient::new(ws_config);
    let mut rx = ws.connect().await.expect("WebSocket connection failed");
    info!("[2/3] WebSocket connected — streaming trades + order book");

    // --- 3. Event Loop ---
    // This is your new main loop. It runs forever, processing every event
    // the exchange sends. This is where your strategy will live.
    //
    // `rx.recv().await` pauses until the next event arrives through the
    // channel. When the WS connection drops, recv() returns None and we exit.
    info!("[3/3] Entering event loop — Ctrl+C to exit");

    let mut trade_count: u64 = 0;
    let mut book_count: u64 = 0;

    while let Some(event) = rx.recv().await {
        match event {
            StreamEvent::Trade(trade) => {
                trade_count += 1;
                // Every executed trade on the exchange, in real time.
                // This is your tick-by-tick price feed.
                info!(
                    "[TRADE #{:>6}] {} {} {:.2} @ ${:.2}",
                    trade_count,
                    trade.inst_id,
                    trade.side.to_uppercase(),
                    trade.size,
                    trade.price,
                );

                // -------------------------------------------------------
                // PHASE 2 HOOK: Your strategy logic goes here.
                // Example pseudocode:
                //   instrument.update_from_trade(&trade);
                //   if strategy.should_enter(&instrument) {
                //       order_manager.market_buy(&interface, ...);
                //   }
                // -------------------------------------------------------
            }

            StreamEvent::Book(book) => {
                book_count += 1;
                // Top-of-book update. best_bid and best_ask give you the
                // current spread — the tightest price you can buy/sell at.
                if let (Some(best_bid), Some(best_ask)) = (book.bids.first(), book.asks.first()) {
                    let spread = best_ask.price - best_bid.price;
                    let spread_bps = (spread / best_bid.price) * 10_000.0;

                    // Only log every 50th update to avoid flooding the terminal.
                    // In production, you process EVERY update silently.
                    if book_count % 50 == 0 {
                        info!(
                            "[BOOK  #{:>6}] {} | Bid: ${:.2} ({:.4}) | Ask: ${:.2} ({:.4}) | Spread: {:.1} bps",
                            book_count,
                            book.inst_id,
                            best_bid.price,
                            best_bid.size,
                            best_ask.price,
                            best_ask.size,
                            spread_bps,
                        );
                    }
                }

                // -------------------------------------------------------
                // PHASE 2 HOOK: Order book strategy goes here.
                // This is where your FPGA pipeline will eventually feed:
                //   let filtered_signal = fpga.process(&book);
                //   if filtered_signal.indicates_momentum() { ... }
                // -------------------------------------------------------
            }
        }
    }

    // If we get here, the WebSocket connection dropped.
    error!("Stream ended — connection lost. Total trades: {}, book updates: {}", trade_count, book_count);

    // FUTURE: Implement reconnection logic here. For now, just exit.
    // In production, you'd loop back to step 2 with exponential backoff.
}
