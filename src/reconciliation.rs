// src/reconciliation.rs
//
// ============================================================================
// PHASE 2F — REST RECONCILIATION (v0.12)
// ============================================================================
//
// The private WebSocket pushes order/position events in near-real-time.
// When the connection is healthy, the local Positions registry is
// authoritative. But during any disconnect window — network blip,
// OKX maintenance, our own exponential-backoff stall — events that
// occur on the exchange are NOT replayed when the WS reconnects.
// OKX simply resumes sending events from the moment of reconnection
// onward.
//
// Without reconciliation:
//   - A Pending order that filled during the gap stays Pending locally.
//   - An Open position that hit stop/TP during the gap stays Open locally.
//   - The compounding gate (has_open_golden) gives wrong answers.
//   - Equity tracking drifts from exchange truth.
//
// This module fills the gap. It calls REST /trade/orders-history (which
// returns recent orders regardless of WS state), converts each entry
// into the same OrderUpdate type that the WS produces, and feeds them
// through Positions::apply_order_update.
//
// CRITICAL: idempotency.
//
// The state machine in position.rs is designed to handle the same
// OrderUpdate arriving multiple times — state transitions are guarded
// (Pending → Open requires the entry to have a fill; Open → Closed
// requires a stop/TP terminal state). So if reconciliation re-applies
// an event already received via WS, nothing breaks. This is the
// design property that lets us run reconciliation aggressively (every
// 5 minutes by default + on every reconnect) without worrying about
// double-counting.
//
// ============================================================================
// CALL CADENCE
// ============================================================================
//
// Two triggers:
//
//   1. Periodic — every PERIODIC_INTERVAL (default 5 minutes). Catches
//      both disconnect-window gaps and any quietly-rejected orders that
//      WS may have missed.
//
//   2. Post-reconnect — fired by main.rs immediately after the public
//      WS reconnects. Most disconnects affect both WS connections
//      simultaneously, so this is when state is most likely stale.
//
// We do NOT reconcile on every signal — that would generate one REST
// call per signal, and during high-activity periods that's too much
// load on OKX's rate limit (100 req/min for trade endpoints).
//
// ============================================================================
// SCOPE
// ============================================================================
//
// We only reconcile orders for the inst_ids the bot is actively
// trading. The orders-history endpoint is filtered by instId per call,
// so for multi-instrument deployments we'd loop. v0.12 hardcodes
// BTC-USDT-SWAP since that's our only active instrument.

use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{info, warn, debug};
use serde_json::Value;

use crate::okx_interface::OkxInterface;
use crate::position::{OrderUpdate, Positions};

/// Default cadence for periodic reconciliation. 5 minutes balances
/// freshness against OKX's rate limit (1 call every 5 min = 12/hour
/// vs. the 100/min limit, leaving plenty of headroom for trading).
pub const DEFAULT_PERIODIC_INTERVAL_SECS: u64 = 300;

#[derive(Debug, Clone, Default)]
pub struct ReconciliationStats {
    /// Number of REST calls successfully completed in this pass.
    pub api_calls: u32,
    /// Total order entries returned by OKX across all calls.
    pub orders_seen: u32,
    /// Order entries that we have a TrackedPosition for (i.e. the
    /// bot submitted them). Foreign orders — placed via OKX UI by a
    /// human, etc. — are skipped.
    pub orders_recognized: u32,
    /// State transitions that actually fired (i.e. the OrderUpdate
    /// changed something in Positions). Idempotent re-applications
    /// don't count.
    pub state_changes: u32,
    pub errors: u32,
}

/// Run one reconciliation pass against the given inst_id list.
/// Returns aggregate stats. Errors on individual instruments are
/// logged but don't fail the whole pass.
pub async fn reconcile(
    interface: &OkxInterface,
    positions: &Arc<Mutex<Positions>>,
    inst_ids: &[&str],
    inst_type: &str,
) -> ReconciliationStats {
    let mut stats = ReconciliationStats::default();

    for inst_id in inst_ids {
        debug!("[reconcile] fetching history for {}", inst_id);
        match interface.get_orders_history(inst_type, inst_id).await {
            Ok(json) => {
                stats.api_calls += 1;
                let pass_changes = apply_history_to_positions(&json, positions, &mut stats).await;
                if pass_changes > 0 {
                    info!(
                        "[reconcile] {}: {} state changes from {} orders",
                        inst_id, pass_changes, stats.orders_seen,
                    );
                } else {
                    debug!("[reconcile] {}: no state changes ({} orders inspected)",
                           inst_id, stats.orders_seen);
                }
            }
            Err(e) => {
                stats.errors += 1;
                warn!("[reconcile] {} failed: {} (will retry next pass)", inst_id, e);
            }
        }
    }

    stats
}

/// Walk the OKX response and apply each order to Positions.
/// Returns the number of state changes (transitions that actually
/// fired) so the caller can log meaningfully.
///
/// OKX response shape:
///   { "code": "0", "msg": "", "data": [
///       { "instId": "...", "ordId": "...", "clOrdId": "...",
///         "state": "filled" | "canceled" | "live" | "partially_filled",
///         "fillPx": "75000.0", "accFillSz": "10", "uTime": "..." (ms),
///         ... },
///       ...
///   ]}
async fn apply_history_to_positions(
    json: &Value,
    positions: &Arc<Mutex<Positions>>,
    stats: &mut ReconciliationStats,
) -> u32 {
    let code = json.get("code").and_then(|v| v.as_str()).unwrap_or("?");
    if code != "0" {
        let msg = json.get("msg").and_then(|v| v.as_str()).unwrap_or("(no message)");
        warn!("[reconcile] OKX returned code={} msg={}", code, msg);
        stats.errors += 1;
        return 0;
    }

    let data = match json.get("data").and_then(|v| v.as_array()) {
        Some(d) => d,
        None => {
            warn!("[reconcile] response missing 'data' array");
            return 0;
        }
    };

    let mut changes = 0u32;
    let mut pos = positions.lock().await;

    for entry in data {
        stats.orders_seen += 1;

        let update = match parse_order_entry(entry) {
            Some(u) => u,
            None => continue,  // unparseable; logged inside parse_order_entry
        };

        // Only count this as a recognized order if we already have a
        // TrackedPosition for the clOrdId. Foreign orders (placed via
        // UI) are skipped — they're not ours to manage.
        let recognized = pos.has_client_id(&update.client_ord_id);
        if !recognized {
            continue;
        }
        stats.orders_recognized += 1;

        // Snapshot the state field BEFORE applying. If it changes,
        // that's a real transition (the apply was not idempotent).
        let before_state = pos.state_for_client_id(&update.client_ord_id);
        pos.apply_order_update(&update);
        let after_state = pos.state_for_client_id(&update.client_ord_id);

        if before_state != after_state {
            changes += 1;
            stats.state_changes += 1;
            info!(
                "[reconcile] state change clOrdId={} {:?} → {:?}",
                update.client_ord_id, before_state, after_state,
            );
        }
    }

    changes
}

/// Parse a single OKX order entry into our OrderUpdate type.
/// Returns None and logs on parse failure (rare — usually means OKX
/// changed their schema).
fn parse_order_entry(entry: &Value) -> Option<OrderUpdate> {
    let cl_ord_id = entry.get("clOrdId")?.as_str()?.to_string();
    if cl_ord_id.is_empty() {
        // Orders placed via UI without clOrdId — not ours.
        return None;
    }
    let ord_id = entry.get("ordId")?.as_str()?.to_string();
    let state = entry.get("state")?.as_str()?.to_string();
    let fill_px = entry.get("fillPx")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<f64>().ok());
    let acc_fill_sz = entry.get("accFillSz")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<u64>().ok());
    let timestamp_ms = entry.get("uTime")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);

    Some(OrderUpdate {
        client_ord_id: cl_ord_id,
        ord_id,
        state,
        fill_price: fill_px,
        acc_fill_size: acc_fill_sz,
        timestamp_ms,
    })
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_filled_order_entry() {
        let entry = serde_json::json!({
            "instId": "BTC-USDT-SWAP",
            "clOrdId": "augur_h1_bull_golden_001",
            "ordId": "1234567890",
            "state": "filled",
            "fillPx": "75123.5",
            "accFillSz": "100",
            "uTime": "1745212800000",
        });
        let update = parse_order_entry(&entry).expect("should parse");
        assert_eq!(update.client_ord_id, "augur_h1_bull_golden_001");
        assert_eq!(update.ord_id, "1234567890");
        assert_eq!(update.state, "filled");
        assert_eq!(update.fill_price, Some(75123.5));
        assert_eq!(update.acc_fill_size, Some(100));
        assert_eq!(update.timestamp_ms, 1745212800000);
    }

    #[test]
    fn parse_skips_orders_without_cl_ord_id() {
        // UI-placed orders don't carry our clOrdId pattern.
        let entry = serde_json::json!({
            "instId": "BTC-USDT-SWAP",
            "clOrdId": "",
            "ordId": "9999999",
            "state": "filled",
        });
        assert!(parse_order_entry(&entry).is_none());
    }

    #[test]
    fn parse_handles_canceled_with_no_fill() {
        let entry = serde_json::json!({
            "instId": "BTC-USDT-SWAP",
            "clOrdId": "augur_h1_bear_sniper_007",
            "ordId": "5555555",
            "state": "canceled",
            "fillPx": "",
            "accFillSz": "0",
            "uTime": "1745212800000",
        });
        let update = parse_order_entry(&entry).expect("should parse canceled");
        assert_eq!(update.state, "canceled");
        // Empty string fillPx → None.
        assert_eq!(update.fill_price, None);
        assert_eq!(update.acc_fill_size, Some(0));
    }

    #[test]
    fn parse_returns_none_on_missing_required_field() {
        let entry = serde_json::json!({
            "instId": "BTC-USDT-SWAP",
            // clOrdId missing entirely
            "ordId": "1111",
            "state": "filled",
        });
        assert!(parse_order_entry(&entry).is_none());
    }

    #[tokio::test]
    async fn apply_history_returns_zero_on_error_response() {
        // OKX returns code != "0" on auth or rate-limit failures.
        let json = serde_json::json!({
            "code": "50113",
            "msg": "Invalid request",
            "data": [],
        });
        let positions = Arc::new(Mutex::new(Positions::new()));
        let mut stats = ReconciliationStats::default();
        let changes = apply_history_to_positions(&json, &positions, &mut stats).await;
        assert_eq!(changes, 0);
        assert_eq!(stats.errors, 1);
    }

    #[tokio::test]
    async fn apply_history_with_no_recognized_orders_returns_zero() {
        // Bot has no positions tracked, so even valid orders in the
        // response don't trigger anything (they're foreign).
        let json = serde_json::json!({
            "code": "0",
            "msg": "",
            "data": [
                {
                    "instId": "BTC-USDT-SWAP",
                    "clOrdId": "augur_unknown_clOrdId",
                    "ordId": "99",
                    "state": "filled",
                    "fillPx": "75000",
                    "accFillSz": "10",
                    "uTime": "1745212800000",
                }
            ],
        });
        let positions = Arc::new(Mutex::new(Positions::new()));
        let mut stats = ReconciliationStats::default();
        let changes = apply_history_to_positions(&json, &positions, &mut stats).await;
        assert_eq!(changes, 0);
        assert_eq!(stats.orders_seen, 1);
        assert_eq!(stats.orders_recognized, 0);
    }

    #[test]
    fn parse_order_entry_handles_partial_fill() {
        let entry = serde_json::json!({
            "instId": "BTC-USDT-SWAP",
            "clOrdId": "augur_h1_bull_golden_002",
            "ordId": "5555",
            "state": "partially_filled",
            "fillPx": "75100.0",
            "accFillSz": "50",  // partial of 100
            "uTime": "1745212800000",
        });
        let update = parse_order_entry(&entry).expect("should parse");
        assert_eq!(update.state, "partially_filled");
        assert_eq!(update.acc_fill_size, Some(50));
    }
}
