// src/candle.rs
//
// ============================================================================
// WHY THIS MODULE EXISTS
// ============================================================================
//
// Your WebSocket stream delivers raw trade ticks — individual executions like
// "0.15 BTC sold at $67,432.10 at timestamp 1710000065123". This is the
// highest-resolution data, but strategies don't operate on individual ticks.
//
// Both the ABC-BRC strategy (swing detection, Fibonacci retracement) and the
// DSP pipeline (spectral analysis, filtering) need CANDLES — aggregated
// Open/High/Low/Close/Volume bars over fixed time intervals.
//
// This module takes the raw tick firehose and produces candles at whatever
// timeframe you want. It's the shared foundation both paths build on.
//
// ARCHITECTURE:
// =============
//
//   TradeUpdate (from WS) ──► CandleAggregator ──► completed Candle
//                                    │
//                                    ├── current_candle (in-progress, live)
//                                    └── history (VecDeque of closed candles)
//
// The aggregator aligns candle boundaries to UTC epoch time. A 1-minute
// candle that opens at 12:03:00.000 UTC will close at 12:04:00.000 UTC,
// regardless of when trades actually arrive. This means:
//   - Candle open/close times are deterministic and reproducible
//   - Backtesting against historical data will produce identical boundaries
//   - Multiple aggregators at different timeframes stay phase-locked
//
// RUST CONCEPT — VecDeque:
// ========================
// VecDeque is a double-ended queue. We push completed candles to the back
// and pop old ones from the front when we exceed our window size. This gives
// us O(1) insertion AND O(1) eviction — perfect for a rolling window.
// Think of it as a fixed-size conveyor belt of candles.

use std::collections::VecDeque;
use std::fmt;
use crate::ws_types::TradeUpdate;

// ============================================================================
// TIMEFRAME
// ============================================================================
// Each variant represents a candle duration. The `duration_ms()` method
// returns the period in milliseconds, which is all the aggregator needs
// to compute candle boundaries.
//
// We derive Copy + Clone because Timeframe is just a tag — there's no heap
// data to worry about. Passing it around is as cheap as passing a u8.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Timeframe {
    M1,   // 1 minute  — scalping, tick-level structure
    M5,   // 5 minutes — short-term patterns
    M15,  // 15 minutes — intraday swing setups
    H1,   // 1 hour    — ABC-BRC primary timeframe
    H4,   // 4 hours   — higher-timeframe structure (FMG guide uses H4)
    D1,   // 1 day     — trend context
}

impl Timeframe {
    /// Returns the candle duration in milliseconds.
    /// This is the ONLY place timeframe-to-duration mapping lives.
    /// Everything else in the module works in terms of raw milliseconds.
    pub fn duration_ms(&self) -> u64 {
        match self {
            Timeframe::M1  => 60_000,
            Timeframe::M5  => 300_000,
            Timeframe::M15 => 900_000,
            Timeframe::H1  => 3_600_000,
            Timeframe::H4  => 14_400_000,
            Timeframe::D1  => 86_400_000,
        }
    }

    /// Human-readable label for logging.
    pub fn label(&self) -> &'static str {
        match self {
            Timeframe::M1  => "1m",
            Timeframe::M5  => "5m",
            Timeframe::M15 => "15m",
            Timeframe::H1  => "1h",
            Timeframe::H4  => "4h",
            Timeframe::D1  => "1d",
        }
    }
}

impl fmt::Display for Timeframe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.label())
    }
}

// ============================================================================
// CANDLE
// ============================================================================
// A single OHLCV bar. This is the fundamental data unit that both the
// ABC-BRC swing detector and the DSP pipeline consume.
//
// We derive Clone so candles can be cheaply copied when strategies need
// to snapshot the current state. A Candle is 72 bytes — small enough to
// live on the stack and pass by value without worry.

#[derive(Debug, Clone)]
pub struct Candle {
    /// UTC-aligned start time of this candle (inclusive), in epoch ms.
    pub open_time: u64,
    /// UTC-aligned end time of this candle (exclusive), in epoch ms.
    /// A trade at exactly close_time belongs to the NEXT candle.
    pub close_time: u64,

    pub open:  f64,
    pub high:  f64,
    pub low:   f64,
    pub close: f64,

    /// Total traded volume (in base currency, e.g. BTC) during this candle.
    pub volume: f64,
    /// Number of individual trades aggregated into this candle.
    /// Useful for volume-spread analysis and the DSP pipeline.
    pub trade_count: u64,
}

impl Candle {
    /// Creates a new candle seeded by the first trade in its period.
    ///
    /// RUST CONCEPT — why `open_time` and `close_time` are passed in
    /// rather than derived from `trade`:
    /// The candle boundaries are determined by the Timeframe, not by the
    /// trade's timestamp. A trade at 12:03:47 inside a 1m candle still
    /// opens the candle at 12:03:00 and closes at 12:04:00. Separating
    /// boundary computation (in CandleAggregator) from construction
    /// (here) keeps each piece simple and testable.
    fn new(open_time: u64, close_time: u64, trade: &TradeUpdate) -> Self {
        Candle {
            open_time,
            close_time,
            open:  trade.price,
            high:  trade.price,
            low:   trade.price,
            close: trade.price,
            volume: trade.size,
            trade_count: 1,
        }
    }

    /// Folds a new trade into this candle. Updates high, low, close, and
    /// accumulates volume. Open is NEVER changed after construction —
    /// it's always the price of the first trade in the period.
    fn update(&mut self, trade: &TradeUpdate) {
        if trade.price > self.high {
            self.high = trade.price;
        }
        if trade.price < self.low {
            self.low = trade.price;
        }
        // Close is always the MOST RECENT trade price.
        self.close = trade.price;
        self.volume += trade.size;
        self.trade_count += 1;
    }

    /// The candle's body size (absolute difference between open and close).
    /// Used by swing detection to gauge conviction of a move.
    pub fn body(&self) -> f64 {
        (self.close - self.open).abs()
    }

    /// The full range of the candle (high minus low).
    /// The ratio of body() to range() tells you about wicks/shadows —
    /// important for reading price rejection at Fibonacci levels.
    pub fn range(&self) -> f64 {
        self.high - self.low
    }

    /// Is this a bullish (green) candle? Close > Open.
    pub fn is_bullish(&self) -> bool {
        self.close > self.open
    }

    /// Is this a bearish (red) candle? Close < Open.
    pub fn is_bearish(&self) -> bool {
        self.close < self.open
    }

    /// Midpoint of the candle body. Useful as a reference level for
    /// Fibonacci calculations and mean-reversion signals.
    pub fn midpoint(&self) -> f64 {
        (self.high + self.low) / 2.0
    }

    /// Volume-weighted average price (approximation).
    /// True VWAP needs per-tick price*volume, but (H+L+C)/3 * volume
    /// is a standard candle-level approximation.
    pub fn typical_price(&self) -> f64 {
        (self.high + self.low + self.close) / 3.0
    }
}

impl fmt::Display for Candle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let direction = if self.is_bullish() { "▲" } else { "▼" };
        write!(
            f,
            "{} O:{:.2} H:{:.2} L:{:.2} C:{:.2} V:{:.4} ({}t)",
            direction, self.open, self.high, self.low, self.close,
            self.volume, self.trade_count
        )
    }
}

// ============================================================================
// CANDLE AGGREGATOR
// ============================================================================
// This is the engine that converts a stream of TradeUpdates into candles.
//
// USAGE PATTERN:
//   let mut agg = CandleAggregator::new(Timeframe::M1, 500);
//   // In your event loop:
//   if let Some(closed_candle) = agg.update(&trade) {
//       // A candle just closed! Feed it to your strategy.
//   }
//   // You can also peek at the in-progress candle:
//   if let Some(live) = agg.current() { ... }
//
// WHY `Option<Candle>` as the return type of update()?
// Most trades just modify the current candle and return None. Only when a
// trade crosses a candle boundary do you get Some(closed_candle). This
// makes the event loop clean:
//   - None → business as usual, candle still building
//   - Some(candle) → a complete bar just formed, strategy should react

pub struct CandleAggregator {
    /// Which timeframe this aggregator produces.
    timeframe: Timeframe,

    /// The candle currently being built. None if no trades have arrived yet.
    current_candle: Option<Candle>,

    /// Rolling window of completed candles, newest at the back.
    /// When len() exceeds max_history, the oldest candle is evicted.
    ///
    /// RUST CONCEPT — VecDeque vs Vec:
    /// Vec is efficient for push_back but O(n) for remove(0).
    /// VecDeque is O(1) for both push_back AND pop_front, which is
    /// exactly what a rolling window needs.
    history: VecDeque<Candle>,

    /// Maximum number of completed candles to retain.
    /// 500 candles at M1 ≈ 8.3 hours. At H1 ≈ 20.8 days.
    /// At H4 ≈ 83 days. Tune this based on your strategy's lookback.
    max_history: usize,

    /// Running count of total candles produced (never resets).
    /// Useful for logging and diagnostics.
    pub candles_produced: u64,
}

impl CandleAggregator {
    /// Creates a new aggregator for the given timeframe.
    ///
    /// `max_history`: how many completed candles to keep in the rolling
    /// window. Older candles are evicted when this limit is reached.
    pub fn new(timeframe: Timeframe, max_history: usize) -> Self {
        CandleAggregator {
            timeframe,
            current_candle: None,
            history: VecDeque::with_capacity(max_history),
            max_history,
            candles_produced: 0,
        }
    }

    /// Feed a trade into the aggregator. Returns Some(candle) if a candle
    /// just closed, None otherwise.
    ///
    /// THE CORE ALGORITHM:
    /// 1. Compute which candle period this trade belongs to (epoch alignment).
    /// 2. If no current candle exists → start one.
    /// 3. If the trade belongs to the current candle → update it.
    /// 4. If the trade belongs to a NEW period → close the old candle,
    ///    push it to history, start a new candle with this trade.
    ///
    /// Edge case: if there's a gap longer than one candle period (e.g. exchange
    /// downtime), we close the old candle and start fresh. We do NOT synthesize
    /// empty candles for the gap — that would introduce phantom data points
    /// that could mislead the swing detector and Fibonacci calculations.
    pub fn update(&mut self, trade: &TradeUpdate) -> Option<Candle> {
        let period_ms = self.timeframe.duration_ms();

        // --- Compute the candle boundary this trade belongs to ---
        // Integer division floors to the nearest period boundary.
        //
        // Example: trade at 1710000065123ms with 60000ms period
        //   1710000065123 / 60000 = 28500001 (integer division)
        //   28500001 * 60000 = 1710000060000  ← candle open time
        //   1710000060000 + 60000 = 1710000120000  ← candle close time
        let candle_open = (trade.timestamp_ms / period_ms) * period_ms;
        let candle_close = candle_open + period_ms;

        match &mut self.current_candle {
            // No current candle — this is the very first trade.
            None => {
                self.current_candle = Some(Candle::new(candle_open, candle_close, trade));
                None
            }

            // Current candle exists — does this trade belong to it?
            Some(current) => {
                if candle_open == current.open_time {
                    // Same period — just update the existing candle.
                    current.update(trade);
                    None
                } else {
                    // New period! The current candle is now complete.
                    //
                    // RUST CONCEPT — std::mem::replace:
                    // We need to simultaneously:
                    //   1. Take ownership of the old candle (to return it)
                    //   2. Replace it with a new candle (for the new period)
                    //
                    // `replace` does this atomically — it swaps the value
                    // behind the mutable reference and returns the old one.
                    // No cloning needed, no temporary Option dance.
                    let closed = std::mem::replace(
                        current,
                        Candle::new(candle_open, candle_close, trade),
                    );

                    // Push the closed candle to the history window.
                    self.push_to_history(closed.clone());

                    Some(closed)
                }
            }
        }
    }

    /// Push a completed candle to the history, evicting the oldest if full.
    fn push_to_history(&mut self, candle: Candle) {
        if self.history.len() >= self.max_history {
            self.history.pop_front(); // Evict oldest
        }
        self.history.push_back(candle);
        self.candles_produced += 1;
    }

    // ========================================================================
    // ACCESSORS — These are what your strategy modules will call.
    // ========================================================================

    /// The candle currently being built (not yet closed).
    /// Returns None only before the first trade arrives.
    pub fn current(&self) -> Option<&Candle> {
        self.current_candle.as_ref()
    }

    /// The most recently completed candle.
    /// Returns None if fewer than 1 candle has closed.
    pub fn last_closed(&self) -> Option<&Candle> {
        self.history.back()
    }

    /// The Nth most recent completed candle (0 = most recent).
    /// Returns None if the history doesn't go back that far.
    ///
    /// Example: candle_ago(2) returns the candle from 3 periods ago
    /// (0=last, 1=second-to-last, 2=third-to-last).
    pub fn candle_ago(&self, n: usize) -> Option<&Candle> {
        if n >= self.history.len() {
            return None;
        }
        // history is ordered oldest→newest, so index from the back.
        let idx = self.history.len() - 1 - n;
        self.history.get(idx)
    }

    /// How many completed candles are in the history window.
    pub fn history_len(&self) -> usize {
        self.history.len()
    }

    /// Full read-only access to the candle history.
    /// Ordered oldest (front) → newest (back).
    ///
    /// This is what the DSP pipeline will iterate over for FFT input,
    /// and what the swing detector will scan for local extrema.
    pub fn history(&self) -> &VecDeque<Candle> {
        &self.history
    }

    /// Extract a slice of the N most recent close prices.
    /// Convenience method for indicators that work on close-price arrays
    /// (moving averages, RSI, Bollinger bands, etc).
    ///
    /// Returns fewer than N if the history is shorter.
    pub fn recent_closes(&self, n: usize) -> Vec<f64> {
        let start = if n >= self.history.len() {
            0
        } else {
            self.history.len() - n
        };

        self.history
            .iter()
            .skip(start)
            .map(|c| c.close)
            .collect()
    }

    /// Extract a slice of the N most recent high prices.
    pub fn recent_highs(&self, n: usize) -> Vec<f64> {
        let start = if n >= self.history.len() {
            0
        } else {
            self.history.len() - n
        };

        self.history
            .iter()
            .skip(start)
            .map(|c| c.high)
            .collect()
    }

    /// Extract a slice of the N most recent low prices.
    pub fn recent_lows(&self, n: usize) -> Vec<f64> {
        let start = if n >= self.history.len() {
            0
        } else {
            self.history.len() - n
        };

        self.history
            .iter()
            .skip(start)
            .map(|c| c.low)
            .collect()
    }

    /// Extract a slice of the N most recent volumes.
    pub fn recent_volumes(&self, n: usize) -> Vec<f64> {
        let start = if n >= self.history.len() {
            0
        } else {
            self.history.len() - n
        };

        self.history
            .iter()
            .skip(start)
            .map(|c| c.volume)
            .collect()
    }

    /// The timeframe this aggregator is producing.
    pub fn timeframe(&self) -> Timeframe {
        self.timeframe
    }

    /// Returns true if the history window is fully populated.
    /// Strategies should check this before running lookback-dependent
    /// calculations to avoid operating on incomplete data.
    pub fn is_warmed_up(&self, min_candles: usize) -> bool {
        self.history.len() >= min_candles
    }
}

impl fmt::Display for CandleAggregator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "CandleAggregator[{}] history: {}/{}, produced: {}",
            self.timeframe,
            self.history.len(),
            self.max_history,
            self.candles_produced,
        )
    }
}

// ============================================================================
// MULTI-TIMEFRAME AGGREGATOR
// ============================================================================
// The ABC-BRC strategy operates across multiple timeframes (H4 for swing,
// H1 for entry timing, per the FMG guide). The DSP pipeline might want M1
// for high-resolution spectral data alongside H1 for trend context.
//
// MultiTimeframeAggregator fans each trade out to all registered timeframes
// simultaneously. One call to update() can produce candle closes at any
// combination of timeframes.
//
// USAGE:
//   let mut mtf = MultiTimeframeAggregator::new(vec![
//       (Timeframe::M1,  500),   // 500 x 1m candles ≈ 8.3 hours
//       (Timeframe::M15, 200),   // 200 x 15m candles ≈ 50 hours
//       (Timeframe::H1,  168),   // 168 x 1h candles = 7 days
//       (Timeframe::H4,  180),   // 180 x 4h candles = 30 days
//   ]);
//
//   // In your event loop:
//   let closes = mtf.update(&trade);
//   for (timeframe, candle) in &closes {
//       info!("Candle closed on {}: {}", timeframe, candle);
//   }

pub struct MultiTimeframeAggregator {
    aggregators: Vec<CandleAggregator>,
}

impl MultiTimeframeAggregator {
    /// Create a multi-timeframe aggregator.
    /// Each tuple is (Timeframe, max_history_for_that_timeframe).
    pub fn new(configs: Vec<(Timeframe, usize)>) -> Self {
        let aggregators = configs
            .into_iter()
            .map(|(tf, max)| CandleAggregator::new(tf, max))
            .collect();

        MultiTimeframeAggregator { aggregators }
    }

    /// Feed a trade to ALL timeframes. Returns a Vec of (Timeframe, Candle)
    /// for every timeframe that just closed a candle.
    ///
    /// Most ticks return an empty Vec. When an M1 candle closes, you get
    /// one entry. At the top of the hour, you might get M1 + M5 + M15 + H1
    /// all closing simultaneously.
    pub fn update(&mut self, trade: &TradeUpdate) -> Vec<(Timeframe, Candle)> {
        let mut closed = Vec::new();

        for agg in &mut self.aggregators {
            if let Some(candle) = agg.update(trade) {
                closed.push((agg.timeframe(), candle));
            }
        }

        closed
    }

    /// Get the aggregator for a specific timeframe.
    /// Returns None if that timeframe wasn't configured.
    pub fn get(&self, timeframe: Timeframe) -> Option<&CandleAggregator> {
        self.aggregators.iter().find(|a| a.timeframe() == timeframe)
    }

    /// Mutable access to a specific timeframe's aggregator.
    pub fn get_mut(&mut self, timeframe: Timeframe) -> Option<&mut CandleAggregator> {
        self.aggregators.iter_mut().find(|a| a.timeframe() == timeframe)
    }

    /// Status summary of all timeframes — useful for periodic logging.
    pub fn status_summary(&self) -> String {
        self.aggregators
            .iter()
            .map(|a| format!("{}:{}/{}", a.timeframe(), a.history_len(), a.candles_produced))
            .collect::<Vec<_>>()
            .join(" | ")
    }
}

// ============================================================================
// TESTS
// ============================================================================
// These tests verify the core aggregation logic using synthetic trades.
// Run with: cargo test
//
// RUST CONCEPT — #[cfg(test)]:
// This entire module is conditionally compiled. It only exists when you
// run `cargo test`. The production binary never includes test code.

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: create a synthetic trade at a given price and timestamp.
    fn trade(price: f64, timestamp_ms: u64) -> TradeUpdate {
        TradeUpdate {
            inst_id: "BTC-USDT-SWAP".to_string(),
            price,
            size: 0.1,
            side: "buy".to_string(),
            timestamp_ms,
        }
    }

    #[test]
    fn test_first_trade_creates_candle() {
        let mut agg = CandleAggregator::new(Timeframe::M1, 100);

        // First trade — should create a candle but NOT close one.
        let result = agg.update(&trade(67000.0, 1_710_000_065_000));
        assert!(result.is_none());
        assert!(agg.current().is_some());

        let current = agg.current().unwrap();
        assert_eq!(current.open, 67000.0);
        assert_eq!(current.high, 67000.0);
        assert_eq!(current.low, 67000.0);
        assert_eq!(current.close, 67000.0);

        // Verify UTC alignment: 1710000065000 / 60000 = 28500001
        // 28500001 * 60000 = 1710000060000
        assert_eq!(current.open_time, 1_710_000_060_000);
        assert_eq!(current.close_time, 1_710_000_120_000);
    }

    #[test]
    fn test_trades_within_same_candle() {
        let mut agg = CandleAggregator::new(Timeframe::M1, 100);
        let base = 1_710_000_060_000; // Exactly on a 1m boundary

        agg.update(&trade(67000.0, base + 1000));  // +1s
        agg.update(&trade(67100.0, base + 5000));  // +5s  (new high)
        agg.update(&trade(66900.0, base + 10000)); // +10s (new low)
        agg.update(&trade(67050.0, base + 30000)); // +30s (new close)

        let c = agg.current().unwrap();
        assert_eq!(c.open, 67000.0);
        assert_eq!(c.high, 67100.0);
        assert_eq!(c.low, 66900.0);
        assert_eq!(c.close, 67050.0);
        assert_eq!(c.trade_count, 4);
        assert_eq!(c.volume, 0.4); // 4 trades × 0.1
    }

    #[test]
    fn test_candle_closes_on_new_period() {
        let mut agg = CandleAggregator::new(Timeframe::M1, 100);
        let base = 1_710_000_060_000;

        // Trades in the first candle period
        agg.update(&trade(67000.0, base + 1000));
        agg.update(&trade(67200.0, base + 30000));

        // Trade in the NEXT period — should close the first candle
        let closed = agg.update(&trade(67150.0, base + 61000));

        assert!(closed.is_some());
        let closed = closed.unwrap();
        assert_eq!(closed.open, 67000.0);
        assert_eq!(closed.close, 67200.0); // Last trade in the period
        assert_eq!(closed.trade_count, 2);
        assert_eq!(agg.history_len(), 1);
        assert_eq!(agg.candles_produced, 1);

        // Current candle should now be the new period
        let current = agg.current().unwrap();
        assert_eq!(current.open, 67150.0);
        assert_eq!(current.trade_count, 1);
    }

    #[test]
    fn test_history_eviction() {
        let mut agg = CandleAggregator::new(Timeframe::M1, 3); // Only keep 3
        let period = 60_000;

        // Generate 5 candle periods (each with one trade)
        for i in 0..5 {
            let ts_in_period = (i * period) + 1000;
            let ts_next = ((i + 1) * period) + 1000;
            agg.update(&trade(67000.0 + i as f64, ts_in_period));
            agg.update(&trade(67000.0 + i as f64, ts_next)); // triggers close
        }

        // History should be capped at 3
        assert_eq!(agg.history_len(), 3);
        // 5 candle periods close: 0,1,2,3,4 (each iteration closes one)
        assert_eq!(agg.candles_produced, 5);

        // Periods 0 and 1 were evicted. Oldest remaining is period 2.
        // Period 2 was opened by the trade that closed period 1 (price 67001.0).
        let oldest = agg.history().front().unwrap();
        assert_eq!(oldest.open, 67001.0);
    }

    #[test]
    fn test_recent_closes() {
        let mut agg = CandleAggregator::new(Timeframe::M1, 100);
        let period = 60_000;

        // Build 5 completed candles with distinct close prices
        for i in 0..6 {
            let ts = (i * period) + 1000;
            agg.update(&trade(67000.0 + (i as f64 * 10.0), ts));
        }
        // After 6 trades in 6 periods, we have 5 closed candles

        let closes = agg.recent_closes(3);
        assert_eq!(closes.len(), 3);
        // Most recent 3 close prices (candles at index 2,3,4 closed with
        // prices 67020, 67030, 67040)
        assert_eq!(closes[0], 67020.0);
        assert_eq!(closes[1], 67030.0);
        assert_eq!(closes[2], 67040.0);
    }

    #[test]
    fn test_multi_timeframe() {
        let mut mtf = MultiTimeframeAggregator::new(vec![
            (Timeframe::M1, 100),
            (Timeframe::M5, 100),
        ]);

        let m1_period = 60_000;

        // Generate trades across 6 minutes — should close 5 M1 candles
        // and 1 M5 candle.
        let mut m1_closes = 0;
        let mut m5_closes = 0;

        for i in 0..7 {
            let ts = (i * m1_period) + 1000;
            let closed = mtf.update(&trade(67000.0, ts));
            for (tf, _candle) in &closed {
                match tf {
                    Timeframe::M1 => m1_closes += 1,
                    Timeframe::M5 => m5_closes += 1,
                    _ => {}
                }
            }
        }

        assert_eq!(m1_closes, 6);
        assert!(m5_closes >= 1);
    }

    #[test]
    fn test_candle_helpers() {
        let candle = Candle {
            open_time: 0,
            close_time: 60_000,
            open: 67000.0,
            high: 67500.0,
            low: 66800.0,
            close: 67300.0,
            volume: 1.5,
            trade_count: 42,
        };

        assert!(candle.is_bullish());
        assert!(!candle.is_bearish());
        assert_eq!(candle.body(), 300.0);       // |67300 - 67000|
        assert_eq!(candle.range(), 700.0);      // 67500 - 66800
        assert_eq!(candle.midpoint(), 67150.0); // (67500 + 66800) / 2
    }

    #[test]
    fn test_is_warmed_up() {
        let mut agg = CandleAggregator::new(Timeframe::M1, 100);
        assert!(!agg.is_warmed_up(5));

        let period = 60_000;
        for i in 0..6 {
            agg.update(&trade(67000.0, (i * period) + 1000));
        }

        assert!(agg.is_warmed_up(5));
        assert!(!agg.is_warmed_up(10));
    }
}
