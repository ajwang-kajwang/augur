// src/swing.rs
//
// ============================================================================
// WHY THIS MODULE EXISTS
// ============================================================================
//
// The FMG ABC-BRC strategy operates on price geometry: impulse legs from A→B,
// corrective pullbacks from B→C, and targets projected to D. Every point in
// that sequence is a SWING POINT — a local high or low in the candle history
// that marks a structural pivot.
//
// This module scans candle histories (produced by the candle engine) and
// identifies those pivots. The output is a time-ordered sequence of
// SwingPoints that downstream modules will consume:
//
//   - ABCD pattern recognizer: matches 4 alternating swings to A-B-C-D shape
//   - Fibonacci calculator: anchors retracement levels between two swings
//   - Market structure classifier: reads the sequence to determine HH/HL/LH/LL
//
// ============================================================================
// THE PIVOT DETECTION ALGORITHM
// ============================================================================
//
// A candle at index `i` is a SWING HIGH if its `high` is STRICTLY greater
// than the `high` of the `lookback` candles before it AND the `lookback`
// candles after it. The same logic inverted (on `low` with strict less-than)
// defines a SWING LOW.
//
// Example with lookback=3 and candle highs [102, 103, 104, 108, 106, 105, 103]:
//   Index 3 (high=108) is a swing HIGH because:
//     - Before: 102 < 108, 103 < 108, 104 < 108  ✓
//     - After:  106 < 108, 105 < 108, 103 < 108  ✓
//
// KEY PROPERTY: Swings are confirmed with a LAG of `lookback` candles. When
// candle T closes, we can only confirm swings up to candle T - lookback,
// because we need `lookback` candles of forward confirmation. This is
// fundamental to pivot detection and cannot be eliminated without lying.
//
// STRICT INEQUALITY (`>` not `>=`): If two consecutive candles print the
// same exact high, NEITHER qualifies as a swing high. This prevents spurious
// pivots in flat/ranging periods where the same extreme is touched multiple
// times. A "double top" fails to confirm by design — we wait for a clearer
// structural break.
//
// ============================================================================
// INCREMENTAL SCANNING
// ============================================================================
//
// Naive approach: re-scan the entire candle history on every call, re-detect
// the same old swings. Wasteful. Our approach: track the timestamp of the
// most recent candle we've evaluated, and only check candles newer than that.
//
// Why track by TIMESTAMP and not INDEX: the candle history is a VecDeque
// with eviction — the oldest candle drops off the front when the history
// fills up. An index would become invalid after eviction. A timestamp is
// stable across the lifetime of the system.
//
// ============================================================================

use std::collections::VecDeque;
use std::fmt;
use crate::candle::Candle;

// ============================================================================
// SWING TYPE
// ============================================================================
// A swing is either a local high (pivot top) or a local low (pivot bottom).
// Derived Copy because it's a tag — no heap data, cheap to pass around.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwingType {
    High,
    Low,
}

impl fmt::Display for SwingType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SwingType::High => write!(f, "HIGH"),
            SwingType::Low  => write!(f, "LOW"),
        }
    }
}

// ============================================================================
// SWING POINT
// ============================================================================
// A confirmed pivot — the candle's timestamp, the pivot price (high for a
// swing high, low for a swing low), and the type.
//
// This is the atomic unit the ABCD recognizer consumes. The FMG guide's
// A, B, C, D labels are just swings with a specific ordering constraint —
// A and C are the same type, B and D are the same type, and the types
// alternate.

#[derive(Debug, Clone)]
pub struct SwingPoint {
    /// Open time of the candle where this swing was formed, in epoch ms.
    /// Use this as the time axis when plotting or cross-referencing candles.
    pub timestamp: u64,
    /// The pivot price. For a High, this is the candle's `high`.
    /// For a Low, this is the candle's `low`.
    pub price: f64,
    pub swing_type: SwingType,
}
#[allow(dead_code)]
impl SwingPoint {
    pub fn is_high(&self) -> bool { matches!(self.swing_type, SwingType::High) }
    pub fn is_low(&self) -> bool { matches!(self.swing_type, SwingType::Low) }
}

impl fmt::Display for SwingPoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} @ ${:.2} [ts={}]", self.swing_type, self.price, self.timestamp)
    }
}

// ============================================================================
// SWING DETECTOR
// ============================================================================
//
// USAGE PATTERN:
//   // At startup, create a detector with a chosen lookback and history cap.
//   let mut detector = SwingDetector::new(3, 50);
//
//   // Each time a candle closes on the associated timeframe, call update().
//   // Pass the FULL candle history (VecDeque from the aggregator).
//   let new_swings = detector.update(agg.history());
//   for swing in &new_swings {
//       println!("Confirmed: {}", swing);
//   }
//
// TUNING THE LOOKBACK:
//   - lookback=2  → very sensitive, many pivots, noisy on M1/M5
//   - lookback=3  → moderate, default for H1/H4 in ABC-BRC
//   - lookback=5  → conservative, only significant structural pivots
//   - lookback=10 → only major multi-day swings on H1
//
// Higher lookback = fewer swings, more lag, more reliable.
// The right value depends on the timeframe and strategy. For H1 and H4 on
// BTC perpetuals, lookback=3 produces swings that align well with the
// kind of structural pivots the FMG guide's charts illustrate.

pub struct SwingDetector {
    /// Number of candles of confirmation required on each side of a pivot.
    lookback: usize,

    /// Confirmed swings, oldest at the front, newest at the back.
    /// We use VecDeque for the same reason candle.rs does — O(1) front
    /// eviction when we exceed the history cap.
    confirmed: VecDeque<SwingPoint>,

    /// Maximum number of confirmed swings to retain. When exceeded, the
    /// oldest swing is evicted. For ABC-BRC, 50 swings on H1 covers
    /// multiple days of structural context — more than enough.
    max_swings: usize,

    /// Open_time of the most recent candle we've evaluated as a pivot
    /// candidate. On the next update() call, we only check candles with
    /// timestamps strictly greater than this.
    ///
    /// Why Option<u64>: before the first call, we've analyzed nothing.
    /// None means "check everything in the history that's confirmable."
    last_analyzed_time: Option<u64>,
}
#[allow(dead_code)]
impl SwingDetector {
    /// Create a new detector.
    ///
    /// `lookback`: candles of confirmation required on each side.
    /// `max_swings`: rolling history cap; older swings are evicted.
    ///
    /// Panics if lookback == 0 (would make every candle a swing — useless).
    pub fn new(lookback: usize, max_swings: usize) -> Self {
        assert!(lookback > 0, "SwingDetector lookback must be >= 1");
        SwingDetector {
            lookback,
            confirmed: VecDeque::with_capacity(max_swings),
            max_swings,
            last_analyzed_time: None,
        }
    }

    /// Scan the candle history for newly-confirmable swings.
    ///
    /// Returns the Vec of swings discovered in this call (may be empty).
    /// These swings are also pushed into the detector's internal history,
    /// accessible via `confirmed()` and friends.
    ///
    /// PERFORMANCE: O(k * lookback) where k is the number of new candidates
    /// to evaluate since the last call. In steady state, that's typically
    /// just 1 candidate per call — the candle at position (len - lookback - 1)
    /// that just became eligible for confirmation.
    pub fn update(&mut self, history: &VecDeque<Candle>) -> Vec<SwingPoint> {
        let mut new_swings = Vec::new();

        // Need at least 2*lookback+1 candles to confirm even one swing:
        // `lookback` candles before + 1 candidate + `lookback` candles after.
        if history.len() < 2 * self.lookback + 1 {
            return new_swings;
        }

        // Valid candidate indices are [lookback, len - lookback).
        // Anything outside that range doesn't have enough neighbours to
        // confirm.
        let first_candidate = self.lookback;
        let last_candidate  = history.len() - self.lookback;

        // Track the newest timestamp we examine so we can update our cursor
        // after the loop. If no candidates are examined, cursor is unchanged.
        let mut newest_examined: Option<u64> = None;

        for i in first_candidate..last_candidate {
            let candidate = &history[i];

            // Skip candidates we've already analyzed on a previous call.
            if let Some(cutoff) = self.last_analyzed_time {
                if candidate.open_time <= cutoff {
                    continue;
                }
            }

            newest_examined = Some(candidate.open_time);

            // Check for swing HIGH: candidate's high must be strictly greater
            // than all `lookback` candles on each side.
            //
            // `(i - self.lookback)..i` is the window before (exclusive of i).
            // `(i + 1)..=(i + self.lookback)` is the window after (inclusive
            // of i + lookback — note the `=` in the range).
            let is_high = ((i - self.lookback)..i)
                .all(|j| history[j].high < candidate.high)
                && ((i + 1)..=(i + self.lookback))
                    .all(|j| history[j].high < candidate.high);

            if is_high {
                let swing = SwingPoint {
                    timestamp: candidate.open_time,
                    price: candidate.high,
                    swing_type: SwingType::High,
                };
                self.push(swing.clone());
                new_swings.push(swing);
                continue; // Can't be both high and low at the same candle.
            }

            // Check for swing LOW: candidate's low must be strictly less
            // than all `lookback` candles on each side.
            let is_low = ((i - self.lookback)..i)
                .all(|j| history[j].low > candidate.low)
                && ((i + 1)..=(i + self.lookback))
                    .all(|j| history[j].low > candidate.low);

            if is_low {
                let swing = SwingPoint {
                    timestamp: candidate.open_time,
                    price: candidate.low,
                    swing_type: SwingType::Low,
                };
                self.push(swing.clone());
                new_swings.push(swing);
            }
        }

        // Advance the cursor to the newest candidate we examined. We use
        // `.or(self.last_analyzed_time)` to preserve the existing cursor
        // if no candidates were examined this call.
        if let Some(t) = newest_examined {
            self.last_analyzed_time = Some(t);
        }

        new_swings
    }

    /// Internal: push a new swing, evicting the oldest if at capacity.
    fn push(&mut self, swing: SwingPoint) {
        if self.confirmed.len() >= self.max_swings {
            self.confirmed.pop_front();
        }
        self.confirmed.push_back(swing);
    }

    // ========================================================================
    // READ API — what strategy modules consume
    // ========================================================================

    /// All confirmed swings in chronological order (oldest first).
    pub fn confirmed(&self) -> &VecDeque<SwingPoint> {
        &self.confirmed
    }

    /// The most recent confirmed swing of any type.
    pub fn last(&self) -> Option<&SwingPoint> {
        self.confirmed.back()
    }

    /// The most recent confirmed swing HIGH.
    /// Useful for anchoring Fibonacci retracement from the last pivot top.
    pub fn last_high(&self) -> Option<&SwingPoint> {
        self.confirmed.iter().rev().find(|s| s.is_high())
    }

    /// The most recent confirmed swing LOW.
    /// Useful for anchoring Fibonacci retracement from the last pivot bottom.
    pub fn last_low(&self) -> Option<&SwingPoint> {
        self.confirmed.iter().rev().find(|s| s.is_low())
    }

    /// The most recent N swings of any type, newest-last (chronological).
    /// Returns fewer than N if the history doesn't go back that far.
    ///
    /// This is the accessor the ABCD pattern recognizer will use — it needs
    /// the last 4 swings and checks whether they alternate in type.
    pub fn recent(&self, n: usize) -> Vec<&SwingPoint> {
        let start = self.confirmed.len().saturating_sub(n);
        self.confirmed.iter().skip(start).collect()
    }

    /// Number of confirmed swings held in the history.
    pub fn len(&self) -> usize {
        self.confirmed.len()
    }

    pub fn is_empty(&self) -> bool {
        self.confirmed.is_empty()
    }

    /// The lookback setting this detector was created with.
    pub fn lookback(&self) -> usize {
        self.lookback
    }
}

impl fmt::Display for SwingDetector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SwingDetector[lookback={}] confirmed: {}/{}",
            self.lookback, self.confirmed.len(), self.max_swings)
    }
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthesize a candle with the specified high and low (open/close
    /// don't matter for swing detection).
    fn candle(open_time: u64, high: f64, low: f64) -> Candle {
        Candle {
            open_time,
            close_time: open_time + 3_600_000, // arbitrary 1h; irrelevant here
            open: (high + low) / 2.0,
            close: (high + low) / 2.0,
            high,
            low,
            volume: 1.0,
            trade_count: 1,
        }
    }

    /// Build a candle history from (high, low) pairs. Timestamps
    /// increment by 1h starting at 0.
    fn build(bars: &[(f64, f64)]) -> VecDeque<Candle> {
        bars.iter().enumerate()
            .map(|(i, (h, l))| candle(i as u64 * 3_600_000, *h, *l))
            .collect()
    }

    #[test]
    fn test_not_enough_history() {
        // lookback=3 requires at least 7 candles for one swing candidate.
        let mut det = SwingDetector::new(3, 50);
        let hist = build(&[(100.0, 90.0), (101.0, 91.0), (102.0, 92.0)]);
        let swings = det.update(&hist);
        assert!(swings.is_empty());
        assert_eq!(det.len(), 0);
    }

    #[test]
    fn test_simple_swing_high() {
        // 5-candle history with lookback=2. Index 2 (high=110) is the peak.
        //   highs: 100, 105, 110, 104, 102
        let mut det = SwingDetector::new(2, 50);
        let hist = build(&[
            (100.0, 90.0),
            (105.0, 95.0),
            (110.0, 100.0),  // ← swing high here
            (104.0, 94.0),
            (102.0, 92.0),
        ]);
        let swings = det.update(&hist);
        assert_eq!(swings.len(), 1);
        assert_eq!(swings[0].swing_type, SwingType::High);
        assert_eq!(swings[0].price, 110.0);
        assert_eq!(swings[0].timestamp, 2 * 3_600_000);
    }

    #[test]
    fn test_simple_swing_low() {
        // Inverted: lows dip at index 2.
        //   lows: 100, 95, 85, 92, 98
        let mut det = SwingDetector::new(2, 50);
        let hist = build(&[
            (110.0, 100.0),
            (105.0, 95.0),
            (95.0, 85.0),   // ← swing low here
            (100.0, 92.0),
            (108.0, 98.0),
        ]);
        let swings = det.update(&hist);
        assert_eq!(swings.len(), 1);
        assert_eq!(swings[0].swing_type, SwingType::Low);
        assert_eq!(swings[0].price, 85.0);
    }

    #[test]
    fn test_no_swing_on_edges() {
        // With lookback=2, indices 0, 1, 3, 4 can NEVER be swings regardless
        // of their values — they don't have enough neighbours. Only index 2
        // is a valid candidate.
        //
        // Here we put an extreme high at the END of the history — it should
        // NOT be detected as a swing because there aren't 2 candles after it.
        let mut det = SwingDetector::new(2, 50);
        let hist = build(&[
            (100.0, 90.0),
            (101.0, 91.0),
            (102.0, 92.0),  // middle — not higher than 200.0 below
            (103.0, 93.0),
            (200.0, 90.0),  // extreme, but no forward confirmation possible
        ]);
        let swings = det.update(&hist);
        assert_eq!(swings.len(), 0);
    }

    #[test]
    fn test_strict_inequality() {
        // Two CONSECUTIVE candles with identical highs and lows. Because
        // comparison is strict (>), neither candle can qualify as a swing
        // against its tied neighbour. And the bars around them can't either.
        let mut det = SwingDetector::new(1, 50);
        let hist = build(&[
            (100.0, 90.0),   // 0
            (105.0, 95.0),   // 1 — ties with index 2
            (105.0, 95.0),   // 2 — ties with index 1
            (103.0, 93.0),   // 3
            (100.0, 90.0),   // 4
        ]);
        let swings = det.update(&hist);
        // With strict inequality, no candle is unambiguously the highest
        // or lowest among its neighbours. No swings should confirm.
        assert!(swings.is_empty(),
            "strict inequality should reject ties, got {:?}", swings);
    }

    #[test]
    fn test_alternating_swings() {
        // An idealized ABCD-style sequence with lookback=1 and clearly
        // separated pivots:
        //   highs: 100, 110, 102, 115, 108, 120, 104
        //   lows:   95, 105,  92, 110,  98, 115,  96
        // Swing highs expected at indices 1 (110), 3 (115), 5 (120)
        // Swing lows expected at indices 2 (92), 4 (98)
        let mut det = SwingDetector::new(1, 50);
        let hist = build(&[
            (100.0,  95.0),  // 0
            (110.0, 105.0),  // 1 — high
            (102.0,  92.0),  // 2 — low
            (115.0, 110.0),  // 3 — high
            (108.0,  98.0),  // 4 — low
            (120.0, 115.0),  // 5 — high
            (104.0,  96.0),  // 6
        ]);
        let swings = det.update(&hist);
        assert_eq!(swings.len(), 5);
        assert_eq!(swings[0].swing_type, SwingType::High);
        assert_eq!(swings[0].price, 110.0);
        assert_eq!(swings[1].swing_type, SwingType::Low);
        assert_eq!(swings[1].price, 92.0);
        assert_eq!(swings[2].swing_type, SwingType::High);
        assert_eq!(swings[2].price, 115.0);
        assert_eq!(swings[3].swing_type, SwingType::Low);
        assert_eq!(swings[3].price, 98.0);
        assert_eq!(swings[4].swing_type, SwingType::High);
        assert_eq!(swings[4].price, 120.0);
    }

    #[test]
    fn test_incremental_update_no_redetection() {
        // Call update() once with a small history. Call again with a
        // larger history that includes the same old swing PLUS a new one.
        // The first swing must NOT be re-reported by the second call.
        //
        // Data is carefully chosen so that no INTERMEDIATE candles (e.g.
        // index 2 in the extended history) accidentally qualify as swings:
        //   - All lows are monotone downward until the new pivot
        //   - Only the intended pivots (indices 1 and 3) are local extrema
        let mut det = SwingDetector::new(1, 50);

        let hist_a = build(&[
            (100.0, 90.0),
            (110.0, 99.0),   // swing high at index 1
            (108.0, 100.0),
        ]);
        let first = det.update(&hist_a);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].price, 110.0);
        assert_eq!(det.len(), 1);

        // Extend the history. Index 2 (108, 100) is NOT a pivot because
        // its low (100) is higher than index 1's low (99) — fails the
        // strict-less-than check for a swing low.
        let hist_b = build(&[
            (100.0,  90.0),  // 0
            (110.0,  99.0),  // 1 — old swing, already reported
            (108.0, 100.0),  // 2 — NOT a pivot (not lowest, not highest)
            (115.0, 105.0),  // 3 — new swing high
            (112.0, 102.0),  // 4
        ]);
        let second = det.update(&hist_b);
        // Only the NEW swing should be reported on the second call.
        assert_eq!(second.len(), 1, "expected only the new swing, got {:?}", second);
        assert_eq!(second[0].price, 115.0);
        assert_eq!(det.len(), 2);
    }

    #[test]
    fn test_eviction_at_capacity() {
        // With max_swings=2, the third swing should evict the first.
        let mut det = SwingDetector::new(1, 2);
        let hist = build(&[
            (100.0,  95.0),  // 0
            (110.0, 105.0),  // 1 — high (will be evicted)
            (102.0,  92.0),  // 2 — low
            (115.0, 110.0),  // 3 — high
            (108.0,  98.0),  // 4 — low (will cause eviction of index 1)
            (120.0, 115.0),  // 5 — high (will cause further eviction)
            (104.0,  96.0),  // 6
        ]);
        det.update(&hist);
        // Only the most recent 2 swings remain.
        assert_eq!(det.len(), 2);
        let remaining: Vec<_> = det.confirmed().iter().collect();
        assert_eq!(remaining[0].price, 98.0);  // swing low at index 4
        assert_eq!(remaining[1].price, 120.0); // swing high at index 5
    }

    #[test]
    fn test_last_high_last_low_accessors() {
        // Four swings in sequence: high → low → high → low.
        // Each pivot is a clear local extremum with no accidental rivals.
        let mut det = SwingDetector::new(1, 50);
        let hist = build(&[
            (100.0,  95.0),  // 0
            (110.0, 105.0),  // 1 — swing HIGH (110)
            (102.0,  92.0),  // 2 — swing LOW (92)
            (115.0, 110.0),  // 3 — swing HIGH (115) — most recent high
            (108.0,  98.0),  // 4 — swing LOW (98)  — most recent low
            (112.0, 102.0),  // 5 — note: low 102 > 98, so index 4 stays the low
        ]);
        det.update(&hist);
        assert_eq!(det.last_high().unwrap().price, 115.0);
        assert_eq!(det.last_low().unwrap().price, 98.0);
        // last() returns the most recent swing of any type — the low at 98.
        assert_eq!(det.last().unwrap().price, 98.0);
    }

    #[test]
    fn test_recent_accessor() {
        let mut det = SwingDetector::new(1, 50);
        let hist = build(&[
            (100.0,  95.0),
            (110.0, 105.0),  // high
            (102.0,  92.0),  // low
            (115.0, 110.0),  // high
            (108.0,  98.0),  // low
            (120.0, 115.0),  // high
            (104.0,  96.0),
        ]);
        det.update(&hist);
        // Last 3 should be low(92), high(115), low(98), high(120)... wait,
        // there are 5 swings total. Last 3: high(115), low(98), high(120).
        let last_three = det.recent(3);
        assert_eq!(last_three.len(), 3);
        assert_eq!(last_three[0].price, 115.0);
        assert_eq!(last_three[1].price, 98.0);
        assert_eq!(last_three[2].price, 120.0);

        // recent(100) should return all 5.
        assert_eq!(det.recent(100).len(), 5);
    }

    #[test]
    #[should_panic(expected = "lookback must be >= 1")]
    fn test_zero_lookback_panics() {
        SwingDetector::new(0, 50);
    }
}
