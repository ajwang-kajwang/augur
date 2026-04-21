// src/order_manager.rs
//
// v0.6 async. Takes a TradeSignal from the risk engine and constructs a
// limit order with an ATTACHED OCO exit pair (stop-loss + take-profit).
// Both legs are submitted in a single API call via OKX's `attachAlgoOrds`.

use crate::okx_interface::OkxInterface;
use crate::risk::TradeSignal;
use crate::abc_brc::PatternDirection;
use std::error::Error;
use std::fmt;
use tracing::{info, warn, error};

#[derive(Debug, Clone)]
pub struct OrderSubmission {
    pub entry_ord_id: String,
    pub client_ord_id: String,
}

#[derive(Debug)]
pub enum SubmissionError {
    Rejected { code: String, msg: String },
    Transport(String),
    Malformed(String),
}

impl fmt::Display for SubmissionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SubmissionError::Rejected { code, msg } =>
                write!(f, "OKX rejected order: code={} msg={}", code, msg),
            SubmissionError::Transport(e) => write!(f, "transport error: {}", e),
            SubmissionError::Malformed(e) => write!(f, "malformed response: {}", e),
        }
    }
}

impl Error for SubmissionError {}

pub struct OrderManager;
#[allow(dead_code)]
impl OrderManager {
    pub async fn market_buy(interface: &OkxInterface, symbol: &str, size: &str) {
        info!(">>> MARKET BUY {} contracts of {}", size, symbol);
        match interface.place_order(symbol, "buy", size).await {
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

    pub async fn submit_signal(
        interface: &OkxInterface,
        inst_id: &str,
        signal: &TradeSignal,
        client_ord_id: &str,
    ) -> Result<OrderSubmission, Box<dyn Error>> {
        let payload = build_payload(inst_id, signal, client_ord_id);
        info!(
            "Submitting {} {} {}: {}ct @ ${:.2} | SL ${:.2} | TP ${:.2}",
            signal.direction, signal.entry_zone, inst_id,
            signal.contracts, signal.entry_price,
            signal.stop_price, signal.target_price,
        );

        let resp = interface.place_order_payload(&payload)
            .await
            .map_err(|e| SubmissionError::Transport(e.to_string()))?;

        let top_code = resp["code"].as_str().unwrap_or("");
        let top_msg = resp["msg"].as_str().unwrap_or("");
        if top_code != "0" {
            return Err(Box::new(SubmissionError::Rejected {
                code: top_code.to_string(),
                msg: top_msg.to_string(),
            }));
        }

        let data = resp["data"].as_array()
            .ok_or_else(|| SubmissionError::Malformed("missing data array".to_string()))?;
        let first = data.first()
            .ok_or_else(|| SubmissionError::Malformed("empty data array".to_string()))?;
        let per_order_code = first["sCode"].as_str().unwrap_or("0");
        if per_order_code != "0" {
            let per_msg = first["sMsg"].as_str().unwrap_or("").to_string();
            return Err(Box::new(SubmissionError::Rejected {
                code: per_order_code.to_string(),
                msg: per_msg,
            }));
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

    pub async fn cancel(interface: &OkxInterface, inst_id: &str, ord_id: &str)
        -> Result<(), SubmissionError>
    {
        info!("Cancelling order {} on {}", ord_id, inst_id);
        let resp = interface.cancel_order(inst_id, ord_id)
            .await
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

pub fn build_payload(
    inst_id: &str,
    signal: &TradeSignal,
    client_ord_id: &str,
) -> serde_json::Value {
    let pos_side = match signal.direction {
        PatternDirection::Bullish => "long",
        PatternDirection::Bearish => "short",
    };
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
            entry_price: 84230.50, stop_price: 83400.00, target_price: 89500.00,
            contracts: 12, risk_amount: 99.66, reward_amount: 632.34, rr_ratio: 6.35,
        }
    }

    fn sample_bearish_signal() -> TradeSignal {
        TradeSignal {
            direction: PatternDirection::Bearish,
            timeframe: Timeframe::H1,
            entry_zone: EntryZone::Sniper,
            entry_price: 84250.00, stop_price: 85000.00, target_price: 80100.00,
            contracts: 13, risk_amount: 97.50, reward_amount: 539.50, rr_ratio: 5.53,
        }
    }

    #[test]
    fn test_bullish_payload_shape() {
        let signal = sample_bullish_signal();
        let payload = build_payload("BTC-USDT-SWAP", &signal, "augur-test-1");
        assert_eq!(payload["instId"], "BTC-USDT-SWAP");
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
    }

    #[test]
    fn test_bearish_payload_flips_side() {
        let signal = sample_bearish_signal();
        let payload = build_payload("BTC-USDT-SWAP", &signal, "augur-test-2");
        assert_eq!(payload["side"], "sell");
        assert_eq!(payload["posSide"], "short");
        assert_eq!(payload["px"], "84250.00");
        assert_eq!(payload["attachAlgoOrds"][0]["slTriggerPx"], "85000.00");
        assert_eq!(payload["attachAlgoOrds"][0]["tpTriggerPx"], "80100.00");
    }

    #[test]
    fn test_client_order_id_is_used_verbatim() {
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
