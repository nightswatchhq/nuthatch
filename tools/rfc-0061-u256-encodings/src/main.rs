//! #1222 experiment: one real uint256 column, written four ways, sized, for DuckDB to read back.
//! Usage: spike-1222 <nest_dir> <table> <column> <out_dir>
use std::{fs::File, path::Path, sync::Arc};

use anyhow::{bail, Context, Result};
use arrow::array::{
    Array, ArrayRef, BooleanArray, Decimal128Array, Decimal256Array, FixedSizeBinaryArray, StringArray,
};
use arrow::datatypes::{i256, DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use num_bigint::BigUint;
use parquet::arrow::{arrow_reader::ParquetRecordBatchReaderBuilder, ArrowWriter};
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::WriterProperties;

fn read_column(nest: &Path, table: &str, column: &str) -> Result<Vec<Option<String>>> {
    let manifest: serde_json::Value =
        serde_json::from_reader(File::open(nest.join("segments/manifest.json"))?)?;
    let mut out = Vec::new();
    for seg in manifest["tables"][table].as_array().context("table not in catalogue")? {
        let path = nest.join("segments").join(seg["file"].as_str().unwrap());
        let reader = ParquetRecordBatchReaderBuilder::try_new(File::open(&path)?)?.build()?;
        for batch in reader {
            let batch = batch?;
            let Some(col) = batch.column_by_name(column) else { continue };
            let s = col.as_any().downcast_ref::<StringArray>().context("column is not Utf8")?;
            out.extend((0..s.len()).map(|i| s.is_valid(i).then(|| s.value(i).to_string())));
        }
    }
    Ok(out)
}

fn write(path: &Path, fields: Vec<Field>, cols: Vec<ArrayRef>) -> Result<u64> {
    let schema = Arc::new(Schema::new(fields));
    let props = WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::try_new(3)?))
        .set_max_row_group_size(1 << 20)
        .build();
    let mut w = ArrowWriter::try_new(File::create(path)?, schema.clone(), Some(props))?;
    w.write(&RecordBatch::try_new(schema, cols)?)?;
    w.close()?;
    Ok(std::fs::metadata(path)?.len())
}

fn main() -> Result<()> {
    let a: Vec<String> = std::env::args().collect();
    if a.len() != 5 { bail!("usage: spike-1222 <nest_dir> <table> <column> <out_dir>") }
    let (nest, table, column, out) = (Path::new(&a[1]), &a[2], &a[3], Path::new(&a[4]));
    std::fs::create_dir_all(out)?;
    let text = read_column(nest, table, column)?;
    let big: Vec<Option<BigUint>> = text.iter()
        .map(|t| t.as_ref().map(|s| BigUint::parse_bytes(s.as_bytes(), 10).expect("decimal text")))
        .collect();
    let digits = |v: &BigUint| v.to_str_radix(10).len();
    let (mut over38, mut over76, mut max_u256) = (0usize, 0usize, 0usize);
    let max = (BigUint::from(1u8) << 256usize) - 1u8;
    for v in big.iter().flatten() {
        if digits(v) > 38 { over38 += 1 }
        if digits(v) > 76 { over76 += 1 }
        if *v == max { max_u256 += 1 }
    }
    let n = text.len();
    let nulls = text.iter().filter(|t| t.is_none()).count();
    println!("rows={n} nulls={nulls} over_38_digits={over38} over_76_digits={over76} exactly_2^256-1={max_u256}");

    // A: today's physical form.
    let a_arr: ArrayRef = Arc::new(StringArray::from(text.clone()));
    let size_a = write(&out.join("a_text.parquet"), vec![Field::new(column, DataType::Utf8, true)], vec![a_arr.clone()])?;

    // B: FIXED_LEN_BYTE_ARRAY(32), big-endian, no logical type.
    let b_arr: ArrayRef = Arc::new(FixedSizeBinaryArray::try_from_sparse_iter_with_size(
        big.iter().map(|v| v.as_ref().map(|v| {
            let be = v.to_bytes_be();
            let mut w = vec![0u8; 32 - be.len()];
            w.extend_from_slice(&be);
            w
        })), 32)?);
    let size_b = write(&out.join("b_flba32.parquet"), vec![Field::new(column, DataType::FixedSizeBinary(32), true)], vec![b_arr])?;

    // C: DECIMAL(76,0) on FLBA(32). Values past 76 digits cannot be represented: NULL, counted above.
    let c_arr: ArrayRef = Arc::new(Decimal256Array::from(big.iter().map(|v| v.as_ref().and_then(|v| {
        (digits(v) <= 76).then(|| {
            let be = v.to_bytes_be();
            let mut w = [0u8; 32];
            w[32 - be.len()..].copy_from_slice(&be);
            i256::from_be_bytes(w)
        })
    })).collect::<Vec<_>>()).with_precision_and_scale(76, 0)?);
    let size_c = write(&out.join("c_decimal76.parquet"), vec![Field::new(column, DataType::Decimal256(76, 0), true)], vec![c_arr])?;

    // D: today's text, plus RFC-0047's view columns made physical: DECIMAL(38,0) and an overflow flag.
    let d_dec: ArrayRef = Arc::new(Decimal128Array::from(big.iter().map(|v| v.as_ref().and_then(|v| {
        (digits(v) <= 38).then(|| v.to_string().parse::<i128>().unwrap())
    })).collect::<Vec<_>>()).with_precision_and_scale(38, 0)?);
    let d_ovf: ArrayRef = Arc::new(BooleanArray::from(big.iter().map(|v| v.as_ref().map(|v| digits(v) > 38)).collect::<Vec<_>>()));
    let size_d = write(&out.join("d_text_dec38.parquet"), vec![
        Field::new(column, DataType::Utf8, true),
        Field::new(format!("{column}_dec"), DataType::Decimal128(38, 0), true),
        Field::new(format!("{column}_overflow"), DataType::Boolean, true),
    ], vec![a_arr, d_dec, d_ovf])?;

    let mb = |b: u64| b as f64 / 1_048_576.0;
    println!("A text           {:>9.2} MB  1.00x", mb(size_a));
    for (name, s) in [("B flba32", size_b), ("C decimal76", size_c), ("D text+dec38+ovf", size_d)] {
        println!("{name:<16} {:>9.2} MB  {:.2}x", mb(s), s as f64 / size_a as f64);
    }
    Ok(())
}
