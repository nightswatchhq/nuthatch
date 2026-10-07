//! RFC-0062 S0 spike, never merged: a maintained view is answered from a stored copy of its own
//! request-time evaluation, keyed on every input that evaluation reads.
//!
//! `NUTHATCH_S0_MAINTAINED=bet,token` names the views. A request that reaches one looks for
//! `maintained/<view>/<identity>.parquet`; present, the view is bound to it, absent, the request
//! answers from the definition as before and one builder thread writes the copy in the background.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use sha2::{Digest, Sha256};

use crate::analytics::HotRows;

pub(crate) const ENV: &str = "NUTHATCH_S0_MAINTAINED";

pub(crate) fn declared() -> BTreeSet<String> {
    std::env::var(ENV)
        .unwrap_or_default()
        .split(',')
        .map(|v| v.trim().to_ascii_lowercase())
        .filter(|v| !v.is_empty())
        .collect()
}

/// Everything the view's evaluation reads: its name, the engine, the authored files, the sealed
/// segments at the served watermark, and the hot rows of every table its definition reaches.
pub(crate) fn identity(
    view: &str,
    engine: &str,
    files: &BTreeMap<PathBuf, String>,
    sealed: &str,
    closure: &BTreeSet<String>,
    hot: &HotRows,
) -> String {
    let mut h = Sha256::new();
    let mut field = |bytes: &[u8]| {
        h.update((bytes.len() as u64).to_le_bytes());
        h.update(bytes);
    };
    field(view.as_bytes());
    field(engine.as_bytes());
    for (path, stamp) in files {
        field(path.to_string_lossy().as_bytes());
        field(stamp.as_bytes());
    }
    field(sealed.as_bytes());
    for (table, rows) in hot {
        if rows.is_empty() || !closure.contains(&table.to_ascii_lowercase()) {
            continue;
        }
        field(table.as_bytes());
        field(&serde_json::to_vec(rows).unwrap_or_default());
    }
    hex::encode(h.finalize())
}

pub(crate) fn path(dir: &Path, view: &str, id: &str) -> PathBuf {
    dir.join("maintained").join(view).join(format!("{id}.parquet"))
}

fn in_flight() -> &'static Mutex<HashSet<PathBuf>> {
    static S: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    S.get_or_init(Default::default)
}

/// One build at a time per process, so the copies never hold more than one evaluation's memory.
fn build_lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(Default::default)
}

thread_local! {
    pub(crate) static WRITE_TO: std::cell::RefCell<Option<(String, PathBuf)>> =
        const { std::cell::RefCell::new(None) };
}

pub(crate) fn writing() -> Option<(String, PathBuf)> {
    WRITE_TO.with(|w| w.borrow().clone())
}

/// Start a background build of `view` into `target` unless one is already running or queued.
pub(crate) fn spawn_build(
    dir: &Path,
    view: &str,
    target: PathBuf,
    hot: HotRows,
    sealed_through: u64,
    declared: Vec<crate::registry::TableSchema>,
) {
    if !in_flight()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(target.clone())
    {
        return;
    }
    let (dir, view) = (dir.to_path_buf(), view.to_string());
    std::thread::spawn(move || {
        let _one = build_lock().lock().unwrap_or_else(|p| p.into_inner());
        let started = Instant::now();
        let tmp = target.with_extension("parquet.tmp");
        let made = (|| -> anyhow::Result<u64> {
            std::fs::create_dir_all(target.parent().expect("has a parent"))?;
            WRITE_TO.with(|w| *w.borrow_mut() = Some((view.clone(), tmp.clone())));
            let out = crate::analytics::materialise(&dir, &view, &hot, sealed_through, &declared);
            WRITE_TO.with(|w| *w.borrow_mut() = None);
            out?;
            std::fs::rename(&tmp, &target)?;
            Ok(std::fs::metadata(&target)?.len())
        })();
        match made {
            Ok(bytes) => tracing::info!(
                "s0: maintained {view} written in {:.3} s, {bytes} bytes, {}",
                started.elapsed().as_secs_f64(),
                target.display()
            ),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                tracing::warn!("s0: maintained {view} failed: {e:#}")
            }
        }
        in_flight()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&target);
    });
}
