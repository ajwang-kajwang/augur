// Generate synthetic trade + book Parquet files for smoke testing.
// v0.11: produces both streams so ladder-mode can be smoke-tested too.
//
// cargo run --example gen_synthetic -- /tmp/bt_data
use std::env;
use std::path::PathBuf;
use std::fs::File;
use std::sync::Arc;
use arrow::array::{Float64Array, UInt64Array, StringArray, ArrayRef};
use arrow::datatypes::{Schema, Field, DataType};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;

fn main() {
    let args: Vec<String> = env::args().collect();
    let dir = PathBuf::from(args.get(1).cloned().unwrap_or_else(|| "/tmp/bt_data".to_string()));
    std::fs::create_dir_all(&dir).unwrap();

    // --- Trade schema + data ---
    let trade_schema = Arc::new(Schema::new(vec![
        Field::new("timestamp_ms", DataType::UInt64, false),
        Field::new("price",        DataType::Float64, false),
        Field::new("size",         DataType::Float64, false),
        Field::new("side",         DataType::Utf8,    false),
        Field::new("inst_id",      DataType::Utf8,    false),
    ]));

    let mut ts: Vec<u64> = Vec::new();
    let mut px: Vec<f64> = Vec::new();
    let mut sz: Vec<f64> = Vec::new();
    let mut side: Vec<String> = Vec::new();
    let mut iid: Vec<String> = Vec::new();

    let hours: Vec<(f64, f64)> = vec![
        (95.0, 89.0), (92.0, 88.0),
        (90.0, 85.0),            // A (low)
        (95.0, 90.0), (98.0, 93.0),
        (105.0, 98.0),           // B (high)
        (100.0, 95.0),
        (95.0, 92.0),            // C (low)
        (100.0, 95.0), (115.0, 100.0),
    ];

    let mut cursor = 0u64;
    for (h, l) in &hours {
        let hour_start = cursor;
        for i in 0..100 {
            let frac = i as f64 / 100.0;
            let p = if frac < 0.5 { l + (h - l) * (frac * 2.0) }
                    else { h - (h - l) * ((frac - 0.5) * 2.0) };
            ts.push(hour_start + (i * 36_000));
            px.push(p);
            sz.push(0.01);
            side.push("buy".to_string());
            iid.push("BTC-USDT-SWAP".to_string());
        }
        cursor += 3_600_000;
    }

    let trade_arrays: Vec<ArrayRef> = vec![
        Arc::new(UInt64Array::from(ts.clone())),
        Arc::new(Float64Array::from(px.clone())),
        Arc::new(Float64Array::from(sz)),
        Arc::new(StringArray::from(side)),
        Arc::new(StringArray::from(iid)),
    ];
    let trade_batch = RecordBatch::try_new(trade_schema.clone(), trade_arrays).unwrap();
    let trade_path = dir.join("BTC-USDT-SWAP_trades_20260420_000000_000000000.parquet");
    let trade_file = File::create(&trade_path).unwrap();
    let props = WriterProperties::builder().set_compression(Compression::SNAPPY).build();
    let mut w = ArrowWriter::try_new(trade_file, trade_schema, Some(props.clone())).unwrap();
    w.write(&trade_batch).unwrap();
    w.close().unwrap();
    println!("wrote {} ({} rows)", trade_path.display(), trade_batch.num_rows());

    // --- Book schema + synthetic top-5 around each trade price ---
    let mut book_fields = vec![
        Field::new("timestamp_ms", DataType::UInt64, false),
        Field::new("inst_id",      DataType::Utf8,    false),
    ];
    for i in 0..5 { book_fields.push(Field::new(&format!("bid_price_{}", i), DataType::Float64, false)); }
    for i in 0..5 { book_fields.push(Field::new(&format!("bid_size_{}",  i), DataType::Float64, false)); }
    for i in 0..5 { book_fields.push(Field::new(&format!("ask_price_{}", i), DataType::Float64, false)); }
    for i in 0..5 { book_fields.push(Field::new(&format!("ask_size_{}",  i), DataType::Float64, false)); }
    let book_schema = Arc::new(Schema::new(book_fields));

    // Emit a book snapshot for every trade, with spread=0.5 and 2.0 BTC
    // per level on each side.
    let n = ts.len();
    let mut b_ts: Vec<u64> = Vec::with_capacity(n);
    let mut b_iid: Vec<String> = Vec::with_capacity(n);
    let mut bid_px: [Vec<f64>; 5] = Default::default();
    let mut bid_sz: [Vec<f64>; 5] = Default::default();
    let mut ask_px: [Vec<f64>; 5] = Default::default();
    let mut ask_sz: [Vec<f64>; 5] = Default::default();
    for i in 0..n {
        let mid = px[i];
        b_ts.push(ts[i]);
        b_iid.push("BTC-USDT-SWAP".to_string());
        for lvl in 0..5 {
            let off = lvl as f64 * 0.1;
            bid_px[lvl].push(mid - 0.25 - off);
            ask_px[lvl].push(mid + 0.25 + off);
            bid_sz[lvl].push(2.0);
            ask_sz[lvl].push(2.0);
        }
    }
    let mut book_arrays: Vec<ArrayRef> = Vec::new();
    book_arrays.push(Arc::new(UInt64Array::from(b_ts)));
    book_arrays.push(Arc::new(StringArray::from(b_iid)));
    for lvl in 0..5 { book_arrays.push(Arc::new(Float64Array::from(std::mem::take(&mut bid_px[lvl])))); }
    for lvl in 0..5 { book_arrays.push(Arc::new(Float64Array::from(std::mem::take(&mut bid_sz[lvl])))); }
    for lvl in 0..5 { book_arrays.push(Arc::new(Float64Array::from(std::mem::take(&mut ask_px[lvl])))); }
    for lvl in 0..5 { book_arrays.push(Arc::new(Float64Array::from(std::mem::take(&mut ask_sz[lvl])))); }
    let book_batch = RecordBatch::try_new(book_schema.clone(), book_arrays).unwrap();
    let book_path = dir.join("BTC-USDT-SWAP_books_20260420_000000_000000000.parquet");
    let book_file = File::create(&book_path).unwrap();
    let mut w = ArrowWriter::try_new(book_file, book_schema, Some(props)).unwrap();
    w.write(&book_batch).unwrap();
    w.close().unwrap();
    println!("wrote {} ({} rows)", book_path.display(), book_batch.num_rows());
}
