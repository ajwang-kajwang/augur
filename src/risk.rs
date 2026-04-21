// src/risk.rs
//
// ============================================================================
// WHY THIS MODULE EXISTS
// ============================================================================
//
// The pattern recognizer produces geometric trade setups. The Fibonacci
// module produces price anchors. This module is the GATE that converts
// those into executable trade signals.
//
// It encapsulates three responsibilities the FMG guide emphasizes:
//
//   1. POSITION SIZING from a fixed risk percentage of account equity.
//      The guide recommends 1% per trade, 2% maximum across a compounded
//      Golden + Sniper pair.
//
//   2. INVALIDATION-BASED STOP PLACEMENT. Stops go beyond the pattern's
//      structural invalidation point (swing A) — NOT at an arbitrary
//      monetary distance. The guide spends an entire chapter on this.
//
//   3. R:R GATING. The minimum 1:2 ratio must be satisfied against
//      Target 3 (95.5% extension). Signals that fail this gate are
//      rejected with a diagnostic reason so we can log why.
//
// The engine is STATELESS. Account balance, pattern, and Fibonacci sequence
// are all passed in explicitly. Position tracking (for compounding) lives
// in a separate module that will be added in Phase 2E — keeping state out
// of the risk engine makes it deterministic and easy to test.
//
// ============================================================================
// INVALIDATION LOGIC
// ============================================================================
//
// For a BULLISH ABCD: if price drops below the A swing low, the pattern
// is structurally broken — the "higher low" thesis has failed. Stop goes
// just below A, with a small buffer for wick noise.
//
// For a BEARISH ABCD: inverse — stop just above the A swing high.
//
// The buffer is expressed as a fraction of the A-B range, not a flat
// percentage of price. This scales naturally across instruments and
// volatility regimes. Default is 0.5% of the A-B range, which empirically
// catches wick noise without materially widening the stop.
//
// ============================================================================
// POSITION SIZING MATH
// ============================================================================
//
//   risk_amount = account_balance * risk_per_trade_pct
//   risk_per_contract = |entry_price - stop_price| * contract_face_value
//   contracts = floor(risk_amount / risk_per_contract)
//
// For BTC-USDT-SWAP on OKX: 1 contract = 0.01 BTC face value. A $1000 risk
// with a $1000 per-BTC stop distance yields 100 contracts (1 BTC notional).
// We floor rather than round — better to under-risk than over-risk.

use std::fmt;
use crate::abc_brc::{AbcdPattern, PatternDirection};
use crate::fibonacci::FibSequence;
use crate::candle::Timeframe;

// ============================================================================
// INSTRUMENT METADATA
// ============================================================================
// For now, hardcoded for BTC-USDT-SWAP. When multi-instrument support lands
// in Phase 5, this becomes a per-symbol registry.

/// OKX BTC-USDT-SWAP: 1 contract = 0.01 BTC face value.
pub const BTC_USDT_SWAP_CONTRACT_FACE: f64 = 0.01;

// ============================================================================
// ENTRY ZONE
// ============================================================================
// Which Fibonacci level this signal is anchored to. Golden is the primary
// entry; Sniper is the compounding add-on.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryZone {
    Golden,
    Sniper,
}

impl fmt::Display for EntryZone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EntryZone::Golden => write!(f, "GOLDEN"),
            EntryZone::Sniper => write!(f, "SNIPER"),
        }
    }
}

// ============================================================================
// RISK PARAMS
// ============================================================================

#[derive(Debug, Clone)]
pub struct RiskParams {
    /// Account equity in USDT.
    pub account_balance: f64,
    /// Fraction of equity risked on each primary (Golden) entry. FMG default 0.01.
    pub risk_per_trade_pct: f64,
    /// Minimum acceptable reward/risk ratio against Target 3. FMG default 2.0.
    pub min_rr_ratio: f64,
    /// Stop buffer beyond A swing, as fraction of A-B range. Default 0.005 (0.5%).
    pub stop_buffer_pct: f64,
    /// Contract face value for the instrument being traded (in base asset units).
    pub contract_face_value: f64,
}

impl RiskParams {
    /// Default parameters for BTC-USDT-SWAP per the FMG guide.
    pub fn fmg_default(account_balance: f64) -> Self {
        RiskParams {
            account_balance,
            risk_per_trade_pct: 0.01,
            min_rr_ratio: 2.0,
            stop_buffer_pct: 0.005,
            contract_face_value: BTC_USDT_SWAP_CONTRACT_FACE,
        }
    }
}

// ============================================================================
// TRADE SIGNAL
// ============================================================================
// A fully-specified trade ready for submission to the order manager.
// Every price is computed; every size is computed; R:R is pre-validated.

#[derive(Debug, Clone)]
pub struct TradeSignal {
    pub direction: PatternDirection,
    pub timeframe: Timeframe,
    pub entry_zone: EntryZone,

    /// Limit price for the entry order.
    pub entry_price: f64,
    /// Stop-loss trigger price (at pattern invalidation + buffer).
    pub stop_price: f64,
    /// Take-profit trigger price (Target 3 by default).
    pub target_price: f64,

    /// Integer contract count to trade.
    pub contracts: u64,
    /// Notional risk in quote currency (USDT).
    pub risk_amount: f64,
    /// Notional reward in quote currency if target hits.
    pub reward_amount: f64,
    /// reward / risk ratio.
    pub rr_ratio: f64,
}
#[allow(dead_code)]
impl TradeSignal {
    /// OKX API side: "buy" for bullish entries, "sell" for bearish entries.
    pub fn entry_side(&self) -> &'static str {
        match self.direction {
            PatternDirection::Bullish => "buy",
            PatternDirection::Bearish => "sell",
        }
    }

    /// OKX API side for exit orders (opposite of entry).
    pub fn exit_side(&self) -> &'static str {
        match self.direction {
            PatternDirection::Bullish => "sell",
            PatternDirection::Bearish => "buy",
        }
    }
}

impl fmt::Display for TradeSignal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {} {} [{}] {}ct @ ${:.2} | SL ${:.2} | TP ${:.2} | Risk ${:.2} Reward ${:.2} (R:R 1:{:.2})",
            self.direction,
            self.entry_zone,
            self.timeframe,
            self.entry_side().to_uppercase(),
            self.contracts,
            self.entry_price,
            self.stop_price,
            self.target_price,
            self.risk_amount,
            self.reward_amount,
            self.rr_ratio,
        )
    }
}

// ============================================================================
// REJECTION REASON
// ============================================================================
// When a pattern fails the risk gate, we want to know WHY. The reason
// becomes a structured log field so we can later analyze rejection
// distributions and tune parameters.

#[derive(Debug, Clone, PartialEq)]
pub enum RejectionReason {
    /// Entry and stop prices collapse to the same value (degenerate pattern).
    DegenerateStop,
    /// Reward/risk ratio below the configured minimum.
    InsufficientRr { actual: f64, required: f64 },
    /// Computed position size floors to zero contracts (stop too tight or
    /// risk allocation too small).
    PositionTooSmall,
    /// Account balance is zero or negative.
    InvalidBalance,
}

impl fmt::Display for RejectionReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RejectionReason::DegenerateStop =>
                write!(f, "degenerate stop (entry == stop)"),
            RejectionReason::InsufficientRr { actual, required } =>
                write!(f, "R:R 1:{:.2} below minimum 1:{:.2}", actual, required),
            RejectionReason::PositionTooSmall =>
                write!(f, "position size < 1 contract"),
            RejectionReason::InvalidBalance =>
                write!(f, "account balance invalid"),
        }
    }
}

// ============================================================================
// RISK ENGINE
// ============================================================================

pub struct RiskEngine {
    params: RiskParams,
}

impl RiskEngine {
    pub fn new(params: RiskParams) -> Self {
        RiskEngine { params }
    }

    pub fn params(&self) -> &RiskParams { &self.params }

    /// Build a trade signal anchored to the Golden zone (67.4% retracement).
    /// This is the primary entry per the FMG guide.
    pub fn evaluate_golden(
        &self,
        pattern: &AbcdPattern,
        fib: &FibSequence,
        timeframe: Timeframe,
    ) -> Result<TradeSignal, RejectionReason> {
        self.build_signal(pattern, fib, timeframe, EntryZone::Golden, fib.golden_zone())
    }

    /// Build a trade signal anchored to the Sniper zone (91% retracement).
    /// This is the compounding add-on per the FMG guide.
    pub fn evaluate_sniper(
        &self,
        pattern: &AbcdPattern,
        fib: &FibSequence,
        timeframe: Timeframe,
    ) -> Result<TradeSignal, RejectionReason> {
        self.build_signal(pattern, fib, timeframe, EntryZone::Sniper, fib.sniper_zone())
    }

    // ------------------------------------------------------------------------

    fn build_signal(
        &self,
        pattern: &AbcdPattern,
        fib: &FibSequence,
        timeframe: Timeframe,
        entry_zone: EntryZone,
        entry_price: f64,
    ) -> Result<TradeSignal, RejectionReason> {
        // --- Guard: balance must be positive ---
        if self.params.account_balance <= 0.0 {
            return Err(RejectionReason::InvalidBalance);
        }

        // --- Compute stop at pattern invalidation (just beyond A) ---
        let stop_price = self.compute_stop_price(pattern);

        // --- Guard: entry and stop must differ ---
        let stop_distance = (entry_price - stop_price).abs();
        if stop_distance <= 0.0 {
            return Err(RejectionReason::DegenerateStop);
        }

        // --- Compute target (Target 3) and validate R:R ---
        let target_price = fib.target3();
        let target_distance = (target_price - entry_price).abs();
        let rr_ratio = target_distance / stop_distance;

        if rr_ratio < self.params.min_rr_ratio {
            return Err(RejectionReason::InsufficientRr {
                actual: rr_ratio,
                required: self.params.min_rr_ratio,
            });
        }

        // --- Compute position size ---
        let risk_amount = self.params.account_balance * self.params.risk_per_trade_pct;
        // risk per contract = price distance × face value of one contract
        let risk_per_contract = stop_distance * self.params.contract_face_value;
        let contracts_float = risk_amount / risk_per_contract;
        // Floor to integer contracts — under-risking is safer than over-risking.
        let contracts = contracts_float.floor() as u64;

        if contracts == 0 {
            return Err(RejectionReason::PositionTooSmall);
        }

        // Recompute actual risk and reward in USDT given the floored size.
        let actual_risk = contracts as f64 * stop_distance * self.params.contract_face_value;
        let actual_reward = contracts as f64 * target_distance * self.params.contract_face_value;

        Ok(TradeSignal {
            direction: pattern.direction,
            timeframe,
            entry_zone,
            entry_price,
            stop_price,
            target_price,
            contracts,
            risk_amount: actual_risk,
            reward_amount: actual_reward,
            rr_ratio,
        })
    }

    /// Stop price at pattern invalidation.
    ///
    /// BULLISH: stop goes BELOW A (a break below the A low invalidates the
    /// "higher low" thesis). We subtract a small buffer beyond A to avoid
    /// wick noise.
    ///
    /// BEARISH: stop goes ABOVE A (a break above the A high invalidates
    /// the "lower high" thesis).
    fn compute_stop_price(&self, pattern: &AbcdPattern) -> f64 {
        let buffer = pattern.ab_range() * self.params.stop_buffer_pct;
        match pattern.direction {
            PatternDirection::Bullish => pattern.a.price - buffer,
            PatternDirection::Bearish => pattern.a.price + buffer,
        }
    }
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::swing::{SwingPoint, SwingType};

    const EPS: f64 = 1e-6;
    fn near(a: f64, b: f64) -> bool { (a - b).abs() < EPS }

    /// Construct a bullish ABCD with A=100, B=150, C=120.
    fn bullish_pattern() -> AbcdPattern {
        AbcdPattern {
            direction: PatternDirection::Bullish,
            a: SwingPoint { timestamp: 0,         price: 100.0, swing_type: SwingType::Low  },
            b: SwingPoint { timestamp: 3_600_000, price: 150.0, swing_type: SwingType::High },
            c: SwingPoint { timestamp: 7_200_000, price: 120.0, swing_type: SwingType::Low  },
        }
    }

    /// Construct a bearish ABCD with A=150, B=100, C=130.
    fn bearish_pattern() -> AbcdPattern {
        AbcdPattern {
            direction: PatternDirection::Bearish,
            a: SwingPoint { timestamp: 0,         price: 150.0, swing_type: SwingType::High },
            b: SwingPoint { timestamp: 3_600_000, price: 100.0, swing_type: SwingType::Low  },
            c: SwingPoint { timestamp: 7_200_000, price: 130.0, swing_type: SwingType::High },
        }
    }

    #[test]
    fn test_bullish_golden_signal_passes_rr_gate() {
        // Bullish A=100, B=150, range=50
        //   Golden entry = 150 - 0.674*50 = 116.30
        //   Stop = 100 - 0.005*50 = 99.75
        //   Target3 = 150 + 0.955*50 = 197.75
        //   risk = 116.30 - 99.75 = 16.55
        //   reward = 197.75 - 116.30 = 81.45
        //   R:R = 81.45 / 16.55 ≈ 4.92
        let pattern = bullish_pattern();
        let fib = FibSequence::from_pattern(&pattern);
        let engine = RiskEngine::new(RiskParams::fmg_default(10_000.0));

        let signal = engine.evaluate_golden(&pattern, &fib, Timeframe::H1)
            .expect("Golden signal should pass R:R gate");

        assert!(near(signal.entry_price, 116.30));
        assert!(near(signal.stop_price, 99.75));
        assert!(near(signal.target_price, 197.75));
        assert!(signal.rr_ratio >= 2.0);
        assert_eq!(signal.entry_zone, EntryZone::Golden);
        assert_eq!(signal.direction, PatternDirection::Bullish);
        assert_eq!(signal.entry_side(), "buy");
        assert_eq!(signal.exit_side(), "sell");
    }

    #[test]
    fn test_bearish_golden_signal_passes_rr_gate() {
        // Bearish A=150, B=100, range=50
        //   Golden entry = 100 + 0.674*50 = 133.70
        //   Stop = 150 + 0.005*50 = 150.25
        //   Target3 = 100 - 0.955*50 = 52.25
        //   risk = 150.25 - 133.70 = 16.55
        //   reward = 133.70 - 52.25 = 81.45
        //   R:R = 81.45 / 16.55 ≈ 4.92
        let pattern = bearish_pattern();
        let fib = FibSequence::from_pattern(&pattern);
        let engine = RiskEngine::new(RiskParams::fmg_default(10_000.0));

        let signal = engine.evaluate_golden(&pattern, &fib, Timeframe::H1)
            .expect("bearish Golden should pass");

        assert!(near(signal.entry_price, 133.70));
        assert!(near(signal.stop_price, 150.25));
        assert!(near(signal.target_price, 52.25));
        assert_eq!(signal.entry_side(), "sell");
        assert_eq!(signal.exit_side(), "buy");
    }

    #[test]
    fn test_sniper_has_better_rr_than_golden() {
        // Sniper entry is deeper → tighter stop distance → higher R:R.
        let pattern = bullish_pattern();
        let fib = FibSequence::from_pattern(&pattern);
        let engine = RiskEngine::new(RiskParams::fmg_default(10_000.0));

        let golden = engine.evaluate_golden(&pattern, &fib, Timeframe::H1).unwrap();
        let sniper = engine.evaluate_sniper(&pattern, &fib, Timeframe::H1).unwrap();

        assert!(sniper.rr_ratio > golden.rr_ratio,
            "sniper R:R ({:.2}) should exceed golden R:R ({:.2})",
            sniper.rr_ratio, golden.rr_ratio);
        assert_eq!(sniper.entry_zone, EntryZone::Sniper);
    }

    #[test]
    fn test_position_size_matches_risk_allocation() {
        // Account $10,000 × 1% = $100 risk budget.
        // Stop distance = 16.55 (from bullish golden).
        // Contract face = 0.01 BTC.
        // Risk per contract = 16.55 × 0.01 = $0.1655.
        // Contracts = floor(100 / 0.1655) = 604.
        let pattern = bullish_pattern();
        let fib = FibSequence::from_pattern(&pattern);
        let engine = RiskEngine::new(RiskParams::fmg_default(10_000.0));

        let signal = engine.evaluate_golden(&pattern, &fib, Timeframe::H1).unwrap();
        assert_eq!(signal.contracts, 604);
        // Actual USDT risk should be at or below the budgeted 1%.
        assert!(signal.risk_amount <= 100.0);
        // ...but not materially below (we expect within 1 contract of budget).
        let single_contract_risk = 16.55 * 0.01;
        assert!(signal.risk_amount > 100.0 - single_contract_risk);
    }

    #[test]
    fn test_insufficient_balance_rejected() {
        let pattern = bullish_pattern();
        let fib = FibSequence::from_pattern(&pattern);

        // Zero balance → immediate rejection.
        let engine = RiskEngine::new(RiskParams::fmg_default(0.0));
        assert_eq!(
            engine.evaluate_golden(&pattern, &fib, Timeframe::H1).unwrap_err(),
            RejectionReason::InvalidBalance,
        );
    }

    #[test]
    fn test_tiny_balance_rejected_as_too_small() {
        // $1 balance × 1% = $0.01 risk budget. Stop distance ~$16.55.
        // Risk per contract = $0.1655, so budget affords 0 contracts → rejected.
        let pattern = bullish_pattern();
        let fib = FibSequence::from_pattern(&pattern);
        let engine = RiskEngine::new(RiskParams::fmg_default(1.0));

        match engine.evaluate_golden(&pattern, &fib, Timeframe::H1) {
            Err(RejectionReason::PositionTooSmall) => {}
            other => panic!("expected PositionTooSmall, got {:?}", other),
        }
    }

    #[test]
    fn test_high_min_rr_rejects_signal() {
        // Bullish Golden R:R is ~4.9 — raise min to 10.0 to force rejection.
        let pattern = bullish_pattern();
        let fib = FibSequence::from_pattern(&pattern);
        let mut params = RiskParams::fmg_default(10_000.0);
        params.min_rr_ratio = 10.0;
        let engine = RiskEngine::new(params);

        match engine.evaluate_golden(&pattern, &fib, Timeframe::H1) {
            Err(RejectionReason::InsufficientRr { actual, required }) => {
                assert!(actual < 10.0);
                assert!(near(required, 10.0));
            }
            other => panic!("expected InsufficientRr, got {:?}", other),
        }
    }

    #[test]
    fn test_stop_buffer_scales_with_range() {
        // Stop distance should be exactly range * buffer_pct beyond A.
        let pattern = bullish_pattern();  // A=100, range=50
        let engine = RiskEngine::new(RiskParams::fmg_default(10_000.0));
        let fib = FibSequence::from_pattern(&pattern);
        let signal = engine.evaluate_golden(&pattern, &fib, Timeframe::H1).unwrap();

        // Expected: stop = 100 - 50*0.005 = 99.75
        assert!(near(signal.stop_price, 99.75));
    }

    #[test]
    fn test_signal_risk_matches_recomputed_amount() {
        // The TradeSignal's risk_amount should equal contracts × stop_distance × face.
        let pattern = bullish_pattern();
        let fib = FibSequence::from_pattern(&pattern);
        let engine = RiskEngine::new(RiskParams::fmg_default(10_000.0));

        let signal = engine.evaluate_golden(&pattern, &fib, Timeframe::H1).unwrap();

        let expected_risk = signal.contracts as f64
            * (signal.entry_price - signal.stop_price).abs()
            * BTC_USDT_SWAP_CONTRACT_FACE;
        assert!(near(signal.risk_amount, expected_risk));

        let expected_reward = signal.contracts as f64
            * (signal.target_price - signal.entry_price).abs()
            * BTC_USDT_SWAP_CONTRACT_FACE;
        assert!(near(signal.reward_amount, expected_reward));
    }
}
