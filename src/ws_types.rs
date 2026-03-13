// src/ws_types.rs
//
// WHY THIS FILE EXISTS:
// Raw WebSocket messages arrive as JSON strings. We deserialize them into
// these Rust structs so the rest of the codebase works with typed data
// instead of fragile json["data"][0]["px"] lookups. This is where your
// existing `instrument.rs` pattern of parsing json["data"] evolves to.
//
// OKX WebSocket docs: https://www.okx.com/docs-v5/en/#order-book-trading-market-data-ws

use serde::Deserialize;

// ============================================================================
// RAW OKX ENVELOPE
// ============================================================================
// Every message from OKX wraps data in this structure:
//   { "arg": { "channel": "trades", "instId": "BTC-USDT-SWAP" },
//     "data": [ ... ] }
//
// We deserialize the envelope first, then dispatch based on `channel`.

#[derive(Debug, Deserialize)]
pub struct WsEnvelope {
    pub arg: Option<WsArg>,
    pub data: Option<serde_json::Value>,
    // OKX also sends event messages like subscription confirmations:
    //   { "event": "subscribe", "arg": {...} }
    pub event: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct WsArg {
    pub channel: String,
    #[serde(rename = "instId")]
    pub inst_id: Option<String>,
}

// ============================================================================
// TRADE UPDATE
// ============================================================================
// Channel: "trades"
// Fires on every executed trade on the exchange.
// This is your tick-by-tick feed.

#[derive(Debug, Clone, Deserialize)]
pub struct TradeData {
    #[serde(rename = "instId")]
    pub inst_id: String,
    #[serde(rename = "tradeId")]
    pub trade_id: String,
    /// Price as string from OKX — we parse to f64 downstream.
    pub px: String,
    /// Size (quantity) of the trade.
    pub sz: String,
    /// "buy" or "sell" — the taker's side.
    pub side: String,
    /// Timestamp in milliseconds.
    pub ts: String,
}

// ============================================================================
// ORDER BOOK SNAPSHOT (L2 Top-of-Book)
// ============================================================================
// Channel: "books5"
// Pushes the top 5 bid/ask levels every time they change.
// This is lightweight and perfect for Phase 1. Later, "books-l2-tbt"
// gives you the full 400-level book tick-by-tick for your FPGA pipeline.

#[derive(Debug, Clone, Deserialize)]
pub struct BookData {
    #[serde(rename = "instId")]
    pub inst_id: Option<String>,
    /// Each entry: [price, size, deprecated, num_orders]
    pub asks: Vec<Vec<String>>,
    /// Each entry: [price, size, deprecated, num_orders]
    pub bids: Vec<Vec<String>>,
    pub ts: String,
}

// ============================================================================
// DISPATCHED UPDATES
// ============================================================================
// These are the "clean" types that flow through your mpsc channels.
// The WsClient parses raw JSON into these and sends them downstream.
// Your strategy code never touches raw JSON.

#[derive(Debug, Clone)]
pub struct TradeUpdate {
    pub inst_id: String,
    pub price: f64,
    pub size: f64,
    pub side: String,
    pub timestamp_ms: u64,
}

#[derive(Debug, Clone)]
pub struct BookLevel {
    pub price: f64,
    pub size: f64,
}

#[derive(Debug, Clone)]
pub struct OrderBookUpdate {
    pub inst_id: String,
    pub asks: Vec<BookLevel>,  // Sorted: lowest ask first (best ask at [0])
    pub bids: Vec<BookLevel>,  // Sorted: highest bid first (best bid at [0])
    pub timestamp_ms: u64,
}

// ============================================================================
// UNIFIED STREAM EVENT
// ============================================================================
// This enum multiplexes both channels into a single stream.
// Your main loop does `match event { ... }` to handle each type.

#[derive(Debug, Clone)]
pub enum StreamEvent {
    Trade(TradeUpdate),
    Book(OrderBookUpdate),
}
