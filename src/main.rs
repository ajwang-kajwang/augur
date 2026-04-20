// src/main.rs
//
// ============================================================================
// AUGUR v0.7 — POSITION TRACKING + COMPOUNDING DISCIPLINE
// ============================================================================
//
// What changed from v0.6:
//
//   v0.6: Every pattern produced independent Golden and Sniper signals,
//         both submitted in parallel. This violated the FMG compounding
//         discipline — the Sniper is a COMPOUNDING ADD-ON on top of an
//         already-filled Golden, not an independent entry.
//
//   v0.7: Three additions:
//
//   1. position.rs — tracks every signal from submission → fill → exit
//      via a state machine keyed by clOrdId. Provides the gates that
//      main.rs consults before submitting.
//
//   2. private_ws.rs — authenticated OKX private WebSocket streaming
//      `orders` and `positions` channel events. Drives state transitions
//      in the Positions registry. Has its own reconnect loop.
//
//   3. Unified reconnection — the public WS now reconnects with
//      exponential backoff (1s → 2s → ... → 30s cap) and the event
//      loop is wrapped so that a mid-stream disconnect triggers a
//      clean reconnect rather than terminating the process.
//
// ============================================================================
// SHARED STATE
// ============================================================================
//
// `Positions` is shared between the main event loop (reader) and the
// private WS task (writer) via `Arc<tokio::sync::Mutex<Positions>>`.
// Locks are held for microseconds, never across .await points.
// `Instrument` stays single-owned on the main task.

mod config;
mod okx_interface;
mod instrument;
mod order_manager;
mod ws_client;
mod ws_types;
mod candle;
mod swing;
mod abc_brc;
mod fibonacci;
mod risk;
mod position;
mod private_ws;
mod reconnect;

use std::sync::Arc;
use tokio::sync::Mutex;
use config::Config;
use instrument::Instrument;
use candle::Timeframe;
use ws_client::WsConfig;
use ws_types::StreamEvent;
use fibonacci::FibSequence;
use risk::{RiskEngine, RiskParams, EntryZone};
use order_manager::OrderManager;
use okx_interface::OkxInterface;
use position::Positions;
use private_ws::PrivateWsConfig;
use tracing::{info, warn, error};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    info!("=== AUGUR v0.7 — Position Tracking + Compounding Discipline ===");

    let config = Config::load().expect("Failed to load config");
    info!(
        "[1/6] Config loaded. Paper trading: {} | Trading enabled: {} | Balance: ${:.2}",
        config.is_paper_trading, config.trading_enabled, config.account_balance,
    );

    // Pull fields needed across the loop before we move `config` into
    // OkxInterface (which takes ownership).
    let trading_enabled = config.trading_enabled;
    let account_balance = config.account_balance;
    let is_paper = config.is_paper_trading;
    let instruments = vec!["BTC-USDT-SWAP".to_string()];

    // Clone creds for the private WS BEFORE handing config off to the
    // REST interface.
    let private_ws_config = PrivateWsConfig::from_app_config(&config, instruments.clone());

    let interface = Arc::new(OkxInterface::new(config));
    info!("[2/6] REST interface ready");

    let mut btc = Instrument::new("BTC-USDT-SWAP");
    info!("[3/6] Instrument initialized: candles M1/M15/H1/H4, swings H1/H4 (lookback=3)");

    let risk_engine = RiskEngine::new(RiskParams::fmg_default(account_balance));
    info!(
        "[4/6] Risk engine: risk-per-trade {:.1}% | min R:R 1:{:.1} | stop buffer {:.2}% of range",
        risk_engine.params().risk_per_trade_pct * 100.0,
        risk_engine.params().min_rr_ratio,
        risk_engine.params().stop_buffer_pct * 100.0,
    );

    // Positions registry — shared between main and private WS task.
    let positions = Arc::new(Mutex::new(Positions::new()));

    // Private WS — only spawned if trading is enabled. Without it, the
    // Sniper compounding gate will stay closed (no order events arrive
    // to transition entries from Pending → Open), which fails safe.
    if trading_enabled {
        let positions_clone = Arc::clone(&positions);
        tokio::spawn(async move {
            private_ws::run(private_ws_config, positions_clone).await;
        });
        info!("[5/6] Private WS task spawned — order/position events will flow");
    } else {
        warn!("[5/6] Trading gate CLOSED — private WS not started.");
        warn!("      Sniper compounding gate will remain closed.");
        warn!("      Set AUGUR_TRADING_ENABLED=1 in .env to enable paper execution.");
    }

    info!("[6/6] Entering main event loop");

    let mut trade_count: u64 = 0;
    let mut book_count: u64 = 0;
    let mut signals_built: u64 = 0;
    let mut signals_submitted: u64 = 0;
    let mut sniper_gated_off: u64 = 0;

    // ========================================================================
    // OUTER RECONNECT LOOP
    // ========================================================================
    // When the public WS drops mid-stream, `rx.recv()` returns None and
    // the inner loop exits. We then reconnect with exponential backoff.
    // `Instrument` and all counters survive across reconnects.
    loop {
        let make_config = || {
            if is_paper {
                WsConfig::paper_trading(instruments.clone())
            } else {
                WsConfig::live(instruments.clone())
            }
        };
        let mut rx = reconnect::connect_with_backoff(make_config).await;

        while let Some(event) = rx.recv().await {
            match event {
                StreamEvent::Trade(trade) => {
                    trade_count += 1;
                    let outcome = btc.update_from_trade(&trade);

                    // Candle close logs
                    for (timeframe, candle) in &outcome.closed_candles {
                        match timeframe {
                            Timeframe::M1 | Timeframe::M5 => info!("🕯 [{}] {}", timeframe, candle),
                            Timeframe::M15 => warn!("🕯🕯 [{}] {}", timeframe, candle),
                            Timeframe::H1  => warn!("🔔 [{}] {}", timeframe, candle),
                            Timeframe::H4 | Timeframe::D1 => error!("🔔🔔 [{}] {}", timeframe, candle),
                        }
                    }

                    // Swings → patterns → risk → submission
                    for (timeframe, swing) in &outcome.new_swings {
                        match timeframe {
                            Timeframe::H1 => warn!("⛳ [{}] SWING {} @ ${:.2}",
                                timeframe, swing.swing_type, swing.price),
                            Timeframe::H4 => error!("⛳⛳ [{}] SWING {} @ ${:.2}",
                                timeframe, swing.swing_type, swing.price),
                            _ => {}
                        }

                        let detector = match btc.swing_detector(*timeframe) {
                            Some(d) => d,
                            None => continue,
                        };
                        let pattern = match abc_brc::detect_abcd(detector) {
                            Some(p) => p,
                            None => continue,
                        };
                        let fib = FibSequence::from_pattern(&pattern);

                        warn!("📐 [{}] {}", timeframe, pattern);
                        warn!("    FIB: Golden ${:.2} | Sniper ${:.2} | T3 ${:.2}",
                            fib.golden_zone(), fib.sniper_zone(), fib.target3());

                        if let Some(brc) = abc_brc::detect_brc_default(detector) {
                            warn!("🔀 [{}] {}", timeframe, brc);
                        }
                        if let Some(combo) = abc_brc::detect_combined(
                            detector, abc_brc::DEFAULT_BRC_RETEST_RATIO,
                        ) {
                            error!(
                                "🎯🎯 [{}] HIGH-CONFLUENCE {} | ABCD retrace {:.1}% | BRC accuracy {:.1}%",
                                timeframe, combo.direction(),
                                combo.abcd.c_retracement() * 100.0,
                                combo.brc.retest_accuracy() * 100.0,
                            );
                        }

                        // =========================================================
                        // GOLDEN ENTRY — primary
                        // =========================================================
                        // Gate 1: don't stack on an existing pending/open Golden.
                        let golden_already_active = positions.lock().await
                            .has_active_entry(*timeframe, pattern.direction, EntryZone::Golden);

                        if golden_already_active {
                            info!("⛔ GOLDEN skipped: active entry already tracked for ({}, {})",
                                timeframe, pattern.direction);
                        } else {
                            match risk_engine.evaluate_golden(&pattern, &fib, *timeframe) {
                                Ok(signal) => {
                                    signals_built += 1;
                                    info!("✅ GOLDEN signal #{}: {}", signals_built, signal);
                                    submit_and_record(
                                        &interface, &btc.symbol, &signal,
                                        &positions, trading_enabled,
                                        &mut signals_submitted, trade.timestamp_ms,
                                    ).await;
                                }
                                Err(reason) => info!("⛔ GOLDEN rejected: {}", reason),
                            }
                        }

                        // =========================================================
                        // SNIPER ADD-ON — compounding
                        // =========================================================
                        // Gate 1: don't stack on an existing Sniper.
                        // Gate 2: ONLY fire if a Golden is already OPEN (filled)
                        // for the same (timeframe, direction). This is the FMG
                        // compounding discipline.
                        let sniper_already_active = positions.lock().await
                            .has_active_entry(*timeframe, pattern.direction, EntryZone::Sniper);
                        let golden_is_open = positions.lock().await
                            .has_open_golden(*timeframe, pattern.direction);

                        if sniper_already_active {
                            info!("⛔ SNIPER skipped: already active for ({}, {})",
                                timeframe, pattern.direction);
                        } else if !golden_is_open {
                            sniper_gated_off += 1;
                            info!("⛔ SNIPER gated off: no open Golden for ({}, {}) — FMG compounding rule",
                                timeframe, pattern.direction);
                        } else {
                            match risk_engine.evaluate_sniper(&pattern, &fib, *timeframe) {
                                Ok(signal) => {
                                    signals_built += 1;
                                    info!("✅ SNIPER signal #{}: {}", signals_built, signal);
                                    submit_and_record(
                                        &interface, &btc.symbol, &signal,
                                        &positions, trading_enabled,
                                        &mut signals_submitted, trade.timestamp_ms,
                                    ).await;
                                }
                                Err(reason) => info!("⛔ SNIPER rejected: {}", reason),
                            }
                        }
                    }

                    if trade_count % 1000 == 0 {
                        let pos_summary = positions.lock().await.summary();
                        info!(
                            "[STATUS] Trades: {} | Price: ${:.2} | Signals built/submitted: {}/{} | Sniper gated: {} | Positions: {} | Candles: [{}]",
                            trade_count, btc.last_price,
                            signals_built, signals_submitted, sniper_gated_off,
                            pos_summary, btc.candle_status(),
                        );
                    }
                }

                StreamEvent::Book(book) => {
                    book_count += 1;
                    btc.update_from_book(&book);
                    if book_count % 500 == 0 {
                        info!(
                            "[BOOK #{:>6}] Spread: {:.1} bps | Bid: ${:.2} | Ask: ${:.2}",
                            book_count, btc.book.spread_bps,
                            btc.book.best_bid.as_ref().map_or(0.0, |b| b.price),
                            btc.book.best_ask.as_ref().map_or(0.0, |a| a.price),
                        );
                    }
                }
            }
        }

        // rx.recv() returned None → connection dropped. Outer loop reconnects.
        error!(
            "[public] stream ended — reconnecting. Stats: trades={} books={} signals={}/{} gated={}",
            trade_count, book_count, signals_built, signals_submitted, sniper_gated_off,
        );
    }
}

/// Submit a signal (if trading is enabled) and record it in the Positions
/// registry so compounding gates work on subsequent patterns.
async fn submit_and_record(
    interface: &OkxInterface,
    symbol: &str,
    signal: &risk::TradeSignal,
    positions: &Arc<Mutex<Positions>>,
    trading_enabled: bool,
    submitted_counter: &mut u64,
    now_ms: u64,
) {
    let client_ord_id = format!(
        "augur_{}_{}_{}",
        signal.timeframe, signal.entry_zone, now_ms,
    );

    if !trading_enabled {
        // Dry run: record as unsubmitted so "don't double-submit" gates
        // still fire, but the Sniper gate never opens (no fills arrive).
        positions.lock().await.record_unsubmitted(signal, client_ord_id.clone(), now_ms);
        info!("    (trading gate closed — signal recorded as unsubmitted: {})", client_ord_id);
        return;
    }

    match OrderManager::submit_signal(interface, symbol, signal, &client_ord_id).await {
        Ok(submission) => {
            *submitted_counter += 1;
            positions.lock().await.record_submission(
                signal,
                submission.client_ord_id.clone(),
                submission.entry_ord_id.clone(),
                now_ms,
            );
            info!(
                "    ➤ SUBMITTED ordId={} clOrdId={}",
                submission.entry_ord_id, submission.client_ord_id,
            );
        }
        Err(e) => error!("    ✗ SUBMISSION FAILED: {}", e),
    }
}
