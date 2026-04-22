// src/bin/backtest.rs
//
// Thin CLI wrapper around augur::backtest::BacktestEngine. All logic
// lives in the library module; this file just parses arguments, loads
// config, runs the engine, prints metrics, and exits.

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;
use tracing::{info, error};

use augur::backtest::{BacktestConfig, BacktestEngine, FillMode};

fn print_usage() {
    eprintln!("Usage:");
    eprintln!("  backtest --data <dir> [--equity <usdt>] [--csv <path>] [--fill-mode <mode>]");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --data <dir>         Directory of *_trades_*.parquet files (required)");
    eprintln!("  --equity <usdt>      Starting equity (default 10000)");
    eprintln!("  --csv <path>         Write per-trade CSV to this path (default: skip)");
    eprintln!("  --fill-mode <mode>   Fill simulation: 'optimistic' (default) or 'ladder'");
    eprintln!("                       Ladder mode requires *_books_*.parquet files in the");
    eprintln!("                       same directory (v0.9+ persistence).");
    eprintln!();
    eprintln!("Environment:");
    eprintln!("  RUST_LOG=info        Controls log verbosity");
}

fn parse_args() -> Result<BacktestConfig, String> {
    let mut args: Vec<String> = env::args().skip(1).collect();
    let mut data_dir: Option<PathBuf> = None;
    let mut equity: f64 = 10_000.0;
    let mut csv: Option<PathBuf> = None;
    let mut fill_mode: FillMode = FillMode::Optimistic;

    while let Some(arg) = args.first().cloned() {
        match arg.as_str() {
            "--data" => {
                args.remove(0);
                let v = args.first().ok_or("--data requires a value")?.clone();
                data_dir = Some(PathBuf::from(v));
                args.remove(0);
            }
            "--equity" => {
                args.remove(0);
                let v = args.first().ok_or("--equity requires a value")?.clone();
                equity = v.parse::<f64>().map_err(|_| "--equity must be a number")?;
                args.remove(0);
            }
            "--csv" => {
                args.remove(0);
                let v = args.first().ok_or("--csv requires a value")?.clone();
                csv = Some(PathBuf::from(v));
                args.remove(0);
            }
            "--fill-mode" => {
                args.remove(0);
                let v = args.first().ok_or("--fill-mode requires a value")?.clone();
                fill_mode = match v.as_str() {
                    "optimistic" => FillMode::Optimistic,
                    "ladder"     => FillMode::Ladder,
                    other => return Err(format!(
                        "--fill-mode must be 'optimistic' or 'ladder', got '{}'", other)),
                };
                args.remove(0);
            }
            "-h" | "--help" => return Err("help".to_string()),
            other => return Err(format!("unknown argument: {}", other)),
        }
    }

    let data_dir = data_dir.ok_or("--data is required")?;
    let mut cfg = BacktestConfig::with_defaults(data_dir, equity);
    cfg.csv_output = csv;
    cfg.fill_mode = fill_mode;
    Ok(cfg)
}

fn main() -> ExitCode {
    tracing_subscriber::fmt::init();

    let config = match parse_args() {
        Ok(c) => c,
        Err(msg) => {
            if msg != "help" {
                eprintln!("Error: {}\n", msg);
            }
            print_usage();
            return ExitCode::from(if msg == "help" { 0 } else { 2 });
        }
    };

    info!("=== AUGUR BACKTEST v0.11 ===");
    info!("Data dir       : {}", config.data_dir.display());
    info!("Fill mode      : {}", config.fill_mode);
    info!("Starting equity: ${:.2}", config.starting_equity);
    info!("Risk per trade : {:.1}%", config.risk_params.risk_per_trade_pct * 100.0);
    info!("Min R:R        : 1:{:.1}", config.risk_params.min_rr_ratio);
    if let Some(p) = &config.csv_output {
        info!("CSV output     : {}", p.display());
    } else {
        info!("CSV output     : (none — use --csv to enable)");
    }

    let mut engine = BacktestEngine::new(config);
    match engine.run() {
        Ok(metrics) => {
            metrics.print_summary();
            ExitCode::SUCCESS
        }
        Err(e) => {
            error!("Backtest failed: {}", e);
            ExitCode::FAILURE
        }
    }
}
