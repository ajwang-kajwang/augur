// src/main.rs
//
// ============================================================================
// AUGUR v0.3 — CANDLE AGGREGATION
// ============================================================================
//
// What changed from v0.2:
//
//   v0.2: Event loop printed raw trades and book updates. The "Phase 2 hooks"
//         were pseudocode comments showing where strategy logic would go.
//
//   v0.3: The hooks are now real code. Every trade tick flows into the
//         Instrument, which fans it out to four timeframe aggregators
//         simultaneously. When a candle closes, the event loop logs it
//         and (soon) will route it to strategy modules.
//
// The event loop is still the SINGLE CONSUMER of the WebSocket stream.
// All processing happens synchronously within the loop body. This keeps
// the architecture simple — no shared state, no locks, no data races.
//
// WHAT HAPPENS ON EACH TRADE:
// ===========================
//   1. Trade arrives from WS via mpsc channel
//   2. instrument.update_from_trade() is called
//   3. Inside, the trade is fed to all 4 CandleAggregators (M1, M15, H1, H4)
//   4. If any aggregator crosses a period boundary, it returns the closed candle
//   5. The event loop logs the close and (Phase 2B+) routes to strategy modules
//
// Most ticks produce no candle closes (empty Vec). At minute boundaries,
// the M1 aggregator fires. At 15-minute marks, M1 + M15 fire together.
// At the top of the hour, M1 + M15 + H1 all fire. Every 4 hours, all four.

mod config;
mod okx_interface;
mod instrument;
mod order_manager;
mod ws_client;
mod ws_types;
mod candle;

use config::Config;
use instrument::Instrument;
use candle::Timeframe;
use ws_client::{WsClient, WsConfig};
use ws_types::StreamEvent;
use tracing::{info, warn, error};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    info!("=== AUGUR v0.3 — Candle Aggregation Engine ===");

    // --- 1. Config ---
    let config = Config::load().expect("Failed to load config");
    info!("[1/4] Config loaded. Paper trading: {}", config.is_paper_trading);

    // --- 2. Initialize Instrument ---
    // The Instrument now holds multi-timeframe candle state.
    // Default setup: M1 (500), M15 (200), H1 (168), H4 (180).
    let mut btc = Instrument::new("BTC-USDT-SWAP");
    info!("[2/4] Instrument initialized with candle aggregators: M1, M15, H1, H4");

    // --- 3. Connect WebSocket ---
    let instruments = vec!["BTC-USDT-SWAP".to_string()];

    let ws_config = if config.is_paper_trading {
        WsConfig::paper_trading(instruments)
    } else {
        WsConfig::live(instruments)
    };

    let ws = WsClient::new(ws_config);
    let mut rx = ws.connect().await.expect("WebSocket connection failed");
    info!("[3/4] WebSocket connected — streaming trades + order book");

    // --- 4. Event Loop ---
    info!("[4/4] Entering event loop — building candles from live trades");
    info!("       Candle closes will be logged as they occur.");
    info!("       Periodic status every 1000 trades.");

    let mut trade_count: u64 = 0;
    let mut book_count: u64 = 0;

    while let Some(event) = rx.recv().await {
        match event {
            StreamEvent::Trade(trade) => {
                trade_count += 1;

                // ==========================================================
                // THE v0.3 CORE: Feed the trade into the Instrument.
                // This updates last_price AND all candle aggregators.
                // ==========================================================
                let closed_candles = btc.update_from_trade(&trade);

                // --- Log candle closes ---
                // This is where strategy modules will eventually plug in.
                // For now, we log every close so you can watch candles form
                // in real time and verify the aggregation is correct.
                for (timeframe, candle) in &closed_candles {
                    // Use different log levels by timeframe importance.
                    // M1 closes are frequent (every 60s), so they're info.
                    // Higher timeframes are rarer and more significant.
                    match timeframe {
                        Timeframe::M1 => {
                            info!(
                                "🕯 [{}] CANDLE CLOSE | {}",
                                timeframe, candle
                            );
                        }
                        Timeframe::M5 => {
                            info!(
                                "🕯🕯 [{}] CANDLE CLOSE | {}",
                                timeframe, candle
                            );
                        }
                        Timeframe::M15 => {
                            warn!(
                                "🕯🕯🕯 [{}] CANDLE CLOSE | {}",
                                timeframe, candle
                            );
                        }
                        Timeframe::H1 => {
                            warn!(
                                "🔔 [{}] CANDLE CLOSE | {}",
                                timeframe, candle
                            );
                        }
                        Timeframe::H4 | Timeframe::D1 => {
                            // H4 and D1 closes are significant structural events.
                            error!(
                                "🔔🔔 [{}] CANDLE CLOSE | {}",
                                timeframe, candle
                            );
                        }
                    }

                    // ---------------------------------------------------------
                    // PHASE 2B HOOK: Strategy signal generation goes here.
                    //
                    // When the swing detector is built, this becomes:
                    //
                    //   if let Some(swing) = swing_detector.update(timeframe, &candle) {
                    //       if let Some(signal) = abc_brc.evaluate(&btc, &swing) {
                    //           order_manager.submit_signal(&signal);
                    //       }
                    //   }
                    //
                    // For the DSP pipeline, M1 closes feed the FFT buffer:
                    //
                    //   if *timeframe == Timeframe::M1 {
                    //       dsp_engine.ingest_candle(&candle);
                    //   }
                    // ---------------------------------------------------------
                }

                // --- Periodic status logging ---
                // Every 1000 trades, print a summary of candle aggregator state.
                // This lets you monitor warmup progress without flooding the log.
                if trade_count % 1000 == 0 {
                    info!(
                        "[STATUS] Trades: {} | Price: ${:.2} | Candles: [{}]",
                        trade_count,
                        btc.last_price,
                        btc.candle_status(),
                    );

                    // Show current in-progress candles if available
                    if let Some(m1) = btc.current_candle(Timeframe::M1) {
                        info!(
                            "  M1 building: O:{:.2} H:{:.2} L:{:.2} C:{:.2} ({}t)",
                            m1.open, m1.high, m1.low, m1.close, m1.trade_count
                        );
                    }

                    // Report warmup status for strategy-critical timeframes
                    let h1_ready = btc.is_warmed_up(Timeframe::H1, 20);
                    let h4_ready = btc.is_warmed_up(Timeframe::H4, 10);
                    if !h1_ready || !h4_ready {
                        info!(
                            "  Warmup: H1 {}/20 | H4 {}/10",
                            btc.candle_agg(Timeframe::H1)
                                .map_or(0, |a| a.history_len()),
                            btc.candle_agg(Timeframe::H4)
                                .map_or(0, |a| a.history_len()),
                        );
                    } else {
                        info!("  All strategy timeframes warmed up ✓");
                    }
                }
            }

            StreamEvent::Book(book) => {
                book_count += 1;

                // Update the Instrument's top-of-book state.
                btc.update_from_book(&book);

                // Log spread periodically — useful for monitoring execution
                // conditions and detecting unusual market microstructure.
                if book_count % 500 == 0 {
                    info!(
                        "[BOOK #{:>6}] Spread: {:.1} bps | Bid: ${:.2} | Ask: ${:.2}",
                        book_count,
                        btc.book.spread_bps,
                        btc.book.best_bid.as_ref().map_or(0.0, |b| b.price),
                        btc.book.best_ask.as_ref().map_or(0.0, |a| a.price),
                    );
                }

                // ---------------------------------------------------------
                // PHASE 2 HOOK: Order book strategy / DSP pipeline.
                //
                // The top-of-book is now always available at btc.book.
                // The FPGA/DSP pipeline will read this directly:
                //
                //   dsp_engine.ingest_book(&btc.book);
                // ---------------------------------------------------------
            }
        }
    }

    error!(
        "Stream ended — connection lost. Trades: {}, Book updates: {}, Candles produced: [{}]",
        trade_count,
        book_count,
        btc.candle_status(),
    );
}
