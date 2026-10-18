//! Crash-safe job persistence.
//!
//! Everything needed to resume a download lives in one JSON document per job, written
//! with [`crate::util::atomic_write`] (temp file + `fsync` + `rename`). A process kill
//! at any instant therefore leaves either the old state or the new state, never a
//! half-written one.

use std::path::{Path, PathBuf};

use crate::error::{Result, RiptideError};
use crate::job::Job;

#[derive(Clone, Debug)]
pub struct JobStore {
    dir: PathBuf,
}

impl JobStore {
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        Ok(JobStore { dir })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn path_for(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    pub fn save(&self, job: &Job) -> Result<()> {
        let data = serde_json::to_vec_pretty(job)?;
        crate::util::atomic_write(&self.path_for(&job.id), &data)?;
        Ok(())
    }

    pub fn load(&self, id: &str) -> Result<Job> {
        let path = self.path_for(id);
        let data = std::fs::read(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                RiptideError::NoSuchJob(id.to_string())
            } else {
                RiptideError::Io(e)
            }
        })?;
        Ok(serde_json::from_slice(&data)?)
    }

    /// Load every job in the store. Unparseable files are skipped rather than
    /// aborting the whole listing.
    pub fn list(&self) -> Result<Vec<Job>> {
        let mut jobs = Vec::new();
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(jobs),
            Err(e) => return Err(e.into()),
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            if let Ok(data) = std::fs::read(&p) {
                if let Ok(job) = serde_json::from_slice::<Job>(&data) {
                    jobs.push(job);
                }
            }
        }
        jobs.sort_by_key(|j| j.created_ms);
        Ok(jobs)
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        match std::fs::remove_file(self.path_for(id)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Jobs in a resumable/queued state, oldest first.
    pub fn pending(&self) -> Result<Vec<Job>> {
        Ok(self
            .list()?
            .into_iter()
            .filter(|j| {
                matches!(
                    j.state,
                    crate::job::JobState::Queued
                        | crate::job::JobState::Paused
                        | crate::job::JobState::Scheduled
                )
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{JobKind, JobState};

    #[test]
    fn round_trip_and_listing() {
        let dir = tempfile::tempdir().unwrap();
        let store = JobStore::open(dir.path()).unwrap();

        let mut a = Job::new(
            "aaa".into(),
            "http://x/a".into(),
            dir.path().join("a"),
            JobKind::Http,
        );
        a.total = Some(100);
        a.downloaded = 40;
        a.state = JobState::Done;
        let mut b = Job::new(
            "bbb".into(),
            "http://x/b".into(),
            dir.path().join("b"),
            JobKind::Hls,
        );
        b.state = JobState::Paused;

        store.save(&a).unwrap();
        store.save(&b).unwrap();

        let loaded = store.load("aaa").unwrap();
        assert_eq!(loaded.total, Some(100));
        assert_eq!(loaded.downloaded, 40);

        let all = store.list().unwrap();
        assert_eq!(all.len(), 2);

        let pending = store.pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "bbb");

        store.delete("aaa").unwrap();
        assert_eq!(store.list().unwrap().len(), 1);
    }

    #[test]
    fn missing_job_is_reported_as_such() {
        let dir = tempfile::tempdir().unwrap();
        let store = JobStore::open(dir.path()).unwrap();
        match store.load("nope") {
            Err(RiptideError::NoSuchJob(id)) => assert_eq!(id, "nope"),
            other => panic!("expected NoSuchJob, got {other:?}"),
        }
    }

    #[test]
    fn corrupt_files_do_not_break_listing() {
        let dir = tempfile::tempdir().unwrap();
        let store = JobStore::open(dir.path()).unwrap();
        let good = Job::new(
            "ok".into(),
            "http://x/".into(),
            dir.path().join("x"),
            JobKind::Http,
        );
        store.save(&good).unwrap();
        std::fs::write(dir.path().join("broken.json"), b"{ not json").unwrap();
        std::fs::write(dir.path().join("ignored.txt"), b"hi").unwrap();

        let all = store.list().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, "ok");
    }
}
