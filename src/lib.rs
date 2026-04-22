// src/lib.rs
//
// ============================================================================
// SHARED LIBRARY — v0.10
// ============================================================================
//
// Converted the crate to a library + two binaries in v0.10. The `augur`
// binary is the live trading engine; the `backtest` binary replays
// recorded Parquet files through the same strategy modules to validate
// performance before committing capital.
//
// Both binaries depend on the same module tree. Keeping the strategy
// primitives (candle, swing, abc_brc, fibonacci, risk) in the library
// guarantees that backtest signals are produced by bit-identical code
// to live signals — the whole point of Phase 3. The binaries differ
// only in their data source (WebSocket vs. Parquet) and their
// execution path (REST order manager vs. optimistic fill simulator).
//
// Modules wired through for the binaries to consume:

pub mod config;
pub mod okx_interface;
pub mod instrument;
pub mod order_manager;
pub mod ws_client;
pub mod ws_types;
pub mod candle;
pub mod swing;
pub mod abc_brc;
pub mod fibonacci;
pub mod risk;
pub mod position;
pub mod private_ws;
pub mod reconnect;
pub mod persistence;
pub mod reconciliation;
pub mod health;
pub mod backtest;
