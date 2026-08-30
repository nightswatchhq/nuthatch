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
    // KEEP_FIXTURE=<dir> writes the fixture somewhere durable so other tools can profile the same
    // bytes rather than a freshly generated approximation of them.
    let keep = std::env::var("KEEP_FIXTURE").ok();
    let dir = tempfile::tempdir()?;
    let base: std::path::PathBuf = match &keep {
        Some(d) => std::path::PathBuf::from(d),
        None => dir.path().to_path_buf(),
    };
    let seg = base.join(seal::SEGMENTS_DIR);
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
    println!(
        "order: {}",
        if df_first {
            "datafusion, then duckdb"
        } else {
            "duckdb, then datafusion"
        }
    );

    // **Repeats inside one process** (#964). Writing a 10,000-file fixture costs far more than the
    // query, so a fresh process per repeat would measure the fixture writer. `REPEATS=n` writes once
    // and times both engines n times; the reported figure is the median, per `docs/bench/noise-floor.md`.
    let repeats: usize = std::env::var("REPEATS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1)
        .max(1);
    if repeats > 1 {
        let mut d_all = Vec::new();
        let mut f_all = Vec::new();
        let mut r_all = Vec::new();
        let mut last = None;
        for _ in 0..repeats {
            let (duck, d_ms, _) = duckdb_net_balances(&seg)?;
            let (df, f_ms, _) = datafusion_net_balances(&seg).await?;
            let (rs, r_ms, _) = rust_net_balances(&seg)?;
            d_all.push(d_ms);
            f_all.push(f_ms);
            r_all.push(r_ms);
            last = Some((duck, df, rs));
        }
        d_all.sort_unstable();
        f_all.sort_unstable();
        r_all.sort_unstable();
        let dm = d_all[d_all.len() / 2];
        let fm = f_all[f_all.len() / 2];
        let rm = r_all[r_all.len() / 2];
        let (duck, df, rs) = last.unwrap();
        // **Parity is the acceptance criterion, not the timing** (#987). A faster operator that
        // disagrees with DuckDB on one address has produced nothing, so both candidates are compared
        // against DuckDB and a mismatch fails the run rather than being reported alongside a ratio.
        let df_parity = duck == df;
        let rs_parity = duck == rs;
        println!(
            "RESULT\trows={rows}\tsegments={segments}\trepeats={repeats}\tduck_median_ms={dm}\tdf_median_ms={fm}\trust_median_ms={rm}\tdf_ratio={:.2}\trust_ratio={:.2}\tdf_parity={}\trust_parity={}\taddrs={}",
            fm as f64 / dm.max(1) as f64,
            rm as f64 / dm.max(1) as f64,
            if df_parity { "identical" } else { "DIFFER" },
            if rs_parity { "identical" } else { "DIFFER" },
            duck.len()
        );
        println!("duck_all={d_all:?}");
        println!("df_all={f_all:?}");
        println!("rust_all={r_all:?}");
        if !df_parity || !rs_parity {
            if !rs_parity {
                let mismatch: Vec<String> = duck
                    .iter()
                    .zip(rs.iter())
                    .filter(|(a, b)| a != b)
                    .take(3)
                    .map(|(a, b)| format!("duck {a:?} vs rust {b:?}"))
                    .collect();
                eprintln!(
                    "rust operator disagrees: {} rows vs {}; first: {mismatch:?}",
                    rs.len(),
                    duck.len()
                );
            }
            anyhow::bail!("parity failed at rows={rows} segments={segments}");
        }
        return Ok(());
    }

    // **§11's concurrent-throughput row** (#986). `noise-floor.md` records the distribution going
    // bimodal under concurrency, so p95 is the statistic and a median cannot see it.
    //
    // **The two candidates are not concurrent in the same way, and that is the finding rather than a
    // caveat.** `analytics.rs` holds *one* cached read-only DuckDB connection per directory and
    // queries take a mutex - so concurrent `/sql` against one nest **serialises**, whatever the engine
    // does inside a single query. The specialised operator has no shared connection: it is a pure
    // function over files, so N callers genuinely overlap. Modelling DuckDB without that mutex would
    // measure an engine nuthatch does not deploy.
    let concurrency: usize = std::env::var("CONCURRENCY")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if concurrency > 0 {
        use std::sync::{Arc, Mutex};
        let per_client: usize = std::env::var("PER_CLIENT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(10);

        // One connection behind one mutex - nuthatch's actual serving path.
        let conn = Arc::new(Mutex::new(duckdb::Connection::open_in_memory()?));
        {
            let c = conn.lock().unwrap();
            c.execute_batch(&format!(
                "CREATE VIEW t AS SELECT * FROM read_parquet('{}/*.parquet');",
                seg.display()
            ))?;
        }
        let duck_sql = "SELECT addr, SUM(d)::VARCHAR AS net FROM (\
                          SELECT \"to\" AS addr, TRY_CAST(\"value\" AS HUGEINT) AS d FROM t \
                          UNION ALL \
                          SELECT \"from\" AS addr, -TRY_CAST(\"value\" AS HUGEINT) AS d FROM t\
                        ) GROUP BY addr HAVING SUM(d) <> 0 ORDER BY addr";

        let run = |label: &str,
                   f: &(dyn Fn() -> anyhow::Result<usize> + Sync)|
         -> anyhow::Result<()> {
            // Warm once so the first client does not pay for cold page cache on everyone's behalf.
            f()?;
            let start = Instant::now();
            let lat: Vec<u128> = std::thread::scope(|sc| {
                let hs: Vec<_> = (0..concurrency)
                    .map(|_| {
                        sc.spawn(|| {
                            let mut v = Vec::with_capacity(per_client);
                            for _ in 0..per_client {
                                let t = Instant::now();
                                let _ = f();
                                v.push(t.elapsed().as_micros());
                            }
                            v
                        })
                    })
                    .collect();
                hs.into_iter().flat_map(|h| h.join().unwrap()).collect()
            });
            let wall = start.elapsed().as_secs_f64();
            let mut l = lat.clone();
            l.sort_unstable();
            let pc = |q: f64| l[((l.len() as f64 - 1.0) * q).round() as usize];
            println!(
                "CONC\t{label}\tclients={concurrency}\tqueries={}\tqps={:.1}\tp50_ms={:.1}\tp95_ms={:.1}\tp99_ms={:.1}",
                l.len(),
                l.len() as f64 / wall,
                pc(0.50) as f64 / 1000.0,
                pc(0.95) as f64 / 1000.0,
                pc(0.99) as f64 / 1000.0
            );
            Ok(())
        };

        println!("== concurrency: {rows} rows, {segments} segments, {concurrency} clients, {per_client} queries each ==");
        let cn = conn.clone();
        run("duckdb-shared-conn", &move || {
            let c = cn.lock().unwrap();
            let mut st = c.prepare(duck_sql)?;
            let mut rows = st.query([])?;
            let mut n = 0usize;
            while rows.next()?.is_some() {
                n += 1;
            }
            Ok(n)
        })?;
        run("rust-operator", &|| Ok(rust_net_balances(&seg)?.0.len()))?;
        return Ok(());
    }

    let (duck, d_ms, d_rss, df, f_ms, f_rss) = if df_first {
        let (df, f_ms, f_rss) = datafusion_net_balances(&seg).await?;
        let (duck, d_ms, d_rss) = duckdb_net_balances(&seg)?;
        (duck, d_ms, d_rss, df, f_ms, f_rss)
    } else {
        let (duck, d_ms, d_rss) = duckdb_net_balances(&seg)?;
        let (df, f_ms, f_rss) = datafusion_net_balances(&seg).await?;
        (duck, d_ms, d_rss, df, f_ms, f_rss)
    };
    println!(
        "duckdb:     {:>6} ms  rss {:>4} MB  {} addresses",
        d_ms,
        d_rss,
        duck.len()
    );
    println!(
        "datafusion: {:>6} ms  rss {:>4} MB  {} addresses",
        f_ms,
        f_rss,
        df.len()
    );

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
/// **RFC-0042 §5.3, the "heavy known fold" arm (#987).** The same `net_balances` answer with no SQL
/// engine in the path: `parquet` + `arrow` read the segments, an `i128` accumulator does the fold.
///
/// #964 measured this query going down the *general SQL* arm and found DataFusion 2.5-2.9x slower than
/// DuckDB at a realistic layout - roughly ten times the margin §7 already calls disqualifying. But
/// §5.3 never routed a heavy known fold to general SQL. This is the arm it actually points at, and it
/// is the measurement the RFC has been missing.
///
/// **Semantics are copied from the SQL, not approximated.** `TRY_CAST` yields NULL on a value that will
/// not parse and `SUM` ignores NULLs, so an unparseable value is **skipped**, never an error and never
/// a zero - a zero would be a different answer that happens to look plausible. `HAVING SUM(d) <> 0`
/// drops the addresses that net out, and `ORDER BY addr` is part of the answer because the parity check
/// compares sequences.
fn rust_net_balances(seg: &std::path::Path) -> anyhow::Result<(Vec<(String, i128)>, u128, u64)> {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    use rayon::prelude::*;
    use rustc_hash::FxHashMap;

    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(seg)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "parquet"))
        .collect();
    // Deterministic order. Not for correctness - i128 addition is associative and exact, unlike the
    // float case #961 measured, so the per-file partials merge to the same answer in any order - but
    // so a timing is not a function of directory iteration order.
    files.sort();

    let t = Instant::now();

    // **Per-file partials, merged.** Profiling (`fold_profile`) put 98 ms in Parquet decode and 124 ms
    // in the accumulator for a 200k-row/20-file fixture, against DuckDB's 28 ms for the whole query -
    // so a single-threaded fold with std's SipHash was never going to be the measurement §5.3 asks
    // for. Exact i128 addition is what makes the split safe: partials merge without reassociation
    // error, which a float sum could not claim (#961).
    // Split by **row group**, not by file. Splitting by file gives a one-segment fixture no
    // parallelism at all, and #964's sweep runs 1 / 100 / 1 000 / 10 000 segments over the same rows -
    // so a file-parallel operator would look strong only where the layout happened to suit it, which
    // is the confound that whole sweep exists to remove.
    let mut units: Vec<(std::path::PathBuf, usize)> = Vec::new();
    for f in &files {
        let n = ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(f)?)?
            .metadata()
            .num_row_groups();
        for g in 0..n {
            units.push((f.clone(), g));
        }
    }

    let partials: Vec<FxHashMap<String, i128>> = units
        .par_iter()
        .map(|(f, group)| -> anyhow::Result<FxHashMap<String, i128>> {
            let mut acc: FxHashMap<String, i128> = FxHashMap::default();
            let reader = ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(f)?)?
                .with_batch_size(8192)
                .with_row_groups(vec![*group])
                .build()?;
            for batch in reader {
                let batch = batch?;
                // `downcast_ref`, not `StringArray::from(col.to_data())` - borrow the Arrow buffer
                // rather than rebuilding the array per batch.
                let col = |name: &str| -> anyhow::Result<&arrow::array::StringArray> {
                    let idx = batch
                        .schema()
                        .index_of(name)
                        .map_err(|e| anyhow::anyhow!("column {name}: {e}"))?;
                    batch
                        .column(idx)
                        .as_any()
                        .downcast_ref::<arrow::array::StringArray>()
                        .ok_or_else(|| anyhow::anyhow!("column {name} is not Utf8"))
                };
                let from = col("from")?;
                let to = col("to")?;
                let value = col("value")?;
                for i in 0..batch.num_rows() {
                    // TRY_CAST: unparseable -> NULL -> ignored by SUM. Skip, never default to 0 - a
                    // zero would be a different answer that happens to look plausible.
                    let Ok(d) = value.value(i).parse::<i128>() else {
                        continue;
                    };
                    for (addr, signed) in [(to.value(i), d), (from.value(i), -d)] {
                        if let Some(v) = acc.get_mut(addr) {
                            *v += signed;
                        } else {
                            acc.insert(addr.to_string(), signed);
                        }
                    }
                }
            }
            Ok(acc)
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    let mut acc: FxHashMap<String, i128> = FxHashMap::default();
    for part in partials {
        for (k, v) in part {
            *acc.entry(k).or_insert(0) += v;
        }
    }

    // `HAVING SUM(d) <> 0` then `ORDER BY addr` - both are part of the answer, because the parity
    // check compares sequences rather than sets.
    let mut out: Vec<(String, i128)> = acc.into_iter().filter(|(_, v)| *v != 0).collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    let ms = t.elapsed().as_millis();
    Ok((out, ms, rss_mb()))
}

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
        let n = if i < small_count {
            per_small
        } else {
            per_large
        };
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
fn write_fixture_offset(path: &std::path::Path, rows: usize, offset: usize) -> anyhow::Result<()> {
    use arrow::array::{StringArray, UInt64Array};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use std::sync::Arc;

    let addrs: Vec<String> = (0..512).map(|i| format!("0x{i:040x}")).collect();
    let idx = |i: usize| i + offset;
    let from: Vec<&str> = (0..rows).map(|i| addrs[idx(i) % 512].as_str()).collect();
    let to: Vec<&str> = (0..rows)
        .map(|i| addrs[(idx(i) * 7 + 3) % 512].as_str())
        .collect();
    // Values large enough that i64 would overflow and i128 would not - the reason HUGEINT is in the
    // query at all. A fixture of small values would let a broken cast pass.
    let value: Vec<String> = (0..rows)
        .map(|i| {
            format!(
                "{}",
                1_000_000_000_000_000_000u128 * (idx(i) as u128 % 97 + 1)
            )
        })
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
            Arc::new(StringArray::from(
                value.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            )),
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
