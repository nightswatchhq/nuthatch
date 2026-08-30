//! Where does the #987 operator's time actually go? Measure, do not reason.
use arrow::array::Array;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use std::collections::HashMap;
use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let dir = std::env::args().nth(1).expect("dir");
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "parquet"))
        .collect();
    files.sort();
    println!("{} files", files.len());

    // 1. decode only
    let t = Instant::now();
    let mut rows = 0usize;
    for f in &files {
        let r = ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(f)?)?
            .with_batch_size(8192)
            .build()?;
        for b in r {
            rows += b?.num_rows();
        }
    }
    println!(
        "decode only:        {:>6} ms  ({rows} rows)",
        t.elapsed().as_millis()
    );

    // 2. decode + touch the three columns
    let t = Instant::now();
    let mut bytes = 0usize;
    for f in &files {
        let r = ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(f)?)?
            .with_batch_size(8192)
            .build()?;
        for b in r {
            let b = b?;
            for name in ["from", "to", "value"] {
                let i = b.schema().index_of(name)?;
                let a = b
                    .column(i)
                    .as_any()
                    .downcast_ref::<arrow::array::StringArray>()
                    .unwrap();
                for j in 0..a.len() {
                    bytes += a.value(j).len();
                }
            }
        }
    }
    println!(
        "+ column access:    {:>6} ms  ({bytes} bytes)",
        t.elapsed().as_millis()
    );

    // 3. + i128 parse
    let t = Instant::now();
    let mut sum = 0i128;
    for f in &files {
        let r = ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(f)?)?
            .with_batch_size(8192)
            .build()?;
        for b in r {
            let b = b?;
            let i = b.schema().index_of("value")?;
            let v = b
                .column(i)
                .as_any()
                .downcast_ref::<arrow::array::StringArray>()
                .unwrap();
            for j in 0..v.len() {
                if let Ok(d) = v.value(j).parse::<i128>() {
                    sum = sum.wrapping_add(d);
                }
            }
        }
    }
    println!(
        "+ i128 parse:       {:>6} ms  (sum {sum})",
        t.elapsed().as_millis()
    );

    // 4. full fold
    let t = Instant::now();
    let mut acc: HashMap<String, i128> = HashMap::with_capacity(1024);
    for f in &files {
        let r = ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(f)?)?
            .with_batch_size(8192)
            .build()?;
        for b in r {
            let b = b?;
            let g = |n: &str| -> anyhow::Result<&arrow::array::StringArray> {
                let i = b.schema().index_of(n)?;
                Ok(b.column(i)
                    .as_any()
                    .downcast_ref::<arrow::array::StringArray>()
                    .unwrap())
            };
            let (fr, to, va) = (g("from")?, g("to")?, g("value")?);
            for j in 0..b.num_rows() {
                let Ok(d) = va.value(j).parse::<i128>() else {
                    continue;
                };
                for (a, s) in [(to.value(j), d), (fr.value(j), -d)] {
                    if let Some(x) = acc.get_mut(a) {
                        *x += s
                    } else {
                        acc.insert(a.to_string(), s);
                    }
                }
            }
        }
    }
    println!(
        "full fold:          {:>6} ms  ({} addrs)",
        t.elapsed().as_millis(),
        acc.len()
    );
    Ok(())
}
