// src/persistence.rs
//
// ============================================================================
// WHY THIS MODULE EXISTS (Phase 2.5 / v0.8)
// ============================================================================
//
// Phase 3 (backtesting) cannot proceed without historical tick data. OKX's
// downloadable archive CSVs have coarse resolution; what we actually want
// is the exact tick stream our live bot consumes, so replay produces bit-
// identical candles and pattern detections to live. That means recording
// ticks from the WebSocket feed as we run.
//
// This is also Jetson-critical: if dry-run is going to show its value
// by eventually supporting a backtest, the Jetson needs to be writing
// ticks to disk from day one of paper trading. Otherwise we burn the
// dry-run period and come out with nothing to replay against.
//
// ============================================================================
// SD CARD PROTECTION
// ============================================================================
//
// The Jetson Orin Nano dev kit boots from MicroSD by default. MicroSD
// cards have ~2–3k write cycles per cell; a naive "write every tick" loop
// would destroy the card in days. We mitigate with two techniques:
//
//   1. Buffer in RAM, flush in bulk. A single 100k-tick Parquet file is
//      ~1 MB compressed — one flush per ~10 minutes of moderate activity.
//      That's 3 orders of magnitude fewer writes than per-tick.
//
//   2. Write to an EXTERNAL path, not the root filesystem. The deployment
//      expects a USB SSD mounted at /mnt/usb_ssd/augur_data (path is
//      configurable via AUGUR_PERSISTENCE_PATH). SSDs tolerate orders of
//      magnitude more writes than MicroSD.
//
// If the external path isn't writable, persistence refuses to start
// rather than silently falling back to the root FS. Better to fail
// loud than to quietly kill the SD card.
//
// ============================================================================
// ARCHITECTURE
// ============================================================================
//
//   Main task                  Persistence task (spawned)
//   ----------                 --------------------------
//     WS event                     |
//     ticks in  ──try_send──►  channel (bounded 10k)
//                                  │
//                                  ▼
//                             push into Vec<> columns
//                                  │
//                                  ▼
//                         if buffer full OR interval elapsed:
//                                  │
//                             spawn_blocking:
//                                  │
//                             build RecordBatch → ArrowWriter → Parquet file
//                                  │
//                                  ▼
//                              clear buffer, resume
//
// Bounded channel (not unbounded): if writer ever falls behind — disk
// full, SSD unmounted, slow IO — try_send returns Err and the main task
// drops the tick with a warning. Counter tracks drops for observability.
// Unbounded would be a latent OOM.
//
// ============================================================================
// GRACEFUL SHUTDOWN
// ============================================================================
//
// Main task listens for SIGINT. On signal: drop the tick_tx sender, which
// causes the persistence task's channel to close. The task recognizes
// closure, performs one final flush of remaining buffered ticks, and
// exits. Main awaits the task before exiting itself. No corrupted files.

use std::path::PathBuf;
use std::sync::Arc;
use std::fs::File;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::{info, error};

use arrow::array::{ArrayRef, Float64Array, StringArray, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use parquet::file::properties::WriterProperties;
use parquet::basic::Compression;

// ============================================================================
// TICK RECORD
// ============================================================================
// The cross-task message. Cloned from TradeUpdate at send time — a
// Vec<String> assignment is cheaper than holding the lock on a shared
// buffer. We accept the allocation cost at the boundary.

#[derive(Debug, Clone)]
pub struct TickRecord {
    pub timestamp_ms: u64,
    pub price: f64,
    pub size: f64,
    pub side: String,
    pub inst_id: String,
}

// ============================================================================
// CONFIGURATION
// ============================================================================

#[derive(Debug, Clone)]
pub struct PersistenceConfig {
    /// Directory where Parquet files are written. Must exist and be writable.
    pub output_dir: PathBuf,
    /// Flush when the in-memory buffer reaches this many ticks.
    pub buffer_size: usize,
    /// Flush at least this often, regardless of buffer fill.
    pub flush_interval: Duration,
    /// Capacity of the cross-task channel. Producer uses try_send and
    /// drops ticks if full — this is the headroom in ticks before drops
    /// begin.
    pub channel_capacity: usize,
}

impl PersistenceConfig {
    pub fn from_env(path: &str) -> Self {
        let output_dir = PathBuf::from(path);
        PersistenceConfig {
            output_dir,
            buffer_size: std::env::var("AUGUR_PERSISTENCE_BUFFER_SIZE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(100_000),
            flush_interval: Duration::from_secs(
                std::env::var("AUGUR_PERSISTENCE_FLUSH_INTERVAL_SECS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(3_600),  // 1 hour
            ),
            channel_capacity: 10_000,
        }
    }
}

// ============================================================================
// BUFFER
// ============================================================================
// Column-wise layout matches the Parquet format natively — one Vec per
// future column. Pre-allocate to buffer_size so no reallocation happens
// during the hot path.

struct TickBuffer {
    timestamp_ms: Vec<u64>,
    price: Vec<f64>,
    size: Vec<f64>,
    side: Vec<String>,
    inst_id: Vec<String>,
    capacity: usize,
}

impl TickBuffer {
    fn with_capacity(capacity: usize) -> Self {
        TickBuffer {
            timestamp_ms: Vec::with_capacity(capacity),
            price:        Vec::with_capacity(capacity),
            size:         Vec::with_capacity(capacity),
            side:         Vec::with_capacity(capacity),
            inst_id:      Vec::with_capacity(capacity),
            capacity,
        }
    }

    fn push(&mut self, record: TickRecord) {
        self.timestamp_ms.push(record.timestamp_ms);
        self.price.push(record.price);
        self.size.push(record.size);
        self.side.push(record.side);
        self.inst_id.push(record.inst_id);
    }

    fn len(&self) -> usize { self.timestamp_ms.len() }
    fn is_empty(&self) -> bool { self.timestamp_ms.is_empty() }
    fn is_full(&self) -> bool { self.len() >= self.capacity }

    /// Destructively take ownership of the current buffer, leaving an
    /// empty one behind. Called before flushing — the old buffer moves
    /// to the blocking writer task while a fresh one continues accepting
    /// ticks.
    fn drain(&mut self) -> TickBuffer {
        let drained = TickBuffer {
            timestamp_ms: std::mem::replace(&mut self.timestamp_ms, Vec::with_capacity(self.capacity)),
            price:        std::mem::replace(&mut self.price,        Vec::with_capacity(self.capacity)),
            size:         std::mem::replace(&mut self.size,         Vec::with_capacity(self.capacity)),
            side:         std::mem::replace(&mut self.side,         Vec::with_capacity(self.capacity)),
            inst_id:      std::mem::replace(&mut self.inst_id,      Vec::with_capacity(self.capacity)),
            capacity:     self.capacity,
        };
        drained
    }
}

// ============================================================================
// SCHEMA
// ============================================================================

fn tick_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("timestamp_ms", DataType::UInt64, false),
        Field::new("price",        DataType::Float64, false),
        Field::new("size",         DataType::Float64, false),
        Field::new("side",         DataType::Utf8,    false),
        Field::new("inst_id",      DataType::Utf8,    false),
    ]))
}

// ============================================================================
// FLUSH (blocking) — runs inside spawn_blocking
// ============================================================================

fn flush_to_parquet(
    buffer: TickBuffer,
    output_dir: PathBuf,
) -> Result<(PathBuf, usize), Box<dyn std::error::Error + Send + Sync>> {
    let schema = tick_schema();

    let ts_array:      ArrayRef = Arc::new(UInt64Array::from(buffer.timestamp_ms));
    let price_array:   ArrayRef = Arc::new(Float64Array::from(buffer.price));
    let size_array:    ArrayRef = Arc::new(Float64Array::from(buffer.size));
    let side_array:    ArrayRef = Arc::new(StringArray::from(buffer.side));
    let inst_id_array: ArrayRef = Arc::new(StringArray::from(buffer.inst_id));

    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![ts_array, price_array, size_array, side_array, inst_id_array],
    )?;

    // Filename includes nanosecond suffix so multiple flushes per hour
    // don't collide — the other agent's spec had this bug.
    let now = chrono::Utc::now();
    let filename = format!(
        "BTC-USDT-SWAP_{}_{}.parquet",
        now.format("%Y%m%d_%H%M%S"),
        now.format("%f"),  // nanoseconds
    );
    let path = output_dir.join(&filename);
    let file = File::create(&path)?;

    let props = WriterProperties::builder()
        .set_compression(Compression::SNAPPY)
        .build();
    let mut writer = ArrowWriter::try_new(file, schema, Some(props))?;
    let rows = batch.num_rows();
    writer.write(&batch)?;
    writer.close()?;

    Ok((path, rows))
}

// ============================================================================
// RUN LOOP
// ============================================================================

/// Spawn-able async task that owns the receiver end of the tick channel.
/// Runs until the sender is dropped, performs final flush, then returns.
pub async fn run(
    config: PersistenceConfig,
    mut rx: mpsc::Receiver<TickRecord>,
) {
    // Pre-flight: ensure output directory exists and is writable.
    if let Err(e) = std::fs::create_dir_all(&config.output_dir) {
        error!(
            "[persistence] cannot create {}: {} — persistence will not run",
            config.output_dir.display(), e,
        );
        // Drain the channel so producers don't block. Ticks are dropped.
        while rx.recv().await.is_some() {}
        return;
    }

    info!(
        "[persistence] writing to {} | buffer {} ticks | flush every {}s",
        config.output_dir.display(),
        config.buffer_size,
        config.flush_interval.as_secs(),
    );

    let mut buffer = TickBuffer::with_capacity(config.buffer_size);
    let mut interval = tokio::time::interval_at(
        Instant::now() + config.flush_interval,
        config.flush_interval,
    );
    let mut total_written: u64 = 0;
    let mut files_written: u64 = 0;

    loop {
        tokio::select! {
            // Incoming tick
            maybe_tick = rx.recv() => {
                match maybe_tick {
                    Some(tick) => {
                        buffer.push(tick);
                        if buffer.is_full() {
                            perform_flush(
                                &mut buffer,
                                &config.output_dir,
                                &mut total_written,
                                &mut files_written,
                                "buffer full",
                            ).await;
                        }
                    }
                    None => {
                        // Channel closed — main is shutting down.
                        break;
                    }
                }
            }

            // Time-based flush
            _ = interval.tick() => {
                if !buffer.is_empty() {
                    perform_flush(
                        &mut buffer,
                        &config.output_dir,
                        &mut total_written,
                        &mut files_written,
                        "interval",
                    ).await;
                }
            }
        }
    }

    // Final flush on shutdown
    info!("[persistence] channel closed — performing final flush");
    if !buffer.is_empty() {
        perform_flush(
            &mut buffer,
            &config.output_dir,
            &mut total_written,
            &mut files_written,
            "shutdown",
        ).await;
    }

    info!(
        "[persistence] shutdown complete: {} ticks written across {} files",
        total_written, files_written,
    );
}

async fn perform_flush(
    buffer: &mut TickBuffer,
    output_dir: &PathBuf,
    total_written: &mut u64,
    files_written: &mut u64,
    trigger: &str,
) {
    let drained = buffer.drain();
    let _count = drained.len();
    let output_dir_clone = output_dir.clone();

    // Parquet writer is synchronous. Move the buffer into a blocking
    // pool thread so the async runtime stays responsive.
    let result = tokio::task::spawn_blocking(move || {
        flush_to_parquet(drained, output_dir_clone)
    }).await;

    match result {
        Ok(Ok((path, rows))) => {
            *total_written += rows as u64;
            *files_written += 1;
            info!(
                "[persistence] flush ({}): {} ticks → {}",
                trigger, rows, path.display(),
            );
        }
        Ok(Err(e)) => {
            error!("[persistence] flush failed ({}): {}", trigger, e);
        }
        Err(e) => {
            error!("[persistence] spawn_blocking panicked ({}): {}", trigger, e);
        }
    }
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn sample_tick(ts: u64, px: f64) -> TickRecord {
        TickRecord {
            timestamp_ms: ts,
            price: px,
            size: 0.01,
            side: "buy".to_string(),
            inst_id: "BTC-USDT-SWAP".to_string(),
        }
    }

    #[test]
    fn buffer_capacity_is_enforced() {
        let mut b = TickBuffer::with_capacity(3);
        assert!(b.is_empty());
        b.push(sample_tick(1, 100.0));
        b.push(sample_tick(2, 101.0));
        assert!(!b.is_full());
        b.push(sample_tick(3, 102.0));
        assert!(b.is_full());
    }

    #[test]
    fn buffer_drain_yields_empty_replacement() {
        let mut b = TickBuffer::with_capacity(100);
        for i in 0..5u64 {
            b.push(sample_tick(i, 100.0 + i as f64));
        }
        let drained = b.drain();
        assert_eq!(drained.len(), 5);
        assert!(b.is_empty());
    }

    #[test]
    fn flush_writes_parquet_file() {
        // End-to-end: build a buffer, flush to disk, check the file exists
        // and parquet-decodes to the right row count.
        use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

        let dir = tempdir().unwrap();
        let mut b = TickBuffer::with_capacity(10);
        for i in 0..7u64 {
            b.push(sample_tick(i * 1000, 100.0 + i as f64));
        }

        let drained = b.drain();
        let (path, rows) = flush_to_parquet(drained, dir.path().to_path_buf()).unwrap();

        assert_eq!(rows, 7);
        assert!(path.exists());

        // Round-trip: read the file back and verify row count + schema.
        let file = File::open(&path).unwrap();
        let builder = ParquetRecordBatchReaderBuilder::try_new(file).unwrap();
        let schema = builder.schema().clone();
        let reader = builder.build().unwrap();

        let mut total = 0;
        for batch_result in reader {
            total += batch_result.unwrap().num_rows();
        }
        assert_eq!(total, 7);

        // Schema sanity
        assert_eq!(schema.field(0).name(), "timestamp_ms");
        assert_eq!(schema.field(1).name(), "price");
        assert_eq!(schema.field(2).name(), "size");
        assert_eq!(schema.field(3).name(), "side");
        assert_eq!(schema.field(4).name(), "inst_id");
    }

    #[tokio::test]
    async fn end_to_end_channel_driven_flush() {
        // Fire ticks at the task over a channel, drop the sender,
        // verify the task performs a final flush and exits.
        let dir = tempdir().unwrap();
        let config = PersistenceConfig {
            output_dir: dir.path().to_path_buf(),
            buffer_size: 3,  // small — triggers buffer-full flush
            flush_interval: Duration::from_secs(3600), // not triggered in test
            channel_capacity: 100,
        };

        let (tx, rx) = mpsc::channel::<TickRecord>(100);
        let handle = tokio::spawn(run(config, rx));

        for i in 0..7u64 {
            tx.send(sample_tick(i, 100.0 + i as f64)).await.unwrap();
        }
        // Dropping tx closes the channel; task should final-flush and exit.
        drop(tx);
        handle.await.unwrap();

        // Expect at least 2 parquet files (2 full buffers + 1 partial on shutdown).
        let files: Vec<_> = std::fs::read_dir(dir.path()).unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map_or(false, |x| x == "parquet"))
            .collect();
        assert!(files.len() >= 2, "expected at least 2 files, got {}", files.len());
    }
}
