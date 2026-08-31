//! #296: does a compact row encoding actually reduce **resident memory**?
//!
//! `docs/decisions/296-compact-rows.md` settled two things by measurement and left one open, and the
//! board decision (2026-08-31) is conditional on closing it:
//!
//!  * **Measured:** two production cursors sit at 1.45 GB and 1.42 GB RSS against a 2 GB budget.
//!  * **Measured:** row payloads are 2.45-2.49x larger as JSON than as a schema-driven binary
//!    encoding, on two independent real table shapes.
//!  * **Open:** what any of that does to RSS. `VmRSS` is process-wide - allocator, DuckDB, HTTP,
//!    ingestion - and four uncontrolled snapshots of a running nest cannot forecast it. Review was
//!    right to reject the linear fit that stood there.
//!
//! So this builds the same rows into two redb stores - one holding today's JSON strings, one holding
//! a compact encoding - and measures **the file, and the RSS of reading it back**. Same rows, same
//! store, one variable.
//!
//! **This is a prototype, not a format.** It exists to answer "is the saving real enough to spend
//! part of the upgrade promise on", and the honest outcome may be no. Nothing here is wired into the
//! product and the encoding is deliberately the simplest thing faithful to the model in the decision
//! document, not a design.
//!
//! `#[ignore]`d: it is a measurement, and a timing/memory number inside the CI-critical suite is
//! flaky under contention and machine-dependent.
//!
//! ```text
//! cargo test --test bench_compact_rows -- --ignored --nocapture
//! ```

use std::path::Path;

use serde_json::json;

/// Peak resident set of this process, in bytes. macOS and Linux spell it differently.
fn rss_bytes() -> u64 {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("VmRSS:"))
                    .and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
                    .map(|kb| kb * 1024)
            })
            .unwrap_or(0)
    }
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &std::process::id().to_string()])
            .output()
            .ok();
        out.and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(|kb| kb * 1024)
            .unwrap_or(0)
    }
}

/// A row the shape of a real one: the implicit columns plus an ERC-20 transfer's parameters.
///
/// Modelled on `staking__tokens_delegated` and `usdc__transfer`, the two shapes the decision
/// document measured, so the payload ratio here can be checked against the 2.45-2.49x it reported.
fn row_json(i: u64) -> String {
    let addr = |n: u64| format!("0x{:040x}", n);
    let h32 = |n: u64| format!("0x{:064x}", n);
    json!({
        "table": "usdc__transfer",
        "block_number": 60_000_000 + i,
        "block_hash": h32(i),
        "block_timestamp": 1_700_000_000u64 + i,
        "tx_hash": h32(i.wrapping_mul(7)),
        "log_index": i % 8,
        "address": addr(0xa0b8),
        "_seq": ((60_000_000 + i) << 20) | (i % 8),
        "from": addr(i % 5_000),
        "to": addr((i * 3) % 5_000),
        "value": format!("{}", 1_000_000_000_000_000_000u128 + i as u128),
        "value_dec": format!("{}", 1_000_000_000_000_000_000u128 + i as u128),
        "value_overflow": false,
    })
    .to_string()
}

/// The compact form, faithful to the model the decision document priced.
///
/// Field names are dropped (the schema has them), hashes are 32 raw bytes rather than 66-char hex,
/// addresses 20 rather than 42, the `uint256` is its 32-byte word rather than a decimal string,
/// block numbers and timestamps are varints, and `_seq` is **not stored at all** because it is
/// derived from `(block << 20) | log_index`.
fn row_compact(i: u64) -> Vec<u8> {
    fn varint(out: &mut Vec<u8>, mut n: u64) {
        while n >= 0x80 {
            out.push((n as u8) | 0x80);
            n >>= 7;
        }
        out.push(n as u8);
    }
    // The same values `row_json` writes, not zeros. The first version filled the fixed-width tail
    // with zero bytes while the JSON row carried values derived from `i`, so the two encodings did
    // not represent the same row and nothing in the harness noticed. `decoders_agree` below is what
    // stops that recurring.
    fn word32(n: u128) -> [u8; 32] {
        let mut w = [0u8; 32];
        w[16..].copy_from_slice(&n.to_be_bytes());
        w
    }
    fn addr20(n: u64) -> [u8; 20] {
        let mut a = [0u8; 20];
        a[12..].copy_from_slice(&n.to_be_bytes());
        a
    }
    let mut b = Vec::with_capacity(160);
    b.extend_from_slice(&[0u8, 1]); // table id, from a per-store dictionary
    varint(&mut b, 60_000_000 + i); // block_number
    varint(&mut b, 1_700_000_000 + i); // block_timestamp
    varint(&mut b, i % 8); // log_index
    b.extend_from_slice(&word32(i as u128)); // block_hash
    b.extend_from_slice(&word32(i.wrapping_mul(7) as u128)); // tx_hash
    b.extend_from_slice(&addr20(0xa0b8)); // address
    b.extend_from_slice(&addr20(i % 5_000)); // from
    b.extend_from_slice(&addr20((i * 3) % 5_000)); // to
    b.extend_from_slice(&word32(1_000_000_000_000_000_000u128 + i as u128)); // value
    b.push(0); // value_overflow
               // `value_dec` is not stored: it is the same number, and the duplicate is schema redundancy
               // rather than encoding. The decision document excludes it from every figure for exactly this
               // reason, so it is excluded here too.
    b
}

/// Build a store of `n` rows, values produced by `val`. Returns the file size in bytes.
fn build_store(path: &Path, n: u64, compact: bool) -> u64 {
    use redb::{Database, TableDefinition};
    const T: TableDefinition<&str, &[u8]> = TableDefinition::new("entities");
    let db = Database::create(path).expect("create");
    // Written in windows, as `commit_window` does, rather than one giant transaction: the write
    // cache and the resulting page layout both depend on transaction size, and a single 1.6M-row
    // commit is not a shape the product ever produces.
    let mut i = 0u64;
    while i < n {
        let end = (i + 5_000).min(n);
        let wtx = db.begin_write().expect("begin");
        {
            let mut t = wtx.open_table(T).expect("open");
            for j in i..end {
                let key = format!("usdc__transfer:{:012}:{}", 60_000_000 + j, j % 8);
                if compact {
                    t.insert(key.as_str(), row_compact(j).as_slice())
                        .expect("insert");
                } else {
                    t.insert(key.as_str(), row_json(j).as_bytes())
                        .expect("insert");
                }
            }
        }
        wtx.commit().expect("commit");
        i = end;
    }
    drop(db);
    std::fs::metadata(path).expect("stat").len()
}

/// Child process: open one store at one cache size, fill the cache by scanning, report RSS.
///
/// A **separate process** because RSS is process-wide (the correction that review made to the first
/// pass at this issue). Two stores opened in one process share an allocator, a page cache and each
/// other's high-water mark, and the second number would be meaningless.
fn measure_child() {
    use redb::{Builder, ReadableTable, TableDefinition};
    const T: TableDefinition<&str, &[u8]> = TableDefinition::new("entities");

    let path = std::env::var("NUTHATCH_296_STORE").expect("store");
    let cache: usize = std::env::var("NUTHATCH_296_CACHE")
        .expect("cache")
        .parse()
        .expect("num");
    let compact = std::env::var("NUTHATCH_296_ENC").expect("enc") == "compact";

    let base = rss_bytes();
    let db = Builder::new()
        .set_cache_size(cache)
        .open(&path)
        .expect("open");

    // Full scan: the worst case for residency, and the case a budget has to survive. Sum the bytes
    // so the read cannot be optimised away.
    let rtx = db.begin_read().expect("rtx");
    let t = rtx.open_table(T).expect("table");
    let mut rows = 0u64;
    let mut bytes = 0u64;
    for e in t.iter().expect("iter") {
        let (_k, v) = e.expect("entry");
        bytes += v.value().len() as u64;
        rows += 1;
    }
    let scanned = rss_bytes();

    // Then a point-read workload, which is what the hot store actually serves. With a cache smaller
    // than the store this is where the cost of a smaller cache shows up.
    let t0 = std::time::Instant::now();
    let mut hits = 0u64;
    let mut seed = 0x2933u64;
    for _ in 0..50_000 {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let j = (seed >> 33) % rows.max(1);
        let key = format!("usdc__transfer:{:012}:{}", 60_000_000 + j, j % 8);
        if let Some(v) = t.get(key.as_str()).expect("get") {
            // Decode, rather than just measuring the fetch: today's read path runs `serde_json` on
            // every row it returns, and a fixed-width/varint reader is the thing that replaces it.
            // Comparing raw fetches would hide the one benefit the encoding owns outright.
            hits += decode_one(v.value(), compact);
        }
    }
    let point_us = t0.elapsed().as_micros() as f64 / 50_000.0;

    println!(
        "MEASURE rows={rows} bytes={bytes} hits={hits} base_rss={base} scan_rss={} point_us={point_us:.1}",
        scanned
    );
}

/// The fields a caller gets back from one stored row. Both decoders must produce **all** of it, or
/// the comparison is not one.
///
/// The first version of this harness parsed the JSON object in full with `serde_json` and, on the
/// compact side, read three varints and sliced twenty bytes - then reported the difference as a
/// decode win. Review caught it. That is a full parse against a partial one, and the number it
/// produced was not a measurement of anything. This struct exists so the two paths cannot drift
/// apart again: every field is materialised on both sides, and `checksum` forces all of it to be
/// read rather than optimised out.
#[derive(Default)]
struct Row {
    block: u64,
    ts: u64,
    log_index: u64,
    seq: u64,
    block_hash: [u8; 32],
    tx_hash: [u8; 32],
    address: [u8; 20],
    from: [u8; 20],
    to: [u8; 20],
    value: [u8; 32],
    overflow: bool,
}

impl Row {
    fn checksum(&self) -> u64 {
        let b = |x: &[u8]| {
            x.iter()
                .fold(0u64, |a, &c| a.wrapping_mul(31).wrapping_add(c as u64))
        };
        self.block
            ^ self.ts
            ^ self.log_index
            ^ self.seq
            ^ b(&self.block_hash)
            ^ b(&self.tx_hash)
            ^ b(&self.address)
            ^ b(&self.from)
            ^ b(&self.to)
            ^ b(&self.value)
            ^ self.overflow as u64
    }
}

/// Decode one stored row into `Row`, by whichever encoding it is in.
fn decode_one(buf: &[u8], compact: bool) -> u64 {
    let mut r = Row::default();
    if compact {
        fn varint(buf: &[u8], p: &mut usize) -> u64 {
            let (mut n, mut shift) = (0u64, 0u32);
            loop {
                let b = buf[*p];
                *p += 1;
                n |= ((b & 0x7f) as u64) << shift;
                if b < 0x80 {
                    return n;
                }
                shift += 7;
            }
        }
        let mut p = 2usize; // table id
        r.block = varint(buf, &mut p);
        r.ts = varint(buf, &mut p);
        r.log_index = varint(buf, &mut p);
        let mut take = |n: usize, out: &mut [u8]| {
            out.copy_from_slice(&buf[p..p + n]);
            p += n;
        };
        take(32, &mut r.block_hash);
        take(32, &mut r.tx_hash);
        take(20, &mut r.address);
        take(20, &mut r.from);
        take(20, &mut r.to);
        take(32, &mut r.value);
        r.overflow = buf[p] != 0;
        // `_seq` is derived rather than stored - that saving is part of the encoding.
        r.seq = (r.block << 20) | r.log_index;
    } else {
        let v: serde_json::Value = serde_json::from_slice(buf).expect("json");
        r.block = v["block_number"].as_u64().unwrap_or(0);
        r.ts = v["block_timestamp"].as_u64().unwrap_or(0);
        r.log_index = v["log_index"].as_u64().unwrap_or(0);
        r.seq = v["_seq"].as_u64().unwrap_or(0);
        // The hex strings must actually be turned into bytes: that is the work the compact side
        // does not have to do, and it is the whole of the difference being measured.
        let un = |v: &serde_json::Value, out: &mut [u8]| {
            let s = v.as_str().unwrap_or("");
            let _ = hex::decode_to_slice(s.strip_prefix("0x").unwrap_or(s), out);
        };
        un(&v["block_hash"], &mut r.block_hash);
        un(&v["tx_hash"], &mut r.tx_hash);
        un(&v["address"], &mut r.address);
        un(&v["from"], &mut r.from);
        un(&v["to"], &mut r.to);
        // `value` is stored as a decimal string and its word form is what a caller wants.
        let n: u128 = v["value"].as_str().unwrap_or("0").parse().unwrap_or(0);
        r.value[16..].copy_from_slice(&n.to_be_bytes());
        r.overflow = v["value_overflow"].as_bool().unwrap_or(false);
    }
    r.checksum()
}

#[test]
#[ignore = "measurement: builds ~2 GB of stores and reports RSS; run deliberately"]
fn compact_rows_rss() {
    if std::env::var("NUTHATCH_296_STORE").is_ok() {
        measure_child();
        return;
    }

    let n: u64 = std::env::var("NUTHATCH_296_ROWS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1_600_000);

    let gib = 1024 * 1024 * 1024usize;

    // `NUTHATCH_296_DIR` keeps the stores between runs: building 1.6M rows twice takes minutes, and
    // re-measuring the same pair of files is the whole point of a sweep.
    let tmp = tempfile::tempdir().expect("tmp");
    let dir = std::env::var("NUTHATCH_296_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| tmp.path().to_path_buf());
    std::fs::create_dir_all(&dir).expect("dir");

    let json_path = dir.join("json.redb");
    let compact_path = dir.join("compact.redb");
    // A reused store must prove it holds what this run is about to report on. Without the stamp,
    // `NUTHATCH_296_ROWS=1000` against a directory built at 1,600,000 prints bytes-per-row computed
    // from one number while the children measure the other, and the two disagree silently. Review
    // caught that too.
    let stamp_path = dir.join("stamp");
    let stamp = format!("rows={n} enc=v1");
    let reuse = json_path.exists()
        && compact_path.exists()
        && std::fs::read_to_string(&stamp_path).ok().as_deref() == Some(stamp.as_str());
    if reuse {
        println!("reusing stores in {} ({stamp})", dir.display());
    } else {
        if json_path.exists() || compact_path.exists() {
            let found = std::fs::read_to_string(&stamp_path).unwrap_or_else(|_| "unstamped".into());
            println!(
                "rebuilding: {} holds `{found}`, this run wants `{stamp}`",
                dir.display()
            );
            let _ = std::fs::remove_file(&json_path);
            let _ = std::fs::remove_file(&compact_path);
        }
        println!("building {n} rows into two stores (this takes a few minutes)");
    }
    let json_len = if reuse {
        std::fs::metadata(&json_path).expect("stat").len()
    } else {
        build_store(&json_path, n, false)
    };
    let compact_len = if reuse {
        std::fs::metadata(&compact_path).expect("stat").len()
    } else {
        build_store(&compact_path, n, true)
    };

    std::fs::write(&stamp_path, &stamp).expect("stamp");

    println!(
        "\nfile: json {:.2} GB ({:.0} B/row), compact {:.2} GB ({:.0} B/row), ratio {:.2}x",
        json_len as f64 / 1e9,
        json_len as f64 / n as f64,
        compact_len as f64 / 1e9,
        compact_len as f64 / n as f64,
        json_len as f64 / compact_len as f64,
    );

    // Three configurations, each in its own process. The third is the one that matters: redb's
    // default cache is a **fixed 1 GiB ceiling** (`Builder::new` calls `set_cache_size(1 GiB)`,
    // split 90/10 read/write), not a fraction of the file. If that ceiling is what today's RSS is
    // resting against, then shrinking the rows and shrinking the cache buy the same thing - and one
    // of them is a config line rather than a storage migration that spends the RFC-0020 promise.
    let exe = std::env::current_exe().expect("exe");

    // The discriminating sweep. If RSS is governed by the **file**, the two encodings stay ~2x apart
    // at every cache size. If it is governed by the **cache setting**, they converge as soon as the
    // cache is smaller than both files - and then the encoding is not what is buying the memory.
    //
    // This is the control that decides #296, so it is run rather than reasoned about.
    let mut cases: Vec<(String, &Path, usize)> = Vec::new();
    for (enc, path) in [("json", &json_path), ("compact", &compact_path)] {
        for (cname, cache) in [
            ("1 GiB (today)", gib),
            ("512 MiB", gib / 2),
            ("256 MiB", gib / 4),
            ("128 MiB", gib / 8),
        ] {
            cases.push((format!("{enc:<7} @ {cname}"), path.as_path(), cache));
        }
    }

    println!();
    for (label, path, cache) in &cases {
        let (label, path, cache) = (label.as_str(), *path, *cache);
        let out = std::process::Command::new(&exe)
            .args(["compact_rows_rss", "--ignored", "--nocapture", "--exact"])
            .env("NUTHATCH_296_STORE", path)
            .env("NUTHATCH_296_CACHE", cache.to_string())
            .env(
                "NUTHATCH_296_ENC",
                if label.starts_with("compact") {
                    "compact"
                } else {
                    "json"
                },
            )
            .output()
            .expect("spawn");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let line = stdout
            .lines()
            .find(|l| l.starts_with("MEASURE "))
            .unwrap_or_else(|| panic!("child produced no measurement:\n{stdout}"));
        let field = |k: &str| -> f64 {
            line.split_whitespace()
                .find_map(|f| f.strip_prefix(k))
                .and_then(|v| v.parse().ok())
                .unwrap_or_else(|| panic!("missing {k} in {line}"))
        };
        // Belt as well as braces on the stamp: the child says what it actually walked, and a
        // disagreement with what the parent priced is a hard stop rather than a footnote.
        let seen = field("rows=") as u64;
        assert_eq!(
            seen, n,
            "{label}: child measured {seen} rows, parent priced {n}"
        );
        println!(
            "{label:<28} rss {:>6.2} GB   point-read {:>6.1} us",
            field("scan_rss=") / 1e9,
            field("point_us="),
        );
    }
}

/// The two encodings must represent the **same row**, or the latency comparison above is between two
/// different pieces of work and means nothing.
///
/// This is the check whose absence review found twice: the first pass compared a full JSON parse
/// against a partial compact read, and the second still wrote zero bytes into every fixed-width
/// field of the compact row while the JSON row carried real values. Both were invisible because
/// nothing ever compared the two decoders' output. Now something does, and it is a **normal test**
/// rather than an ignored one, so it runs in CI where the measurement itself does not.
#[test]
fn the_two_encodings_decode_to_the_same_row() {
    for i in [0u64, 1, 7, 127, 128, 5_000, 999_999, 1_599_999] {
        let j = decode_one(row_json(i).as_bytes(), false);
        let c = decode_one(&row_compact(i), true);
        assert_eq!(j, c, "row {i}: json decoded to {j:#x}, compact to {c:#x}");
    }

    // And it must be able to tell rows apart - an encoder that returned a constant would satisfy the
    // loop above without encoding anything at all.
    let distinct: std::collections::HashSet<u64> =
        (0..64).map(|i| decode_one(&row_compact(i), true)).collect();
    assert_eq!(distinct.len(), 64, "compact decode is not row-dependent");
}
