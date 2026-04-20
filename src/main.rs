// src/main.rs
//
// ============================================================================
// AUGUR v0.6 — RISK GATE + ORDER EXECUTION
// ============================================================================
//
// What changed from v0.5:
//
//   v0.5: Event loop logged detected ABCD patterns and their Fibonacci
//         anchors. No trade signals were generated.
//
//   v0.6: Detected patterns flow through the RISK ENGINE, which performs
//         invalidation-based stop placement, R:R gating against Target 3,
//         and position sizing from account equity. Passing signals are
//         submitted to the ORDER MANAGER, which builds an OKX limit order
//         with an attached OCO exit pair (stop + take-profit).
//
// ============================================================================
// SAFETY: TRADING GATE
// ============================================================================
//
// The `trading_enabled` flag in Config defaults to false. When disabled,
// signals are built and logged but NOT submitted to the exchange. This
// lets us observe the signal stream on paper-trading keys before
// committing to automated execution.
//
// To enable: set `AUGUR_TRADING_ENABLED=1` in the `.env` file.
// The OKX `x-simulated-trading: 1` header remains active regardless,
// so enabling trading routes orders to the OKX testnet — not live funds.

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

use config::Config;
use instrument::Instrument;
use candle::Timeframe;
use ws_client::{WsClient, WsConfig};
use ws_types::StreamEvent;
use fibonacci::FibSequence;
use risk::{RiskEngine, RiskParams};
use order_manager::OrderManager;
use okx_interface::OkxInterface;
use tracing::{info, warn, error};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    info!("=== AUGUR v0.6 — Risk Gate + Order Execution ===");

    let config = Config::load().expect("Failed to load config");
    info!(
        "[1/5] Config loaded. Paper trading: {} | Trading enabled: {} | Balance: ${:.2}",
        config.is_paper_trading, config.trading_enabled, config.account_balance,
    );

    // The REST interface is used by the order manager. Cloned config
    // because the main loop still needs to read trading_enabled and
    // account_balance.
    let trading_enabled = config.trading_enabled;
    let account_balance = config.account_balance;
    let interface = OkxInterface::new(Config {
        api_key: config.api_key.clone(),
        secret_key: config.secret_key.clone(),
        passphrase: config.passphrase.clone(),
        is_paper_trading: config.is_paper_trading,
        trading_enabled: config.trading_enabled,
        account_balance: config.account_balance,
    });
    info!("[2/5] REST interface ready");

    let mut btc = Instrument::new("BTC-USDT-SWAP");
    info!("[3/5] Instrument initialized: candles M1/M15/H1/H4, swings H1/H4 (lookback=3)");

    // Risk engine — configured from Config defaults. These can be tuned
    // later via additional .env variables (min_rr, stop_buffer, etc.).
    let risk_engine = RiskEngine::new(RiskParams::fmg_default(account_balance));
    info!(
        "[4/5] Risk engine: risk-per-trade {:.1}% | min R:R 1:{:.1} | stop buffer {:.2}% of range",
        risk_engine.params().risk_per_trade_pct * 100.0,
        risk_engine.params().min_rr_ratio,
        risk_engine.params().stop_buffer_pct * 100.0,
    );

    let instruments = vec!["BTC-USDT-SWAP".to_string()];
    let ws_config = if config.is_paper_trading {
        WsConfig::paper_trading(instruments)
    } else {
        WsConfig::live(instruments)
    };

    let ws = WsClient::new(ws_config);
    let mut rx = ws.connect().await.expect("WebSocket connection failed");
    info!("[5/5] WebSocket connected — entering event loop");

    if !trading_enabled {
        warn!("⚠️  Trading gate is CLOSED — signals will be logged but NOT submitted.");
        warn!("    Set AUGUR_TRADING_ENABLED=1 in .env to enable paper-trading execution.");
    }

    let mut trade_count: u64 = 0;
    let mut book_count: u64 = 0;
    let mut signals_built: u64 = 0;
    let mut signals_submitted: u64 = 0;

    while let Some(event) = rx.recv().await {
        match event {
            StreamEvent::Trade(trade) => {
                trade_count += 1;

                let outcome = btc.update_from_trade(&trade);

                // --- Candle closes ---
                for (timeframe, candle) in &outcome.closed_candles {
                    match timeframe {
                        Timeframe::M1 | Timeframe::M5 => {
                            info!("🕯 [{}] {}", timeframe, candle);
                        }
                        Timeframe::M15 => warn!("🕯🕯 [{}] {}", timeframe, candle),
                        Timeframe::H1  => warn!("🔔 [{}] {}", timeframe, candle),
                        Timeframe::H4 | Timeframe::D1 => error!("🔔🔔 [{}] {}", timeframe, candle),
                    }
                }

                // --- Swings + pattern detection + risk gate + order submission ---
                for (timeframe, swing) in &outcome.new_swings {
                    // Log the swing itself
                    match timeframe {
                        Timeframe::H1 => warn!(
                            "⛳ [{}] SWING {} @ ${:.2}",
                            timeframe, swing.swing_type, swing.price,
                        ),
                        Timeframe::H4 => error!(
                            "⛳⛳ [{}] SWING {} @ ${:.2}",
                            timeframe, swing.swing_type, swing.price,
                        ),
                        _ => {}
                    }

                    // Pattern detection gated on swing detector availability
                    let detector = match btc.swing_detector(*timeframe) {
                        Some(d) => d,
                        None => continue,
                    };

                    // ABCD is the primary signal generator. BRC and combined
                    // patterns are logged for context; only ABCD feeds the
                    // risk engine in v0.6.
                    let pattern = match abc_brc::detect_abcd(detector) {
                        Some(p) => p,
                        None => continue,
                    };

                    let fib = FibSequence::from_pattern(&pattern);

                    warn!("📐 [{}] {}", timeframe, pattern);
                    warn!(
                        "    FIB: Golden ${:.2} | Sniper ${:.2} | T3 ${:.2}",
                        fib.golden_zone(), fib.sniper_zone(), fib.target3(),
                    );

                    // Also log BRC and combined for confluence visibility
                    if let Some(brc) = abc_brc::detect_brc_default(detector) {
                        warn!("🔀 [{}] {}", timeframe, brc);
                    }
                    if let Some(combo) = abc_brc::detect_combined(
                        detector,
                        abc_brc::DEFAULT_BRC_RETEST_RATIO,
                    ) {
                        error!(
                            "🎯🎯 [{}] HIGH-CONFLUENCE {} | ABCD retrace {:.1}% | BRC accuracy {:.1}%",
                            timeframe,
                            combo.direction(),
                            combo.abcd.c_retracement() * 100.0,
                            combo.brc.retest_accuracy() * 100.0,
                        );
                    }

                    // =====================================================
                    // RISK GATE
                    // =====================================================
                    // Evaluate both entry zones. Golden is the primary;
                    // Sniper is the add-on. In v0.6 we emit both signals
                    // whenever they pass — Phase 2E will add position
                    // tracking so Sniper only fires when a Golden is
                    // already active (compounding discipline).

                    match risk_engine.evaluate_golden(&pattern, &fib, *timeframe) {
                        Ok(signal) => {
                            signals_built += 1;
                            info!("✅ GOLDEN signal #{}: {}", signals_built, signal);
                            submit_if_enabled(
                                &interface,
                                &btc.symbol,
                                &signal,
                                trading_enabled,
                                &mut signals_submitted,
                                trade.timestamp_ms,
                            );
                        }
                        Err(reason) => {
                            info!("⛔ GOLDEN rejected: {}", reason);
                        }
                    }

                    match risk_engine.evaluate_sniper(&pattern, &fib, *timeframe) {
                        Ok(signal) => {
                            signals_built += 1;
                            info!("✅ SNIPER signal #{}: {}", signals_built, signal);
                            submit_if_enabled(
                                &interface,
                                &btc.symbol,
                                &signal,
                                trading_enabled,
                                &mut signals_submitted,
                                trade.timestamp_ms,
                            );
                        }
                        Err(reason) => {
                            info!("⛔ SNIPER rejected: {}", reason);
                        }
                    }
                }

                // --- Periodic status ---
                if trade_count % 1000 == 0 {
                    info!(
                        "[STATUS] Trades: {} | Price: ${:.2} | Signals built/submitted: {}/{} | Candles: [{}] | Swings: [{}]",
                        trade_count,
                        btc.last_price,
                        signals_built, signals_submitted,
                        btc.candle_status(),
                        btc.swing_status(),
                    );
                }
            }

            StreamEvent::Book(book) => {
                book_count += 1;
                btc.update_from_book(&book);

                if book_count % 500 == 0 {
                    info!(
                        "[BOOK #{:>6}] Spread: {:.1} bps | Bid: ${:.2} | Ask: ${:.2}",
                        book_count,
                        btc.book.spread_bps,
                        btc.book.best_bid.as_ref().map_or(0.0, |b| b.price),
                        btc.book.best_ask.as_ref().map_or(0.0, |a| a.price),
                    );
                }
            }
        }
    }

    error!(
        "Stream ended. Trades: {}, Books: {}, Signals built: {}, Submitted: {}",
        trade_count, book_count, signals_built, signals_submitted,
    );
}

/// Submit a signal to the order manager if the trading gate is open.
/// When closed, this is a no-op aside from a logged notification.
fn submit_if_enabled(
    interface: &OkxInterface,
    symbol: &str,
    signal: &risk::TradeSignal,
    trading_enabled: bool,
    submitted_counter: &mut u64,
    now_ms: u64,
) {
    if !trading_enabled {
        info!("    (trading gate closed — signal not submitted)");
        return;
    }

    // Build a unique client order ID so we can correlate logs with
    // exchange records. Format: augur_<TF>_<ZONE>_<timestamp>.
    let client_ord_id = format!(
        "augur_{}_{}_{}",
        signal.timeframe, signal.entry_zone, now_ms,
    );

    match OrderManager::submit_signal(interface, symbol, signal, &client_ord_id) {
        Ok(submission) => {
            *submitted_counter += 1;
            info!(
                "    ➤ SUBMITTED ordId={} clOrdId={}",
                submission.entry_ord_id, submission.client_ord_id,
            );
        }
        Err(e) => {
            error!("    ✗ SUBMISSION FAILED: {}", e);
        }
    }
}
