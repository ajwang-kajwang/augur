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

// v0.10: modules moved to lib.rs so the backtest binary can share them.
use augur::{
    config, instrument, candle, ws_client, ws_types,
    fibonacci, risk, order_manager, okx_interface,
    position, private_ws, reconnect, persistence, abc_brc,
    reconciliation, health,
};

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, watch};
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
use persistence::{PersistenceConfig, TickRecord, book_record_from_update};
use health::HealthConfig;
use tracing::{info, warn, error};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    info!("=== AUGUR v0.12 — Reconciliation + 4-Week Hardening ===");

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
    let persistence_enabled = config.persistence_enabled;
    let persistence_path = config.persistence_path.clone();
    let instruments = vec!["BTC-USDT-SWAP".to_string()];

    // ========================================================================
    // PERSISTENCE CHANNEL + TASK (v0.8)
    // ========================================================================
    // Bounded channels — if a writer ever stalls (SSD failure, disk full),
    // records drop rather than memory growing unboundedly. Both writers
    // share infrastructure but run as independent tasks: a book-writer
    // stall does not affect trade recording and vice versa.
    //
    // v0.9 adds the book channel alongside the existing tick channel.
    // Phase 4 DSP work on order-flow imbalance, book pressure, and
    // Kalman-filtered mid-price all require per-tick book snapshots,
    // which trades alone don't carry.
    let persistence_handles = if persistence_enabled {
        let pcfg = PersistenceConfig::from_env(&persistence_path);
        let handles = persistence::spawn_all(pcfg);
        if handles.is_some() {
            info!("[persistence] enabled — trades AND books will be recorded to {}", persistence_path);
        }
        handles
    } else {
        warn!("[persistence] DISABLED — no tick or book data will be recorded.");
        warn!("            Phase 3 backtesting requires this to be enabled.");
        None
    };
    // Split the handles so each producer side can be cloned/moved independently.
    let (tick_tx, book_tx, persistence_tasks) = match persistence_handles {
        Some(h) => (Some(h.tick_tx), Some(h.book_tx), Some((h.tick_task, h.book_task))),
        None => (None, None, None),
    };

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

    // ========================================================================
    // BACKGROUND TASKS — Phase 2F + 4-Week Hardening (v0.12)
    // ========================================================================
    // Two long-running background tasks share a single shutdown signal.
    // Both terminate cleanly when SIGINT triggers the watch channel
    // change, ensuring journalctl shows orderly task exits rather than
    // abrupt aborts.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // --- Reconciliation task (always spawned, regardless of trading_enabled) ---
    // Even in dry-run, periodic reconciliation is harmless — the REST
    // call returns recent orders, and if Positions has nothing tracked
    // for those clOrdIds (which it won't, in dry-run), they're silently
    // skipped as foreign. The only cost is one /trade/orders-history
    // hit every 5 minutes. We run it in dry-run so the wiring is
    // exercised continuously and any auth/signing bugs surface early
    // rather than at first live deployment.
    let reconciliation_task = {
        let interface = Arc::clone(&interface);
        let positions = Arc::clone(&positions);
        let mut shutdown = shutdown_rx.clone();
        let interval_secs = std::env::var("AUGUR_RECONCILE_INTERVAL_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(reconciliation::DEFAULT_PERIODIC_INTERVAL_SECS);
        info!(
            "[reconcile] periodic reconciliation enabled — every {}s against {}",
            interval_secs, "BTC-USDT-SWAP",
        );
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(interval_secs));
            // Skip the immediate first tick so we don't reconcile before
            // the WS connection has had a chance to come up.
            tick.tick().await;
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        let stats = reconciliation::reconcile(
                            &interface, &positions,
                            &["BTC-USDT-SWAP"], "SWAP",
                        ).await;
                        if stats.errors > 0 {
                            warn!(
                                "[reconcile] pass had {} errors (api_calls={}, orders_seen={})",
                                stats.errors, stats.api_calls, stats.orders_seen,
                            );
                        }
                    }
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() {
                            info!("[reconcile] task stopping on shutdown signal");
                            break;
                        }
                    }
                }
            }
        })
    };

    // --- Health monitor (spawned only when persistence is enabled) ---
    // The monitor writes a canary file each pass to verify the SSD mount
    // is still writable. If persistence is disabled, the mount path may
    // not exist at all, so we skip the monitor entirely.
    let health_task = if persistence_enabled {
        let cfg = HealthConfig::from_env(std::path::PathBuf::from(&persistence_path));
        let shutdown = shutdown_rx.clone();
        Some(tokio::spawn(health::run(cfg, shutdown)))
    } else {
        None
    };

    info!("[6/6] Entering main event loop");

    let mut trade_count: u64 = 0;
    let mut book_count: u64 = 0;
    let mut signals_built: u64 = 0;
    let mut signals_submitted: u64 = 0;
    let mut sniper_gated_off: u64 = 0;
    let mut ticks_dropped: u64 = 0;
    let mut books_dropped: u64 = 0;

    // ========================================================================
    // OUTER RECONNECT LOOP + GRACEFUL SHUTDOWN (v0.8)
    // ========================================================================
    // `tokio::select!` races the trading loop against Ctrl+C. On SIGINT,
    // drop the tick_tx (closing the persistence channel) and await the
    // writer's final flush before exiting. This prevents the last buffer
    // of ticks from being lost and avoids corrupting the Parquet file.
    let trading_loop = async {
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

                    // --- Record to persistence (v0.8) ---
                    // try_send is non-blocking: if the writer is behind,
                    // drop the tick and log. Dropping in the hot path is
                    // preferable to stalling the event loop.
                    if let Some(tx) = &tick_tx {
                        let record = TickRecord {
                            timestamp_ms: trade.timestamp_ms,
                            price: trade.price,
                            size: trade.size,
                            side: trade.side.clone(),
                            inst_id: trade.inst_id.clone(),
                        };
                        if let Err(e) = tx.try_send(record) {
                            ticks_dropped += 1;
                            if ticks_dropped % 100 == 1 {
                                warn!("[persistence] dropped tick #{}: {}", ticks_dropped, e);
                            }
                        }
                    }

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
                            "[STATUS] Trades: {} | Price: ${:.2} | Signals built/submitted: {}/{} | Sniper gated: {} | Ticks/books dropped: {}/{} | Positions: {} | Candles: [{}]",
                            trade_count, btc.last_price,
                            signals_built, signals_submitted, sniper_gated_off,
                            ticks_dropped, books_dropped,
                            pos_summary, btc.candle_status(),
                        );
                    }
                }

                StreamEvent::Book(book) => {
                    book_count += 1;

                    // --- Record book snapshot to persistence (v0.9) ---
                    // Same try_send pattern as ticks — non-blocking drop
                    // if the book writer ever falls behind. Book updates
                    // arrive far more frequently than trades on liquid
                    // pairs (every microstructure change, not just fills).
                    if let Some(tx) = &book_tx {
                        let record = book_record_from_update(&book);
                        if tx.try_send(record).is_err() {
                            books_dropped += 1;
                            if books_dropped % 500 == 1 {
                                warn!("[persistence] dropped book snapshot #{}", books_dropped);
                            }
                        }
                    }

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

        // v0.12 Phase 2F: fire a reconciliation pass on disconnect.
        // The most likely cause of any local/exchange state drift is
        // the disconnect window we just experienced — events fired on
        // OKX during the gap won't be replayed by the WS. We reconcile
        // BEFORE the outer loop reconnects so by the time the next
        // tick arrives, Positions matches exchange truth.
        //
        // This runs even in dry-run; it's idempotent and exercises the
        // wiring continuously, surfacing any auth/signing bugs early.
        info!("[reconcile] post-disconnect pass");
        let post_stats = reconciliation::reconcile(
            &interface, &positions,
            &["BTC-USDT-SWAP"], "SWAP",
        ).await;
        if post_stats.state_changes > 0 {
            warn!(
                "[reconcile] post-disconnect: {} state changes — exchange events were missed during the gap",
                post_stats.state_changes,
            );
        }
    }
    };  // end async block

    // Race the trading loop against SIGINT.
    tokio::select! {
        _ = trading_loop => {
            // Unreachable: the inner loop is infinite.
            error!("Trading loop returned unexpectedly");
        }
        _ = tokio::signal::ctrl_c() => {
            info!("🛑 SIGINT received — beginning graceful shutdown");
        }
    }

    // Shutdown sequence.
    info!("Signaling background tasks to stop");
    let _ = shutdown_tx.send(true);

    info!("Dropping persistence channels — writers will final-flush");
    drop(tick_tx);
    drop(book_tx);

    // Await persistence writers (must finish flushing buffered ticks/books).
    if let Some((tick_task, book_task)) = persistence_tasks {
        let deadline = std::time::Duration::from_secs(30);
        match tokio::time::timeout(deadline, async {
            let (t, b) = tokio::join!(tick_task, book_task);
            (t, b)
        }).await {
            Ok((Ok(()), Ok(()))) => info!("Both persistence tasks shut down cleanly"),
            Ok((t, b)) => {
                if let Err(e) = t { error!("Tick writer panicked: {}", e); }
                if let Err(e) = b { error!("Book writer panicked: {}", e); }
            }
            Err(_) => error!("Persistence tasks did not finish within 30s — some records may be lost"),
        }
    }

    // Await background monitor tasks. These have lighter shutdown work
    // (no buffered state to flush) so a 5s timeout is generous.
    let monitor_deadline = std::time::Duration::from_secs(5);
    if let Err(_) = tokio::time::timeout(monitor_deadline, reconciliation_task).await {
        warn!("Reconciliation task did not finish within 5s");
    }
    if let Some(task) = health_task {
        if let Err(_) = tokio::time::timeout(monitor_deadline, task).await {
            warn!("Health monitor did not finish within 5s");
        }
    }

    info!(
        "Shutdown complete. Final stats: trades={} books={} signals built/submitted={}/{} ticks/books dropped={}/{}",
        trade_count, book_count, signals_built, signals_submitted,
        ticks_dropped, books_dropped,
    );
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
