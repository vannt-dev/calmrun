//! Shared queue state: `state.json` guarded by an OS file lock on `state.lock`.

use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// One running or waiting job.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Job {
    pub id: u64,
    /// The calmrun process that owns the entry.
    pub pid: u32,
    /// That process's start time (seconds since the epoch, from the OS) to rule out pid reuse.
    pub pid_start: u64,
    pub label: String,
    pub kind: String,
    pub cost: u32,
    #[serde(default)]
    pub keys: Vec<String>,
    pub cwd: String,
    #[serde(default)]
    pub high: bool,
    /// Queue order; lower goes first. `bump` makes it smaller than every other entry.
    pub order: i64,
    pub queued_ms: u64,
    #[serde(default)]
    pub started_ms: Option<u64>,
    /// Child process id once started (for display).
    #[serde(default)]
    pub child_pid: Option<u32>,
}

impl Job {
    pub fn running(&self) -> bool {
        self.started_ms.is_some()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct State {
    #[serde(default)]
    pub next_id: u64,
    #[serde(default)]
    pub jobs: Vec<Job>,
}

impl State {
    pub fn next_order(&self) -> i64 {
        self.jobs
            .iter()
            .map(|j| j.order)
            .max()
            .map_or(0, |o| o + 1)
            .max(self.next_id as i64)
    }
}

pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn open(dir: &Path) -> io::Result<Store> {
        std::fs::create_dir_all(dir)?;
        Ok(Store {
            dir: dir.to_path_buf(),
        })
    }

    fn state_path(&self) -> PathBuf {
        self.dir.join("state.json")
    }

    pub fn history_path(&self) -> PathBuf {
        self.dir.join("history.jsonl")
    }

    /// Run `f` on the state with the lock held; writes the state back if `f` changed it.
    /// The lock is an OS lock (LockFileEx / flock), so a crashed holder frees it.
    pub fn with_lock<R>(&self, f: impl FnOnce(&mut State) -> R) -> io::Result<R> {
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.dir.join("state.lock"))?;
        lock.lock()?;
        let before = self.read();
        let mut state = before.clone();
        let out = f(&mut state);
        if state != before {
            self.write(&state)?;
        }
        drop(lock);
        Ok(out)
    }

    /// Read the state without the lock (for `status`; may be a moment stale).
    pub fn read(&self) -> State {
        match std::fs::read(self.state_path()) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(_) => State::default(),
        }
    }

    fn write(&self, state: &State) -> io::Result<()> {
        let tmp = self.dir.join(format!("state.json.{}.tmp", std::process::id()));
        {
            let mut f = File::create(&tmp)?;
            f.write_all(&serde_json::to_vec_pretty(state).map_err(io::Error::other)?)?;
            f.sync_all()?;
        }
        let target = self.state_path();
        let mut last = None;
        for attempt in 0..20 {
            match std::fs::rename(&tmp, &target) {
                Ok(()) => return Ok(()),
                // Windows: a reader or scanner may hold the target open for a moment.
                Err(e) => {
                    last = Some(e);
                    std::thread::sleep(Duration::from_millis(10 + attempt * 10));
                }
            }
        }
        let _ = std::fs::remove_file(&tmp);
        Err(last.unwrap_or_else(|| io::Error::other("rename failed")))
    }
}

/// Drop entries whose owner is gone (pid dead or reused by a different process).
pub fn prune(state: &mut State, alive: impl Fn(u32, u64) -> bool) -> usize {
    let before = state.jobs.len();
    state.jobs.retain(|j| alive(j.pid, j.pid_start));
    before - state.jobs.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(id: u64, pid: u32) -> Job {
        Job {
            id,
            pid,
            pid_start: 100,
            label: "x".into(),
            kind: "test".into(),
            cost: 1,
            keys: vec![],
            cwd: ".".into(),
            high: false,
            order: id as i64,
            queued_ms: 0,
            started_ms: None,
            child_pid: None,
        }
    }

    #[test]
    fn lock_roundtrip_and_prune() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        store
            .with_lock(|s| {
                s.jobs.push(job(1, 10));
                s.jobs.push(job(2, 20));
            })
            .unwrap();
        assert_eq!(store.read().jobs.len(), 2);
        let removed = store.with_lock(|s| prune(s, |pid, _| pid == 10)).unwrap();
        assert_eq!(removed, 1);
        assert_eq!(
            store.read().jobs.iter().map(|j| j.id).collect::<Vec<_>>(),
            vec![1]
        );
    }

    #[test]
    fn corrupt_state_reads_as_empty() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("state.json"), "{not json").unwrap();
        let store = Store::open(tmp.path()).unwrap();
        assert!(store.read().jobs.is_empty());
    }
}
