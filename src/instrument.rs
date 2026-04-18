// src/instrument.rs
//
// ============================================================================
// EVOLUTION FROM v0.1 → v0.3
// ============================================================================
//
// v0.1: Instrument held a symbol and a last_price, updated via REST polling.
//       It asked "what's the price?" and got a snapshot back.
//
// v0.3: Instrument is now a LIVE STATE CONTAINER. It holds:
//       - The latest trade price (like before, but updated in real time)
//       - The current best bid/ask spread (from order book stream)
//       - Multi-timeframe candle aggregators (built from trade ticks)
//
// The key architectural shift: Instrument no longer *fetches* data.
// Data is *pushed into it* by the event loop via update_from_trade()
// and update_from_book(). The Instrument's job is to HOLD and ORGANIZE
// state so strategy modules can query it.
//
// OWNERSHIP MODEL:
// ================
// The event loop in main.rs owns the Instrument. Each StreamEvent gets
// routed to the appropriate update method. Strategy modules borrow
// &Instrument (immutable reference) to read state. This keeps things
// simple — no Arc<Mutex<>> needed because everything runs on one task.
//
// FUTURE: When the DSP pipeline needs to read order book state from a
// separate tokio task, we'll introduce Arc<RwLock<Instrument>>. But
// that's a Phase 3+ concern — don't overcomplicate until forced to.

use crate::candle::{Candle, CandleAggregator, MultiTimeframeAggregator, Timeframe};
use crate::ws_types::{TradeUpdate, OrderBookUpdate, BookLevel};
use tracing::info;

// ============================================================================
// TOP-OF-BOOK STATE
// ============================================================================
// Holds the most recent order book snapshot. Updated on every `books5` push.
// This is separate from candles — it's microstructure data that the DSP
// pipeline will eventually consume directly.

#[derive(Debug, Clone)]
pub struct TopOfBook {
    pub best_bid: Option<BookLevel>,
    pub best_ask: Option<BookLevel>,
    /// Spread in absolute price terms.
    pub spread: f64,
    /// Spread in basis points (1 bp = 0.01%).
    pub spread_bps: f64,
    /// Timestamp of the last book update.
    pub last_update_ms: u64,
}

impl TopOfBook {
    fn new() -> Self {
        TopOfBook {
            best_bid: None,
            best_ask: None,
            spread: 0.0,
            spread_bps: 0.0,
            last_update_ms: 0,
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

// ============================================================================
// INSTRUMENT
// ============================================================================

pub struct Instrument {
    /// The trading pair, e.g. "BTC-USDT-SWAP".
    pub symbol: String,

    /// Latest trade price. Updated on every trade tick.
    /// This is the same field as v0.1, just updated differently.
    pub last_price: f64,

    /// Latest trade side ("buy" or "sell"). Useful for trade flow analysis.
    pub last_side: String,

    /// Timestamp of the most recent trade, in epoch milliseconds.
    pub last_trade_ms: u64,

    /// Running count of trades processed since startup.
    pub trade_count: u64,

    /// Current top-of-book state from the order book stream.
    pub book: TopOfBook,

    /// Multi-timeframe candle aggregators.
    /// This is the core of v0.3 — candles at M1, M15, H1, H4 built
    /// simultaneously from the same trade tick stream.
    ///
    /// WHY THESE TIMEFRAMES:
    /// - M1:  High-resolution data for the DSP pipeline (FFT, filtering).
    ///        Also useful for precise entry timing.
    /// - M15: The ABC-BRC intraday structure. Swing points are visible here.
    /// - H1:  Primary timeframe for ABC-BRC pattern detection and Fibonacci.
    ///        The FMG guide's examples are mostly H1/H4.
    /// - H4:  Higher-timeframe structure context. The guide uses H4 for
    ///        swing targets and multi-timeframe compounding.
    candles: MultiTimeframeAggregator,
}

impl Instrument {
    /// Creates a new Instrument with default multi-timeframe candle setup.
    ///
    /// The history sizes are chosen to give each timeframe enough lookback
    /// for the strategies that will consume them:
    /// - M1  × 500  ≈ 8.3 hours  (DSP: enough for spectral windows)
    /// - M15 × 200  ≈ 50 hours   (ABC-BRC: ~2 days of swing structure)
    /// - H1  × 168  = 7 days     (ABC-BRC: primary pattern timeframe)
    /// - H4  × 180  = 30 days    (ABC-BRC: higher-timeframe context)
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
        }
    }

    /// Creates an Instrument with custom timeframe configuration.
    /// Use this if you want different timeframes or history depths.
    pub fn with_timeframes(symbol: &str, configs: Vec<(Timeframe, usize)>) -> Self {
        Instrument {
            symbol: symbol.to_string(),
            last_price: 0.0,
            last_side: String::new(),
            last_trade_ms: 0,
            trade_count: 0,
            book: TopOfBook::new(),
            candles: MultiTimeframeAggregator::new(configs),
        }
    }

    // ========================================================================
    // INGEST METHODS — called by the event loop
    // ========================================================================

    /// Process a trade tick from the WebSocket stream.
    ///
    /// Updates the last price, feeds the trade into all candle aggregators,
    /// and returns any candles that just closed.
    ///
    /// The return type Vec<(Timeframe, Candle)> lets the event loop know
    /// exactly which timeframes produced new candles on this tick. Most
    /// ticks return an empty Vec. Candle closes cluster at period boundaries.
    pub fn update_from_trade(&mut self, trade: &TradeUpdate) -> Vec<(Timeframe, Candle)> {
        // Update tick-level state
        self.last_price = trade.price;
        self.last_side = trade.side.clone();
        self.last_trade_ms = trade.timestamp_ms;
        self.trade_count += 1;

        // Fan the trade out to all timeframe aggregators.
        // This is the key operation — one trade can close candles at
        // multiple timeframes simultaneously.
        self.candles.update(trade)
    }

    /// Process an order book update from the WebSocket stream.
    /// Updates the top-of-book state (bid, ask, spread).
    pub fn update_from_book(&mut self, book: &OrderBookUpdate) {
        self.book.update(book);
    }

    // ========================================================================
    // CANDLE ACCESSORS — used by strategy modules
    // ========================================================================

    /// Get the candle aggregator for a specific timeframe.
    /// Returns None if that timeframe wasn't configured.
    ///
    /// Example:
    ///   let h1 = instrument.candles(Timeframe::H1).unwrap();
    ///   let last_3 = h1.recent_closes(3);
    pub fn candle_agg(&self, timeframe: Timeframe) -> Option<&CandleAggregator> {
        self.candles.get(timeframe)
    }

    /// Convenience: get the current (in-progress) candle at a timeframe.
    pub fn current_candle(&self, timeframe: Timeframe) -> Option<&Candle> {
        self.candles.get(timeframe)?.current()
    }

    /// Convenience: get the last closed candle at a timeframe.
    pub fn last_candle(&self, timeframe: Timeframe) -> Option<&Candle> {
        self.candles.get(timeframe)?.last_closed()
    }

    /// Check if a specific timeframe has enough history for your strategy.
    pub fn is_warmed_up(&self, timeframe: Timeframe, min_candles: usize) -> bool {
        self.candles
            .get(timeframe)
            .map_or(false, |a| a.is_warmed_up(min_candles))
    }

    /// Status line for all timeframes — useful for periodic logging.
    pub fn candle_status(&self) -> String {
        self.candles.status_summary()
    }
}
