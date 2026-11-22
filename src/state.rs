//! Shared queue state: `state.json` guarded by an OS file lock on `state.lock`.

use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// One running or waiting job.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
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
    /// Set when the owner gave up waiting (`--wait-timeout`): the entry keeps its place for a
    /// re-run with `--ticket <id>` until this time, and is skipped by the scheduler meanwhile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parked_until: Option<u64>,
    /// Memory of the running command's whole process tree, sampled every few seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_bytes: Option<u64>,
    /// Set by a waiting owner while the drive of its cwd has less free space than required.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disk_short: bool,
    /// Named resources this job takes one slot of (`--needs db`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub needs: Vec<String>,
}

impl Job {
    /// Whether the job has started.
    pub fn running(&self) -> bool {
        self.started_ms.is_some()
    }

    /// Whether the job only holds a ticket.
    pub fn parked(&self) -> bool {
        self.parked_until.is_some()
    }
}

/// The contents of `state.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct State {
    #[serde(default)]
    pub next_id: u64,
    #[serde(default)]
    pub jobs: Vec<Job>,
    /// The last free-RAM reading, shared so every waiter decides on the same number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ram_sample: Option<RamSample>,
}

/// Free RAM as read by some calmrun at a moment.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct RamSample {
    pub free: u64,
    pub at_ms: u64,
}

impl State {
    /// An order value behind every existing entry.
    pub fn next_order(&self) -> i64 {
        self.jobs
            .iter()
            .map(|j| j.order)
            .max()
            .map_or(0, |o| o + 1)
            .max(self.next_id as i64)
    }
}

/// Rename retries when Windows briefly holds the target open (a reader or a scanner).
const RENAME_RETRIES: u64 = 20;
const RENAME_BACKOFF_MS: u64 = 10;
/// Attempts to read a state file that exists but cannot be read right now.
const READ_RETRIES: u64 = 20;

/// A queue's files: `state.json`, `state.lock`, and the shared `history.jsonl`.
pub struct Store {
    dir: PathBuf,
    history: PathBuf,
}

impl Store {
    /// The default queue, in `dir`.
    pub fn open(dir: &Path) -> io::Result<Store> {
        std::fs::create_dir_all(dir)?;
        Ok(Store {
            dir: dir.to_path_buf(),
            history: dir.join("history.jsonl"),
        })
    }

    /// A separate queue (`--id`) with its own state, sharing the history file of `home`.
    pub fn open_queue(home: &Path, id: &str) -> io::Result<Store> {
        let dir = home.join("queues").join(id);
        std::fs::create_dir_all(&dir)?;
        Ok(Store {
            dir,
            history: home.join("history.jsonl"),
        })
    }

    fn state_path(&self) -> PathBuf {
        self.dir.join("state.json")
    }

    /// The shared `history.jsonl`.
    pub fn history_path(&self) -> &Path {
        &self.history
    }

    /// The state without the lock, for work that must not happen under it (refreshing
    /// process information). May be a moment stale; empty on any error.
    pub fn peek(&self) -> State {
        std::fs::read(self.state_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// Run `f` on the state with the lock held; writes the state back only if `f` changed
    /// it. The state file is read once per call. The lock is an OS lock (LockFileEx /
    /// flock), so a crashed holder frees it. Never sleep or spawn inside `f`.
    pub fn with_lock<R>(&self, f: impl FnOnce(&mut State) -> R) -> io::Result<R> {
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.dir.join("state.lock"))?;
        lock.lock()?;
        let before = self.read_bytes()?;
        let mut state = self.parse_or_set_aside(&before);
        let out = f(&mut state);
        let after = serde_json::to_vec_pretty(&state).map_err(io::Error::other)?;
        if after != before {
            self.write(&after)?;
        }
        drop(lock);
        Ok(out)
    }

    /// The state file's bytes (empty if it does not exist). Other read errors (a scanner
    /// holding the file on Windows) are retried; if they persist the call fails instead of
    /// treating the queue as empty.
    fn read_bytes(&self) -> io::Result<Vec<u8>> {
        let mut attempt = 0;
        loop {
            match std::fs::read(self.state_path()) {
                Ok(b) => return Ok(b),
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(e) if attempt + 1 >= READ_RETRIES => return Err(e),
                Err(_) => {
                    attempt += 1;
                    std::thread::sleep(Duration::from_millis(RENAME_BACKOFF_MS * attempt));
                }
            }
        }
    }

    /// Parse the state. A corrupt file is moved aside (`state.json.corrupt-<ms>`) so it can
    /// be inspected, and the queue starts empty.
    fn parse_or_set_aside(&self, bytes: &[u8]) -> State {
        if bytes.is_empty() {
            return State::default();
        }
        match serde_json::from_slice(bytes) {
            Ok(s) => s,
            Err(e) => {
                let ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_millis());
                let aside = self.dir.join(format!("state.json.corrupt-{ms}"));
                let _ = std::fs::rename(self.state_path(), &aside);
                eprintln!(
                    "calmrun: warning: the queue state was unreadable ({e}); kept it as {}",
                    aside.display()
                );
                State::default()
            }
        }
    }

    fn write(&self, bytes: &[u8]) -> io::Result<()> {
        let tmp = self.dir.join(format!("state.json.{}.tmp", std::process::id()));
        {
            let mut f = File::create(&tmp)?;
            f.write_all(bytes)?;
            f.sync_all()?;
        }
        let target = self.state_path();
        let mut last = None;
        for attempt in 0..RENAME_RETRIES {
            match std::fs::rename(&tmp, &target) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    last = Some(e);
                    std::thread::sleep(Duration::from_millis(RENAME_BACKOFF_MS * (attempt + 1)));
                }
            }
        }
        let _ = std::fs::remove_file(&tmp);
        Err(last.unwrap_or_else(|| io::Error::other("rename failed")))
    }
}

/// Drop entries whose owner is gone (pid dead or reused by a different process). A parked
/// entry has no owner; it stays until its ticket expires.
pub fn prune(state: &mut State, now_ms: u64, alive: impl Fn(u32, u64) -> bool) -> usize {
    let before = state.jobs.len();
    state.jobs.retain(|j| match j.parked_until {
        Some(until) => until > now_ms,
        None => alive(j.pid, j.pid_start),
    });
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
            parked_until: None,
            mem_bytes: None,
            disk_short: false,
            needs: Vec::new(),
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
        assert_eq!(store.with_lock(|s| s.jobs.len()).unwrap(), 2);
        let removed = store.with_lock(|s| prune(s, 0, |pid, _| pid == 10)).unwrap();
        assert_eq!(removed, 1);
        store
            .with_lock(|s| {
                let mut parked = job(3, 30);
                parked.parked_until = Some(500);
                s.jobs.push(parked);
            })
            .unwrap();
        assert_eq!(store.with_lock(|s| prune(s, 400, |pid, _| pid == 10)).unwrap(), 0);
        assert_eq!(store.with_lock(|s| prune(s, 600, |pid, _| pid == 10)).unwrap(), 1);
        assert_eq!(
            store
                .with_lock(|s| s.jobs.iter().map(|j| j.id).collect::<Vec<_>>())
                .unwrap(),
            vec![1]
        );
    }

    #[test]
    fn corrupt_state_reads_as_empty() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("state.json"), "{not json").unwrap();
        let store = Store::open(tmp.path()).unwrap();
        assert!(store.with_lock(|s| s.jobs.is_empty()).unwrap());
        let kept: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("state.json.corrupt-"))
            .collect();
        assert_eq!(kept.len(), 1, "the corrupt file is kept for inspection");
        assert_eq!(std::fs::read_to_string(kept[0].path()).unwrap(), "{not json");
    }

    #[test]
    fn unreadable_state_is_an_error_not_an_empty_queue() {
        let tmp = tempfile::tempdir().unwrap();
        // A directory where the file should be: reading fails with something other than
        // "not found" on every OS.
        std::fs::create_dir_all(tmp.path().join("state.json")).unwrap();
        let store = Store::open(tmp.path()).unwrap();
        assert!(store.with_lock(|s| s.jobs.len()).is_err());
    }
}
