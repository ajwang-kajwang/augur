// src/position.rs
//
// ============================================================================
// WHY THIS MODULE EXISTS (Phase 2E)
// ============================================================================
//
// v0.6 submitted Golden and Sniper signals independently — both fired in
// parallel whenever an ABCD pattern completed with passing R:R. That's
// wrong per the FMG guide: the Sniper is a COMPOUNDING ADD-ON that should
// only fire when a Golden position is already filled and price drills
// deeper. Without this discipline, we'd open two independent positions
// at retracement levels the guide never intended as simultaneous entries.
//
// This module is the state machine that fixes that. It tracks every
// signal from submission through fill through exit, keyed by the client
// order ID, so main.rs can ask "do I have an open Golden for this
// (timeframe, direction)?" before submitting a Sniper.
//
// It also prevents double-submission: if we already have a Pending or
// Open entry for (timeframe, direction, zone), a second pattern firing
// on the same swing shouldn't stack another identical entry.
//
// ============================================================================
// STATE MACHINE
// ============================================================================
//
//   Pending ──(order event: filled)──► Open
//      │
//      └───(order event: canceled)──► Canceled
//
//   Open ────(order event: OCO leg filled)──► Closed
//
// Transitions are driven by order updates from the private WebSocket
// channel. Until the private WS is wired in (or when it's disconnected
// / login failed), signals that get submitted stay in Pending forever,
// which fails-closed: no Sniper add-ons will fire, degrading gracefully
// rather than doubling up entries blindly.
//
// ============================================================================
// SHARED-STATE CONCURRENCY
// ============================================================================
//
// The Positions registry is shared between the main event loop (which
// reads it on every pattern) and the private WS task (which writes to
// it on every order update). We use `Arc<tokio::sync::Mutex<Positions>>`
// rather than channels because:
//
//   - Reads are frequent (every pattern detection) and need to be fast
//   - Updates are rare (only on actual order state changes)
//   - A mutex lock held for microseconds is cheaper than the channel
//     round-trip that would be needed to marshal a query-and-response
//
// The Mutex is tokio's async-aware variant — holding it across an
// .await would be a bug, but we never do that.

use std::collections::HashMap;
use std::fmt;
use tracing::{info, warn};
use crate::abc_brc::PatternDirection;
use crate::candle::Timeframe;
use crate::risk::{EntryZone, TradeSignal};

// ============================================================================
// POSITION STATE
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionState {
    /// Entry limit submitted, resting on the book, not yet filled.
    Pending,
    /// Entry fully or partially filled. The OCO exit pair (attached
    /// algo orders) is active on the exchange. This is the state the
    /// Sniper compounding gate checks for.
    Open,
    /// Exited via TP or SL (or manual close).
    Closed,
    /// Entry canceled before any fill.
    Canceled,
}

impl fmt::Display for PositionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PositionState::Pending  => write!(f, "PENDING"),
            PositionState::Open     => write!(f, "OPEN"),
            PositionState::Closed   => write!(f, "CLOSED"),
            PositionState::Canceled => write!(f, "CANCELED"),
        }
    }
}

// ============================================================================
// TRACKED POSITION
// ============================================================================

#[derive(Debug, Clone)]
pub struct TrackedPosition {
    pub client_ord_id: String,
    /// None until the order manager confirms the submission and supplies
    /// the exchange-assigned order ID. Also None for "unsubmitted" entries
    /// (trading gate closed) — useful for exercising the compounding logic
    /// during dry runs.
    pub entry_ord_id: Option<String>,

    pub direction: PatternDirection,
    pub timeframe: Timeframe,
    pub entry_zone: EntryZone,

    pub state: PositionState,

    pub contracts: u64,
    pub entry_price: f64,
    pub stop_price: f64,
    pub target_price: f64,

    /// Average fill price once the entry fills. None while Pending.
    pub fill_price: Option<f64>,
    /// Accumulated filled size (supports partial fills).
    pub filled_size: u64,

    pub submitted_at_ms: u64,
    pub last_update_ms: u64,
}

impl TrackedPosition {
    pub fn from_signal(
        signal: &TradeSignal,
        client_ord_id: String,
        entry_ord_id: Option<String>,
        now_ms: u64,
    ) -> Self {
        TrackedPosition {
            client_ord_id,
            entry_ord_id,
            direction: signal.direction,
            timeframe: signal.timeframe,
            entry_zone: signal.entry_zone,
            state: PositionState::Pending,
            contracts: signal.contracts,
            entry_price: signal.entry_price,
            stop_price: signal.stop_price,
            target_price: signal.target_price,
            fill_price: None,
            filled_size: 0,
            submitted_at_ms: now_ms,
            last_update_ms: now_ms,
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(self.state, PositionState::Pending | PositionState::Open)
    }

    pub fn is_open(&self) -> bool {
        matches!(self.state, PositionState::Open)
    }
}

impl fmt::Display for TrackedPosition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {} {} {} [{}] {}ct @ ${:.2}",
            self.state,
            self.direction,
            self.entry_zone,
            self.timeframe,
            &self.client_ord_id,
            self.contracts,
            self.entry_price,
        )
    }
}

// ============================================================================
// ORDER UPDATE
// ============================================================================
// Normalized representation of an OKX `orders` channel event. The private
// WS task parses raw JSON into this type and passes it to
// `Positions::apply_order_update()`.

#[derive(Debug, Clone)]
pub struct OrderUpdate {
    pub client_ord_id: String,
    pub ord_id: String,
    /// OKX state field: "live", "partially_filled", "filled", "canceled".
    pub state: String,
    pub fill_price: Option<f64>,
    /// Accumulated filled size (OKX reports this cumulatively).
    pub acc_fill_size: Option<u64>,
    pub timestamp_ms: u64,
}

// ============================================================================
// POSITIONS REGISTRY
// ============================================================================

pub struct Positions {
    /// Keyed by clOrdId (stable, assigned at submission time).
    by_client_id: HashMap<String, TrackedPosition>,
    /// Secondary index: exchange ord_id → client_ord_id, populated once
    /// the order manager confirms submission. Allows order updates that
    /// arrive with only the exchange ID to be routed to the right entry.
    by_ord_id: HashMap<String, String>,
}

impl Positions {
    pub fn new() -> Self {
        Positions {
            by_client_id: HashMap::new(),
            by_ord_id: HashMap::new(),
        }
    }

    /// Register a signal that was submitted to the exchange.
    pub fn record_submission(
        &mut self,
        signal: &TradeSignal,
        client_ord_id: String,
        entry_ord_id: String,
        now_ms: u64,
    ) {
        let tracked = TrackedPosition::from_signal(
            signal, client_ord_id.clone(), Some(entry_ord_id.clone()), now_ms,
        );
        self.by_ord_id.insert(entry_ord_id, client_ord_id.clone());
        self.by_client_id.insert(client_ord_id, tracked);
    }

    /// Register a signal that was BUILT but not submitted (trading gate
    /// closed). We still track these — in dry runs this lets us exercise
    /// the compounding gate logic so the observed signal stream mirrors
    /// what would have been traded.
    ///
    /// Note: unsubmitted entries never transition out of Pending because
    /// no order events will arrive for them.
    pub fn record_unsubmitted(
        &mut self,
        signal: &TradeSignal,
        client_ord_id: String,
        now_ms: u64,
    ) {
        let tracked = TrackedPosition::from_signal(
            signal, client_ord_id.clone(), None, now_ms,
        );
        self.by_client_id.insert(client_ord_id, tracked);
    }

    /// Apply an order update from the private WS channel.
    pub fn apply_order_update(&mut self, update: &OrderUpdate) {
        // Resolve to clOrdId: prefer the one in the update; fall back to
        // the ord_id → clOrdId index.
        let client_ord_id = if !update.client_ord_id.is_empty() {
            update.client_ord_id.clone()
        } else if let Some(cl) = self.by_ord_id.get(&update.ord_id) {
            cl.clone()
        } else {
            // Order event for something we didn't submit (or submitted
            // in a previous process lifetime). Ignore.
            return;
        };

        let tracked = match self.by_client_id.get_mut(&client_ord_id) {
            Some(t) => t,
            None => return,
        };

        tracked.last_update_ms = update.timestamp_ms;
        if let Some(px) = update.fill_price { tracked.fill_price = Some(px); }
        if let Some(sz) = update.acc_fill_size { tracked.filled_size = sz; }

        let old_state = tracked.state;
        tracked.state = match update.state.as_str() {
            "live" | "partially_filled" => {
                // partially_filled is reported before full fill — treat
                // as still Pending until fully filled. This is conservative:
                // we don't open the Sniper gate on a partial.
                PositionState::Pending
            }
            "filled" => PositionState::Open,
            "canceled" => PositionState::Canceled,
            // Unknown states leave us in whatever we were.
            _ => tracked.state,
        };

        if tracked.state != old_state {
            info!(
                "📘 Position {} {} → {} (filled {}ct @ {:?})",
                client_ord_id, old_state, tracked.state,
                tracked.filled_size, tracked.fill_price,
            );
        }
    }

    /// Mark a position Closed. Used when we detect an OCO exit fill
    /// (the attached algo order's clOrdId is derived by appending "_oco"
    /// to the entry clOrdId) or via explicit cancel_position().
    pub fn mark_closed_by_parent(&mut self, parent_client_ord_id: &str, now_ms: u64) {
        if let Some(tracked) = self.by_client_id.get_mut(parent_client_ord_id) {
            if tracked.state == PositionState::Open {
                tracked.state = PositionState::Closed;
                tracked.last_update_ms = now_ms;
                info!("📕 Position {} CLOSED (OCO exit filled)", parent_client_ord_id);
            }
        } else {
            warn!("mark_closed: unknown parent {}", parent_client_ord_id);
        }
    }

    // ========================================================================
    // COMPOUNDING GATES — what main.rs asks before submitting
    // ========================================================================

    /// Is there an Open Golden position for this (timeframe, direction)?
    /// The FMG Sniper add-on is only valid when this is true.
    pub fn has_open_golden(
        &self,
        timeframe: Timeframe,
        direction: PatternDirection,
    ) -> bool {
        self.by_client_id.values().any(|p|
            p.is_open()
            && p.entry_zone == EntryZone::Golden
            && p.timeframe == timeframe
            && p.direction == direction
        )
    }

    /// Is there ANY active (Pending or Open) entry for this
    /// (timeframe, direction, zone)? Used to prevent double-stacking
    /// when the same pattern keeps re-detecting.
    pub fn has_active_entry(
        &self,
        timeframe: Timeframe,
        direction: PatternDirection,
        zone: EntryZone,
    ) -> bool {
        self.by_client_id.values().any(|p|
            p.is_active()
            && p.timeframe == timeframe
            && p.direction == direction
            && p.entry_zone == zone
        )
    }

    // ========================================================================
    // INTROSPECTION
    // ========================================================================

    pub fn all(&self) -> Vec<&TrackedPosition> {
        self.by_client_id.values().collect()
    }

    pub fn active(&self) -> Vec<&TrackedPosition> {
        self.by_client_id.values().filter(|p| p.is_active()).collect()
    }

    pub fn summary(&self) -> String {
        let mut counts = [0usize; 4]; // pending, open, closed, canceled
        for p in self.by_client_id.values() {
            let i = match p.state {
                PositionState::Pending  => 0,
                PositionState::Open     => 1,
                PositionState::Closed   => 2,
                PositionState::Canceled => 3,
            };
            counts[i] += 1;
        }
        format!(
            "pending:{} open:{} closed:{} canceled:{}",
            counts[0], counts[1], counts[2], counts[3],
        )
    }
}

impl Default for Positions {
    fn default() -> Self { Self::new() }
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

    fn mk_signal(direction: PatternDirection, zone: EntryZone, tf: Timeframe) -> TradeSignal {
        TradeSignal {
            direction, timeframe: tf, entry_zone: zone,
            entry_price: 100.0, stop_price: 95.0, target_price: 120.0,
            contracts: 10, risk_amount: 50.0, reward_amount: 200.0, rr_ratio: 4.0,
        }
    }

    #[test]
    fn new_submission_is_pending_and_active() {
        let mut p = Positions::new();
        let s = mk_signal(PatternDirection::Bullish, EntryZone::Golden, Timeframe::H1);
        p.record_submission(&s, "cl_1".into(), "ord_1".into(), 1_000);

        assert_eq!(p.active().len(), 1);
        assert!(p.has_active_entry(Timeframe::H1, PatternDirection::Bullish, EntryZone::Golden));
        // Pending, not Open — the Sniper gate is CLOSED.
        assert!(!p.has_open_golden(Timeframe::H1, PatternDirection::Bullish));
    }

    #[test]
    fn filled_entry_opens_sniper_gate() {
        let mut p = Positions::new();
        let s = mk_signal(PatternDirection::Bullish, EntryZone::Golden, Timeframe::H1);
        p.record_submission(&s, "cl_1".into(), "ord_1".into(), 1_000);

        p.apply_order_update(&OrderUpdate {
            client_ord_id: "cl_1".into(),
            ord_id: "ord_1".into(),
            state: "filled".into(),
            fill_price: Some(100.0),
            acc_fill_size: Some(10),
            timestamp_ms: 2_000,
        });

        // Now open — Sniper gate for (H1, Bullish) is open.
        assert!(p.has_open_golden(Timeframe::H1, PatternDirection::Bullish));
        // But not for other tfs or directions.
        assert!(!p.has_open_golden(Timeframe::H4, PatternDirection::Bullish));
        assert!(!p.has_open_golden(Timeframe::H1, PatternDirection::Bearish));
    }

    #[test]
    fn partial_fill_keeps_gate_closed() {
        // Conservative: partial fill doesn't open the Sniper gate.
        let mut p = Positions::new();
        let s = mk_signal(PatternDirection::Bullish, EntryZone::Golden, Timeframe::H1);
        p.record_submission(&s, "cl_1".into(), "ord_1".into(), 1_000);
        p.apply_order_update(&OrderUpdate {
            client_ord_id: "cl_1".into(),
            ord_id: "ord_1".into(),
            state: "partially_filled".into(),
            fill_price: Some(100.0),
            acc_fill_size: Some(3),
            timestamp_ms: 2_000,
        });
        assert!(!p.has_open_golden(Timeframe::H1, PatternDirection::Bullish));
    }

    #[test]
    fn canceled_entry_becomes_inactive() {
        let mut p = Positions::new();
        let s = mk_signal(PatternDirection::Bullish, EntryZone::Golden, Timeframe::H1);
        p.record_submission(&s, "cl_1".into(), "ord_1".into(), 1_000);
        p.apply_order_update(&OrderUpdate {
            client_ord_id: "cl_1".into(),
            ord_id: "ord_1".into(),
            state: "canceled".into(),
            fill_price: None, acc_fill_size: None,
            timestamp_ms: 2_000,
        });
        assert!(!p.has_active_entry(Timeframe::H1, PatternDirection::Bullish, EntryZone::Golden));
    }

    #[test]
    fn unsubmitted_entries_block_double_submit_but_never_open_gate() {
        // In dry-run (trading gate closed), we still record entries so the
        // main loop's "don't double submit" check keeps working.
        let mut p = Positions::new();
        let s = mk_signal(PatternDirection::Bullish, EntryZone::Golden, Timeframe::H1);
        p.record_unsubmitted(&s, "cl_dry_1".into(), 1_000);

        assert!(p.has_active_entry(Timeframe::H1, PatternDirection::Bullish, EntryZone::Golden));
        // No order events will ever arrive — gate stays closed.
        assert!(!p.has_open_golden(Timeframe::H1, PatternDirection::Bullish));
    }

    #[test]
    fn closed_position_releases_gate() {
        let mut p = Positions::new();
        let s = mk_signal(PatternDirection::Bullish, EntryZone::Golden, Timeframe::H1);
        p.record_submission(&s, "cl_1".into(), "ord_1".into(), 1_000);
        p.apply_order_update(&OrderUpdate {
            client_ord_id: "cl_1".into(), ord_id: "ord_1".into(),
            state: "filled".into(),
            fill_price: Some(100.0), acc_fill_size: Some(10),
            timestamp_ms: 2_000,
        });
        assert!(p.has_open_golden(Timeframe::H1, PatternDirection::Bullish));

        // OCO leg filled — position closes.
        p.mark_closed_by_parent("cl_1", 3_000);
        assert!(!p.has_open_golden(Timeframe::H1, PatternDirection::Bullish));
        assert!(!p.has_active_entry(Timeframe::H1, PatternDirection::Bullish, EntryZone::Golden));
    }

    #[test]
    fn order_id_index_routes_updates_missing_client_id() {
        // Some OKX events omit clOrdId — the by_ord_id index routes them.
        let mut p = Positions::new();
        let s = mk_signal(PatternDirection::Bullish, EntryZone::Golden, Timeframe::H1);
        p.record_submission(&s, "cl_1".into(), "ord_1".into(), 1_000);

        p.apply_order_update(&OrderUpdate {
            client_ord_id: "".into(),      // missing
            ord_id: "ord_1".into(),
            state: "filled".into(),
            fill_price: Some(100.0), acc_fill_size: Some(10),
            timestamp_ms: 2_000,
        });
        assert!(p.has_open_golden(Timeframe::H1, PatternDirection::Bullish));
    }
}
