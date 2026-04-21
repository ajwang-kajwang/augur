// src/instrument.rs
//
// Live market state container. Holds top-of-book, multi-timeframe candle
// aggregators, and per-timeframe swing detectors. Fed by the event loop.

use crate::candle::{Candle, CandleAggregator, MultiTimeframeAggregator, Timeframe};
use crate::swing::{SwingDetector, SwingPoint};
use crate::ws_types::{TradeUpdate, OrderBookUpdate, BookLevel};

#[derive(Debug, Clone)]
pub struct TopOfBook {
    pub best_bid: Option<BookLevel>,
    pub best_ask: Option<BookLevel>,
    pub spread: f64,
    pub spread_bps: f64,
    pub last_update_ms: u64,
}

impl TopOfBook {
    fn new() -> Self {
        TopOfBook {
            best_bid: None, best_ask: None,
            spread: 0.0, spread_bps: 0.0, last_update_ms: 0,
        }
    }

    fn update(&mut self, book: &OrderBookUpdate) {
        self.best_bid = book.bids.first().cloned();
        self.best_ask = book.asks.first().cloned();
        self.last_update_ms = book.timestamp_ms;

        if let (Some(bid), Some(ask)) = (&self.best_bid, &self.best_ask) {
            self.spread = ask.price - bid.price;
            if bid.price > 0.0 {
                self.spread_bps = (self.spread / bid.price) * 10_000.0;
            }
        }
    }
}

/// What happened as a result of ingesting one trade tick.
pub struct IngestOutcome {
    pub closed_candles: Vec<(Timeframe, Candle)>,
    pub new_swings: Vec<(Timeframe, SwingPoint)>,
}

pub struct Instrument {
    pub symbol: String,
    pub last_price: f64,
    pub last_side: String,
    pub last_trade_ms: u64,
    pub trade_count: u64,
    pub book: TopOfBook,

    candles: MultiTimeframeAggregator,
    swing_detectors: Vec<(Timeframe, SwingDetector)>,
}
#[allow(dead_code)]
impl Instrument {
    /// Default config: candles M1×500, M15×200, H1×168, H4×180;
    /// swing detectors on H1 and H4 (lookback=3, max_swings=50).
    pub fn new(symbol: &str) -> Self {
        Instrument {
            symbol: symbol.to_string(),
            last_price: 0.0,
            last_side: String::new(),
            last_trade_ms: 0,
            trade_count: 0,
            book: TopOfBook::new(),
            candles: MultiTimeframeAggregator::new(vec![
                (Timeframe::M1,  500),
                (Timeframe::M15, 200),
                (Timeframe::H1,  168),
                (Timeframe::H4,  180),
            ]),
            swing_detectors: vec![
                (Timeframe::H1, SwingDetector::new(3, 50)),
                (Timeframe::H4, SwingDetector::new(3, 50)),
            ],
        }
    }

    pub fn with_config(
        symbol: &str,
        candle_configs: Vec<(Timeframe, usize)>,
        swing_configs: Vec<(Timeframe, usize, usize)>,
    ) -> Self {
        Instrument {
            symbol: symbol.to_string(),
            last_price: 0.0,
            last_side: String::new(),
            last_trade_ms: 0,
            trade_count: 0,
            book: TopOfBook::new(),
            candles: MultiTimeframeAggregator::new(candle_configs),
            swing_detectors: swing_configs.into_iter()
                .map(|(tf, lb, max)| (tf, SwingDetector::new(lb, max)))
                .collect(),
        }
    }

    /// Process a trade tick. Updates tick state, fans the trade out to all
    /// candle aggregators, and scans swing detectors on any timeframe whose
    /// candle just closed.
    pub fn update_from_trade(&mut self, trade: &TradeUpdate) -> IngestOutcome {
        self.last_price = trade.price;
        self.last_side = trade.side.clone();
        self.last_trade_ms = trade.timestamp_ms;
        self.trade_count += 1;

        let closed_candles = self.candles.update(trade);

        let mut new_swings = Vec::new();
        for (tf, _closed) in &closed_candles {
            let detector = self.swing_detectors.iter_mut()
                .find(|(dtf, _)| dtf == tf);

            if let Some((_, det)) = detector {
                if let Some(agg) = self.candles.get(*tf) {
                    for swing in det.update(agg.history()) {
                        new_swings.push((*tf, swing));
                    }
                }
            }
        }

        IngestOutcome { closed_candles, new_swings }
    }

    pub fn update_from_book(&mut self, book: &OrderBookUpdate) {
        self.book.update(book);
    }

    // -- Candle accessors --

    pub fn candle_agg(&self, timeframe: Timeframe) -> Option<&CandleAggregator> {
        self.candles.get(timeframe)
    }

    pub fn current_candle(&self, timeframe: Timeframe) -> Option<&Candle> {
        self.candles.get(timeframe)?.current()
    }

    pub fn last_candle(&self, timeframe: Timeframe) -> Option<&Candle> {
        self.candles.get(timeframe)?.last_closed()
    }

    pub fn is_warmed_up(&self, timeframe: Timeframe, min_candles: usize) -> bool {
        self.candles.get(timeframe)
            .map_or(false, |a| a.is_warmed_up(min_candles))
    }

    pub fn candle_status(&self) -> String {
        self.candles.status_summary()
    }

    // -- Swing accessors --

    pub fn swing_detector(&self, timeframe: Timeframe) -> Option<&SwingDetector> {
        self.swing_detectors.iter()
            .find(|(tf, _)| *tf == timeframe)
            .map(|(_, det)| det)
    }

    pub fn swing_status(&self) -> String {
        self.swing_detectors.iter()
            .map(|(tf, det)| format!("{}:{}swings", tf, det.len()))
            .collect::<Vec<_>>()
            .join(" | ")
    }
}
