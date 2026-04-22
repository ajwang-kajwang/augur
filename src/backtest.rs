// src/backtest.rs
//
// ============================================================================
// WHY THIS MODULE EXISTS — PHASE 3A + 3B
// ============================================================================
//
// The whole architecture from v0.3 onwards was built so that the backtest
// and live systems share strategy code. The candle engine is deterministic
// (UTC-aligned boundaries). The swing detector is stateless given a candle
// history. Pattern recognizers and the Fibonacci ladder are pure
// functions. The risk engine is pure. The only difference between live
// and backtest is the DATA SOURCE and the EXECUTION PATH:
//
//   Live:      WebSocket trades → Instrument → signals → OrderManager → OKX
//   Backtest:  Parquet trades   → Instrument → signals → FillSim → Portfolio
//
// This module is the FillSim + Portfolio + metrics glue. Everything else
// (Instrument, pattern detection, risk gating) is shared verbatim with
// the live binary.
//
// ============================================================================
// TWO FILL MODES — v0.11
// ============================================================================
//
// Phase 3A shipped OPTIMISTIC fills:
//
//   Bullish Golden: fills when any trade price <= entry_price
//   Bearish Golden: fills when any trade price >= entry_price
//
//   Stop/TP use the same directional-crossing logic. Zero slippage,
//   zero partial fills, zero book consultation.
//
// Phase 3B adds LADDER-WALK fills against recorded book snapshots:
//
//   The portfolio tracks the most recent top-5 book snapshot. When a
//   limit entry triggers (tick crosses limit price), we walk the
//   OPPOSITE SIDE of the book from best to worst, filling size at each
//   level up to the limit price. Remaining size stays pending. Fill
//   price is the volume-weighted average of consumed levels.
//
//   Stops and take-profits become market orders at trigger (matching
//   the v0.6 OCO config where tpOrdPx/slOrdPx = "-1"). They walk the
//   opposite side with NO price limit, consuming depth aggressively
//   until filled. If the top-5 doesn't carry enough depth, the fill
//   price degrades to the deepest visible level — honest reporting
//   of slippage rather than an assumption of infinite liquidity.
//
// WHEN TO USE WHICH MODE:
//
//   Optimistic: upper-bound strategy validation. Answers "does pattern
//               detection fire on real tape + do R multiples cluster
//               positive?" Fast because book data doesn't load.
//
//   Ladder:     realistic cost accounting. Answers "what Sharpe survives
//               after realistic slippage?" Requires recorded book corpus
//               (v0.9+ persistence). Slower due to book file reads and
//               merge-sort bookkeeping.
//
// Optimistic is the baseline; ladder is the reality check. Expect
// ladder Sharpe to be 60-80% of optimistic Sharpe on the same data.
//
// ============================================================================
// COMPOUNDING IN BACKTEST
// ============================================================================
//
// Same rules as live: Sniper only fires when a Golden is already OPEN
// for the same (timeframe, direction). BacktestPortfolio exposes
// `has_open_golden()` and `has_active_entry()` matching the Positions
// API so main.rs's gating logic translates verbatim.
//
// In ladder mode, "OPEN" means entry is FULLY filled. A partially-filled
// Golden keeps the Sniper gate closed — conservative, matches the live
// semantics from position.rs (partial_filled transitions to Pending, not
// Open).
//
// ============================================================================
// TIMESTAMP ORDER
// ============================================================================
//
// Trade and book Parquet files each carry timestamps. Within a file,
// rows are in write order = timestamp order. Across files, filename
// sort order (YYYYMMDD_HHMMSS_NNN prefix) matches timestamp order.
//
// In ladder mode we MERGE the two streams by timestamp via a heap-free
// k=2 merge: keep one front element from each stream, emit the earlier
// one, advance that stream. A warning logs cross-file regressions; a
// within-file regression would indicate a corrupted write and is not
// expected.

use std::path::{Path, PathBuf};
use std::collections::HashMap;
use std::fs::File;
use serde::Serialize;
use tracing::{info, warn, debug};

use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use arrow::array::{Float64Array, UInt64Array, StringArray, Array};

use crate::abc_brc::{self, PatternDirection};
use crate::candle::Timeframe;
use crate::fibonacci::FibSequence;
use crate::instrument::Instrument;
use crate::risk::{EntryZone, RiskEngine, RiskParams, TradeSignal};
use crate::ws_types::TradeUpdate;

// ============================================================================
// FILL MODE (v0.11)
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillMode {
    /// Phase 3A. Limit orders fill on tick crossing; stops/TPs fill at
    /// trigger price. Zero slippage, zero partial fills. Fast.
    Optimistic,
    /// Phase 3B. Limit orders walk the opposing ask/bid ladder; stops/TPs
    /// walk market against the current book snapshot. Partial fills
    /// tracked. Requires book Parquet files alongside trades.
    Ladder,
}

impl std::fmt::Display for FillMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FillMode::Optimistic => write!(f, "optimistic"),
            FillMode::Ladder     => write!(f, "ladder"),
        }
    }
}

// ============================================================================
// BOOK SNAPSHOT (v0.11)
// ============================================================================
// Matches the persistence.rs BookRecord schema: top-5 bid + ask with
// prices and sizes. Stored as fixed-size arrays for cache locality and
// to mirror how it comes out of Arrow.

#[derive(Debug, Clone)]
pub struct BookSnapshot {
    pub timestamp_ms: u64,
    pub bid_prices: [f64; 5],
    pub bid_sizes:  [f64; 5],
    pub ask_prices: [f64; 5],
    pub ask_sizes:  [f64; 5],
}

impl BookSnapshot {
    pub fn empty() -> Self {
        BookSnapshot {
            timestamp_ms: 0,
            bid_prices: [0.0; 5], bid_sizes: [0.0; 5],
            ask_prices: [0.0; 5], ask_sizes: [0.0; 5],
        }
    }

    /// Does this snapshot carry any depth? Zero-valued snapshots arise
    /// before the first book event is seen (portfolio starts with an
    /// empty placeholder) — we refuse to fill against them.
    pub fn is_empty(&self) -> bool {
        self.bid_sizes.iter().all(|&s| s == 0.0)
            && self.ask_sizes.iter().all(|&s| s == 0.0)
    }

    pub fn best_bid(&self) -> Option<(f64, f64)> {
        if self.bid_sizes[0] > 0.0 { Some((self.bid_prices[0], self.bid_sizes[0])) }
        else { None }
    }

    pub fn best_ask(&self) -> Option<(f64, f64)> {
        if self.ask_sizes[0] > 0.0 { Some((self.ask_prices[0], self.ask_sizes[0])) }
        else { None }
    }
}

// ============================================================================
// LADDER FILL RESULT
// ============================================================================

#[derive(Debug, Clone)]
pub struct LadderFill {
    /// Contracts actually filled (may be less than requested if depth
    /// ran out OR limit price couldn't be satisfied).
    pub filled_contracts: u64,
    /// Volume-weighted average fill price across consumed levels.
    /// Zero if nothing filled.
    pub avg_price: f64,
    /// True if size requested was fully satisfied.
    pub fully_filled: bool,
}

impl LadderFill {
    pub fn zero() -> Self {
        LadderFill { filled_contracts: 0, avg_price: 0.0, fully_filled: false }
    }
    pub fn is_nothing(&self) -> bool { self.filled_contracts == 0 }
}

/// Walk a price ladder to fill `requested_contracts` at or better than
/// `limit_price`. Used for limit entries.
///
///   - `prices`, `sizes`: the 5 levels in best-first order. For a BUY
///     order we pass the ASK ladder; for SELL we pass the BID ladder.
///     Sizes are in BTC-face units (per OKX convention for BTC-USDT-SWAP:
///     1 contract = 0.01 BTC, so a book-side size of 12.3 = 1230 contracts
///     of capacity at that level).
///   - `contract_face`: BTC per contract (0.01 for BTC-USDT-SWAP).
///   - `limit_price`: maximum acceptable price for a buy, minimum for a sell.
///   - `is_buy`: true for bullish entries (ascending ask ladder, price ≤ limit
///     is acceptable). False for bearish entries (descending bid ladder, price ≥ limit).
///
/// Returns the aggregated fill.
pub fn walk_ladder_limit(
    prices: &[f64; 5],
    sizes: &[f64; 5],
    requested_contracts: u64,
    contract_face: f64,
    limit_price: f64,
    is_buy: bool,
) -> LadderFill {
    let mut consumed_contracts: u64 = 0;
    let mut weighted_sum: f64 = 0.0;

    for i in 0..5 {
        if sizes[i] <= 0.0 { continue; }  // level absent (padded)

        // Price check: buy wants price <= limit, sell wants price >= limit.
        let price_ok = if is_buy { prices[i] <= limit_price }
                       else      { prices[i] >= limit_price };
        if !price_ok { break; }  // ladder is sorted; first fail = all subsequent fail

        // Available capacity at this level in contracts.
        let level_capacity = (sizes[i] / contract_face).floor() as u64;
        if level_capacity == 0 { continue; }

        let need = requested_contracts - consumed_contracts;
        let take = std::cmp::min(level_capacity, need);

        weighted_sum += prices[i] * take as f64;
        consumed_contracts += take;

        if consumed_contracts >= requested_contracts { break; }
    }

    if consumed_contracts == 0 {
        LadderFill::zero()
    } else {
        LadderFill {
            filled_contracts: consumed_contracts,
            avg_price: weighted_sum / consumed_contracts as f64,
            fully_filled: consumed_contracts >= requested_contracts,
        }
    }
}

/// Walk a price ladder UNCONDITIONALLY to fill a market order. Used for
/// stops and take-profits once triggered. No price limit — we take
/// whatever's available at each level until filled or depth exhausts.
///
/// If depth runs out before size is met, the returned fill reports
/// `fully_filled = false` and `filled_contracts < requested`. The
/// remaining unfilled portion is the caller's problem (in practice
/// this should be logged and the residual treated as "stopped at the
/// worst visible price" — rare on BTC-USDT-SWAP top-5 given typical
/// position sizes).
pub fn walk_ladder_market(
    prices: &[f64; 5],
    sizes: &[f64; 5],
    requested_contracts: u64,
    contract_face: f64,
) -> LadderFill {
    let mut consumed_contracts: u64 = 0;
    let mut weighted_sum: f64 = 0.0;

    for i in 0..5 {
        if sizes[i] <= 0.0 { continue; }

        let level_capacity = (sizes[i] / contract_face).floor() as u64;
        if level_capacity == 0 { continue; }

        let need = requested_contracts - consumed_contracts;
        let take = std::cmp::min(level_capacity, need);

        weighted_sum += prices[i] * take as f64;
        consumed_contracts += take;

        if consumed_contracts >= requested_contracts { break; }
    }

    if consumed_contracts == 0 {
        LadderFill::zero()
    } else {
        LadderFill {
            filled_contracts: consumed_contracts,
            avg_price: weighted_sum / consumed_contracts as f64,
            fully_filled: consumed_contracts >= requested_contracts,
        }
    }
}

// ============================================================================
// CONFIG
// ============================================================================

#[derive(Debug, Clone)]
pub struct BacktestConfig {
    /// Directory of Parquet files. In optimistic mode only `_trades_`
    /// files are read; in ladder mode both `_trades_` and `_books_`
    /// files are merged by timestamp.
    pub data_dir: PathBuf,
    /// Starting equity in USDT. Same semantics as AUGUR_ACCOUNT_BALANCE.
    pub starting_equity: f64,
    /// Risk engine parameters. FMG defaults unless overridden.
    pub risk_params: RiskParams,
    /// Where to write the per-trade CSV. None = no CSV.
    pub csv_output: Option<PathBuf>,
    /// Instrument symbol — for now fixed to BTC-USDT-SWAP (matches
    /// the persistence file naming convention).
    pub symbol: String,
    /// Fill simulation mode (v0.11).
    pub fill_mode: FillMode,
}

impl BacktestConfig {
    pub fn with_defaults(data_dir: PathBuf, starting_equity: f64) -> Self {
        BacktestConfig {
            data_dir,
            starting_equity,
            risk_params: RiskParams::fmg_default(starting_equity),
            csv_output: None,
            symbol: "BTC-USDT-SWAP".to_string(),
            fill_mode: FillMode::Optimistic,
        }
    }
}

// ============================================================================
// COMPLETED TRADE RECORD (exported to CSV + used in metrics)
// ============================================================================

#[derive(Debug, Clone, Serialize)]
pub struct CompletedTrade {
    /// Which entry zone triggered this trade.
    pub zone: String,
    pub direction: String,
    pub timeframe: String,

    pub entry_time_ms: u64,
    pub entry_price: f64,
    pub stop_price: f64,
    pub target_price: f64,
    pub contracts: u64,

    pub exit_time_ms: u64,
    pub exit_price: f64,
    /// "target" (hit TP), "stop" (hit SL), or "eof" (dataset ended
    /// with position still open — counted as closed at last trade price).
    pub exit_reason: String,

    pub pnl_usdt: f64,
    pub r_multiple: f64,
    pub equity_after: f64,
    pub duration_ms: u64,
}

// ============================================================================
// OPEN POSITION (internal state machine)
// ============================================================================

#[derive(Debug, Clone)]
struct OpenPosition {
    signal: TradeSignal,
    /// None = entry still PENDING (limit resting, zero or partial fill).
    /// Some(...) = entry reached FULL fill at this timestamp.
    fill_time_ms: Option<u64>,
    submit_time_ms: u64,
    /// v0.11: cumulative contracts filled. In optimistic mode this
    /// jumps from 0 to `signal.contracts` in one step. In ladder mode
    /// it may accumulate across multiple book snapshots.
    filled_contracts: u64,
    /// v0.11: volume-weighted average fill price across all partial
    /// fills. In optimistic mode this equals `signal.entry_price`.
    avg_fill_price: f64,
}

impl OpenPosition {
    fn is_pending(&self) -> bool { self.fill_time_ms.is_none() }
    fn is_open(&self) -> bool { self.fill_time_ms.is_some() }

    /// Does this trade tick trigger the resting entry limit?
    /// (Optimistic mode uses this directly as a fill signal; ladder mode
    /// uses it as a trigger-to-consult-book signal.)
    fn entry_fills_at(&self, tick_price: f64) -> bool {
        match self.signal.direction {
            PatternDirection::Bullish => tick_price <= self.signal.entry_price,
            PatternDirection::Bearish => tick_price >= self.signal.entry_price,
        }
    }

    /// After fill, does this tick trigger the stop?
    fn stop_hit_at(&self, tick_price: f64) -> bool {
        match self.signal.direction {
            PatternDirection::Bullish => tick_price <= self.signal.stop_price,
            PatternDirection::Bearish => tick_price >= self.signal.stop_price,
        }
    }

    /// After fill, does this tick trigger the take-profit?
    fn target_hit_at(&self, tick_price: f64) -> bool {
        match self.signal.direction {
            PatternDirection::Bullish => tick_price >= self.signal.target_price,
            PatternDirection::Bearish => tick_price <= self.signal.target_price,
        }
    }

    /// For compounding gate: only FULLY-FILLED positions are "open".
    /// Matches the live system's position.rs semantics where partial
    /// fills leave the position in Pending (so Sniper doesn't fire on
    /// a half-filled Golden).
    fn is_fully_open(&self) -> bool {
        self.is_open() && self.filled_contracts >= self.signal.contracts
    }
}

// ============================================================================
// BACKTEST PORTFOLIO
// ============================================================================
//
// Owns:
//   - Equity curve (starting balance + cumulative P&L)
//   - Active positions (pending + open), keyed by a generated ID
//   - History of completed trades (for CSV export + metrics)
//
// Exposes the same gating API as the live Positions module: has_open_golden
// and has_active_entry. This is what main-loop logic in the live binary
// consults, and the backtester's gating decisions must match bit-for-bit.

pub struct BacktestPortfolio {
    starting_equity: f64,
    equity: f64,
    /// Keyed by a synthetic id ("bt_N"), not an exchange order id.
    active: HashMap<String, OpenPosition>,
    completed: Vec<CompletedTrade>,
    next_id: u64,
    /// v0.11: the most recent book snapshot. Used in ladder-fill mode
    /// to source depth for limit-entry walks and market-exit walks.
    /// Starts empty; ladder fills against an empty book are rejected.
    current_book: BookSnapshot,
}

impl BacktestPortfolio {
    pub fn new(starting_equity: f64) -> Self {
        BacktestPortfolio {
            starting_equity,
            equity: starting_equity,
            active: HashMap::new(),
            completed: Vec::new(),
            next_id: 0,
            current_book: BookSnapshot::empty(),
        }
    }

    pub fn equity(&self) -> f64 { self.equity }
    pub fn starting_equity(&self) -> f64 { self.starting_equity }
    pub fn completed(&self) -> &[CompletedTrade] { &self.completed }
    pub fn current_book(&self) -> &BookSnapshot { &self.current_book }

    /// Gating API — matches `position::Positions` public surface.
    /// Called from the main loop before evaluating Golden / Sniper signals.
    ///
    /// v0.11: uses `is_fully_open` so a partially-filled Golden does NOT
    /// open the Sniper gate. This matches the live system's position.rs
    /// where partial fills leave the position in Pending.
    pub fn has_open_golden(&self, tf: Timeframe, dir: PatternDirection) -> bool {
        self.active.values().any(|p|
            p.is_fully_open()
            && p.signal.entry_zone == EntryZone::Golden
            && p.signal.timeframe == tf
            && p.signal.direction == dir
        )
    }

    pub fn has_active_entry(
        &self, tf: Timeframe, dir: PatternDirection, zone: EntryZone,
    ) -> bool {
        self.active.values().any(|p|
            p.signal.timeframe == tf
            && p.signal.direction == dir
            && p.signal.entry_zone == zone
        )
    }

    /// Open a new pending entry. Mirrors `OrderManager::submit_signal`
    /// but with no network round-trip.
    pub fn submit(&mut self, signal: TradeSignal, now_ms: u64) {
        let id = format!("bt_{}", self.next_id);
        self.next_id += 1;
        self.active.insert(id, OpenPosition {
            signal,
            fill_time_ms: None,
            submit_time_ms: now_ms,
            filled_contracts: 0,
            avg_fill_price: 0.0,
        });
    }

    /// Update the current book snapshot (ladder mode only).
    /// Does NOT trigger fills on its own — trades are the fill triggers.
    /// But once triggered, fills consume depth from whatever snapshot
    /// was most recently observed at trigger time. Partial fills that
    /// could not be completed against an earlier snapshot get a retry
    /// attempt here against the new one.
    pub fn process_book(&mut self, snapshot: BookSnapshot) {
        self.current_book = snapshot;

        // Retry partial fills on any position that's already triggered
        // (limit price was touched earlier, but the prior book didn't
        // carry enough depth to fill completely). We walk the ladder
        // again with the remaining size.
        let ids: Vec<String> = self.active.keys().cloned().collect();
        for id in ids {
            let pos = match self.active.get(&id) { Some(p) => p.clone(), None => continue };
            // Only retry partial fills on positions that have been
            // touched at least once (filled_contracts > 0) but not yet
            // fully filled. Pure pending (never touched) entries wait
            // for a tick-cross trigger.
            if pos.filled_contracts > 0 && pos.filled_contracts < pos.signal.contracts {
                let remaining = pos.signal.contracts - pos.filled_contracts;
                let is_buy = matches!(pos.signal.direction, PatternDirection::Bullish);
                let (prices, sizes) = if is_buy {
                    (&self.current_book.ask_prices, &self.current_book.ask_sizes)
                } else {
                    (&self.current_book.bid_prices, &self.current_book.bid_sizes)
                };
                let fill = walk_ladder_limit(
                    prices, sizes,
                    remaining,
                    crate::risk::BTC_USDT_SWAP_CONTRACT_FACE,
                    pos.signal.entry_price,
                    is_buy,
                );
                if !fill.is_nothing() {
                    // Inline the VWAP merge (same logic as the inline
                    // block in process_tick_ladder). We can't extract
                    // this into an &self helper because we hold a
                    // mutable borrow on the HashMap entry.
                    let book_ts = self.current_book.timestamp_ms;
                    let pos_mut = self.active.get_mut(&id).unwrap();
                    let existing_total = pos_mut.avg_fill_price * pos_mut.filled_contracts as f64;
                    let new_total = fill.avg_price * fill.filled_contracts as f64;
                    pos_mut.filled_contracts += fill.filled_contracts;
                    pos_mut.avg_fill_price = (existing_total + new_total) / pos_mut.filled_contracts as f64;
                    if pos_mut.filled_contracts >= pos_mut.signal.contracts
                        && pos_mut.fill_time_ms.is_none()
                    {
                        pos_mut.fill_time_ms = Some(book_ts);
                    }
                }
            }
        }
    }

    /// OPTIMISTIC-mode trade processing. Fills are instant and full
    /// at the limit price. No book consultation.
    pub fn process_tick(&mut self, tick_price: f64, tick_time_ms: u64) {
        let mut to_close: Vec<(String, f64, &'static str)> = Vec::new();

        for (id, pos) in self.active.iter_mut() {
            // --- Pending → Open (full fill at limit price) ---
            if pos.is_pending() {
                if pos.entry_fills_at(tick_price) {
                    pos.fill_time_ms = Some(tick_time_ms);
                    pos.filled_contracts = pos.signal.contracts;
                    pos.avg_fill_price = pos.signal.entry_price;
                }
            }

            // --- Open → Completed (stop/TP fills at their trigger price) ---
            if pos.is_open() {
                if pos.stop_hit_at(tick_price) {
                    to_close.push((id.clone(), pos.signal.stop_price, "stop"));
                } else if pos.target_hit_at(tick_price) {
                    to_close.push((id.clone(), pos.signal.target_price, "target"));
                }
            }
        }

        for (id, exit_price, reason) in to_close {
            let pos = self.active.remove(&id).unwrap();
            self.record_close(pos, exit_price, reason, tick_time_ms);
        }
    }

    /// LADDER-mode trade processing. A tick that crosses an entry limit
    /// triggers a book walk (not an instant fill). Stops and TPs become
    /// market orders at trigger, walking the opposite side with no
    /// price limit.
    pub fn process_tick_ladder(&mut self, tick_price: f64, tick_time_ms: u64) {
        let mut to_close: Vec<(String, LadderFill, &'static str)> = Vec::new();

        // Clone the book once — we read from it throughout this loop.
        // Copying 5 levels × 2 sides × f64 is cheap and keeps the
        // borrow checker off our back.
        let book = self.current_book.clone();
        let refuse_empty = book.is_empty();

        for (id, pos) in self.active.iter_mut() {
            // --- Pending → (partial) Open — ladder-walk against opposite side ---
            if pos.is_pending() && pos.entry_fills_at(tick_price) && !refuse_empty {
                let remaining = pos.signal.contracts - pos.filled_contracts;
                if remaining > 0 {
                    let is_buy = matches!(pos.signal.direction, PatternDirection::Bullish);
                    let (prices, sizes) = if is_buy {
                        (&book.ask_prices, &book.ask_sizes)
                    } else {
                        (&book.bid_prices, &book.bid_sizes)
                    };
                    let fill = walk_ladder_limit(
                        prices, sizes, remaining,
                        crate::risk::BTC_USDT_SWAP_CONTRACT_FACE,
                        pos.signal.entry_price,
                        is_buy,
                    );
                    if !fill.is_nothing() {
                        // Merge inline (can't call the helper; we hold
                        // a mutable borrow on the HashMap entry).
                        let existing_total = pos.avg_fill_price * pos.filled_contracts as f64;
                        let new_total = fill.avg_price * fill.filled_contracts as f64;
                        pos.filled_contracts += fill.filled_contracts;
                        pos.avg_fill_price = (existing_total + new_total) / pos.filled_contracts as f64;
                        if pos.filled_contracts >= pos.signal.contracts && pos.fill_time_ms.is_none() {
                            pos.fill_time_ms = Some(tick_time_ms);
                        }
                    }
                }
            }

            // --- Open → Completed — stop/TP become market walks ---
            if pos.is_open() {
                let (trigger, reason): (Option<f64>, &'static str) =
                    if pos.stop_hit_at(tick_price) { (Some(pos.signal.stop_price), "stop") }
                    else if pos.target_hit_at(tick_price) { (Some(pos.signal.target_price), "target") }
                    else { (None, "") };
                if trigger.is_some() && !refuse_empty {
                    // Market exit: walk opposite side with no price limit.
                    let is_buy_exit = matches!(pos.signal.direction, PatternDirection::Bearish);
                    let (prices, sizes) = if is_buy_exit {
                        (&book.ask_prices, &book.ask_sizes)
                    } else {
                        (&book.bid_prices, &book.bid_sizes)
                    };
                    let fill = walk_ladder_market(
                        prices, sizes, pos.filled_contracts,
                        crate::risk::BTC_USDT_SWAP_CONTRACT_FACE,
                    );
                    if !fill.is_nothing() {
                        to_close.push((id.clone(), fill, reason));
                    } else if trigger.is_some() {
                        // Depth is gone — fall back to trigger-price close
                        // rather than leaving the position stranded. Honest
                        // reporting: marks the close but warns in the log.
                        let synth = LadderFill {
                            filled_contracts: pos.filled_contracts,
                            avg_price: trigger.unwrap(),
                            fully_filled: true,
                        };
                        to_close.push((id.clone(), synth, reason));
                        warn!("[backtest:ladder] {} trigger with empty book — falling back to trigger price", reason);
                    }
                }
            }
        }

        for (id, fill, reason) in to_close {
            let pos = self.active.remove(&id).unwrap();
            self.record_close_ladder(pos, fill.avg_price, reason, tick_time_ms);
        }
    }

    /// End-of-dataset: force-close everything still open at the last
    /// observed price. Pending entries that never filled are discarded
    /// (no exposure was ever taken). Partially-filled entries are
    /// closed at the filled portion only.
    pub fn force_close_all(&mut self, last_price: f64, last_time_ms: u64) {
        let ids: Vec<String> = self.active.keys().cloned().collect();
        for id in ids {
            let pos = self.active.remove(&id).unwrap();
            // Closing criterion:
            //   - Fully open (is_open): close the whole thing.
            //   - Partially filled (filled > 0 but fill_time_ms is None):
            //     close the filled portion — it represents real exposure.
            //   - Pure pending (filled == 0): discard, no exposure.
            if pos.is_open() || pos.filled_contracts > 0 {
                self.record_close(pos, last_price, "eof", last_time_ms);
            }
        }
    }

    fn record_close(
        &mut self, pos: OpenPosition, exit_price: f64,
        reason: &str, exit_time_ms: u64,
    ) {
        // v0.11: use the actual VWAP fill price (may differ from limit
        // in ladder mode; equals limit in optimistic mode).
        let fill_price = if pos.avg_fill_price > 0.0 { pos.avg_fill_price }
                         else { pos.signal.entry_price };
        // Use filled contracts, not requested. In optimistic mode these
        // are always equal; in ladder mode they may differ on partials.
        let filled = if pos.filled_contracts > 0 { pos.filled_contracts }
                     else { pos.signal.contracts };
        let size_btc = filled as f64 * crate::risk::BTC_USDT_SWAP_CONTRACT_FACE;
        let pnl = match pos.signal.direction {
            PatternDirection::Bullish => (exit_price - fill_price) * size_btc,
            PatternDirection::Bearish => (fill_price - exit_price) * size_btc,
        };
        // Risk scaled to filled fraction — if only half-filled, risk is
        // only half the per-signal risk amount.
        let filled_frac = filled as f64 / pos.signal.contracts as f64;
        let risk = pos.signal.risk_amount.abs() * filled_frac;
        let r_multiple = if risk > 0.0 { pnl / risk } else { 0.0 };
        self.equity += pnl;
        let fill_time = pos.fill_time_ms.unwrap_or(pos.submit_time_ms);
        self.completed.push(CompletedTrade {
            zone: format!("{}", pos.signal.entry_zone),
            direction: format!("{}", pos.signal.direction),
            timeframe: format!("{}", pos.signal.timeframe),
            entry_time_ms: fill_time,
            entry_price: fill_price,
            stop_price: pos.signal.stop_price,
            target_price: pos.signal.target_price,
            contracts: filled,
            exit_time_ms,
            exit_price,
            exit_reason: reason.to_string(),
            pnl_usdt: pnl,
            r_multiple,
            equity_after: self.equity,
            duration_ms: exit_time_ms.saturating_sub(fill_time),
        });
    }

    /// Ladder-mode close: the exit price is already the VWAP from a
    /// market walk. Delegates to `record_close` — the P&L math is
    /// identical; the only difference from optimistic mode is that
    /// `exit_price` here is a volume-weighted result rather than a
    /// trigger price.
    fn record_close_ladder(
        &mut self, pos: OpenPosition, vwap_exit_price: f64,
        reason: &str, exit_time_ms: u64,
    ) {
        self.record_close(pos, vwap_exit_price, reason, exit_time_ms);
    }
}

// ============================================================================
// METRICS
// ============================================================================

#[derive(Debug, Clone, Serialize)]
pub struct BacktestMetrics {
    pub starting_equity: f64,
    pub ending_equity: f64,
    pub total_return_pct: f64,
    pub total_trades: usize,
    pub winners: usize,
    pub losers: usize,
    pub win_rate: f64,
    /// Sum of winning P&L divided by absolute sum of losing P&L.
    /// > 1 means strategy is profitable even with <50% win rate.
    pub profit_factor: f64,
    pub avg_r_multiple: f64,
    pub max_drawdown_pct: f64,
    /// Annualized Sharpe ratio on daily-resampled equity. NaN if
    /// dataset spans less than 2 days.
    pub sharpe_annualized: f64,
    pub avg_trade_duration_sec: f64,
    pub total_pnl: f64,
    pub gross_win: f64,
    pub gross_loss: f64,
}

impl BacktestMetrics {
    pub fn compute(portfolio: &BacktestPortfolio) -> Self {
        let trades = portfolio.completed();
        let n = trades.len();

        if n == 0 {
            return BacktestMetrics {
                starting_equity: portfolio.starting_equity(),
                ending_equity: portfolio.equity(),
                total_return_pct: 0.0,
                total_trades: 0,
                winners: 0, losers: 0,
                win_rate: 0.0,
                profit_factor: 0.0,
                avg_r_multiple: 0.0,
                max_drawdown_pct: 0.0,
                sharpe_annualized: f64::NAN,
                avg_trade_duration_sec: 0.0,
                total_pnl: 0.0,
                gross_win: 0.0, gross_loss: 0.0,
            };
        }

        let winners: Vec<&CompletedTrade> = trades.iter().filter(|t| t.pnl_usdt > 0.0).collect();
        let losers: Vec<&CompletedTrade> = trades.iter().filter(|t| t.pnl_usdt < 0.0).collect();

        let gross_win:  f64 = winners.iter().map(|t| t.pnl_usdt).sum();
        let gross_loss: f64 = losers.iter().map(|t| t.pnl_usdt).sum::<f64>().abs();
        let total_pnl = gross_win - gross_loss;

        let win_rate = winners.len() as f64 / n as f64;
        let profit_factor = if gross_loss > 0.0 { gross_win / gross_loss }
                            else if gross_win > 0.0 { f64::INFINITY }
                            else { 0.0 };
        let avg_r: f64 = trades.iter().map(|t| t.r_multiple).sum::<f64>() / n as f64;
        let avg_dur_sec: f64 = trades.iter()
            .map(|t| t.duration_ms as f64 / 1000.0).sum::<f64>() / n as f64;

        // --- Max drawdown on equity curve ---
        let mut peak = portfolio.starting_equity();
        let mut max_dd_pct = 0.0f64;
        for t in trades {
            if t.equity_after > peak { peak = t.equity_after; }
            let dd = (peak - t.equity_after) / peak;
            if dd > max_dd_pct { max_dd_pct = dd; }
        }

        // --- Annualized Sharpe on daily-resampled equity ---
        let sharpe = Self::compute_sharpe(portfolio.starting_equity(), trades);

        BacktestMetrics {
            starting_equity: portfolio.starting_equity(),
            ending_equity: portfolio.equity(),
            total_return_pct: (portfolio.equity() - portfolio.starting_equity())
                              / portfolio.starting_equity() * 100.0,
            total_trades: n,
            winners: winners.len(),
            losers: losers.len(),
            win_rate,
            profit_factor,
            avg_r_multiple: avg_r,
            max_drawdown_pct: max_dd_pct * 100.0,
            sharpe_annualized: sharpe,
            avg_trade_duration_sec: avg_dur_sec,
            total_pnl,
            gross_win, gross_loss,
        }
    }

    /// Resample equity to daily granularity and compute annualized
    /// Sharpe. Uses the standard (mean / stdev) * sqrt(365) with a
    /// zero risk-free rate assumption — crypto trades 24/7, no weekend
    /// gaps, so 365 not 252 trading days.
    fn compute_sharpe(starting: f64, trades: &[CompletedTrade]) -> f64 {
        if trades.is_empty() { return f64::NAN; }

        // Bucket by UTC day (exit time).
        let mut by_day: HashMap<u64, f64> = HashMap::new();
        for t in trades {
            let day = t.exit_time_ms / 86_400_000;  // ms/day
            *by_day.entry(day).or_insert(0.0) += t.pnl_usdt;
        }
        if by_day.len() < 2 { return f64::NAN; }

        // Build daily equity curve and take log returns.
        let mut days: Vec<u64> = by_day.keys().copied().collect();
        days.sort();
        let mut equity = starting;
        let mut returns: Vec<f64> = Vec::new();
        for d in &days {
            let prev = equity;
            equity += by_day[d];
            if prev > 0.0 {
                returns.push(equity / prev - 1.0);
            }
        }
        if returns.len() < 2 { return f64::NAN; }

        let mean = returns.iter().sum::<f64>() / returns.len() as f64;
        let var = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>()
                  / (returns.len() - 1) as f64;
        let std = var.sqrt();
        if std == 0.0 { return f64::NAN; }
        (mean / std) * (365f64).sqrt()
    }

    pub fn print_summary(&self) {
        println!();
        println!("=== BACKTEST RESULTS ===");
        println!("  Starting equity       : ${:>12.2}", self.starting_equity);
        println!("  Ending equity         : ${:>12.2}", self.ending_equity);
        println!("  Total return          :  {:>11.2}%", self.total_return_pct);
        println!("  Total P&L             : ${:>12.2}", self.total_pnl);
        println!();
        println!("  Total trades          :  {:>12}", self.total_trades);
        println!("    Winners             :  {:>12}", self.winners);
        println!("    Losers              :  {:>12}", self.losers);
        println!("    Win rate            :  {:>11.2}%", self.win_rate * 100.0);
        println!();
        println!("  Profit factor         :  {:>12.3}", self.profit_factor);
        println!("  Avg R multiple        :  {:>12.3}", self.avg_r_multiple);
        println!("  Max drawdown          :  {:>11.2}%", self.max_drawdown_pct);
        println!("  Sharpe (annualized)   :  {:>12.3}", self.sharpe_annualized);
        println!("  Avg trade duration    :  {:>9.1} sec", self.avg_trade_duration_sec);
        println!();
        println!("  Gross win             : ${:>12.2}", self.gross_win);
        println!("  Gross loss            : ${:>12.2}", self.gross_loss);
        println!("=========================");
        println!();
    }
}

// ============================================================================
// PARQUET REPLAY — discover + read files in timestamp order
// ============================================================================

/// Yields a sorted list of trade Parquet files in `data_dir`.
/// Files are identified by the "_trades_" substring in the filename
/// (matches the v0.8+ persistence naming). Sorted lexicographically,
/// which aligns with timestamp order because filenames embed
/// YYYYMMDD_HHMMSS_NNN.
pub fn discover_trade_files(data_dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(data_dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension().map_or(false, |x| x == "parquet")
                && p.file_name().map_or(false, |n| n.to_string_lossy().contains("_trades_"))
        })
        .collect();
    files.sort();
    Ok(files)
}

/// Read a single Parquet file and emit each row as a TradeUpdate,
/// preserving the order rows were written.
pub fn read_trade_file(path: &Path) -> Result<Vec<TradeUpdate>, Box<dyn std::error::Error>> {
    let file = File::open(path)?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;
    let reader = builder.build()?;

    let mut out: Vec<TradeUpdate> = Vec::new();
    for batch_result in reader {
        let batch = batch_result?;
        let ts    = batch.column(0).as_any().downcast_ref::<UInt64Array>()
            .ok_or("timestamp_ms column not UInt64")?;
        let px    = batch.column(1).as_any().downcast_ref::<Float64Array>()
            .ok_or("price column not Float64")?;
        let sz    = batch.column(2).as_any().downcast_ref::<Float64Array>()
            .ok_or("size column not Float64")?;
        let side  = batch.column(3).as_any().downcast_ref::<StringArray>()
            .ok_or("side column not Utf8")?;
        let inst  = batch.column(4).as_any().downcast_ref::<StringArray>()
            .ok_or("inst_id column not Utf8")?;

        for i in 0..batch.num_rows() {
            out.push(TradeUpdate {
                inst_id: inst.value(i).to_string(),
                price:   px.value(i),
                size:    sz.value(i),
                side:    side.value(i).to_string(),
                timestamp_ms: ts.value(i),
            });
        }
    }
    Ok(out)
}

// ============================================================================
// BOOK PARQUET READER (v0.11)
// ============================================================================
// Mirrors the trade reader. Expects the 22-column wide schema from
// persistence.rs v0.9: timestamp_ms, inst_id, bid_price_0..4,
// bid_size_0..4, ask_price_0..4, ask_size_0..4.

/// Yields a sorted list of book Parquet files in `data_dir`, identified
/// by the "_books_" substring.
pub fn discover_book_files(data_dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(data_dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension().map_or(false, |x| x == "parquet")
                && p.file_name().map_or(false, |n| n.to_string_lossy().contains("_books_"))
        })
        .collect();
    files.sort();
    Ok(files)
}

/// Read a single book Parquet file into a vector of BookSnapshots,
/// preserving row (= timestamp) order.
pub fn read_book_file(path: &Path) -> Result<Vec<BookSnapshot>, Box<dyn std::error::Error>> {
    let file = File::open(path)?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;
    let reader = builder.build()?;

    let mut out: Vec<BookSnapshot> = Vec::new();
    for batch_result in reader {
        let batch = batch_result?;
        // Column layout per persistence.rs::BookRecord::schema():
        //   0 timestamp_ms, 1 inst_id,
        //   2..6  bid_price_0..4
        //   7..11 bid_size_0..4
        //   12..16 ask_price_0..4
        //   17..21 ask_size_0..4
        let ts = batch.column(0).as_any().downcast_ref::<UInt64Array>()
            .ok_or("timestamp_ms column not UInt64")?;

        // Small helper to pull 5 consecutive f64 columns starting at `start`.
        let cols = |start: usize| -> Result<[&Float64Array; 5], Box<dyn std::error::Error>> {
            Ok([
                batch.column(start  ).as_any().downcast_ref::<Float64Array>().ok_or("col not Float64")?,
                batch.column(start+1).as_any().downcast_ref::<Float64Array>().ok_or("col not Float64")?,
                batch.column(start+2).as_any().downcast_ref::<Float64Array>().ok_or("col not Float64")?,
                batch.column(start+3).as_any().downcast_ref::<Float64Array>().ok_or("col not Float64")?,
                batch.column(start+4).as_any().downcast_ref::<Float64Array>().ok_or("col not Float64")?,
            ])
        };
        let bid_px = cols(2)?;
        let bid_sz = cols(7)?;
        let ask_px = cols(12)?;
        let ask_sz = cols(17)?;

        for i in 0..batch.num_rows() {
            let mut snap = BookSnapshot {
                timestamp_ms: ts.value(i),
                bid_prices: [0.0; 5], bid_sizes: [0.0; 5],
                ask_prices: [0.0; 5], ask_sizes: [0.0; 5],
            };
            for lvl in 0..5 {
                snap.bid_prices[lvl] = bid_px[lvl].value(i);
                snap.bid_sizes [lvl] = bid_sz[lvl].value(i);
                snap.ask_prices[lvl] = ask_px[lvl].value(i);
                snap.ask_sizes [lvl] = ask_sz[lvl].value(i);
            }
            out.push(snap);
        }
    }
    Ok(out)
}

// ============================================================================
// MERGED EVENT STREAM (v0.11 — ladder mode)
// ============================================================================
// In ladder mode we need to consume BOTH trade and book streams in
// unified timestamp order. The naive approach — load everything into
// two Vecs and merge — works for corpus sizes in the tens of GB range
// (Jetson has enough RAM, and we're processing offline). If the corpus
// outgrows memory we'd switch to a streaming merge with per-file
// readers; a straightforward extension the current interface supports.

#[derive(Debug, Clone)]
pub enum BacktestEvent {
    Trade(TradeUpdate),
    Book(BookSnapshot),
}

impl BacktestEvent {
    pub fn timestamp_ms(&self) -> u64 {
        match self {
            BacktestEvent::Trade(t) => t.timestamp_ms,
            BacktestEvent::Book(b)  => b.timestamp_ms,
        }
    }
}

/// k=2 merge: consume two streams into one timestamp-ordered sequence.
/// When timestamps tie, books win (so portfolio's current_book is fresh
/// before the trade that would consume it). This is a conservative
/// choice — in practice ties are rare on millisecond timestamps.
pub fn merge_streams(
    mut trades: Vec<TradeUpdate>, mut books: Vec<BookSnapshot>,
) -> Vec<BacktestEvent> {
    // Reverse so we can pop from the end (O(1)) in forward order.
    trades.reverse();
    books.reverse();
    let mut out: Vec<BacktestEvent> = Vec::with_capacity(trades.len() + books.len());
    loop {
        match (trades.last(), books.last()) {
            (Some(t), Some(b)) => {
                if b.timestamp_ms <= t.timestamp_ms {
                    out.push(BacktestEvent::Book(books.pop().unwrap()));
                } else {
                    out.push(BacktestEvent::Trade(trades.pop().unwrap()));
                }
            }
            (Some(_), None) => {
                while let Some(t) = trades.pop() { out.push(BacktestEvent::Trade(t)); }
                break;
            }
            (None, Some(_)) => {
                while let Some(b) = books.pop() { out.push(BacktestEvent::Book(b)); }
                break;
            }
            (None, None) => break,
        }
    }
    out
}

// ============================================================================
// BACKTEST ENGINE — the main replay loop
// ============================================================================

pub struct BacktestEngine {
    config: BacktestConfig,
    instrument: Instrument,
    risk: RiskEngine,
    portfolio: BacktestPortfolio,
    /// For cross-file ordering validation.
    last_tick_ts: u64,
}

impl BacktestEngine {
    pub fn new(config: BacktestConfig) -> Self {
        let risk_params = config.risk_params.clone();
        let starting = config.starting_equity;
        let symbol = config.symbol.clone();
        BacktestEngine {
            config,
            instrument: Instrument::new(&symbol),
            risk: RiskEngine::new(risk_params),
            portfolio: BacktestPortfolio::new(starting),
            last_tick_ts: 0,
        }
    }

    pub fn portfolio(&self) -> &BacktestPortfolio { &self.portfolio }

    /// Run the full replay. Dispatches on config.fill_mode:
    ///   - Optimistic: trades only, direct-crossing fills (v0.10 path)
    ///   - Ladder:     merged trade + book stream, depth-aware fills (v0.11)
    pub fn run(&mut self) -> Result<BacktestMetrics, Box<dyn std::error::Error>> {
        info!("[backtest] fill mode: {}", self.config.fill_mode);
        match self.config.fill_mode {
            FillMode::Optimistic => self.run_optimistic(),
            FillMode::Ladder     => self.run_ladder(),
        }
    }

    /// Optimistic replay: trades only, unchanged from v0.10.
    fn run_optimistic(&mut self) -> Result<BacktestMetrics, Box<dyn std::error::Error>> {
        let files = discover_trade_files(&self.config.data_dir)?;
        if files.is_empty() {
            return Err(format!("no trade Parquet files found in {}",
                self.config.data_dir.display()).into());
        }
        info!("[backtest] found {} trade files", files.len());

        let mut total_ticks: u64 = 0;
        let mut last_price = 0.0f64;

        for (file_idx, path) in files.iter().enumerate() {
            debug!("[backtest] reading {} ({}/{})",
                path.display(), file_idx + 1, files.len());
            let ticks = read_trade_file(path)?;
            info!("[backtest] loaded {} ticks from {}",
                ticks.len(), path.file_name().unwrap().to_string_lossy());

            for trade in ticks {
                if trade.timestamp_ms < self.last_tick_ts && total_ticks > 0 {
                    warn!("[backtest] out-of-order tick: {} < {} (file={})",
                        trade.timestamp_ms, self.last_tick_ts,
                        path.file_name().unwrap().to_string_lossy(),
                    );
                }
                self.last_tick_ts = trade.timestamp_ms;
                last_price = trade.price;
                total_ticks += 1;

                self.process_tick(&trade);
            }
        }

        self.portfolio.force_close_all(last_price, self.last_tick_ts);
        info!("[backtest] replayed {} ticks across {} files", total_ticks, files.len());
        info!("[backtest] completed {} trades", self.portfolio.completed().len());

        if let Some(csv_path) = &self.config.csv_output {
            self.write_csv(csv_path)?;
            info!("[backtest] wrote per-trade CSV to {}", csv_path.display());
        }

        Ok(BacktestMetrics::compute(&self.portfolio))
    }

    /// Ladder replay: merges trade + book streams, routes each event
    /// to the correct portfolio handler. Books update `current_book`;
    /// trades trigger ladder-walked fills against it.
    fn run_ladder(&mut self) -> Result<BacktestMetrics, Box<dyn std::error::Error>> {
        let trade_files = discover_trade_files(&self.config.data_dir)?;
        let book_files  = discover_book_files(&self.config.data_dir)?;
        if trade_files.is_empty() {
            return Err(format!("no trade Parquet files found in {}",
                self.config.data_dir.display()).into());
        }
        if book_files.is_empty() {
            return Err(format!(
                "no book Parquet files found in {} — ladder mode requires book data. \
                 Did you record with AUGUR_PERSISTENCE_ENABLED=1 on v0.9+?",
                self.config.data_dir.display()).into());
        }
        info!("[backtest] found {} trade files and {} book files",
            trade_files.len(), book_files.len());

        // Load all trades and books into memory. For a month of BTC-
        // USDT-SWAP that's ~2-3 GB; the Jetson (8 GB RAM) handles it.
        // A streaming merge across per-file readers is a straightforward
        // future extension for larger corpora.
        let mut all_trades: Vec<TradeUpdate> = Vec::new();
        for path in &trade_files {
            let ticks = read_trade_file(path)?;
            debug!("[backtest] loaded {} ticks from {}",
                ticks.len(), path.file_name().unwrap().to_string_lossy());
            all_trades.extend(ticks);
        }
        let mut all_books: Vec<BookSnapshot> = Vec::new();
        for path in &book_files {
            let snaps = read_book_file(path)?;
            debug!("[backtest] loaded {} book snapshots from {}",
                snaps.len(), path.file_name().unwrap().to_string_lossy());
            all_books.extend(snaps);
        }
        info!("[backtest] total: {} trades + {} book snapshots",
            all_trades.len(), all_books.len());

        let events = merge_streams(all_trades, all_books);
        info!("[backtest] merged into {} events", events.len());

        let mut last_price = 0.0f64;
        let mut trades_seen: u64 = 0;
        let mut books_seen: u64 = 0;

        for event in events {
            match event {
                BacktestEvent::Book(snap) => {
                    self.last_tick_ts = snap.timestamp_ms;
                    self.portfolio.process_book(snap);
                    books_seen += 1;
                }
                BacktestEvent::Trade(trade) => {
                    self.last_tick_ts = trade.timestamp_ms;
                    last_price = trade.price;
                    trades_seen += 1;
                    self.process_tick_ladder(&trade);
                }
            }
        }

        self.portfolio.force_close_all(last_price, self.last_tick_ts);
        info!("[backtest] replayed {} trades + {} books", trades_seen, books_seen);
        info!("[backtest] completed {} trades", self.portfolio.completed().len());

        if let Some(csv_path) = &self.config.csv_output {
            self.write_csv(csv_path)?;
            info!("[backtest] wrote per-trade CSV to {}", csv_path.display());
        }

        Ok(BacktestMetrics::compute(&self.portfolio))
    }

    /// Per-tick state update (optimistic mode). Unchanged from v0.10.
    pub fn process_tick(&mut self, trade: &TradeUpdate) {
        self.portfolio.process_tick(trade.price, trade.timestamp_ms);
        let outcome = self.instrument.update_from_trade(trade);
        self.run_strategy_step(&outcome, trade.timestamp_ms);
    }

    /// Per-tick state update (ladder mode). Portfolio uses the
    /// ladder-aware fill path, then strategy runs unchanged.
    pub fn process_tick_ladder(&mut self, trade: &TradeUpdate) {
        self.portfolio.process_tick_ladder(trade.price, trade.timestamp_ms);
        let outcome = self.instrument.update_from_trade(trade);
        self.run_strategy_step(&outcome, trade.timestamp_ms);
    }

    /// Strategy step shared by both modes: for each newly-confirmed
    /// swing, run pattern detection → Fibonacci → risk gate → submit.
    /// This is the bit-identical code path with main.rs's live loop.
    fn run_strategy_step(
        &mut self, outcome: &crate::instrument::IngestOutcome, now_ms: u64,
    ) {
        for (timeframe, _swing) in &outcome.new_swings {
            let detector = match self.instrument.swing_detector(*timeframe) {
                Some(d) => d, None => continue,
            };
            let pattern = match abc_brc::detect_abcd(detector) {
                Some(p) => p, None => continue,
            };
            let fib = FibSequence::from_pattern(&pattern);

            // --- GOLDEN ---
            let golden_active = self.portfolio
                .has_active_entry(*timeframe, pattern.direction, EntryZone::Golden);
            if !golden_active {
                if let Ok(signal) = self.risk.evaluate_golden(&pattern, &fib, *timeframe) {
                    self.portfolio.submit(signal, now_ms);
                }
            }

            // --- SNIPER (compounding gate) ---
            let sniper_active = self.portfolio
                .has_active_entry(*timeframe, pattern.direction, EntryZone::Sniper);
            let golden_open = self.portfolio
                .has_open_golden(*timeframe, pattern.direction);
            if !sniper_active && golden_open {
                if let Ok(signal) = self.risk.evaluate_sniper(&pattern, &fib, *timeframe) {
                    self.portfolio.submit(signal, now_ms);
                }
            }
        }
    }

    fn write_csv(&self, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
        // Build a writer with automatic headers disabled so we can force-
        // write the header once at the top — even when no trades exist.
        // Otherwise an empty backtest produces a zero-byte CSV which
        // breaks downstream scripts that expect at least the header row.
        let file = std::fs::File::create(path)?;
        let mut wtr = csv::WriterBuilder::new()
            .has_headers(false)
            .from_writer(file);
        wtr.write_record([
            "zone", "direction", "timeframe",
            "entry_time_ms", "entry_price", "stop_price", "target_price", "contracts",
            "exit_time_ms", "exit_price", "exit_reason",
            "pnl_usdt", "r_multiple", "equity_after", "duration_ms",
        ])?;
        for trade in self.portfolio.completed() {
            wtr.serialize(trade)?;
        }
        wtr.flush()?;
        Ok(())
    }
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abc_brc::PatternDirection;
    use crate::candle::Timeframe;
    use crate::risk::EntryZone;

    fn mk_signal(dir: PatternDirection, zone: EntryZone, tf: Timeframe,
                 entry: f64, stop: f64, target: f64) -> TradeSignal {
        // Risk in USDT = |entry - stop| × contracts × face
        let stop_dist = (entry - stop).abs();
        let target_dist = (target - entry).abs();
        let contracts = 100u64;
        let risk_amount = stop_dist * contracts as f64 * crate::risk::BTC_USDT_SWAP_CONTRACT_FACE;
        let reward_amount = target_dist * contracts as f64 * crate::risk::BTC_USDT_SWAP_CONTRACT_FACE;
        TradeSignal {
            direction: dir, timeframe: tf, entry_zone: zone,
            entry_price: entry, stop_price: stop, target_price: target,
            contracts,
            risk_amount, reward_amount,
            rr_ratio: reward_amount / risk_amount,
        }
    }

    #[test]
    fn bullish_limit_fills_when_price_crosses_down() {
        let mut pf = BacktestPortfolio::new(10_000.0);
        // Bullish @ 100, stop 95, target 120
        let sig = mk_signal(PatternDirection::Bullish, EntryZone::Golden,
                            Timeframe::H1, 100.0, 95.0, 120.0);
        pf.submit(sig, 1000);

        // Tick above entry — no fill yet.
        pf.process_tick(101.0, 1500);
        assert_eq!(pf.completed().len(), 0);
        assert!(pf.has_active_entry(Timeframe::H1, PatternDirection::Bullish, EntryZone::Golden));
        // Gate still closed (pending, not open).
        assert!(!pf.has_open_golden(Timeframe::H1, PatternDirection::Bullish));

        // Tick at entry → fills, gate opens.
        pf.process_tick(100.0, 2000);
        assert!(pf.has_open_golden(Timeframe::H1, PatternDirection::Bullish));
    }

    #[test]
    fn bullish_target_hit_produces_winner() {
        let mut pf = BacktestPortfolio::new(10_000.0);
        let sig = mk_signal(PatternDirection::Bullish, EntryZone::Golden,
                            Timeframe::H1, 100.0, 95.0, 120.0);
        pf.submit(sig, 1000);
        pf.process_tick(100.0, 2000);  // fill
        pf.process_tick(120.0, 3000);  // target

        assert_eq!(pf.completed().len(), 1);
        let t = &pf.completed()[0];
        assert_eq!(t.exit_reason, "target");
        assert_eq!(t.exit_price, 120.0);
        // P&L = (120 - 100) × 100ct × 0.01 = $20.00
        assert!((t.pnl_usdt - 20.0).abs() < 1e-9);
        // R = pnl / risk; risk = (100-95) × 100 × 0.01 = 5.0, so R = 4.0
        assert!((t.r_multiple - 4.0).abs() < 1e-9);
        assert!((pf.equity() - 10_020.0).abs() < 1e-9);
    }

    #[test]
    fn bullish_stop_hit_produces_loser() {
        let mut pf = BacktestPortfolio::new(10_000.0);
        let sig = mk_signal(PatternDirection::Bullish, EntryZone::Golden,
                            Timeframe::H1, 100.0, 95.0, 120.0);
        pf.submit(sig, 1000);
        pf.process_tick(100.0, 2000);  // fill
        pf.process_tick(95.0, 3000);   // stop

        assert_eq!(pf.completed().len(), 1);
        let t = &pf.completed()[0];
        assert_eq!(t.exit_reason, "stop");
        assert_eq!(t.exit_price, 95.0);
        // P&L = (95 - 100) × 100 × 0.01 = -$5.00
        assert!((t.pnl_usdt - (-5.0)).abs() < 1e-9);
        assert!((t.r_multiple - (-1.0)).abs() < 1e-9);
    }

    #[test]
    fn bearish_fill_and_target() {
        let mut pf = BacktestPortfolio::new(10_000.0);
        // Bearish @ 100, stop 105 (above entry), target 80 (below entry)
        let sig = mk_signal(PatternDirection::Bearish, EntryZone::Golden,
                            Timeframe::H1, 100.0, 105.0, 80.0);
        pf.submit(sig, 1000);

        pf.process_tick(99.0, 1500);
        // Bearish limit resting at 100 fills when price rises to or above 100.
        // Tick at 99 → still below entry, no fill.
        assert_eq!(pf.completed().len(), 0);

        pf.process_tick(100.0, 2000);  // fills
        assert!(pf.has_open_golden(Timeframe::H1, PatternDirection::Bearish));

        pf.process_tick(80.0, 3000);   // target
        assert_eq!(pf.completed().len(), 1);
        let t = &pf.completed()[0];
        assert_eq!(t.exit_reason, "target");
        // P&L = (100 - 80) × 100 × 0.01 = $20
        assert!((t.pnl_usdt - 20.0).abs() < 1e-9);
    }

    #[test]
    fn stop_takes_priority_over_target_on_same_tick() {
        // Pathological: a gap tick that covers both stop and target.
        // Conservative behavior — choose the stop (worst-case outcome).
        let mut pf = BacktestPortfolio::new(10_000.0);
        let sig = mk_signal(PatternDirection::Bullish, EntryZone::Golden,
                            Timeframe::H1, 100.0, 95.0, 105.0);
        pf.submit(sig, 1000);
        pf.process_tick(100.0, 2000);  // fill
        // Gap down through stop AND target on same tick? Impossible
        // mathematically (stop < entry < target for bullish), so pick
        // a case where a single tick hits BOTH bounds - we need to
        // construct a scenario where target < stop which only happens
        // in bearish geometry. Re-do with bearish.
        let mut pf = BacktestPortfolio::new(10_000.0);
        let sig = mk_signal(PatternDirection::Bearish, EntryZone::Golden,
                            Timeframe::H1, 100.0, 105.0, 95.0);
        pf.submit(sig, 1000);
        pf.process_tick(100.0, 2000);  // fill at entry
        // Now tick at 110: hits bearish stop (price >= 105).
        // Tick at 94 would hit target only.
        // Tick at 106 hits stop only.
        // For pathological "same tick covers both", imagine a tick at
        // 106 and simultaneously at 94 — can't happen with a single tick.
        // So the actual pathology is 2 adjacent ticks: stop first, then target.
        pf.process_tick(106.0, 3000);
        assert_eq!(pf.completed()[0].exit_reason, "stop");
    }

    #[test]
    fn pending_entry_discarded_at_eof() {
        // Submit an entry that never fills. End the dataset. Should
        // NOT appear in completed (no exposure was taken).
        let mut pf = BacktestPortfolio::new(10_000.0);
        let sig = mk_signal(PatternDirection::Bullish, EntryZone::Golden,
                            Timeframe::H1, 100.0, 95.0, 120.0);
        pf.submit(sig, 1000);
        pf.process_tick(101.0, 2000);  // above entry, no fill
        pf.force_close_all(102.0, 3000);
        assert_eq!(pf.completed().len(), 0);
        assert_eq!(pf.equity(), 10_000.0);
    }

    #[test]
    fn open_position_at_eof_marked_as_eof_close() {
        let mut pf = BacktestPortfolio::new(10_000.0);
        let sig = mk_signal(PatternDirection::Bullish, EntryZone::Golden,
                            Timeframe::H1, 100.0, 95.0, 120.0);
        pf.submit(sig, 1000);
        pf.process_tick(100.0, 2000);  // fill
        pf.process_tick(110.0, 3000);  // still running
        pf.force_close_all(110.0, 4000);
        assert_eq!(pf.completed().len(), 1);
        assert_eq!(pf.completed()[0].exit_reason, "eof");
        assert_eq!(pf.completed()[0].exit_price, 110.0);
    }

    #[test]
    fn compounding_gate_prevents_sniper_before_golden_fills() {
        // With no filled Golden, a Sniper gate check should return false.
        let mut pf = BacktestPortfolio::new(10_000.0);
        let sig = mk_signal(PatternDirection::Bullish, EntryZone::Golden,
                            Timeframe::H1, 100.0, 95.0, 120.0);
        pf.submit(sig, 1000);
        // Pending — gate not open.
        assert!(!pf.has_open_golden(Timeframe::H1, PatternDirection::Bullish));
        // Fill it.
        pf.process_tick(100.0, 2000);
        assert!(pf.has_open_golden(Timeframe::H1, PatternDirection::Bullish));
        // Stop it out.
        pf.process_tick(95.0, 3000);
        // No longer open — Sniper gate re-closes.
        assert!(!pf.has_open_golden(Timeframe::H1, PatternDirection::Bullish));
    }

    #[test]
    fn metrics_on_empty_portfolio_returns_zeros() {
        let pf = BacktestPortfolio::new(10_000.0);
        let m = BacktestMetrics::compute(&pf);
        assert_eq!(m.total_trades, 0);
        assert_eq!(m.total_return_pct, 0.0);
        assert!(m.sharpe_annualized.is_nan());
    }

    #[test]
    fn metrics_winners_losers_profit_factor() {
        let mut pf = BacktestPortfolio::new(10_000.0);
        // Winner: +$20
        let s1 = mk_signal(PatternDirection::Bullish, EntryZone::Golden, Timeframe::H1,
                           100.0, 95.0, 120.0);
        pf.submit(s1, 1000);
        pf.process_tick(100.0, 2000);
        pf.process_tick(120.0, 3000);

        // Loser: -$5
        let s2 = mk_signal(PatternDirection::Bullish, EntryZone::Golden, Timeframe::H1,
                           100.0, 95.0, 120.0);
        pf.submit(s2, 4000);
        pf.process_tick(100.0, 5000);
        pf.process_tick(95.0, 6000);

        let m = BacktestMetrics::compute(&pf);
        assert_eq!(m.total_trades, 2);
        assert_eq!(m.winners, 1);
        assert_eq!(m.losers, 1);
        assert!((m.win_rate - 0.5).abs() < 1e-9);
        assert!((m.gross_win - 20.0).abs() < 1e-9);
        assert!((m.gross_loss - 5.0).abs() < 1e-9);
        // profit factor = 20/5 = 4.0
        assert!((m.profit_factor - 4.0).abs() < 1e-9);
        // total pnl = 15, ending equity = 10015
        assert!((m.total_pnl - 15.0).abs() < 1e-9);
    }

    #[test]
    fn max_drawdown_tracks_peak_to_trough() {
        let mut pf = BacktestPortfolio::new(10_000.0);
        // Up, then down, then recover partially.
        for (entry, exit_win) in [
            (100.0, 120.0),  // +20
            (100.0, 120.0),  // +20 (peak at 10040)
            (100.0, 95.0),   // -5
            (100.0, 95.0),   // -5 (trough at 10030, DD from 10040 = 10)
        ] {
            let s = mk_signal(PatternDirection::Bullish, EntryZone::Golden, Timeframe::H1,
                              100.0, 95.0, 120.0);
            pf.submit(s, 0);
            pf.process_tick(100.0, 100);
            pf.process_tick(exit_win, 200);
        }
        let m = BacktestMetrics::compute(&pf);
        // Peak 10040, trough 10030. DD = 10/10040 ≈ 0.0996%
        assert!((m.max_drawdown_pct - 0.0996).abs() < 0.01);
    }

    // ========================================================================
    // LADDER-MODE TESTS (v0.11 — Phase 3B)
    // ========================================================================

    /// Helper: synthesize a book with a controllable spread and depth.
    /// Ask side walks UP from mid+spread/2 with `levels` evenly spaced
    /// at $0.50 apart; bid walks DOWN from mid-spread/2 similarly.
    /// Each level has `size_per_level` BTC available.
    fn mk_book(mid: f64, spread: f64, size_per_level: f64) -> BookSnapshot {
        let mut bp = [0.0f64; 5]; let mut bs = [0.0f64; 5];
        let mut ap = [0.0f64; 5]; let mut asz = [0.0f64; 5];
        for i in 0..5 {
            let off = i as f64 * 0.5;  // $0.50 between levels
            bp[i] = mid - spread / 2.0 - off;
            ap[i] = mid + spread / 2.0 + off;
            bs[i] = size_per_level;
            asz[i] = size_per_level;
        }
        BookSnapshot {
            timestamp_ms: 0,
            bid_prices: bp, bid_sizes: bs,
            ask_prices: ap, ask_sizes: asz,
        }
    }

    #[test]
    fn walk_ladder_limit_respects_price() {
        // Asks: 100.5, 101.0, 101.5, 102.0, 102.5 (each 1.0 BTC = 100 contracts).
        // Buy limit at 101.0 should take the first 2 levels (100.5 + 101.0)
        // = 200 contracts, avg price = (100.5 + 101.0) / 2 = 100.75.
        let bk = mk_book(101.0, 1.0, 1.0);  // mid=101, spread=1 → asks start at 101.5
        // Actually re-read: spread=1, mid=101 → asks at 101.5, 102.0, 102.5, 103.0, 103.5
        // We want a book where levels straddle 101 cleanly. Use different params.
        let bk = BookSnapshot {
            timestamp_ms: 0,
            bid_prices: [99.5, 99.0, 98.5, 98.0, 97.5],
            bid_sizes:  [1.0; 5],
            ask_prices: [100.5, 101.0, 101.5, 102.0, 102.5],
            ask_sizes:  [1.0; 5],
        };
        let fill = walk_ladder_limit(
            &bk.ask_prices, &bk.ask_sizes,
            200,  // 200 contracts = 2.0 BTC at 0.01 face
            crate::risk::BTC_USDT_SWAP_CONTRACT_FACE,
            101.0,  // limit price: accept 100.5 and 101.0, reject 101.5+
            true,   // is_buy
        );
        assert_eq!(fill.filled_contracts, 200);
        assert!(fill.fully_filled);
        // VWAP: (100.5 * 100 + 101.0 * 100) / 200 = 100.75
        assert!((fill.avg_price - 100.75).abs() < 1e-9, "got avg_price = {}", fill.avg_price);
        let _ = bk;  // silence first mk_book unused warning
    }

    #[test]
    fn walk_ladder_limit_partial_on_price_ceiling() {
        // Each ask level has 0.5 BTC = 50 contracts. Best two levels are
        // 100.5 and 101.0. Buyer wants 200 contracts at limit=101.0.
        // Depth at the two acceptable levels: 50 + 50 = 100. Partial fill.
        let bk = BookSnapshot {
            timestamp_ms: 0,
            bid_prices: [99.5, 99.0, 98.5, 98.0, 97.5],
            bid_sizes:  [0.5; 5],
            ask_prices: [100.5, 101.0, 101.5, 102.0, 102.5],
            ask_sizes:  [0.5; 5],
        };
        let fill = walk_ladder_limit(
            &bk.ask_prices, &bk.ask_sizes,
            200,
            crate::risk::BTC_USDT_SWAP_CONTRACT_FACE,
            101.0,
            true,
        );
        assert_eq!(fill.filled_contracts, 100);
        assert!(!fill.fully_filled);
        assert!((fill.avg_price - 100.75).abs() < 1e-9);
    }

    #[test]
    fn walk_ladder_limit_zero_fill_when_best_exceeds_limit() {
        // Best ask is 101.5, buyer's limit is 101.0. No fill at all.
        let bk = BookSnapshot {
            timestamp_ms: 0,
            bid_prices: [99.5, 99.0, 98.5, 98.0, 97.5],
            bid_sizes:  [1.0; 5],
            ask_prices: [101.5, 102.0, 102.5, 103.0, 103.5],
            ask_sizes:  [1.0; 5],
        };
        let fill = walk_ladder_limit(
            &bk.ask_prices, &bk.ask_sizes,
            100, crate::risk::BTC_USDT_SWAP_CONTRACT_FACE,
            101.0, true,
        );
        assert!(fill.is_nothing());
        assert_eq!(fill.filled_contracts, 0);
    }

    #[test]
    fn walk_ladder_market_walks_unconditionally() {
        // Stop or TP trigger → market exit. No price limit.
        // Buyer (covering a short): walks the ask ladder.
        let bk = BookSnapshot {
            timestamp_ms: 0,
            bid_prices: [99.5, 99.0, 98.5, 98.0, 97.5],
            bid_sizes:  [1.0; 5],
            ask_prices: [100.5, 101.0, 101.5, 102.0, 102.5],
            ask_sizes:  [1.0; 5],
        };
        // Walk 300 contracts = 3 BTC. Takes 3 full levels: 100.5, 101.0, 101.5
        let fill = walk_ladder_market(
            &bk.ask_prices, &bk.ask_sizes,
            300, crate::risk::BTC_USDT_SWAP_CONTRACT_FACE,
        );
        assert_eq!(fill.filled_contracts, 300);
        assert!(fill.fully_filled);
        // VWAP: (100.5 + 101.0 + 101.5) / 3 = 101.0
        assert!((fill.avg_price - 101.0).abs() < 1e-9);
    }

    #[test]
    fn walk_ladder_market_partial_when_depth_exhausts() {
        // 5 levels × 1.0 BTC each = 500 contracts max.
        // Request 700 — 500 fill, 200 left unfilled.
        let bk = BookSnapshot {
            timestamp_ms: 0,
            bid_prices: [99.5, 99.0, 98.5, 98.0, 97.5],
            bid_sizes:  [1.0; 5],
            ask_prices: [100.5, 101.0, 101.5, 102.0, 102.5],
            ask_sizes:  [1.0; 5],
        };
        let fill = walk_ladder_market(
            &bk.ask_prices, &bk.ask_sizes,
            700, crate::risk::BTC_USDT_SWAP_CONTRACT_FACE,
        );
        assert_eq!(fill.filled_contracts, 500);
        assert!(!fill.fully_filled);
        // VWAP: (100.5+101.0+101.5+102.0+102.5)/5 = 101.5
        assert!((fill.avg_price - 101.5).abs() < 1e-9);
    }

    #[test]
    fn merge_streams_orders_by_timestamp() {
        let trades = vec![
            TradeUpdate { timestamp_ms: 10, price: 100.0, size: 0.01,
                side: "buy".into(), inst_id: "BTC-USDT-SWAP".into() },
            TradeUpdate { timestamp_ms: 30, price: 101.0, size: 0.01,
                side: "buy".into(), inst_id: "BTC-USDT-SWAP".into() },
            TradeUpdate { timestamp_ms: 50, price: 102.0, size: 0.01,
                side: "buy".into(), inst_id: "BTC-USDT-SWAP".into() },
        ];
        let books = vec![
            { let mut b = BookSnapshot::empty(); b.timestamp_ms = 20; b },
            { let mut b = BookSnapshot::empty(); b.timestamp_ms = 40; b },
        ];
        let events = merge_streams(trades, books);
        assert_eq!(events.len(), 5);
        // Expected order: trade@10, book@20, trade@30, book@40, trade@50
        assert_eq!(events[0].timestamp_ms(), 10);
        assert_eq!(events[1].timestamp_ms(), 20);
        assert_eq!(events[2].timestamp_ms(), 30);
        assert_eq!(events[3].timestamp_ms(), 40);
        assert_eq!(events[4].timestamp_ms(), 50);
        assert!(matches!(events[0], BacktestEvent::Trade(_)));
        assert!(matches!(events[1], BacktestEvent::Book(_)));
    }

    #[test]
    fn merge_streams_tiebreak_book_wins() {
        // Books win on equal timestamps so portfolio's current_book is
        // refreshed before the trade that would consume it.
        let trades = vec![
            TradeUpdate { timestamp_ms: 50, price: 100.0, size: 0.01,
                side: "buy".into(), inst_id: "BTC-USDT-SWAP".into() },
        ];
        let books = vec![
            { let mut b = BookSnapshot::empty(); b.timestamp_ms = 50; b },
        ];
        let events = merge_streams(trades, books);
        assert!(matches!(events[0], BacktestEvent::Book(_)));
        assert!(matches!(events[1], BacktestEvent::Trade(_)));
    }

    #[test]
    fn merge_streams_handles_empty_books() {
        let trades = vec![
            TradeUpdate { timestamp_ms: 10, price: 100.0, size: 0.01,
                side: "buy".into(), inst_id: "BTC-USDT-SWAP".into() },
            TradeUpdate { timestamp_ms: 20, price: 101.0, size: 0.01,
                side: "buy".into(), inst_id: "BTC-USDT-SWAP".into() },
        ];
        let events = merge_streams(trades, vec![]);
        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|e| matches!(e, BacktestEvent::Trade(_))));
    }

    #[test]
    fn ladder_fill_uses_vwap_not_limit_price() {
        // Bullish entry at 100.0. Book asks start at 99.5 (inside the
        // limit) — we get a BETTER price than the limit. This is
        // realistic: market maker standing on the ask at your limit-
        // crossing price fills you at their resting price.
        let mut pf = BacktestPortfolio::new(10_000.0);
        let book = BookSnapshot {
            timestamp_ms: 0,
            bid_prices: [98.5, 98.0, 97.5, 97.0, 96.5],
            bid_sizes:  [5.0; 5],
            ask_prices: [99.5, 100.0, 100.5, 101.0, 101.5],
            ask_sizes:  [5.0; 5],  // 500 contracts per level
        };
        pf.process_book(book);

        let sig = mk_signal(PatternDirection::Bullish, EntryZone::Golden,
                            Timeframe::H1, 100.0, 95.0, 120.0);
        pf.submit(sig, 100);
        // Tick at 100.0 triggers the entry limit. Ladder walk takes
        // 100 contracts at best ask = 99.5 (better than limit).
        pf.process_tick_ladder(100.0, 200);

        // Move to stop — market exit walks the bid side.
        pf.process_tick_ladder(95.0, 300);

        assert_eq!(pf.completed().len(), 1);
        let t = &pf.completed()[0];
        // Entry VWAP should be 99.5 (better than the 100.0 limit), not 100.0.
        assert!((t.entry_price - 99.5).abs() < 1e-9,
            "entry_price expected 99.5, got {}", t.entry_price);
        // Stop exit walks bid side. 100 contracts at best bid = 98.5.
        // Wait — stop is at 95.0, but ladder walk takes whatever the
        // book offers. Best bid = 98.5.
        assert!((t.exit_price - 98.5).abs() < 1e-9,
            "exit_price expected 98.5, got {}", t.exit_price);
        // P&L = (98.5 - 99.5) × 100 contracts × 0.01 face = -$1.00
        assert!((t.pnl_usdt - (-1.0)).abs() < 1e-9);
    }

    #[test]
    fn ladder_empty_book_refuses_fill() {
        // Without a book snapshot (freshly-constructed portfolio),
        // ladder fills are rejected — the entry stays pending.
        let mut pf = BacktestPortfolio::new(10_000.0);
        let sig = mk_signal(PatternDirection::Bullish, EntryZone::Golden,
                            Timeframe::H1, 100.0, 95.0, 120.0);
        pf.submit(sig, 100);
        // Tick at entry price — but no book, so no fill.
        pf.process_tick_ladder(100.0, 200);
        assert!(pf.has_active_entry(Timeframe::H1, PatternDirection::Bullish, EntryZone::Golden));
        assert!(!pf.has_open_golden(Timeframe::H1, PatternDirection::Bullish));
        assert_eq!(pf.completed().len(), 0);
    }

    #[test]
    fn ladder_partial_fill_keeps_sniper_gate_closed() {
        // A Golden that's only partially filled does NOT open the
        // compounding gate. This matches the live position.rs semantics
        // where partial_filled → Pending.
        let mut pf = BacktestPortfolio::new(10_000.0);

        // Thin book: each level only 0.3 BTC = 30 contracts.
        let book = BookSnapshot {
            timestamp_ms: 0,
            bid_prices: [98.5, 98.0, 97.5, 97.0, 96.5],
            bid_sizes:  [0.3; 5],
            ask_prices: [99.5, 100.0, 100.5, 101.0, 101.5],
            ask_sizes:  [0.3; 5],
        };
        pf.process_book(book);

        // Signal wants 100 contracts. Best 2 ask levels (99.5, 100.0)
        // within limit=100.0 carry 30+30=60 contracts. Partial fill.
        let sig = mk_signal(PatternDirection::Bullish, EntryZone::Golden,
                            Timeframe::H1, 100.0, 95.0, 120.0);
        pf.submit(sig, 100);
        pf.process_tick_ladder(100.0, 200);

        // Gate: is_fully_open is FALSE because 60 < 100.
        assert!(!pf.has_open_golden(Timeframe::H1, PatternDirection::Bullish),
            "Sniper gate should remain closed on partial Golden fill");
        // has_active_entry still true (the pending partial counts as active).
        assert!(pf.has_active_entry(Timeframe::H1, PatternDirection::Bullish, EntryZone::Golden));
    }

    #[test]
    fn ladder_partial_fills_accumulate_across_book_updates() {
        // First book has 30ct at each ask level. Partial fill of 60.
        // Second book arrives with more depth — retry path completes.
        let mut pf = BacktestPortfolio::new(10_000.0);

        let thin_book = BookSnapshot {
            timestamp_ms: 100,
            bid_prices: [98.5, 98.0, 97.5, 97.0, 96.5],
            bid_sizes:  [0.3; 5],
            ask_prices: [99.5, 100.0, 100.5, 101.0, 101.5],
            ask_sizes:  [0.3; 5],  // 30 contracts each level
        };
        pf.process_book(thin_book);

        let sig = mk_signal(PatternDirection::Bullish, EntryZone::Golden,
                            Timeframe::H1, 100.0, 95.0, 120.0);
        pf.submit(sig, 150);

        // Tick triggers: first 2 levels within limit = 60 contracts filled.
        pf.process_tick_ladder(100.0, 200);
        assert!(!pf.has_open_golden(Timeframe::H1, PatternDirection::Bullish));

        // Fresh book has plenty of depth. Retry path completes the fill.
        let deep_book = BookSnapshot {
            timestamp_ms: 300,
            bid_prices: [98.5, 98.0, 97.5, 97.0, 96.5],
            bid_sizes:  [5.0; 5],
            ask_prices: [99.5, 100.0, 100.5, 101.0, 101.5],
            ask_sizes:  [5.0; 5],  // 500 contracts each
        };
        pf.process_book(deep_book);

        // Now fully open — gate opens.
        assert!(pf.has_open_golden(Timeframe::H1, PatternDirection::Bullish),
            "Sniper gate should open after retry completes the fill");
    }

    #[test]
    fn ladder_market_stop_shows_slippage_vs_optimistic() {
        // Same signal, same starting price, but ladder's stop walk
        // should produce a WORSE exit (slippage) than optimistic's
        // trigger-price exit.
        let mk_pf = |ladder: bool| {
            let mut pf = BacktestPortfolio::new(10_000.0);
            if ladder {
                // Bid side thin at best: best_bid=94.8 when stop triggers at 95.
                // Walker takes 94.8 first, then deeper levels.
                let book = BookSnapshot {
                    timestamp_ms: 0,
                    bid_prices: [94.8, 94.5, 94.0, 93.5, 93.0],
                    bid_sizes:  [0.2, 1.0, 1.0, 1.0, 1.0],  // first level thin
                    ask_prices: [99.5, 100.0, 100.5, 101.0, 101.5],
                    ask_sizes:  [5.0; 5],
                };
                pf.process_book(book);
            }
            let sig = mk_signal(PatternDirection::Bullish, EntryZone::Golden,
                                Timeframe::H1, 100.0, 95.0, 120.0);
            pf.submit(sig, 100);
            if ladder {
                pf.process_tick_ladder(100.0, 200);  // fills at 99.5 (better than limit)
                pf.process_tick_ladder(95.0, 300);   // stop triggers
            } else {
                pf.process_tick(100.0, 200);
                pf.process_tick(95.0, 300);
            }
            pf
        };

        let pf_opt = mk_pf(false);
        let pf_lad = mk_pf(true);

        let t_opt = &pf_opt.completed()[0];
        let t_lad = &pf_lad.completed()[0];

        // Optimistic: entry=100, exit=95. P&L = -$5.
        assert!((t_opt.exit_price - 95.0).abs() < 1e-9);
        // Ladder: entry=99.5 (BETTER), exit=walked bid VWAP.
        //   100 contracts = 1.0 BTC. Level 0 = 0.2 BTC = 20ct. Level 1 = 1.0 BTC.
        //   Take 20ct @ 94.8, then 80ct @ 94.5 → VWAP = (20*94.8 + 80*94.5)/100 = 94.56
        assert!((t_lad.exit_price - 94.56).abs() < 1e-6,
            "ladder exit should be 94.56, got {}", t_lad.exit_price);
        // Ladder exit is WORSE (lower) than optimistic exit for a long position.
        assert!(t_lad.exit_price < t_opt.exit_price);
    }

    #[test]
    fn process_book_updates_current_book() {
        let mut pf = BacktestPortfolio::new(10_000.0);
        assert!(pf.current_book().is_empty());
        let book = mk_book(100.0, 1.0, 1.0);
        pf.process_book(book.clone());
        assert!(!pf.current_book().is_empty());
        assert_eq!(pf.current_book().ask_prices[0], book.ask_prices[0]);
    }

    #[test]
    fn config_fill_mode_defaults_to_optimistic() {
        let cfg = BacktestConfig::with_defaults(PathBuf::from("/tmp"), 10_000.0);
        assert_eq!(cfg.fill_mode, FillMode::Optimistic);
    }
}
