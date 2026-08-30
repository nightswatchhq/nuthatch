//! RFC-0013 §4 benchmark gate: DataFusion vs DuckDB over the same sealed segments.
//!
//! Not a microbenchmark. It runs the query that actually matters - `net_balances`, the i128 fold the
//! compliance path depends on - against identical Parquet, asserts the two engines agree, and times
//! both. A gate that measured `SELECT count(*)` would pass and tell us nothing.

use std::time::Instant;

use nuthatch::seal;

fn rss_mb() -> u64 {
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &std::process::id().to_string()])
            .output()
            .ok();
        if let Some(o) = out {
            if let Ok(s) = String::from_utf8(o.stdout) {
                if let Ok(kb) = s.trim().parse::<u64>() {
                    return kb / 1024;
                }
            }
        }
    }
    0
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let rows: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(2_000_000);
    let dir = tempfile::tempdir()?;
    let seg = dir.path().join(seal::SEGMENTS_DIR);
    std::fs::create_dir_all(&seg)?;

    // **#964: segment count is the variable slice 2 could not see.** Slice 2 measured one segment; a
    // real nest has 10,923 at a 6.3 KB median. `SEGMENTS=n` splits the same rows across n files, so a
    // sweep says whether the ratio is a property of the engines or of the layout.
    let segments: usize = std::env::var("SEGMENTS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1)
        .max(1);

    println!("== RFC-0013 gate: {rows} transfer rows across {segments} sealed segment(s) ==");
    let t = Instant::now();
    let written = write_fixture_segments(&seg, rows, segments)?;
    println!(
        "fixture written in {:?} ({} files, rss {} MB)",
        t.elapsed(),
        written,
        rss_mb()
    );

    // **Run order is a confound, not a detail.** Whichever engine goes first pays for warming the OS
    // page cache on the segment, and the second gets it free. The first OBIB run was 3.9x slower than
    // the second for exactly this reason, so `DF_FIRST=1` runs the comparison the other way round; a
    // ratio that survives both orderings is about the engines.
    let df_first = std::env::var("DF_FIRST").is_ok();
    println!("order: {}", if df_first { "datafusion, then duckdb" } else { "duckdb, then datafusion" });

    let (duck, d_ms, d_rss, df, f_ms, f_rss) = if df_first {
        let (df, f_ms, f_rss) = datafusion_net_balances(&seg).await?;
        let (duck, d_ms, d_rss) = duckdb_net_balances(&seg)?;
        (duck, d_ms, d_rss, df, f_ms, f_rss)
    } else {
        let (duck, d_ms, d_rss) = duckdb_net_balances(&seg)?;
        let (df, f_ms, f_rss) = datafusion_net_balances(&seg).await?;
        (duck, d_ms, d_rss, df, f_ms, f_rss)
    };
    println!("duckdb:     {:>6} ms  rss {:>4} MB  {} addresses", d_ms, d_rss, duck.len());
    println!("datafusion: {:>6} ms  rss {:>4} MB  {} addresses", f_ms, f_rss, df.len());

    // Parity is the acceptance criterion, not a nicety: an engine that is faster and disagrees is a
    // regression, and this fold feeds stored state.
    if duck != df {
        let mismatches = duck
            .iter()
            .zip(df.iter())
            .filter(|(a, b)| a != b)
            .take(5)
            .collect::<Vec<_>>();
        println!("\nPARITY: FAIL  ({} vs {} rows)", duck.len(), df.len());
        for (a, b) in mismatches {
            println!("  duckdb {a:?}  !=  datafusion {b:?}");
        }
        std::process::exit(1);
    }
    println!("\nPARITY: identical on {} addresses", duck.len());
    let ratio = f_ms as f64 / d_ms.max(1) as f64;
    println!("RATIO:  datafusion is {ratio:.2}x duckdb's latency (>1 means slower)");
    Ok(())
}

/// A transfer table shaped exactly like a sealed segment: uint256 values as strings, addresses as
/// hex. The string-typed value column is the point - that is what forces the i128 cast both engines
/// have to agree about.
/// Split `rows` across `segments` Parquet files reproducing the **bimodal** shape #889 measured on
/// `horizon-nest`: 80% of segments under 20 KB, a 6.3 KB median, and the three largest being the
/// busiest table's backfill segments.
///
/// The distribution is bimodal because the two seal paths are (#947): the backfill path batches at
/// 20,000 rows cutting on a data-chosen block boundary, while the tip path seals whatever finalised -
/// a few blocks carrying a few rows. So this puts the bulk of the rows in the last 20% of files and
/// scatters the remainder thinly across the first 80%.
///
/// **Not an even split.** An even split is a different problem and would flatter whichever engine
/// handles uniform work better, which is exactly the confound this measurement exists to avoid.
fn write_fixture_segments(
    seg: &std::path::Path,
    rows: usize,
    segments: usize,
) -> anyhow::Result<usize> {
    if segments == 1 {
        write_fixture(&seg.join("t.parquet"), rows)?;
        return Ok(1);
    }
    let small_count = (segments * 4) / 5;
    let large_count = segments - small_count;
    // Give the small files ~5% of the rows between them, the large files the rest.
    let small_total = (rows / 20).min(rows);
    let per_small = (small_total / small_count.max(1)).max(1);
    let small_total = per_small * small_count;
    let per_large = (rows - small_total) / large_count.max(1);

    let mut written = 0usize;
    let mut emitted = 0usize;
    for i in 0..segments {
        let n = if i < small_count { per_small } else { per_large };
        let n = if i == segments - 1 { rows - emitted } else { n };
        if n == 0 {
            continue;
        }
        write_fixture_offset(&seg.join(format!("t-{i:06}.parquet")), n, emitted)?;
        emitted += n;
        written += 1;
    }
    Ok(written)
}

fn write_fixture(path: &std::path::Path, rows: usize) -> anyhow::Result<()> {
    write_fixture_offset(path, rows, 0)
}

/// Rows `offset .. offset + rows` of the same generated table.
///
/// The offset is what makes a segment sweep meaningful: the union of any layout is byte-for-byte the
/// rows a single file of the same total would hold, so **`net_balances` must return an identical
/// answer at every segment count.** A layout that changes the answer is a defect, not a slower plan,
/// and the parity check catches it without a separate oracle.
fn write_fixture_offset(
    path: &std::path::Path,
    rows: usize,
    offset: usize,
) -> anyhow::Result<()> {
    use arrow::array::{StringArray, UInt64Array};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use std::sync::Arc;

    let addrs: Vec<String> = (0..512).map(|i| format!("0x{i:040x}")).collect();
    let idx = |i: usize| i + offset;
    let from: Vec<&str> = (0..rows).map(|i| addrs[idx(i) % 512].as_str()).collect();
    let to: Vec<&str> = (0..rows).map(|i| addrs[(idx(i) * 7 + 3) % 512].as_str()).collect();
    // Values large enough that i64 would overflow and i128 would not - the reason HUGEINT is in the
    // query at all. A fixture of small values would let a broken cast pass.
    let value: Vec<String> = (0..rows)
        .map(|i| format!("{}", 1_000_000_000_000_000_000u128 * (idx(i) as u128 % 97 + 1)))
        .collect();
    let block: Vec<u64> = (0..rows).map(|i| idx(i) as u64 / 100).collect();

    let schema = Arc::new(Schema::new(vec![
        Field::new("block_number", DataType::UInt64, false),
        Field::new("from", DataType::Utf8, false),
        Field::new("to", DataType::Utf8, false),
        Field::new("value", DataType::Utf8, false),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(block)),
            Arc::new(StringArray::from(from)),
            Arc::new(StringArray::from(to)),
            Arc::new(StringArray::from(value.iter().map(|s| s.as_str()).collect::<Vec<_>>())),
        ],
    )?;
    let file = std::fs::File::create(path)?;
    let props = parquet::file::properties::WriterProperties::builder()
        .set_compression(parquet::basic::Compression::SNAPPY)
        .build();
    let mut w = parquet::arrow::ArrowWriter::try_new(file, schema, Some(props))?;
    w.write(&batch)?;
    w.close()?;
    Ok(())
}

fn duckdb_net_balances(seg: &std::path::Path) -> anyhow::Result<(Vec<(String, i128)>, u128, u64)> {
    let conn = duckdb::Connection::open_in_memory()?;
    let glob = format!("{}/*.parquet", seg.display());
    conn.execute_batch(&format!(
        "CREATE VIEW t AS SELECT * FROM read_parquet('{glob}');"
    ))?;
    let sql = "SELECT addr, SUM(d)::VARCHAR AS net FROM (\
                 SELECT \"to\" AS addr, TRY_CAST(\"value\" AS HUGEINT) AS d FROM t \
                 UNION ALL \
                 SELECT \"from\" AS addr, -TRY_CAST(\"value\" AS HUGEINT) AS d FROM t\
               ) GROUP BY addr HAVING SUM(d) <> 0 ORDER BY addr";
    let t = Instant::now();
    let mut stmt = conn.prepare(sql)?;
    let mut out = Vec::new();
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        let addr: String = r.get(0)?;
        let net: String = r.get(1)?;
        out.push((addr, net.parse::<i128>()?));
    }
    let ms = t.elapsed().as_millis();
    Ok((out, ms, rss_mb()))
}

/// **Two Arrows, deliberately.** DataFusion 55 uses Arrow 59; nuthatch (and therefore the fixture, and
/// therefore a real sealed segment) uses Arrow 58. The fixture writer keeps 58 on purpose - the point
/// of this gate is that DataFusion reads what nuthatch actually *seals*, not a file written to suit it.
/// Only the result-reading below uses DataFusion's own Arrow, because those batches are its types.
///
/// RFC-0013 measured 54.1.0 with Arrow 58.3.0; the Arrow bump is part of what "current DataFusion"
/// now means and is not a confound to be engineered away.
async fn datafusion_net_balances(
    seg: &std::path::Path,
) -> anyhow::Result<(Vec<(String, i128)>, u128, u64)> {
    use datafusion::prelude::*;
    let ctx = SessionContext::new();
    // The **directory**, not a file: with SEGMENTS>1 there are many, and DuckDB has always globbed
    // `*.parquet` here. Registering one file would have silently compared all of DuckDB's segments
    // against one of DataFusion's - a parity failure if we were lucky, a meaningless ratio if not.
    ctx.register_parquet("t", seg.to_str().unwrap(), ParquetReadOptions::default())
        .await?;
    // The dialect difference, stated rather than hidden: DataFusion has no HUGEINT. `DECIMAL(38,0)`
    // is the equivalent 128-bit width, and `arrow_cast` is how DataFusion spells a widening cast that
    // yields NULL rather than erroring on overflow.
    let sql = "SELECT addr, CAST(SUM(d) AS VARCHAR) AS net FROM (\
                 SELECT \"to\" AS addr, TRY_CAST(\"value\" AS DECIMAL(38,0)) AS d FROM t \
                 UNION ALL \
                 SELECT \"from\" AS addr, -TRY_CAST(\"value\" AS DECIMAL(38,0)) AS d FROM t\
               ) GROUP BY addr HAVING SUM(d) <> 0 ORDER BY addr";
    let t = Instant::now();
    let batches = ctx.sql(sql).await?.collect().await?;
    let ms = t.elapsed().as_millis();

    // DataFusion reads Parquet strings as `Utf8View`, not `Utf8` - one of the small, real differences
    // a migration would meet everywhere. Cast rather than assume, so the comparison is about the
    // *answer* and not about which string layout each engine happens to prefer.
    let mut out = Vec::new();
    for b in batches {
        let col = |i: usize| -> anyhow::Result<datafusion::arrow::array::ArrayRef> {
            Ok(datafusion::arrow::compute::cast(
                b.column(i),
                &datafusion::arrow::datatypes::DataType::Utf8,
            )?)
        };
        let addr_arr = col(0)?;
        let net_arr = col(1)?;
        let addr = addr_arr
            .as_any()
            .downcast_ref::<datafusion::arrow::array::StringArray>()
            .ok_or_else(|| anyhow::anyhow!("addr column will not cast to Utf8"))?;
        let net = net_arr
            .as_any()
            .downcast_ref::<datafusion::arrow::array::StringArray>()
            .ok_or_else(|| anyhow::anyhow!("net column will not cast to Utf8"))?;
        for i in 0..b.num_rows() {
            out.push((addr.value(i).to_string(), net.value(i).parse::<i128>()?));
        }
    }
    Ok((out, ms, rss_mb()))
}
