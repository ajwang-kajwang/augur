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
// ============================================================================
// WHAT CHANGED IN v0.9
// ============================================================================
//
// v0.8 recorded TRADES only. Phase 4's DSP research needs order book
// snapshots too — order-flow imbalance, book pressure, and Kalman
// filters on mid-price all depend on per-tick bid/ask data that trades
// alone don't carry.
//
// v0.9 adds a parallel BOOK writer sharing the same infrastructure:
//
//   - Same bounded-channel + try_send pattern
//   - Same buffer-full or interval-elapsed flush triggers
//   - Same spawn_blocking pattern so disk IO never stalls the runtime
//   - Same final-flush-on-shutdown guarantee
//   - Separate schema (wide: 22 cols for top-5 bid + ask) and separate
//     filename prefix so book data can be filtered from trades at read time
//
// Both writers run as independent tasks. If the book writer stalls or
// fails, trade recording keeps going (and vice versa). The writer
// factory `spawn_writer()` is generic over the record type to keep
// this cleanly DRY rather than copy-pasted.
//
// ============================================================================
// SD CARD PROTECTION
// ============================================================================
//
// The Jetson Orin Nano dev kit boots from MicroSD. MicroSD has ~2-3k
// write cycles per cell; writing every tick would destroy the card in
// days. We mitigate by buffering in RAM and flushing to an EXTERNAL
// USB SSD mount (AUGUR_PERSISTENCE_PATH). If the external path isn't
// writable, persistence refuses to start rather than silently falling
// back to the root FS.

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
// RECORD TYPES
// ============================================================================
// The cross-task messages. Cloned from the WS types at send time — a
// Vec<String> assignment is cheaper than holding a shared lock on a
// buffer. We accept the allocation cost at the boundary in exchange
// for single-ownership semantics on the writer side.

#[derive(Debug, Clone)]
pub struct TickRecord {
    pub timestamp_ms: u64,
    pub price: f64,
    pub size: f64,
    pub side: String,
    pub inst_id: String,
}

/// Top-5 book snapshot. Fixed-width: exactly 5 levels per side, zero-
/// padded if the exchange returns fewer (never happens in practice on
/// `books5`, but defensive coding doesn't cost much).
///
/// The wide-schema choice: every row is one complete snapshot. The DSP
/// pipeline reads rows directly without unmelting; the backtester's
/// simulated-fill logic walks the ladder in a single column access.
/// If/when we add `books-l2-tbt` recording (400 levels), it gets its
/// own writer with a separate schema — mixing depths in one file would
/// be structurally awkward.
#[derive(Debug, Clone)]
pub struct BookRecord {
    pub timestamp_ms: u64,
    pub inst_id: String,
    /// Bids, best-first (index 0 = top of book bid).
    pub bid_prices: [f64; 5],
    pub bid_sizes:  [f64; 5],
    /// Asks, best-first (index 0 = top of book ask).
    pub ask_prices: [f64; 5],
    pub ask_sizes:  [f64; 5],
}

// ============================================================================
// CONFIGURATION
// ============================================================================

#[derive(Debug, Clone)]
pub struct PersistenceConfig {
    /// Directory where Parquet files are written. Must exist and be writable.
    pub output_dir: PathBuf,
    /// Flush when the in-memory trade buffer reaches this many ticks.
    pub trade_buffer_size: usize,
    /// Flush when the in-memory book buffer reaches this many snapshots.
    /// Books update at roughly 10-50x the rate of trades on liquid pairs,
    /// so this is deliberately larger than the trade buffer to keep flush
    /// cadence comparable.
    pub book_buffer_size: usize,
    /// Flush at least this often, regardless of buffer fill. Both writers
    /// share the interval — it keeps file naming roughly aligned across
    /// the two streams, which helps when reading the corpus back.
    pub flush_interval: Duration,
    /// Capacity of each cross-task channel. Producer uses try_send and
    /// drops on full — this is the headroom in ticks/snapshots before
    /// drops begin.
    pub channel_capacity: usize,
}

impl PersistenceConfig {
    pub fn from_env(path: &str) -> Self {
        let output_dir = PathBuf::from(path);
        PersistenceConfig {
            output_dir,
            trade_buffer_size: std::env::var("AUGUR_PERSISTENCE_TRADE_BUFFER")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(100_000),
            book_buffer_size: std::env::var("AUGUR_PERSISTENCE_BOOK_BUFFER")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(200_000),
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
// WRITABLE TRAIT
// ============================================================================
// Abstracts the column-layout + Parquet-schema concerns so the writer
// loop can be generic over trade vs. book buffers. Three responsibilities:
//
//   1. Describe the Parquet schema (static)
//   2. Provide a fresh, pre-allocated buffer (capacity-aware)
//   3. Push one record, and serialize a full buffer to Arrow arrays

pub trait Writable: Send + 'static {
    type Buffer: BufferOps + Send + 'static;

    /// File prefix: "BTC-USDT-SWAP_trades" or "BTC-USDT-SWAP_books".
    fn file_prefix() -> &'static str;

    /// Static Parquet schema for this record type.
    fn schema() -> Arc<Schema>;

    /// Build a new pre-allocated buffer with the given capacity.
    fn new_buffer(capacity: usize) -> Self::Buffer;

    /// Push one record into the buffer.
    fn push(buffer: &mut Self::Buffer, record: Self);

    /// Convert a filled buffer into a RecordBatch ready for Parquet write.
    fn into_batch(buffer: Self::Buffer)
        -> Result<RecordBatch, Box<dyn std::error::Error + Send + Sync>>;
}

/// Shared buffer operations that don't depend on the record shape.
/// We split these out because the concrete buffer type (TickBuffer,
/// BookBuffer) is associated with the record, but the writer loop only
/// needs the generic drain + len + is_full behaviour.
pub trait BufferOps: Sized {
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool { self.len() == 0 }
    fn is_full(&self, capacity: usize) -> bool { self.len() >= capacity }
    /// Destructively take ownership of the current buffer, leaving an
    /// empty one behind. The drained half goes to the blocking writer
    /// while a fresh buffer keeps accepting records.
    fn drain(&mut self, capacity: usize) -> Self;
}

// ============================================================================
// TICK BUFFER + WRITABLE IMPL
// ============================================================================

pub struct TickBuffer {
    timestamp_ms: Vec<u64>,
    price: Vec<f64>,
    size: Vec<f64>,
    side: Vec<String>,
    inst_id: Vec<String>,
}

impl BufferOps for TickBuffer {
    fn len(&self) -> usize { self.timestamp_ms.len() }
    fn drain(&mut self, capacity: usize) -> Self {
        TickBuffer {
            timestamp_ms: std::mem::replace(&mut self.timestamp_ms, Vec::with_capacity(capacity)),
            price:        std::mem::replace(&mut self.price,        Vec::with_capacity(capacity)),
            size:         std::mem::replace(&mut self.size,         Vec::with_capacity(capacity)),
            side:         std::mem::replace(&mut self.side,         Vec::with_capacity(capacity)),
            inst_id:      std::mem::replace(&mut self.inst_id,      Vec::with_capacity(capacity)),
        }
    }
}

impl Writable for TickRecord {
    type Buffer = TickBuffer;

    fn file_prefix() -> &'static str { "BTC-USDT-SWAP_trades" }

    fn schema() -> Arc<Schema> {
        Arc::new(Schema::new(vec![
            Field::new("timestamp_ms", DataType::UInt64, false),
            Field::new("price",        DataType::Float64, false),
            Field::new("size",         DataType::Float64, false),
            Field::new("side",         DataType::Utf8,    false),
            Field::new("inst_id",      DataType::Utf8,    false),
        ]))
    }

    fn new_buffer(capacity: usize) -> TickBuffer {
        TickBuffer {
            timestamp_ms: Vec::with_capacity(capacity),
            price:        Vec::with_capacity(capacity),
            size:         Vec::with_capacity(capacity),
            side:         Vec::with_capacity(capacity),
            inst_id:      Vec::with_capacity(capacity),
        }
    }

    fn push(buffer: &mut TickBuffer, record: Self) {
        buffer.timestamp_ms.push(record.timestamp_ms);
        buffer.price.push(record.price);
        buffer.size.push(record.size);
        buffer.side.push(record.side);
        buffer.inst_id.push(record.inst_id);
    }

    fn into_batch(buffer: TickBuffer)
        -> Result<RecordBatch, Box<dyn std::error::Error + Send + Sync>>
    {
        let schema = Self::schema();
        let arrays: Vec<ArrayRef> = vec![
            Arc::new(UInt64Array::from(buffer.timestamp_ms)),
            Arc::new(Float64Array::from(buffer.price)),
            Arc::new(Float64Array::from(buffer.size)),
            Arc::new(StringArray::from(buffer.side)),
            Arc::new(StringArray::from(buffer.inst_id)),
        ];
        Ok(RecordBatch::try_new(schema, arrays)?)
    }
}

// ============================================================================
// BOOK BUFFER + WRITABLE IMPL
// ============================================================================
// Wide schema: 22 columns (timestamp + inst_id + 5×bid_price + 5×bid_size
// + 5×ask_price + 5×ask_size). Each row is one complete book snapshot.
//
// Why 22 columns rather than a nested struct or array type? Arrow 49's
// parquet writer supports both, but downstream tools (Polars, pandas,
// cuDF) consume flat schemas with zero ceremony and awkward schemas with
// varying degrees of pain. The DSP layer will almost always compute on
// mid-price = (bid_px_0 + ask_px_0) / 2, spread = ask_px_0 - bid_px_0,
// top-N imbalance = sum(bid_sz_0..bid_sz_4) / sum(ask_sz_0..ask_sz_4) —
// all direct column accesses. Wide wins.

pub struct BookBuffer {
    timestamp_ms: Vec<u64>,
    inst_id:      Vec<String>,
    // 10 price columns + 10 size columns. Stored as 10 separate Vec<f64>
    // each (bid_px_0..4 + ask_px_0..4) and likewise for sizes — this
    // mirrors how they'll come out as Arrow arrays, avoids transpose.
    bid_prices: [Vec<f64>; 5],
    bid_sizes:  [Vec<f64>; 5],
    ask_prices: [Vec<f64>; 5],
    ask_sizes:  [Vec<f64>; 5],
}

impl BookBuffer {
    fn empty(capacity: usize) -> Self {
        // Arrays of 5 Vecs each — can't use Default::default() because
        // Vec<f64> doesn't implement Copy. Build explicitly.
        BookBuffer {
            timestamp_ms: Vec::with_capacity(capacity),
            inst_id:      Vec::with_capacity(capacity),
            bid_prices: [
                Vec::with_capacity(capacity), Vec::with_capacity(capacity),
                Vec::with_capacity(capacity), Vec::with_capacity(capacity),
                Vec::with_capacity(capacity),
            ],
            bid_sizes: [
                Vec::with_capacity(capacity), Vec::with_capacity(capacity),
                Vec::with_capacity(capacity), Vec::with_capacity(capacity),
                Vec::with_capacity(capacity),
            ],
            ask_prices: [
                Vec::with_capacity(capacity), Vec::with_capacity(capacity),
                Vec::with_capacity(capacity), Vec::with_capacity(capacity),
                Vec::with_capacity(capacity),
            ],
            ask_sizes: [
                Vec::with_capacity(capacity), Vec::with_capacity(capacity),
                Vec::with_capacity(capacity), Vec::with_capacity(capacity),
                Vec::with_capacity(capacity),
            ],
        }
    }
}

impl BufferOps for BookBuffer {
    fn len(&self) -> usize { self.timestamp_ms.len() }

    fn drain(&mut self, capacity: usize) -> Self {
        let take = |v: &mut Vec<f64>| std::mem::replace(v, Vec::with_capacity(capacity));
        BookBuffer {
            timestamp_ms: std::mem::replace(&mut self.timestamp_ms, Vec::with_capacity(capacity)),
            inst_id:      std::mem::replace(&mut self.inst_id,      Vec::with_capacity(capacity)),
            bid_prices: [
                take(&mut self.bid_prices[0]), take(&mut self.bid_prices[1]),
                take(&mut self.bid_prices[2]), take(&mut self.bid_prices[3]),
                take(&mut self.bid_prices[4]),
            ],
            bid_sizes: [
                take(&mut self.bid_sizes[0]), take(&mut self.bid_sizes[1]),
                take(&mut self.bid_sizes[2]), take(&mut self.bid_sizes[3]),
                take(&mut self.bid_sizes[4]),
            ],
            ask_prices: [
                take(&mut self.ask_prices[0]), take(&mut self.ask_prices[1]),
                take(&mut self.ask_prices[2]), take(&mut self.ask_prices[3]),
                take(&mut self.ask_prices[4]),
            ],
            ask_sizes: [
                take(&mut self.ask_sizes[0]), take(&mut self.ask_sizes[1]),
                take(&mut self.ask_sizes[2]), take(&mut self.ask_sizes[3]),
                take(&mut self.ask_sizes[4]),
            ],
        }
    }
}

impl Writable for BookRecord {
    type Buffer = BookBuffer;

    fn file_prefix() -> &'static str { "BTC-USDT-SWAP_books" }

    fn schema() -> Arc<Schema> {
        let mut fields = vec![
            Field::new("timestamp_ms", DataType::UInt64, false),
            Field::new("inst_id",      DataType::Utf8,    false),
        ];
        for i in 0..5 {
            fields.push(Field::new(&format!("bid_price_{}", i), DataType::Float64, false));
        }
        for i in 0..5 {
            fields.push(Field::new(&format!("bid_size_{}", i),  DataType::Float64, false));
        }
        for i in 0..5 {
            fields.push(Field::new(&format!("ask_price_{}", i), DataType::Float64, false));
        }
        for i in 0..5 {
            fields.push(Field::new(&format!("ask_size_{}", i),  DataType::Float64, false));
        }
        Arc::new(Schema::new(fields))
    }

    fn new_buffer(capacity: usize) -> BookBuffer { BookBuffer::empty(capacity) }

    fn push(buffer: &mut BookBuffer, record: Self) {
        buffer.timestamp_ms.push(record.timestamp_ms);
        buffer.inst_id.push(record.inst_id);
        for i in 0..5 {
            buffer.bid_prices[i].push(record.bid_prices[i]);
            buffer.bid_sizes[i].push(record.bid_sizes[i]);
            buffer.ask_prices[i].push(record.ask_prices[i]);
            buffer.ask_sizes[i].push(record.ask_sizes[i]);
        }
    }

    fn into_batch(buffer: BookBuffer)
        -> Result<RecordBatch, Box<dyn std::error::Error + Send + Sync>>
    {
        let schema = Self::schema();
        // Destructure the fixed-size arrays so we can move each Vec
        // into the Arrow array without cloning.
        let BookBuffer {
            timestamp_ms, inst_id,
            bid_prices, bid_sizes, ask_prices, ask_sizes,
        } = buffer;

        let [bp0, bp1, bp2, bp3, bp4] = bid_prices;
        let [bs0, bs1, bs2, bs3, bs4] = bid_sizes;
        let [ap0, ap1, ap2, ap3, ap4] = ask_prices;
        let [as0, as1, as2, as3, as4] = ask_sizes;

        let arrays: Vec<ArrayRef> = vec![
            Arc::new(UInt64Array::from(timestamp_ms)),
            Arc::new(StringArray::from(inst_id)),
            Arc::new(Float64Array::from(bp0)), Arc::new(Float64Array::from(bp1)),
            Arc::new(Float64Array::from(bp2)), Arc::new(Float64Array::from(bp3)),
            Arc::new(Float64Array::from(bp4)),
            Arc::new(Float64Array::from(bs0)), Arc::new(Float64Array::from(bs1)),
            Arc::new(Float64Array::from(bs2)), Arc::new(Float64Array::from(bs3)),
            Arc::new(Float64Array::from(bs4)),
            Arc::new(Float64Array::from(ap0)), Arc::new(Float64Array::from(ap1)),
            Arc::new(Float64Array::from(ap2)), Arc::new(Float64Array::from(ap3)),
            Arc::new(Float64Array::from(ap4)),
            Arc::new(Float64Array::from(as0)), Arc::new(Float64Array::from(as1)),
            Arc::new(Float64Array::from(as2)), Arc::new(Float64Array::from(as3)),
            Arc::new(Float64Array::from(as4)),
        ];
        Ok(RecordBatch::try_new(schema, arrays)?)
    }
}

// ============================================================================
// FLUSH (blocking) — runs inside spawn_blocking
// ============================================================================

fn flush_batch<R: Writable>(
    buffer: R::Buffer,
    output_dir: PathBuf,
) -> Result<(PathBuf, usize), Box<dyn std::error::Error + Send + Sync>> {
    let batch = R::into_batch(buffer)?;
    let rows = batch.num_rows();

    // Nanosecond suffix in the filename guarantees uniqueness across
    // rapid back-to-back flushes. Hour-only naming (the original spec)
    // would collide when the buffer fills mid-hour.
    let now = chrono::Utc::now();
    let filename = format!(
        "{}_{}_{}.parquet",
        R::file_prefix(),
        now.format("%Y%m%d_%H%M%S"),
        now.format("%f"),
    );
    let path = output_dir.join(&filename);
    let file = File::create(&path)?;

    let props = WriterProperties::builder()
        .set_compression(Compression::SNAPPY)
        .build();
    let mut writer = ArrowWriter::try_new(file, R::schema(), Some(props))?;
    writer.write(&batch)?;
    writer.close()?;

    Ok((path, rows))
}

// ============================================================================
// GENERIC RUN LOOP
// ============================================================================

/// Spawn-able async task for any Writable record type. Owns the receiver
/// end of its channel. Runs until the sender is dropped, performs final
/// flush, then returns.
pub async fn run_writer<R: Writable>(
    capacity: usize,
    output_dir: PathBuf,
    flush_interval: Duration,
    mut rx: mpsc::Receiver<R>,
) {
    info!(
        "[persistence:{}] writing to {} | buffer {} records | flush every {}s",
        R::file_prefix(), output_dir.display(), capacity, flush_interval.as_secs(),
    );

    let mut buffer = R::new_buffer(capacity);
    let mut interval = tokio::time::interval_at(
        Instant::now() + flush_interval,
        flush_interval,
    );
    let mut total_written: u64 = 0;
    let mut files_written: u64 = 0;

    loop {
        tokio::select! {
            maybe_record = rx.recv() => {
                match maybe_record {
                    Some(record) => {
                        R::push(&mut buffer, record);
                        if buffer.is_full(capacity) {
                            perform_flush::<R>(
                                &mut buffer,
                                &output_dir,
                                capacity,
                                &mut total_written,
                                &mut files_written,
                                "buffer full",
                            ).await;
                        }
                    }
                    None => break,  // Channel closed — shutdown.
                }
            }

            _ = interval.tick() => {
                if !buffer.is_empty() {
                    perform_flush::<R>(
                        &mut buffer,
                        &output_dir,
                        capacity,
                        &mut total_written,
                        &mut files_written,
                        "interval",
                    ).await;
                }
            }
        }
    }

    // Final flush on shutdown.
    info!("[persistence:{}] channel closed — performing final flush", R::file_prefix());
    if !buffer.is_empty() {
        perform_flush::<R>(
            &mut buffer,
            &output_dir,
            capacity,
            &mut total_written,
            &mut files_written,
            "shutdown",
        ).await;
    }

    info!(
        "[persistence:{}] shutdown complete: {} records across {} files",
        R::file_prefix(), total_written, files_written,
    );
}

async fn perform_flush<R: Writable>(
    buffer: &mut R::Buffer,
    output_dir: &PathBuf,
    capacity: usize,
    total_written: &mut u64,
    files_written: &mut u64,
    trigger: &str,
) {
    let drained = buffer.drain(capacity);
    let output_dir_clone = output_dir.clone();

    let result = tokio::task::spawn_blocking(move || {
        flush_batch::<R>(drained, output_dir_clone)
    }).await;

    match result {
        Ok(Ok((path, rows))) => {
            *total_written += rows as u64;
            *files_written += 1;
            info!(
                "[persistence:{}] flush ({}): {} records → {}",
                R::file_prefix(), trigger, rows, path.display(),
            );
        }
        Ok(Err(e)) => {
            error!("[persistence:{}] flush failed ({}): {}", R::file_prefix(), trigger, e);
        }
        Err(e) => {
            error!("[persistence:{}] spawn_blocking panicked ({}): {}", R::file_prefix(), trigger, e);
        }
    }
}

// ============================================================================
// TOP-LEVEL ENTRY — spawns both writers
// ============================================================================

pub struct PersistenceHandles {
    pub tick_tx: mpsc::Sender<TickRecord>,
    pub book_tx: mpsc::Sender<BookRecord>,
    pub tick_task: tokio::task::JoinHandle<()>,
    pub book_task: tokio::task::JoinHandle<()>,
}

/// Validate the output directory and spawn both writer tasks.
/// Returns None if the output path isn't usable (persistence is
/// disabled for this process lifetime — trades and books both drop).
pub fn spawn_all(config: PersistenceConfig) -> Option<PersistenceHandles> {
    if let Err(e) = std::fs::create_dir_all(&config.output_dir) {
        error!(
            "[persistence] cannot create {}: {} — persistence disabled",
            config.output_dir.display(), e,
        );
        return None;
    }

    let (tick_tx, tick_rx) = mpsc::channel::<TickRecord>(config.channel_capacity);
    let (book_tx, book_rx) = mpsc::channel::<BookRecord>(config.channel_capacity);

    let tick_task = tokio::spawn(run_writer::<TickRecord>(
        config.trade_buffer_size,
        config.output_dir.clone(),
        config.flush_interval,
        tick_rx,
    ));

    let book_task = tokio::spawn(run_writer::<BookRecord>(
        config.book_buffer_size,
        config.output_dir.clone(),
        config.flush_interval,
        book_rx,
    ));

    Some(PersistenceHandles { tick_tx, book_tx, tick_task, book_task })
}

// ============================================================================
// BOOK RECORD BUILDER — convenience for main.rs
// ============================================================================

/// Convert an OrderBookUpdate into a BookRecord, zero-padding any side
/// that returns fewer than 5 levels (defensive — `books5` always has 5).
pub fn book_record_from_update(book: &crate::ws_types::OrderBookUpdate) -> BookRecord {
    let mut bid_prices = [0.0f64; 5];
    let mut bid_sizes  = [0.0f64; 5];
    let mut ask_prices = [0.0f64; 5];
    let mut ask_sizes  = [0.0f64; 5];

    for (i, level) in book.bids.iter().take(5).enumerate() {
        bid_prices[i] = level.price;
        bid_sizes[i]  = level.size;
    }
    for (i, level) in book.asks.iter().take(5).enumerate() {
        ask_prices[i] = level.price;
        ask_sizes[i]  = level.size;
    }

    BookRecord {
        timestamp_ms: book.timestamp_ms,
        inst_id: book.inst_id.clone(),
        bid_prices, bid_sizes,
        ask_prices, ask_sizes,
    }
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // ---------------------- TICK TESTS (carried from v0.8) ----------------

    fn sample_tick(ts: u64, px: f64) -> TickRecord {
        TickRecord {
            timestamp_ms: ts, price: px, size: 0.01,
            side: "buy".to_string(), inst_id: "BTC-USDT-SWAP".to_string(),
        }
    }

    #[test]
    fn tick_buffer_capacity_works() {
        let mut b = TickRecord::new_buffer(3);
        assert!(b.is_empty());
        TickRecord::push(&mut b, sample_tick(1, 100.0));
        TickRecord::push(&mut b, sample_tick(2, 101.0));
        assert!(!b.is_full(3));
        TickRecord::push(&mut b, sample_tick(3, 102.0));
        assert!(b.is_full(3));
    }

    #[test]
    fn tick_flush_roundtrips_parquet() {
        use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

        let dir = tempdir().unwrap();
        let mut b = TickRecord::new_buffer(10);
        for i in 0..7u64 {
            TickRecord::push(&mut b, sample_tick(i * 1000, 100.0 + i as f64));
        }
        let (path, rows) = flush_batch::<TickRecord>(b, dir.path().to_path_buf()).unwrap();
        assert_eq!(rows, 7);
        assert!(path.exists());

        let file = File::open(&path).unwrap();
        let builder = ParquetRecordBatchReaderBuilder::try_new(file).unwrap();
        let schema = builder.schema().clone();
        let reader = builder.build().unwrap();
        let mut total = 0;
        for batch_result in reader {
            total += batch_result.unwrap().num_rows();
        }
        assert_eq!(total, 7);
        assert_eq!(schema.field(0).name(), "timestamp_ms");
        assert_eq!(schema.field(1).name(), "price");
    }

    // ---------------------- BOOK TESTS (new in v0.9) ----------------------

    fn sample_book(ts: u64, mid: f64) -> BookRecord {
        // Synthesize a 5-deep book around `mid` with tight 1.0 spread
        // and size rising with distance from top of book.
        let mut bid_prices = [0.0f64; 5];
        let mut bid_sizes  = [0.0f64; 5];
        let mut ask_prices = [0.0f64; 5];
        let mut ask_sizes  = [0.0f64; 5];
        for i in 0..5 {
            let lvl = i as f64;
            bid_prices[i] = mid - 0.5 - lvl;   // 0.5, 1.5, 2.5, 3.5, 4.5 below mid
            ask_prices[i] = mid + 0.5 + lvl;   // 0.5, 1.5, ... above mid
            bid_sizes[i]  = 1.0 + lvl;
            ask_sizes[i]  = 1.0 + lvl;
        }
        BookRecord {
            timestamp_ms: ts,
            inst_id: "BTC-USDT-SWAP".to_string(),
            bid_prices, bid_sizes, ask_prices, ask_sizes,
        }
    }

    #[test]
    fn book_buffer_capacity_works() {
        let mut b = BookRecord::new_buffer(3);
        assert!(b.is_empty());
        BookRecord::push(&mut b, sample_book(1, 100.0));
        BookRecord::push(&mut b, sample_book(2, 101.0));
        assert!(!b.is_full(3));
        BookRecord::push(&mut b, sample_book(3, 102.0));
        assert!(b.is_full(3));
        assert_eq!(b.len(), 3);
    }

    #[test]
    fn book_schema_has_22_columns() {
        let schema = BookRecord::schema();
        let fields: Vec<String> = schema.fields().iter()
            .map(|f| f.name().to_string()).collect();
        // timestamp_ms + inst_id + 5 bid_price + 5 bid_size + 5 ask_price + 5 ask_size
        assert_eq!(fields.len(), 22);
        assert_eq!(fields[0], "timestamp_ms");
        assert_eq!(fields[1], "inst_id");
        assert_eq!(fields[2], "bid_price_0");
        assert_eq!(fields[6], "bid_price_4");
        assert_eq!(fields[7], "bid_size_0");
        assert_eq!(fields[12], "ask_price_0");
        assert_eq!(fields[17], "ask_size_0");
        assert_eq!(fields[21], "ask_size_4");
    }

    #[test]
    fn book_flush_roundtrips_parquet() {
        use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

        let dir = tempdir().unwrap();
        let mut b = BookRecord::new_buffer(10);
        for i in 0..4u64 {
            BookRecord::push(&mut b, sample_book(i * 100, 75_000.0 + i as f64));
        }
        let (path, rows) = flush_batch::<BookRecord>(b, dir.path().to_path_buf()).unwrap();
        assert_eq!(rows, 4);
        assert!(path.exists());
        // Filename starts with "BTC-USDT-SWAP_books" — distinct from trades.
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.starts_with("BTC-USDT-SWAP_books_"), "unexpected filename: {}", name);

        let file = File::open(&path).unwrap();
        let builder = ParquetRecordBatchReaderBuilder::try_new(file).unwrap();
        let schema = builder.schema().clone();
        assert_eq!(schema.fields().len(), 22);
        let reader = builder.build().unwrap();
        let mut total = 0;
        for batch_result in reader {
            total += batch_result.unwrap().num_rows();
        }
        assert_eq!(total, 4);
    }

    #[test]
    fn book_record_from_update_handles_short_book() {
        use crate::ws_types::{OrderBookUpdate, BookLevel};
        // Exchange returns only 2 bid levels and 3 ask levels — should
        // zero-pad the rest rather than panic.
        let update = OrderBookUpdate {
            inst_id: "BTC-USDT-SWAP".to_string(),
            bids: vec![
                BookLevel { price: 100.0, size: 1.5 },
                BookLevel { price:  99.0, size: 2.0 },
            ],
            asks: vec![
                BookLevel { price: 101.0, size: 1.0 },
                BookLevel { price: 102.0, size: 2.5 },
                BookLevel { price: 103.0, size: 3.0 },
            ],
            timestamp_ms: 1234,
        };
        let r = book_record_from_update(&update);
        assert_eq!(r.bid_prices[0], 100.0);
        assert_eq!(r.bid_prices[1],  99.0);
        assert_eq!(r.bid_prices[2],   0.0);  // padded
        assert_eq!(r.ask_prices[0], 101.0);
        assert_eq!(r.ask_prices[2], 103.0);
        assert_eq!(r.ask_prices[3],   0.0);  // padded
    }

    // ---------------------- END-TO-END INTEGRATION TEST -------------------

    #[tokio::test]
    async fn end_to_end_both_writers_flush_on_shutdown() {
        // Fire ticks AND books through both channels simultaneously.
        // Drop both senders. Verify both writers final-flush and exit.
        let dir = tempdir().unwrap();
        let config = PersistenceConfig {
            output_dir: dir.path().to_path_buf(),
            trade_buffer_size: 3,
            book_buffer_size: 3,
            flush_interval: Duration::from_secs(3600),  // not triggered
            channel_capacity: 100,
        };

        let handles = spawn_all(config).expect("spawn should succeed on tempdir");

        for i in 0..7u64 {
            handles.tick_tx.send(sample_tick(i, 100.0 + i as f64)).await.unwrap();
            handles.book_tx.send(sample_book(i, 75_000.0 + i as f64)).await.unwrap();
        }
        // Drop both — writers should final-flush and exit.
        drop(handles.tick_tx);
        drop(handles.book_tx);

        handles.tick_task.await.unwrap();
        handles.book_task.await.unwrap();

        let files: Vec<String> = std::fs::read_dir(dir.path()).unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".parquet"))
            .collect();

        let trade_files: Vec<_> = files.iter()
            .filter(|n| n.contains("_trades_")).collect();
        let book_files: Vec<_> = files.iter()
            .filter(|n| n.contains("_books_")).collect();

        // Each writer should produce at least 2 files (two full + one
        // partial on shutdown with buffer_size=3 and 7 records).
        assert!(trade_files.len() >= 2,
            "expected ≥2 trade files, got {}: {:?}", trade_files.len(), trade_files);
        assert!(book_files.len() >= 2,
            "expected ≥2 book files, got {}: {:?}", book_files.len(), book_files);
    }
}
