//! #1046 - the hot store's RSS ceiling is redb's default, and nobody chose it.
//!
//! `redb::Builder::new()` calls `set_cache_size(1 GiB)` (90% read / 10% write) and `store.rs` never
//! overrode it at any of its three open sites. redb 2.6 does **not** mmap - `file_backend/unix.rs`
//! preads into `Vec<u8>` - so every cached page is heap and lands in `VmRSS`, which is what
//! non-negotiable 2 bounds.
//!
//! Measured in `tests/bench_compact_rows.rs` (#296/#1045) against a 2.16 GB store on Linux: RSS
//! tracks the cache setting almost one-for-one and is **independent of the file**, while a point
//! read costs +0.6 us going from 1 GiB to 256 MiB. The two live Lodestar cursors sit at 1.44 and
//! 1.42 GB against their 2 GB, and the *larger* store has the *smaller* RSS - which a linear
//! `RSS = k x file` cannot produce and a ceiling can.
//!
//! `NUTHATCH_HOT_STORE_CACHE_BYTES` makes that one binary and three settings, so #1046's curve can
//! be measured on the box that enforces the budget rather than on whichever was convenient. **The
//! default is unchanged and this is a no-op until a measured curve says otherwise.**

use nuthatch::store::{
    hot_store_cache_bytes, HOT_STORE_CACHE_BYTES, HOT_STORE_CACHE_CEILING, HOT_STORE_CACHE_FLOOR,
};

/// One lock for every read or write of the variable, for the reason `sql_concurrency_is_measurable`
/// spells out at length (#1035): the variable is process-global, and two unrelated locks exclude
/// each other not at all.
fn env_lock() -> &'static std::sync::Mutex<()> {
    static L: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    L.get_or_init(|| std::sync::Mutex::new(()))
}

const KEY: &str = "NUTHATCH_HOT_STORE_CACHE_BYTES";

/// Restore on **every** path including a panic - a failing assertion is a panic, and a trailing
/// restore statement leaves the override installed for whoever runs next in the same process.
struct EnvRestore(Option<String>);

impl Drop for EnvRestore {
    fn drop(&mut self) {
        set_env(self.0.as_deref());
    }
}

fn set_env(value: Option<&str>) {
    match value {
        Some(v) => std::env::set_var(KEY, v),
        None => std::env::remove_var(KEY),
    }
}

fn with_env<T>(value: Option<&str>, f: impl FnOnce() -> T) -> T {
    // `_restore` drops before `_g`, so the value is put back while the lock is still held.
    let _g = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var(KEY).ok();
    set_env(value);
    let _restore = EnvRestore(prev);
    f()
}

#[test]
fn unset_keeps_redbs_own_default() {
    assert_eq!(
        with_env(None, hot_store_cache_bytes),
        HOT_STORE_CACHE_BYTES,
        "an operator who sets nothing must get exactly what they get today - this knob exists to \
         make the ceiling measurable, not to change it"
    );
    assert_eq!(
        HOT_STORE_CACHE_BYTES,
        1024 * 1024 * 1024,
        "the default is redb's own 1 GiB, so #1046 slice 1 is a no-op until a measured curve says \
         otherwise. Changing this number is a product decision, not a tidy-up"
    );
}

#[test]
fn the_curve_1046_asks_for_is_honoured() {
    let gib = 1024 * 1024 * 1024usize;
    for n in [gib, gib / 2, gib / 4] {
        assert_eq!(
            with_env(Some(&n.to_string()), hot_store_cache_bytes),
            n,
            "#1046 asks for 1 GiB / 512 MiB / 256 MiB on the enforcing box; if any point is not \
             honoured the curve silently flattens and reports a knee that is an artefact"
        );
    }
}

#[test]
fn a_value_below_the_floor_is_clamped_not_obeyed() {
    assert_eq!(
        with_env(Some("1048576"), hot_store_cache_bytes),
        HOT_STORE_CACHE_FLOOR,
        "below the floor redb thrashes rather than caches; a run that asked for 1 MiB and silently \
         got it would publish a latency figure whose meaning is 'the cache was switched off'"
    );
}

#[test]
fn a_value_above_the_ceiling_is_clamped_not_obeyed() {
    let asked = HOT_STORE_CACHE_CEILING * 4;
    assert_eq!(
        with_env(Some(&asked.to_string()), hot_store_cache_bytes),
        HOT_STORE_CACHE_CEILING,
        "the cache is per **store** and one cursor holds one per nest, so raising it multiplies \
         against a 2 GB per-cursor budget. This is the per-nest-vs-per-cursor trap #1024 found in \
         #1006's override, and it gets the same answer"
    );
    assert_eq!(
        HOT_STORE_CACHE_CEILING, HOT_STORE_CACHE_BYTES,
        "the ceiling is deliberately the default: this knob exists to lower a ceiling nobody chose, \
         and raising it past redb's default has no upside and breaks the budget N nests over"
    );
}

#[test]
fn rubbish_falls_back_to_the_default_rather_than_zero() {
    for raw in ["", "  ", "banana", "-1", "1.5", "512MiB", "0"] {
        assert_eq!(
            with_env(Some(raw), hot_store_cache_bytes),
            HOT_STORE_CACHE_BYTES,
            "{raw:?} must fall back to the default; a cache size of 0 parsed from a typo would take \
             the store to a heap-thrashing crawl with no error"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// The test that matters: the setting must reach redb.
//
// Every test above exercises the *parser*. All five stay green if `builder()` is changed to drop
// `set_cache_size` entirely - the knob would parse, clamp, log and do nothing, and #1046's whole
// curve would be a flat line measured against redb's untouched default. That is the shape this
// project keeps finding (a gate that matches its own comment; four tests green with the mechanism
// removed), so the wire itself gets a test.
//
// The only observable is RSS, and RSS is process-wide, so each configuration is measured in **its
// own process** - the correction review made to #296. The store is built once, opened twice through
// the real `Store::open_existing`, and the two figures must differ by more than the store could
// explain any other way.
// ---------------------------------------------------------------------------------------------

/// ~150 MB of store: comfortably above the 16 MiB floor, so a floor-sized cache cannot hold it and
/// a default-sized one can. Small enough to build in seconds.
const ROWS: u64 = 110_000;

fn rss_bytes() -> u64 {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| {
                s.lines().find(|l| l.starts_with("VmRSS:")).and_then(|l| {
                    l.split_whitespace()
                        .nth(1)?
                        .parse::<u64>()
                        .ok()
                        .map(|kb| kb * 1024)
                })
            })
            .unwrap_or(0)
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &std::process::id().to_string()])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(|kb| kb * 1024)
            .unwrap_or(0)
    }
}

fn row_json(i: u64) -> String {
    format!(
        r#"{{"table":"t","block_number":{},"block_hash":"0x{:064x}","block_timestamp":{},"tx_hash":"0x{:064x}","log_index":{},"address":"0x{:040x}","from":"0x{:040x}","to":"0x{:040x}","value":"{}"}}"#,
        i,
        i,
        1_700_000_000u64 + i,
        i.wrapping_mul(7),
        i % 8,
        0xa0b8,
        i % 5_000,
        (i * 3) % 5_000,
        1_000_000_000_000_000_000u128 + i as u128
    )
}

fn key(i: u64) -> String {
    format!("t:{:012}:{}", i, i % 8)
}

/// Child: open the store through the **product path** at the configured cache size, scan it, print
/// RSS. Driven by `NUTHATCH_1046_CHILD` so it never runs during an ordinary test pass.
fn measure_child(path: &str) {
    use nuthatch::store::Store;
    let store = Store::open_existing(std::path::Path::new(path)).expect("open");
    let mut n = 0u64;
    let mut bytes = 0u64;
    for i in 0..ROWS {
        if let Some(v) = store.get_entity(&key(i)).expect("get") {
            bytes += v.len() as u64;
            n += 1;
        }
    }
    println!("CHILD rows={n} bytes={bytes} rss={}", rss_bytes());
}

#[test]
fn the_setting_actually_reaches_redb() {
    use nuthatch::store::Store;

    if let Ok(path) = std::env::var("NUTHATCH_1046_CHILD") {
        measure_child(&path);
        return;
    }

    let dir = tempfile::tempdir().expect("tmp");
    let path = dir.path().join("hot.redb");
    {
        let store = Store::open(&path).expect("create");
        let mut i = 0u64;
        while i < ROWS {
            let end = (i + 5_000).min(ROWS);
            let rows: Vec<(String, String)> = (i..end).map(|j| (key(j), row_json(j))).collect();
            store.commit_window(&rows, None, end).expect("commit");
            i = end;
        }
    }
    let file = std::fs::metadata(&path).expect("stat").len();
    assert!(
        file > 64 * 1024 * 1024,
        "fixture is {file} B; it must exceed the 16 MiB floor by enough that a floor-sized cache \
         cannot hold it, or this test cannot tell a live wire from a cut one"
    );

    let exe = std::env::current_exe().expect("exe");
    let measure = |cache: usize| -> u64 {
        let out = std::process::Command::new(&exe)
            .args([
                "the_setting_actually_reaches_redb",
                "--exact",
                "--nocapture",
            ])
            .env("NUTHATCH_1046_CHILD", &path)
            .env(KEY, cache.to_string())
            .output()
            .expect("spawn");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let line = stdout
            .lines()
            .find(|l| l.starts_with("CHILD "))
            .unwrap_or_else(|| {
                panic!(
                    "child produced no measurement:\n{stdout}\n{}",
                    String::from_utf8_lossy(&out.stderr)
                )
            });
        let f = |k: &str| -> u64 {
            line.split_whitespace()
                .find_map(|t| t.strip_prefix(k))
                .and_then(|v| v.parse().ok())
                .unwrap_or_else(|| panic!("missing {k} in {line}"))
        };
        assert_eq!(
            f("rows="),
            ROWS,
            "child did not read the whole fixture: {line}"
        );
        f("rss=")
    };

    let big = measure(HOT_STORE_CACHE_BYTES);
    let small = measure(HOT_STORE_CACHE_FLOOR);
    println!(
        "fixture {} MB: rss {} MB at the 1 GiB default, {} MB at the {} MiB floor",
        file / 1_000_000,
        big / 1_000_000,
        small / 1_000_000,
        HOT_STORE_CACHE_FLOOR / 1024 / 1024
    );

    // The floor-capped process must be materially smaller. The margin is deliberately far below the
    // ~130 MB the difference in cache ceilings can account for, so ordinary allocator noise cannot
    // produce it and a cut wire cannot survive it.
    let saved = big.saturating_sub(small);
    assert!(
        saved > 32 * 1024 * 1024,
        "RSS at the {} MiB floor was {} MB and at the {} MiB default {} MB - a difference of {} MB. \
         The cache size is not reaching redb: `Store::open_existing` is still getting \
         `Builder::new()`'s untouched default, so #1046's curve would be flat for the wrong reason.",
        HOT_STORE_CACHE_FLOOR / 1024 / 1024,
        small / 1_000_000,
        HOT_STORE_CACHE_BYTES / 1024 / 1024,
        big / 1_000_000,
        saved / 1_000_000,
    );
}
