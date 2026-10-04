//! `large-transfers` - a pure, batched transform component.
//!
//! Reads a batch of transfers (Arrow IPC), keeps only those with value ≥ a threshold, and returns
//! the filtered batch (Arrow IPC). Zero capabilities: it cannot call out, read the clock, or touch
//! the filesystem - deterministic by construction, so its output is safe to feed entity derivation.
//! The whole batch crosses the wasm boundary in one call (the point of the batched WIT).

use std::io::Cursor;

use arrow_array::{BooleanArray, Int64Array, RecordBatch};
use arrow_ipc::reader::StreamReader;
use arrow_ipc::writer::StreamWriter;
use arrow_select::filter::filter_record_batch;

wit_bindgen::generate!({
    world: "pure-transform",
    path: "../../wit",
});

/// 1,000 USDC in base units (6 decimals). A real deployment would parameterise this; the skeleton
/// hardcodes it to keep the component pure and config-free.
const THRESHOLD: i64 = 1_000_000_000;

struct Component;

impl exports::nuthatch::transform::stage::Guest for Component {
    fn run(batch: Vec<u8>) -> Result<Vec<u8>, String> {
        let input = read_batch(&batch)?;

        let value = input
            .column_by_name("value")
            .ok_or("input batch has no `value` column")?
            .as_any()
            .downcast_ref::<Int64Array>()
            .ok_or("`value` column is not Int64")?;

        let mask = BooleanArray::from_iter(
            value
                .iter()
                .map(|v| Some(v.map(|x| x >= THRESHOLD).unwrap_or(false))),
        );
        let filtered = filter_record_batch(&input, &mask).map_err(|e| e.to_string())?;

        write_batch(&filtered)
    }
}

fn read_batch(bytes: &[u8]) -> Result<RecordBatch, String> {
    let mut reader = StreamReader::try_new(Cursor::new(bytes), None).map_err(|e| e.to_string())?;
    reader
        .next()
        .ok_or("empty Arrow IPC stream")?
        .map_err(|e| e.to_string())
}

fn write_batch(batch: &RecordBatch) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    {
        let mut writer =
            StreamWriter::try_new(&mut out, &batch.schema()).map_err(|e| e.to_string())?;
        writer.write(batch).map_err(|e| e.to_string())?;
        writer.finish().map_err(|e| e.to_string())?;
    }
    Ok(out)
}

export!(Component);

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow_array::{ArrayRef, StringArray, UInt64Array};
    use arrow_schema::{DataType, Field, Schema};

    use super::exports::nuthatch::transform::stage::Guest;
    use super::*;

    type Row = (u64, u64, &'static str, &'static str, Option<i64>);

    fn ipc(rows: &[Row]) -> Vec<u8> {
        let schema = Arc::new(Schema::new(vec![
            Field::new("block_number", DataType::UInt64, false),
            Field::new("log_index", DataType::UInt64, false),
            Field::new("from", DataType::Utf8, false),
            Field::new("to", DataType::Utf8, false),
            Field::new("value", DataType::Int64, true),
        ]));
        let cols: Vec<ArrayRef> = vec![
            Arc::new(UInt64Array::from_iter_values(rows.iter().map(|r| r.0))),
            Arc::new(UInt64Array::from_iter_values(rows.iter().map(|r| r.1))),
            Arc::new(StringArray::from_iter_values(rows.iter().map(|r| r.2))),
            Arc::new(StringArray::from_iter_values(rows.iter().map(|r| r.3))),
            Arc::new(Int64Array::from_iter(rows.iter().map(|r| r.4))),
        ];
        write_batch(&RecordBatch::try_new(schema, cols).unwrap()).unwrap()
    }

    fn survivors(out: &[u8]) -> Vec<(u64, u64, i64)> {
        let b = read_batch(out).unwrap();
        let blk = b.column(0).as_any().downcast_ref::<UInt64Array>().unwrap();
        let log = b.column(1).as_any().downcast_ref::<UInt64Array>().unwrap();
        let val = b.column(4).as_any().downcast_ref::<Int64Array>().unwrap();
        (0..b.num_rows())
            .map(|i| (blk.value(i), log.value(i), val.value(i)))
            .collect()
    }

    #[test]
    fn keeps_only_large_transfers_in_input_order() {
        let out = Component::run(ipc(&[
            (1, 0, "0xa", "0xb", Some(5)),
            (1, 1, "0xa", "0xb", Some(2_000_000_000)),
            (2, 0, "0xc", "0xd", Some(1_500_000_000)),
        ]))
        .unwrap();
        assert_eq!(
            survivors(&out),
            vec![(1, 1, 2_000_000_000), (2, 0, 1_500_000_000)]
        );
    }

    #[test]
    fn the_threshold_is_inclusive() {
        let out = Component::run(ipc(&[
            (1, 0, "0xa", "0xb", Some(999_999_999)),
            (1, 1, "0xa", "0xb", Some(1_000_000_000)),
            (1, 2, "0xa", "0xb", Some(1_000_000_001)),
        ]))
        .unwrap();
        assert_eq!(
            survivors(&out),
            vec![(1, 1, 1_000_000_000), (1, 2, 1_000_000_001)]
        );
    }

    #[test]
    fn a_null_value_is_dropped_not_an_error() {
        let out = Component::run(ipc(&[
            (1, 0, "0xa", "0xb", None),
            (1, 1, "0xa", "0xb", Some(1_000_000_000)),
        ]))
        .unwrap();
        assert_eq!(survivors(&out), vec![(1, 1, 1_000_000_000)]);
    }

    #[test]
    fn an_empty_batch_yields_an_empty_batch() {
        let out = Component::run(ipc(&[])).unwrap();
        assert!(survivors(&out).is_empty());
    }

    #[test]
    fn a_batch_without_a_value_column_is_refused() {
        let schema = Arc::new(Schema::new(vec![Field::new("from", DataType::Utf8, false)]));
        let batch = RecordBatch::try_new(
            schema,
            vec![Arc::new(StringArray::from(vec!["0xa"])) as ArrayRef],
        )
        .unwrap();
        let err = Component::run(write_batch(&batch).unwrap()).unwrap_err();
        assert_eq!(err, "input batch has no `value` column");
    }
}
