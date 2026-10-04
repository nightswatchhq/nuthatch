//! `recurrence` - a toy EFFECTFUL stage (RFC-0008 C4) that demonstrates a granted host capability.
//!
//! It reads a batch of addresses (Arrow IPC), and for each keeps a running "how many times have I
//! seen this address across all batches?" count in the host `kv` store - state a *pure* stage could
//! not hold. It emits one annotation per input row: `(address, seen)`. The `kv` import is visible in
//! the component's type, so the host refuses to instantiate it unless `kv` was granted. It still
//! cannot write canonical entities: its only output is the annotation batch the host records.

use std::io::Cursor;
use std::sync::Arc;

use arrow_array::{Array, RecordBatch, StringArray, UInt64Array};
use arrow_ipc::reader::StreamReader;
use arrow_ipc::writer::StreamWriter;
use arrow_schema::{DataType, Field, Schema};

wit_bindgen::generate!({
    world: "effectful-kv",
    path: "../../wit",
});

use nuthatch::transform::kv;

struct Component;

impl exports::nuthatch::transform::effectful::Guest for Component {
    fn run(batch: Vec<u8>) -> Result<Vec<u8>, String> {
        let input = read_batch(&batch)?;
        let addrs = input
            .column_by_name("address")
            .ok_or("input batch has no `address` column")?
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or("`address` column is not utf8")?;

        let out = annotate(addrs, kv::get, kv::set)?;
        write_batch(&out)
    }
}

/// The counting core, with the host `kv` passed in so it runs off-wasm (the imports only link there).
fn annotate(
    addrs: &StringArray,
    get: impl Fn(&str) -> Option<Vec<u8>>,
    mut set: impl FnMut(&str, &[u8]),
) -> Result<RecordBatch, String> {
    let mut out_addr = Vec::new();
    let mut out_seen = Vec::new();
    for i in 0..addrs.len() {
        let addr = addrs.value(i);
        // Read the prior count from the granted kv store, increment, write it back.
        let prev: u64 = get(addr)
            .and_then(|b| String::from_utf8(b).ok())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let seen = prev + 1;
        set(addr, seen.to_string().as_bytes());
        out_addr.push(addr.to_string());
        out_seen.push(seen);
    }

    RecordBatch::try_new(
        Arc::new(Schema::new(vec![
            Field::new("address", DataType::Utf8, false),
            Field::new("seen", DataType::UInt64, false),
        ])),
        vec![
            Arc::new(StringArray::from(out_addr)),
            Arc::new(UInt64Array::from(out_seen)),
        ],
    )
    .map_err(|e| e.to_string())
}

fn read_batch(bytes: &[u8]) -> Result<RecordBatch, String> {
    let mut reader = StreamReader::try_new(Cursor::new(bytes), None).map_err(|e| e.to_string())?;
    match reader.next() {
        Some(b) => b.map_err(|e| e.to_string()),
        None => Err("empty Arrow IPC stream".to_string()),
    }
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
    use std::cell::RefCell;
    use std::collections::HashMap;

    use super::*;

    /// Runs `annotate` against an in-memory kv, as the host would across successive batches.
    fn run(kv: &RefCell<HashMap<String, Vec<u8>>>, addrs: &[&str]) -> Vec<(String, u64)> {
        let col = StringArray::from_iter_values(addrs.iter().copied());
        let out = annotate(
            &col,
            |k| kv.borrow().get(k).cloned(),
            |k, v| {
                kv.borrow_mut().insert(k.to_string(), v.to_vec());
            },
        )
        .unwrap();
        let a = out
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let s = out
            .column(1)
            .as_any()
            .downcast_ref::<UInt64Array>()
            .unwrap();
        (0..out.num_rows())
            .map(|i| (a.value(i).to_string(), s.value(i)))
            .collect()
    }

    #[test]
    fn counts_each_sighting_across_rows_and_batches() {
        let kv = RefCell::new(HashMap::new());
        assert_eq!(
            run(&kv, &["0xa", "0xb", "0xa"]),
            vec![("0xa".into(), 1), ("0xb".into(), 1), ("0xa".into(), 2)]
        );
        assert_eq!(run(&kv, &["0xa"]), vec![("0xa".into(), 3)]);
        assert_eq!(kv.borrow()["0xa"], b"3");
        assert_eq!(kv.borrow()["0xb"], b"1");
    }

    #[test]
    fn an_unparseable_stored_count_restarts_at_one() {
        let kv = RefCell::new(HashMap::from([(
            "0xa".to_string(),
            b"not a number".to_vec(),
        )]));
        assert_eq!(run(&kv, &["0xa"]), vec![("0xa".into(), 1)]);
        assert_eq!(kv.borrow()["0xa"], b"1");
    }

    #[test]
    fn an_empty_batch_writes_nothing() {
        let kv = RefCell::new(HashMap::new());
        assert!(run(&kv, &[]).is_empty());
        assert!(kv.borrow().is_empty());
    }

    #[test]
    fn the_ipc_batch_is_read_and_a_missing_address_column_is_refused() {
        use super::exports::nuthatch::transform::effectful::Guest;
        let schema = Arc::new(Schema::new(vec![Field::new("who", DataType::Utf8, false)]));
        let batch = RecordBatch::try_new(
            schema,
            vec![Arc::new(StringArray::from(vec!["0xa"])) as arrow_array::ArrayRef],
        )
        .unwrap();
        let err = Component::run(write_batch(&batch).unwrap()).unwrap_err();
        assert_eq!(err, "input batch has no `address` column");
    }
}
