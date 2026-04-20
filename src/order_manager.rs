// src/order_manager.rs
//
// ============================================================================
// UPGRADE FROM v0.1 → v0.6
// ============================================================================
//
// v0.1: market_buy() wrapper that placed a single market order with
//       hardcoded side and size. Proof-of-life only.
//
// v0.6: takes a fully-specified TradeSignal from the risk engine and
//       constructs a limit order with an ATTACHED OCO exit pair
//       (take-profit + stop-loss). Both legs are submitted in a single
//       API call via OKX's `attachAlgoOrds` field.
//
// ============================================================================
// WHY attachAlgoOrds
// ============================================================================
//
// The naive approach is three separate API calls: place entry, wait for
// fill, place stop, place take-profit. Problems with this:
//
//   1. Between entry fill and stop placement, the position is naked —
//      a sudden adverse move can blow through the intended risk limit.
//   2. Fill detection requires private WebSocket subscription or polling
//      (added complexity, added latency).
//   3. Three API calls = three sources of failure; partial state is hard
//      to recover from.
//
// OKX's `attachAlgoOrds` embeds the OCO exit pair INSIDE the entry order.
// The exchange holds the algo orders server-side and activates them only
// when the entry fills. No polling, no race window, no partial-state
// recovery logic needed. This is the idiomatic path for the FMG strategy's
// "set and walk away" limit-order discipline.
//
// ============================================================================
// OKX PAYLOAD SHAPE
// ============================================================================
//
// POST /api/v5/trade/order
// {
//   "instId":   "BTC-USDT-SWAP",
//   "tdMode":   "cross",
//   "side":     "buy" | "sell",
//   "posSide":  "long" | "short",   // required under hedging mode
//   "ordType":  "limit",
//   "px":       "<entry_price>",
//   "sz":       "<contracts>",
//   "attachAlgoOrds": [{
//     "attachAlgoClOrdId": "<unique-id>",
//     "tpTriggerPx":       "<target_price>",
//     "tpOrdPx":           "-1",           // -1 = market at trigger
//     "tpTriggerPxType":   "last",
//     "slTriggerPx":       "<stop_price>",
//     "slOrdPx":           "-1",
//     "slTriggerPxType":   "last"
//   }]
// }
//
// The attached algo is reduce-only by construction (it only fires to
// close the position opened by the parent entry).

use crate::okx_interface::OkxInterface;
use crate::risk::TradeSignal;
use crate::abc_brc::PatternDirection;
use std::error::Error;
use std::fmt;
use tracing::{info, warn, error};

// ============================================================================
// SUBMISSION OUTCOME
// ============================================================================

#[derive(Debug, Clone)]
pub struct OrderSubmission {
    /// OKX's order ID for the entry limit order.
    pub entry_ord_id: String,
    /// Our internal client order ID (useful for correlation in logs).
    pub client_ord_id: String,
}

#[derive(Debug)]
pub enum SubmissionError {
    /// OKX rejected the payload. The `code` and `msg` fields from the
    /// response body are preserved for diagnostics.
    Rejected { code: String, msg: String },
    /// Network / transport error — unknown whether the order landed.
    /// Treat this as "unknown state" rather than "definitely failed".
    Transport(String),
    /// Response parsed but lacked the expected shape.
    Malformed(String),
}

impl fmt::Display for SubmissionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SubmissionError::Rejected { code, msg } =>
                write!(f, "OKX rejected order: code={} msg={}", code, msg),
            SubmissionError::Transport(e) =>
                write!(f, "transport error: {}", e),
            SubmissionError::Malformed(e) =>
                write!(f, "malformed response: {}", e),
        }
    }
}

impl Error for SubmissionError {}

// ============================================================================
// ORDER MANAGER
// ============================================================================

pub struct OrderManager;

impl OrderManager {
    /// Preserved from v0.1 — simple market buy for manual testing.
    pub fn market_buy(interface: &OkxInterface, symbol: &str, size: &str) {
        info!(">>> MARKET BUY {} contracts of {}", size, symbol);
        match interface.place_order(symbol, "buy", size) {
            Ok(resp) => {
                if resp["code"] == "0" {
                    let ord_id = resp["data"][0]["ordId"].as_str().unwrap_or("Unknown");
                    info!("Order filled: {}", ord_id);
                } else {
                    warn!("Order rejected: {}", resp["msg"]);
                }
            }
            Err(e) => error!("Execution error: {}", e),
        }
    }

    /// Submit a fully-specified trade signal to OKX as a limit order with
    /// an attached OCO exit pair (stop-loss + take-profit).
    ///
    /// Returns the OKX order ID on success, or a SubmissionError describing
    /// what failed. The caller (main.rs) is responsible for logging and
    /// deciding whether to retry or abandon.
    pub async fn submit_signal(
        interface: &OkxInterface,
        inst_id: &str,
        signal: &TradeSignal,
        client_ord_id: &str,
    ) -> Result<OrderSubmission, Box<dyn std::error::Error>> {
        let payload = build_payload(inst_id, signal, client_ord_id);
        info!(
            "Submitting {} {} {}: {}ct @ ${:.2} | SL ${:.2} | TP ${:.2}",
            signal.direction, signal.entry_zone, inst_id,
            signal.contracts, signal.entry_price,
            signal.stop_price, signal.target_price,
        );

        let resp = interface.place_order_payload(&payload)
            .map_err(|e| SubmissionError::Transport(e.to_string())).await?;

        // OKX wraps both success and failure in { "code": "...", "msg": "...", "data": [...] }.
        // Top-level code "0" means the request was accepted; per-order codes
        // sit inside data[0].sCode.
        let top_code = resp["code"].as_str().unwrap_or("");
        let top_msg  = resp["msg"].as_str().unwrap_or("");

        if top_code != "0" {
            return Err(SubmissionError::Rejected {
                code: top_code.to_string(),
                msg: top_msg.to_string(),
            });
        }

        // Some OKX responses put success inside data[0].sCode — check that too.
        let data = resp["data"].as_array()
            .ok_or_else(|| SubmissionError::Malformed("missing data array".to_string()))?;
        let first = data.first()
            .ok_or_else(|| SubmissionError::Malformed("empty data array".to_string()))?;
        let per_order_code = first["sCode"].as_str().unwrap_or("0");
        if per_order_code != "0" {
            let per_msg = first["sMsg"].as_str().unwrap_or("").to_string();
            return Err(SubmissionError::Rejected {
                code: per_order_code.to_string(),
                msg: per_msg,
            });
        }

        let ord_id = first["ordId"].as_str()
            .ok_or_else(|| SubmissionError::Malformed("missing ordId".to_string()))?
            .to_string();

        info!("Order accepted: ordId={} clOrdId={}", ord_id, client_ord_id);
        Ok(OrderSubmission {
            entry_ord_id: ord_id,
            client_ord_id: client_ord_id.to_string(),
        })
    }

    /// Cancel a previously-placed entry limit order. Attached algo orders
    /// on an unfilled entry are cancelled automatically by OKX when the
    /// parent is cancelled.
    pub fn cancel(interface: &OkxInterface, inst_id: &str, ord_id: &str)
        -> Result<(), SubmissionError>
    {
        info!("Cancelling order {} on {}", ord_id, inst_id);
        let resp = interface.cancel_order(inst_id, ord_id)
            .map_err(|e| SubmissionError::Transport(e.to_string()))?;
        let code = resp["code"].as_str().unwrap_or("");
        if code == "0" {
            info!("Cancelled {}", ord_id);
            Ok(())
        } else {
            Err(SubmissionError::Rejected {
                code: code.to_string(),
                msg: resp["msg"].as_str().unwrap_or("").to_string(),
            })
        }
    }
}

// ============================================================================
// PAYLOAD BUILDER — extracted for testability
// ============================================================================

/// Construct the OKX payload JSON for a TradeSignal. Extracted from the
/// manager so tests can verify the exact field shape without making
/// network calls.
pub fn build_payload(
    inst_id: &str,
    signal: &TradeSignal,
    client_ord_id: &str,
) -> serde_json::Value {
    // Position side under hedge mode: "long" for bullish, "short" for bearish.
    // Under one-way (net) mode OKX ignores this field, so it's safe to set
    // unconditionally.
    let pos_side = match signal.direction {
        PatternDirection::Bullish => "long",
        PatternDirection::Bearish => "short",
    };

    // Format prices to OKX's expected precision. BTC-USDT-SWAP accepts
    // $0.10 tick size — we use 2 decimals to be safe.
    let fmt_px = |p: f64| format!("{:.2}", p);

    serde_json::json!({
        "instId":   inst_id,
        "tdMode":   "cross",
        "side":     signal.entry_side(),
        "posSide":  pos_side,
        "ordType":  "limit",
        "px":       fmt_px(signal.entry_price),
        "sz":       signal.contracts.to_string(),
        "clOrdId":  client_ord_id,
        "attachAlgoOrds": [{
            "attachAlgoClOrdId": format!("{}_oco", client_ord_id),
            "tpTriggerPx":       fmt_px(signal.target_price),
            "tpOrdPx":           "-1",
            "tpTriggerPxType":   "last",
            "slTriggerPx":       fmt_px(signal.stop_price),
            "slOrdPx":           "-1",
            "slTriggerPxType":   "last",
        }],
    })
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::risk::{TradeSignal, EntryZone};
    use crate::abc_brc::PatternDirection;
    use crate::candle::Timeframe;

    fn sample_bullish_signal() -> TradeSignal {
        TradeSignal {
            direction: PatternDirection::Bullish,
            timeframe: Timeframe::H1,
            entry_zone: EntryZone::Golden,
            entry_price: 84230.50,
            stop_price: 83400.00,
            target_price: 89500.00,
            contracts: 12,
            risk_amount: 99.66,
            reward_amount: 632.34,
            rr_ratio: 6.35,
        }
    }

    fn sample_bearish_signal() -> TradeSignal {
        TradeSignal {
            direction: PatternDirection::Bearish,
            timeframe: Timeframe::H1,
            entry_zone: EntryZone::Sniper,
            entry_price: 84250.00,
            stop_price: 85000.00,
            target_price: 80100.00,
            contracts: 13,
            risk_amount: 97.50,
            reward_amount: 539.50,
            rr_ratio: 5.53,
        }
    }

    #[test]
    fn test_bullish_payload_shape() {
        let signal = sample_bullish_signal();
        let payload = build_payload("BTC-USDT-SWAP", &signal, "augur-test-1");

        assert_eq!(payload["instId"], "BTC-USDT-SWAP");
        assert_eq!(payload["tdMode"], "cross");
        assert_eq!(payload["side"], "buy");
        assert_eq!(payload["posSide"], "long");
        assert_eq!(payload["ordType"], "limit");
        assert_eq!(payload["px"], "84230.50");
        assert_eq!(payload["sz"], "12");
        assert_eq!(payload["clOrdId"], "augur-test-1");

        let algo = &payload["attachAlgoOrds"][0];
        assert_eq!(algo["attachAlgoClOrdId"], "augur-test-1_oco");
        assert_eq!(algo["tpTriggerPx"], "89500.00");
        assert_eq!(algo["slTriggerPx"], "83400.00");
        assert_eq!(algo["tpOrdPx"], "-1");  // market at trigger
        assert_eq!(algo["slOrdPx"], "-1");
    }

    #[test]
    fn test_bearish_payload_flips_side() {
        let signal = sample_bearish_signal();
        let payload = build_payload("BTC-USDT-SWAP", &signal, "augur-test-2");

        assert_eq!(payload["side"], "sell");
        assert_eq!(payload["posSide"], "short");
        // Entry above stop (bearish), target below entry
        assert_eq!(payload["px"], "84250.00");
        assert_eq!(payload["attachAlgoOrds"][0]["slTriggerPx"], "85000.00");
        assert_eq!(payload["attachAlgoOrds"][0]["tpTriggerPx"], "80100.00");
    }

    #[test]
    fn test_client_order_id_is_used_verbatim() {
        // The clOrdId must be passed through unchanged — this is how we
        // correlate log entries with exchange order IDs.
        let signal = sample_bullish_signal();
        let my_id = "augur_H1_ABCD_1710000000";
        let payload = build_payload("BTC-USDT-SWAP", &signal, my_id);
        assert_eq!(payload["clOrdId"], my_id);
        assert_eq!(
            payload["attachAlgoOrds"][0]["attachAlgoClOrdId"],
            format!("{}_oco", my_id),
        );
    }
}
