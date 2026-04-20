// src/fibonacci.rs
//
// ============================================================================
// WHY THIS MODULE EXISTS
// ============================================================================
//
// The FMG ABC-BRC guide uses a CUSTOM Fibonacci retracement ladder that
// differs from the standard technical-analysis set. These levels were
// derived from back-testing against the strategy's specific edge.
//
// FMG RETRACEMENT LEVELS (measured on the A→B impulse leg):
//
//   23.6% — "Very fast and aggressive pullback"
//   38.2% — "Fast and aggressive pullback"
//   52.8% — "Medium pullback"
//   67.4% — "Perfect pullback" / Golden Zone   — PRIMARY ENTRY
//   82%   — "Gradual pullback"
//   91%   — "Sniper Zone"                      — ADD-ON ENTRY
//
// FMG EXTENSION LEVELS (the D target — beyond B):
//
//   30.5% — Target 1
//   62.9% — Target 2
//   95.5% — Target 3  (minimum for 1:2+ R:R when stop at invalidation)
//
// ENTRY VALIDATION:
// Per the guide, entries shallower than 67.4% fail the minimum 1:2 R:R
// requirement. The 67.4% Golden and 91% Sniper zones are the only entries
// that pass the R:R gate when targeting D3 (95.5% extension).

use std::fmt;
use crate::abc_brc::{AbcdPattern, PatternDirection};

pub const FIB_RETRACE_AGGRESSIVE: f64 = 0.236;
pub const FIB_RETRACE_FAST:       f64 = 0.382;
pub const FIB_RETRACE_MEDIUM:     f64 = 0.528;
pub const FIB_RETRACE_GOLDEN:     f64 = 0.674;
pub const FIB_RETRACE_GRADUAL:    f64 = 0.820;
pub const FIB_RETRACE_SNIPER:     f64 = 0.910;

pub const FIB_TARGET_1: f64 = 0.305;
pub const FIB_TARGET_2: f64 = 0.629;
pub const FIB_TARGET_3: f64 = 0.955;

#[derive(Debug, Clone)]
pub struct FibLevel {
    pub fraction: f64,
    pub price: f64,
    pub label: &'static str,
}

impl fmt::Display for FibLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({:.1}%): ${:.2}",
            self.label, self.fraction * 100.0, self.price)
    }
}

#[derive(Debug, Clone)]
pub struct FibSequence {
    pub anchor_a: f64,
    pub anchor_b: f64,
    pub direction: PatternDirection,
    pub retracements: Vec<FibLevel>,
    pub targets: Vec<FibLevel>,
}

impl FibSequence {
    pub fn from_leg(a_price: f64, b_price: f64, direction: PatternDirection) -> Self {
        let range = (b_price - a_price).abs();
        let retrace_dir = Self::retrace_direction(direction);
        let target_dir = -retrace_dir;

        let retrace_price = |frac: f64| b_price + retrace_dir * frac * range;
        let target_price  = |frac: f64| b_price + target_dir  * frac * range;

        let retracements = vec![
            FibLevel { fraction: FIB_RETRACE_AGGRESSIVE, price: retrace_price(FIB_RETRACE_AGGRESSIVE), label: "23.6% Aggressive" },
            FibLevel { fraction: FIB_RETRACE_FAST,       price: retrace_price(FIB_RETRACE_FAST),       label: "38.2% Fast"       },
            FibLevel { fraction: FIB_RETRACE_MEDIUM,     price: retrace_price(FIB_RETRACE_MEDIUM),     label: "52.8% Medium"     },
            FibLevel { fraction: FIB_RETRACE_GOLDEN,     price: retrace_price(FIB_RETRACE_GOLDEN),     label: "67.4% Golden"     },
            FibLevel { fraction: FIB_RETRACE_GRADUAL,    price: retrace_price(FIB_RETRACE_GRADUAL),    label: "82% Gradual"      },
            FibLevel { fraction: FIB_RETRACE_SNIPER,     price: retrace_price(FIB_RETRACE_SNIPER),     label: "91% Sniper"       },
        ];

        let targets = vec![
            FibLevel { fraction: FIB_TARGET_1, price: target_price(FIB_TARGET_1), label: "Target 1" },
            FibLevel { fraction: FIB_TARGET_2, price: target_price(FIB_TARGET_2), label: "Target 2" },
            FibLevel { fraction: FIB_TARGET_3, price: target_price(FIB_TARGET_3), label: "Target 3" },
        ];

        FibSequence {
            anchor_a: a_price,
            anchor_b: b_price,
            direction,
            retracements,
            targets,
        }
    }

    pub fn from_pattern(pattern: &AbcdPattern) -> Self {
        Self::from_leg(pattern.a.price, pattern.b.price, pattern.direction)
    }

    fn retrace_direction(direction: PatternDirection) -> f64 {
        match direction {
            PatternDirection::Bullish => -1.0,
            PatternDirection::Bearish =>  1.0,
        }
    }

    pub fn golden_zone(&self) -> f64 { self.retrace_price_at(FIB_RETRACE_GOLDEN) }
    pub fn sniper_zone(&self) -> f64 { self.retrace_price_at(FIB_RETRACE_SNIPER) }
    pub fn target1(&self) -> f64 { self.target_price_at(FIB_TARGET_1) }
    pub fn target2(&self) -> f64 { self.target_price_at(FIB_TARGET_2) }
    pub fn target3(&self) -> f64 { self.target_price_at(FIB_TARGET_3) }

    pub fn retrace_price_at(&self, fraction: f64) -> f64 {
        let range = (self.anchor_b - self.anchor_a).abs();
        self.anchor_b + Self::retrace_direction(self.direction) * fraction * range
    }

    pub fn target_price_at(&self, fraction: f64) -> f64 {
        let range = (self.anchor_b - self.anchor_a).abs();
        self.anchor_b - Self::retrace_direction(self.direction) * fraction * range
    }

    pub fn range(&self) -> f64 { (self.anchor_b - self.anchor_a).abs() }

    pub fn is_valid_entry(&self, price: f64) -> bool {
        let golden = self.golden_zone();
        match self.direction {
            PatternDirection::Bullish => price <= golden,
            PatternDirection::Bearish => price >= golden,
        }
    }

    pub fn is_sniper_zone(&self, price: f64) -> bool {
        let sniper = self.sniper_zone();
        match self.direction {
            PatternDirection::Bullish => price <= sniper,
            PatternDirection::Bearish => price >= sniper,
        }
    }

    pub fn deepest_reached(&self, price: f64) -> Option<&FibLevel> {
        self.retracements.iter().rev().find(|level| {
            match self.direction {
                PatternDirection::Bullish => price <= level.price,
                PatternDirection::Bearish => price >= level.price,
            }
        })
    }
}

impl fmt::Display for FibSequence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f,
            "{} FibSequence A:${:.2} B:${:.2} | Golden: ${:.2} | Sniper: ${:.2} | T3: ${:.2}",
            self.direction,
            self.anchor_a, self.anchor_b,
            self.golden_zone(), self.sniper_zone(), self.target3())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f64 = 1e-9;
    fn near(a: f64, b: f64) -> bool { (a - b).abs() < EPS }

    #[test]
    fn test_bullish_retracements() {
        let seq = FibSequence::from_leg(100.0, 150.0, PatternDirection::Bullish);
        assert!(near(seq.retrace_price_at(0.0), 150.0));
        assert!(near(seq.retrace_price_at(0.50), 125.0));
        assert!(near(seq.retrace_price_at(1.0), 100.0));
        assert!(near(seq.golden_zone(),  150.0 - 0.674 * 50.0));
        assert!(near(seq.sniper_zone(),  150.0 - 0.91  * 50.0));
    }

    #[test]
    fn test_bearish_retracements() {
        let seq = FibSequence::from_leg(150.0, 100.0, PatternDirection::Bearish);
        assert!(near(seq.retrace_price_at(0.0),  100.0));
        assert!(near(seq.retrace_price_at(0.50), 125.0));
        assert!(near(seq.retrace_price_at(1.0),  150.0));
        assert!(near(seq.golden_zone(),  100.0 + 0.674 * 50.0));
        assert!(near(seq.sniper_zone(),  100.0 + 0.91  * 50.0));
    }

    #[test]
    fn test_bullish_targets() {
        let seq = FibSequence::from_leg(100.0, 150.0, PatternDirection::Bullish);
        assert!(near(seq.target1(), 150.0 + 0.305 * 50.0));
        assert!(near(seq.target2(), 150.0 + 0.629 * 50.0));
        assert!(near(seq.target3(), 150.0 + 0.955 * 50.0));
    }

    #[test]
    fn test_bearish_targets() {
        let seq = FibSequence::from_leg(150.0, 100.0, PatternDirection::Bearish);
        assert!(near(seq.target1(), 100.0 - 0.305 * 50.0));
        assert!(near(seq.target2(), 100.0 - 0.629 * 50.0));
        assert!(near(seq.target3(), 100.0 - 0.955 * 50.0));
    }

    #[test]
    fn test_is_valid_entry_bullish() {
        let seq = FibSequence::from_leg(100.0, 150.0, PatternDirection::Bullish);
        let golden = seq.golden_zone();
        assert!( seq.is_valid_entry(golden));
        assert!( seq.is_valid_entry(golden - 1.0));
        assert!( seq.is_valid_entry(seq.sniper_zone()));
        assert!(!seq.is_valid_entry(golden + 1.0));
        assert!(!seq.is_valid_entry(140.0));
    }

    #[test]
    fn test_is_valid_entry_bearish() {
        let seq = FibSequence::from_leg(150.0, 100.0, PatternDirection::Bearish);
        let golden = seq.golden_zone();
        assert!( seq.is_valid_entry(golden));
        assert!( seq.is_valid_entry(golden + 1.0));
        assert!( seq.is_valid_entry(seq.sniper_zone()));
        assert!(!seq.is_valid_entry(golden - 1.0));
        assert!(!seq.is_valid_entry(110.0));
    }

    #[test]
    fn test_is_sniper_zone() {
        let seq = FibSequence::from_leg(100.0, 150.0, PatternDirection::Bullish);
        let sniper = seq.sniper_zone();
        assert!( seq.is_sniper_zone(sniper));
        assert!( seq.is_sniper_zone(sniper - 1.0));
        assert!(!seq.is_sniper_zone(sniper + 1.0));
    }

    #[test]
    fn test_deepest_reached_bullish() {
        let seq = FibSequence::from_leg(100.0, 150.0, PatternDirection::Bullish);
        assert!(seq.deepest_reached(145.0).is_none());
        let l = seq.deepest_reached(135.0).unwrap();
        assert!(near(l.fraction, FIB_RETRACE_AGGRESSIVE));
        let l = seq.deepest_reached(116.0).unwrap();
        assert!(near(l.fraction, FIB_RETRACE_GOLDEN));
        let l = seq.deepest_reached(103.0).unwrap();
        assert!(near(l.fraction, FIB_RETRACE_SNIPER));
    }

    #[test]
    fn test_retracement_ladder_monotonicity() {
        let seq = FibSequence::from_leg(100.0, 150.0, PatternDirection::Bullish);
        for w in seq.retracements.windows(2) {
            assert!(w[0].fraction < w[1].fraction);
            assert!(w[0].price > w[1].price);
        }
        let seq = FibSequence::from_leg(150.0, 100.0, PatternDirection::Bearish);
        for w in seq.retracements.windows(2) {
            assert!(w[0].fraction < w[1].fraction);
            assert!(w[0].price < w[1].price);
        }
    }

    #[test]
    fn test_from_pattern_matches_from_leg() {
        use crate::swing::{SwingPoint, SwingType};

        let pattern = AbcdPattern {
            direction: PatternDirection::Bullish,
            a: SwingPoint { timestamp: 0,         price: 100.0, swing_type: SwingType::Low  },
            b: SwingPoint { timestamp: 3_600_000, price: 150.0, swing_type: SwingType::High },
            c: SwingPoint { timestamp: 7_200_000, price: 120.0, swing_type: SwingType::Low  },
        };

        let a = FibSequence::from_pattern(&pattern);
        let b = FibSequence::from_leg(100.0, 150.0, PatternDirection::Bullish);
        assert!(near(a.golden_zone(), b.golden_zone()));
        assert!(near(a.target3(),     b.target3()));
    }
}
