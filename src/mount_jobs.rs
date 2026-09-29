//! Asynchronous live mounts (#1544).
//!
//! A mount that fetches its nest from a registry can take minutes, longer than an HTTP client will
//! wait, and a caller that timed out had no record of what happened. A mount is now a job: accepted,
//! then fetching, then joining its cursor, then live or failed with a reason. The jobs sit beside the
//! runtime's lock rather than behind it, so reading one never waits on the mount it describes.
//!
//! Unfinished and failed jobs are written to [`JOBS_FILE`], so a restart resumes the one and still
//! reports the other. A live mount is already recorded in `mounts.toml`, which is where a restart
//! reads it from, so a live job is kept in memory only.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Where unfinished and failed mount jobs are kept, beside `mounts.toml`.
pub const JOBS_FILE: &str = "mount-jobs.json";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MountPhase {
    Accepted,
    Fetching,
    Joining,
    Live,
    Failed,
    /// Suspended by the operator (#1548): off its cursor, answering 503, until resumed.
    Suspended,
}

impl MountPhase {
    pub fn finished(self) -> bool {
        matches!(
            self,
            MountPhase::Live | MountPhase::Failed | MountPhase::Suspended
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountJob {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nid: Option<String>,
    pub phase: MountPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub since_unixtime: u64,
}

impl MountJob {
    pub fn new(name: &str, nid: Option<&str>, phase: MountPhase) -> MountJob {
        MountJob {
            name: name.to_string(),
            nid: nid.map(str::to_string),
            phase,
            reason: None,
            since_unixtime: now_unix(),
        }
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Every mount the runtime knows by name: the live ones, and the jobs still running or failed.
pub struct MountJobs {
    file: PathBuf,
    jobs: std::sync::Mutex<BTreeMap<String, MountJob>>,
}

impl MountJobs {
    /// Read the jobs a previous run left. A file that will not parse is reported and set aside rather
    /// than refusing the boot: losing a job's history is better than a runtime that will not start.
    pub fn load(dir: &Path) -> MountJobs {
        let file = dir.join(JOBS_FILE);
        let jobs = match std::fs::read(&file) {
            Ok(raw) => match serde_json::from_slice::<Vec<MountJob>>(&raw) {
                Ok(list) => list.into_iter().map(|j| (j.name.clone(), j)).collect(),
                Err(e) => {
                    tracing::warn!(
                        "{} is unreadable ({e}); earlier mount jobs are not resumed",
                        file.display()
                    );
                    BTreeMap::new()
                }
            },
            Err(_) => BTreeMap::new(),
        };
        MountJobs {
            file,
            jobs: std::sync::Mutex::new(jobs),
        }
    }

    pub fn get(&self, name: &str) -> Option<MountJob> {
        self.jobs.lock().unwrap().get(name).cloned()
    }

    pub fn list(&self) -> Vec<MountJob> {
        self.jobs.lock().unwrap().values().cloned().collect()
    }

    /// The jobs a restart must pick up again.
    pub fn unfinished(&self) -> Vec<MountJob> {
        self.list()
            .into_iter()
            .filter(|j| !j.phase.finished())
            .collect()
    }

    pub fn put(&self, job: MountJob) {
        let mut jobs = self.jobs.lock().unwrap();
        jobs.insert(job.name.clone(), job);
        self.persist(&jobs);
    }

    /// Move a job on. A reason is kept only for a failure.
    pub fn advance(&self, name: &str, phase: MountPhase, reason: Option<String>) {
        let mut jobs = self.jobs.lock().unwrap();
        if let Some(job) = jobs.get_mut(name) {
            job.phase = phase;
            job.reason = reason;
            job.since_unixtime = now_unix();
        }
        self.persist(&jobs);
    }

    pub fn forget(&self, name: &str) {
        let mut jobs = self.jobs.lock().unwrap();
        if jobs.remove(name).is_some() {
            self.persist(&jobs);
        }
    }

    /// Best-effort, as `mounts.toml` is: the job has happened in this process whether or not the
    /// file could be written, and the warning says a restart will not know about it.
    fn persist(&self, jobs: &BTreeMap<String, MountJob>) {
        let kept: Vec<&MountJob> = jobs
            .values()
            .filter(|j| j.phase != MountPhase::Live)
            .collect();
        if let Err(e) = write_atomically(&self.file, &kept) {
            tracing::warn!(
                "mount jobs changed but {} could not be written ({e:#}); a restart will not see them",
                self.file.display()
            );
        }
    }
}

fn write_atomically(file: &Path, jobs: &[&MountJob]) -> Result<()> {
    let tmp = file.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(jobs)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, file).with_context(|| format!("replacing {}", file.display()))?;
    Ok(())
}

/// Remove fetches a killed process left staged under `data/`. Only safe before any job runs.
pub fn clear_stale_fetches(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir.join(crate::runtime::DATA_DIR)) else {
        return;
    };
    for e in entries.flatten() {
        if e.file_name().to_string_lossy().starts_with(".fetch-") {
            if let Err(err) = std::fs::remove_dir_all(e.path()) {
                tracing::warn!("could not clear {}: {err}", e.path().display());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_restart_resumes_unfinished_jobs_reports_failed_ones_and_forgets_live_ones() {
        let d = tempfile::tempdir().unwrap();
        let jobs = MountJobs::load(d.path());
        jobs.put(MountJob::new("a", Some("aa"), MountPhase::Fetching));
        jobs.put(MountJob::new("b", Some("bb"), MountPhase::Accepted));
        jobs.advance("b", MountPhase::Failed, Some("no such nid".into()));
        jobs.put(MountJob::new("c", None, MountPhase::Live));

        let again = MountJobs::load(d.path());
        let names: Vec<String> = again.unfinished().into_iter().map(|j| j.name).collect();
        assert_eq!(names, vec!["a".to_string()]);
        let b = again.get("b").expect("a failed job is still reported");
        assert_eq!(b.phase, MountPhase::Failed);
        assert_eq!(b.reason.as_deref(), Some("no such nid"));
        assert!(
            again.get("c").is_none(),
            "a live mount belongs to mounts.toml, not here"
        );

        again.forget("b");
        assert!(MountJobs::load(d.path()).get("b").is_none());
    }

    #[test]
    fn an_unreadable_jobs_file_does_not_stop_the_runtime() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join(JOBS_FILE), b"{not json").unwrap();
        assert!(MountJobs::load(d.path()).list().is_empty());
    }

    #[test]
    fn stale_fetches_are_cleared_and_datasets_are_not() {
        let d = tempfile::tempdir().unwrap();
        let data = d.path().join(crate::runtime::DATA_DIR);
        std::fs::create_dir_all(data.join(".fetch-abc/nest")).unwrap();
        std::fs::create_dir_all(data.join("aa".repeat(32))).unwrap();
        clear_stale_fetches(d.path());
        assert!(!data.join(".fetch-abc").exists());
        assert!(data.join("aa".repeat(32)).is_dir());
    }
}
