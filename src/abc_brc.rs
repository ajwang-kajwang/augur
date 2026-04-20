// src/abc_brc.rs
//
// ============================================================================
// WHY THIS MODULE EXISTS
// ============================================================================
//
// This is where structural pivots become TRADE SIGNALS. The swing detector
// (src/swing.rs) identifies raw pivots; this module examines the recent
// swing history for two geometric shapes the FMG strategy uses:
//
//   1. ABCD PATTERN — impulse leg + corrective leg.
//      Identified from the last 3 swings in alternating types.
//      A→B is impulse, B→C is correction. D is a PROJECTED target
//      computed via Fibonacci extensions (see fibonacci.rs).
//
//   2. BRC PATTERN — Break of level, Retest, Continuation.
//      Prior structural level taken out; price returns to retest it
//      as inverted support/resistance.
//
// Combining ABCD and BRC at the same zone per the FMG guide increases
// setup probability.

use std::fmt;
use crate::swing::{SwingDetector, SwingPoint, SwingType};

// ============================================================================
// PATTERN DIRECTION
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternDirection {
    Bullish,
    Bearish,
}

impl fmt::Display for PatternDirection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PatternDirection::Bullish => write!(f, "BULLISH"),
            PatternDirection::Bearish => write!(f, "BEARISH"),
        }
    }
}

// ============================================================================
// ABCD PATTERN
// ============================================================================
//
// BEARISH: A(high) → B(low) → C(high, lower than A). Target D below B.
// BULLISH: A(low) → B(high) → C(low, higher than A). Target D above B.

#[derive(Debug, Clone)]
pub struct AbcdPattern {
    pub direction: PatternDirection,
    pub a: SwingPoint,
    pub b: SwingPoint,
    pub c: SwingPoint,
}

impl AbcdPattern {
    pub fn ab_range(&self) -> f64 { (self.b.price - self.a.price).abs() }
    pub fn bc_range(&self) -> f64 { (self.c.price - self.b.price).abs() }

    pub fn c_retracement(&self) -> f64 {
        let ab = self.ab_range();
        if ab > 0.0 { self.bc_range() / ab } else { 0.0 }
    }

    pub fn is_bullish(&self) -> bool { matches!(self.direction, PatternDirection::Bullish) }
    pub fn is_bearish(&self) -> bool { matches!(self.direction, PatternDirection::Bearish) }
}

impl fmt::Display for AbcdPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ABCD A:${:.2} B:${:.2} C:${:.2} (retrace {:.1}%)",
            self.direction, self.a.price, self.b.price, self.c.price,
            self.c_retracement() * 100.0)
    }
}

/// Detect an ABCD in the 3 most recent confirmed swings.
pub fn detect_abcd(detector: &SwingDetector) -> Option<AbcdPattern> {
    let swings = detector.recent(3);
    if swings.len() < 3 { return None; }

    let a = swings[0].clone();
    let b = swings[1].clone();
    let c = swings[2].clone();

    if a.swing_type == b.swing_type || b.swing_type == c.swing_type {
        return None;
    }

    let direction = match (a.swing_type, b.swing_type, c.swing_type) {
        (SwingType::High, SwingType::Low, SwingType::High) => {
            if a.price > b.price && c.price > b.price && c.price < a.price {
                PatternDirection::Bearish
            } else { return None; }
        }
        (SwingType::Low, SwingType::High, SwingType::Low) => {
            if a.price < b.price && c.price < b.price && c.price > a.price {
                PatternDirection::Bullish
            } else { return None; }
        }
        _ => return None,
    };

    Some(AbcdPattern { direction, a, b, c })
}

// ============================================================================
// BRC PATTERN
// ============================================================================

#[derive(Debug, Clone)]
pub struct BrcPattern {
    pub direction: PatternDirection,
    pub level: SwingPoint,
    pub mid: SwingPoint,
    pub break_point: SwingPoint,
    pub retest: SwingPoint,
}

impl BrcPattern {
    pub fn break_depth(&self) -> f64 {
        (self.break_point.price - self.level.price).abs()
    }

    pub fn retest_accuracy(&self) -> f64 {
        let depth = self.break_depth();
        if depth <= 0.0 { return 1.0; }
        (self.retest.price - self.level.price).abs() / depth
    }

    pub fn is_bullish(&self) -> bool { matches!(self.direction, PatternDirection::Bullish) }
    pub fn is_bearish(&self) -> bool { matches!(self.direction, PatternDirection::Bearish) }
}

impl fmt::Display for BrcPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} BRC level:${:.2} break:${:.2} retest:${:.2} (accuracy {:.1}%)",
            self.direction, self.level.price, self.break_point.price,
            self.retest.price, self.retest_accuracy() * 100.0)
    }
}

pub const DEFAULT_BRC_RETEST_RATIO: f64 = 0.30;

pub fn detect_brc(detector: &SwingDetector, max_retest_ratio: f64) -> Option<BrcPattern> {
    let swings = detector.recent(4);
    if swings.len() < 4 { return None; }

    let s0 = swings[0].clone();
    let s1 = swings[1].clone();
    let s2 = swings[2].clone();
    let s3 = swings[3].clone();

    if s0.swing_type == s1.swing_type
        || s1.swing_type == s2.swing_type
        || s2.swing_type == s3.swing_type
    {
        return None;
    }

    let direction = match s0.swing_type {
        SwingType::Low => {
            if s2.price >= s0.price { return None; }
            PatternDirection::Bearish
        }
        SwingType::High => {
            if s2.price <= s0.price { return None; }
            PatternDirection::Bullish
        }
    };

    let break_depth = (s2.price - s0.price).abs();
    if break_depth <= 0.0 { return None; }
    let retest_distance = (s3.price - s0.price).abs();
    if retest_distance / break_depth > max_retest_ratio { return None; }

    Some(BrcPattern {
        direction,
        level: s0,
        mid: s1,
        break_point: s2,
        retest: s3,
    })
}

pub fn detect_brc_default(detector: &SwingDetector) -> Option<BrcPattern> {
    detect_brc(detector, DEFAULT_BRC_RETEST_RATIO)
}

// ============================================================================
// COMBINED
// ============================================================================

#[derive(Debug, Clone)]
pub struct CombinedPattern {
    pub abcd: AbcdPattern,
    pub brc: BrcPattern,
}

impl CombinedPattern {
    pub fn direction(&self) -> PatternDirection { self.abcd.direction }
}

pub fn detect_combined(
    detector: &SwingDetector,
    max_retest_ratio: f64,
) -> Option<CombinedPattern> {
    let abcd = detect_abcd(detector)?;
    let brc = detect_brc(detector, max_retest_ratio)?;
    if abcd.direction != brc.direction { return None; }
    Some(CombinedPattern { abcd, brc })
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candle::Candle;
    use std::collections::VecDeque;

    fn candle(open_time: u64, high: f64, low: f64) -> Candle {
        Candle {
            open_time,
            close_time: open_time + 3_600_000,
            open: (high + low) / 2.0,
            close: (high + low) / 2.0,
            high, low,
            volume: 1.0,
            trade_count: 1,
        }
    }

    fn build(bars: &[(f64, f64)]) -> VecDeque<Candle> {
        bars.iter().enumerate()
            .map(|(i, (h, l))| candle(i as u64 * 3_600_000, *h, *l))
            .collect()
    }

    fn detector_from(bars: &[(f64, f64)], lookback: usize) -> SwingDetector {
        let mut det = SwingDetector::new(lookback, 100);
        det.update(&build(bars));
        det
    }

    #[test]
    fn test_abcd_insufficient_swings() {
        let det = detector_from(
            &[(100.0, 95.0), (110.0, 100.0), (105.0, 92.0), (103.0, 93.0)],
            1,
        );
        assert!(detect_abcd(&det).is_none());
    }

    #[test]
    fn test_bullish_abcd_detected() {
        let det = detector_from(
            &[
                (98.0,  95.0),
                (100.0, 90.0),   // LOW (A)
                (115.0, 105.0),
                (120.0, 110.0),  // HIGH (B)
                (110.0, 105.0),
                (105.0, 100.0),  // LOW (C)
                (113.0, 108.0),
            ],
            1,
        );
        let pattern = detect_abcd(&det).expect("bullish ABCD should be detected");
        assert!(pattern.is_bullish());
        assert_eq!(pattern.a.price, 90.0);
        assert_eq!(pattern.b.price, 120.0);
        assert_eq!(pattern.c.price, 100.0);
    }

    #[test]
    fn test_bearish_abcd_detected() {
        let det = detector_from(
            &[
                (115.0, 110.0),
                (120.0, 115.0),  // HIGH (A)
                (100.0,  95.0),
                (95.0,   90.0),  // LOW (B)
                (100.0,  97.0),
                (105.0, 100.0),  // HIGH (C)
                (102.0,  98.0),
            ],
            1,
        );
        let pattern = detect_abcd(&det).expect("bearish ABCD should be detected");
        assert!(pattern.is_bearish());
        assert_eq!(pattern.a.price, 120.0);
        assert_eq!(pattern.b.price, 90.0);
        assert_eq!(pattern.c.price, 105.0);
    }

    #[test]
    fn test_abcd_rejects_invalid_geometry() {
        // H-L-H but C > A (invalid bearish)
        let det = detector_from(
            &[
                (110.0, 105.0),
                (115.0, 110.0),  // A = 115
                (100.0,  95.0),
                (95.0,   90.0),  // B = 90
                (115.0, 110.0),
                (120.0, 115.0),  // C = 120 > A
                (115.0, 112.0),
            ],
            1,
        );
        assert!(detect_abcd(&det).is_none());
    }

    #[test]
    fn test_abcd_retracement_computation() {
        // Swing detection uses candle HIGHS and LOWS:
        //   A = low of candle 1 = 100
        //   B = high of candle 3 = 155  (not 150!)
        //   C = low of candle 5 = 120
        //   ab_range = |155 - 100| = 55
        //   bc_range = |155 - 120| = 35
        //   retracement = 35 / 55 ≈ 0.6364
        let det = detector_from(
            &[
                (108.0, 103.0),
                (110.0, 100.0),  // A (low) = 100
                (145.0, 140.0),
                (155.0, 150.0),  // B (high) = 155
                (130.0, 125.0),
                (125.0, 120.0),  // C (low) = 120
                (135.0, 128.0),
            ],
            1,
        );
        let pattern = detect_abcd(&det).unwrap();
        assert!((pattern.ab_range() - 55.0).abs() < 1e-9);
        assert!((pattern.bc_range() - 35.0).abs() < 1e-9);
        assert!((pattern.c_retracement() - 35.0 / 55.0).abs() < 1e-9);
    }

    #[test]
    fn test_bearish_brc_detected() {
        let det = detector_from(
            &[
                (105.0, 102.0),
                (102.0, 100.0),  // LOW S0 (level=100)
                (115.0, 112.0),
                (118.0, 115.0),  // HIGH S1
                (95.0,  92.0),
                (93.0,  90.0),   // LOW S2 (break=90)
                (100.0,  97.0),
                (101.0,  98.0),  // HIGH S3 (retest=101)
                (98.0,   95.0),
            ],
            1,
        );
        let pattern = detect_brc_default(&det).expect("bearish BRC should be detected");
        assert!(pattern.is_bearish());
        assert_eq!(pattern.level.price, 100.0);
        assert_eq!(pattern.break_point.price, 90.0);
        assert_eq!(pattern.retest.price, 101.0);
        assert!((pattern.retest_accuracy() - 0.10).abs() < 1e-9);
    }

    #[test]
    fn test_bullish_brc_detected() {
        let det = detector_from(
            &[
                (99.0,  95.0),
                (100.0, 98.0),   // HIGH S0 (level=100)
                (90.0,  85.0),
                (88.0,  82.0),   // LOW S1
                (108.0, 103.0),
                (110.0, 105.0),  // HIGH S2 (break=110)
                (102.0, 99.0),
                (101.0, 98.0),   // LOW S3 (retest=98)
                (105.0, 100.0),
            ],
            1,
        );
        let pattern = detect_brc_default(&det).expect("bullish BRC should be detected");
        assert!(pattern.is_bullish());
        assert_eq!(pattern.level.price, 100.0);
        assert_eq!(pattern.break_point.price, 110.0);
        assert_eq!(pattern.retest.price, 98.0);
        assert!((pattern.retest_accuracy() - 0.20).abs() < 1e-9);
    }

    #[test]
    fn test_brc_rejects_weak_break() {
        let det = detector_from(
            &[
                (105.0, 100.0),
                (103.0,  99.0),  // LOW S0=99
                (115.0, 112.0),
                (118.0, 115.0),  // HIGH S1
                (110.0, 105.0),
                (105.0, 100.0),  // LOW S2=100, NOT below 99
                (115.0, 110.0),
                (113.0, 108.0),
            ],
            1,
        );
        assert!(detect_brc_default(&det).is_none());
    }

    #[test]
    fn test_brc_rejects_distant_retest() {
        let det = detector_from(
            &[
                (105.0, 102.0),
                (102.0, 100.0),  // S0=100
                (115.0, 112.0),
                (118.0, 115.0),  // S1
                (95.0,  92.0),
                (93.0,  90.0),   // S2=90
                (105.0, 103.0),
                (106.0, 104.0),  // S3=106 (retest dist 6 / depth 10 = 60%)
                (99.0,  96.0),
            ],
            1,
        );
        assert!(detect_brc_default(&det).is_none());
        assert!(detect_brc(&det, 0.70).is_some());
    }

    #[test]
    fn test_brc_insufficient_swings() {
        let det = detector_from(
            &[
                (105.0, 100.0),
                (102.0, 98.0),
                (115.0, 112.0),
                (118.0, 115.0),
                (95.0,  92.0),
                (93.0,  90.0),
                (100.0, 97.0),
            ],
            1,
        );
        assert_eq!(det.len(), 3);
        assert!(detect_brc_default(&det).is_none());
    }

    #[test]
    fn test_combined_requires_both_patterns() {
        let det = detector_from(
            &[
                (98.0,  95.0),
                (100.0, 90.0),   // LOW (A)
                (115.0, 105.0),
                (120.0, 110.0),  // HIGH (B)
                (110.0, 105.0),
                (105.0, 100.0),  // LOW (C)
                (113.0, 108.0),
            ],
            1,
        );
        assert!(detect_abcd(&det).is_some());
        assert!(detect_brc_default(&det).is_none());
        assert!(detect_combined(&det, DEFAULT_BRC_RETEST_RATIO).is_none());
    }
}
