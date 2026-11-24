//! Shared queue state: `state.json` guarded by an OS file lock on `state.lock`.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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
    /// Set on a `calmrun hold` waiter: the lease's time to live. Such an entry runs nothing;
    /// once admitted it turns into a [`Lease`] in the same locked step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_ttl_ms: Option<u64>,
    /// The job's folder as a path key, matched against a lease's owning folder.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cwd_key: String,
    /// Leases whose resources this job may use (`CALMRUN_LEASE`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub use_leases: Vec<String>,
    // Fields added in 0.4. All additive with defaults: a 0.3.x binary still reads this state
    // (unknown fields are ignored) and only drops them when it rewrites the file.
    /// `--prio low`: after every normal and high waiter. `high` stays the 0.3 field.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub low: bool,
    /// Moved to the front by `calmrun bump`: later waiters may not pass it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bumped: bool,
    /// Time `calmrun stop` asked the owner to stop this job; the owner stops it and exits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_ms: Option<u64>,
    /// Run time limit of a running job (`--timeout` or the config), for display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// CPU time used so far by the running command's whole process tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_ms: Option<u64>,
    /// Last time the running tree used a noticeable share of CPU (an idle tree may hang).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub busy_ms: Option<u64>,
    /// Full output log of the running command, when calmrun keeps one (MCP tasks, `--log`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_path: Option<String>,
    /// Last time the running command wrote output to that log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_ms: Option<u64>,
    /// CPU the running tree used over the last sample, in percent of one core (`cpu_ms`
    /// alone gives no recent rate).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_pct: Option<u32>,
    // Fields for resources with instances, additive like the 0.4 ones.
    /// Published by a waiting owner: the ready instances of each of its needs that has
    /// instances (`[resources.android] instances`), in the order they are handed out.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub ready: BTreeMap<String, Vec<String>>,
    /// The instance of each such resource the job got when it started (its own, or the one
    /// of a lease it uses).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub instances: BTreeMap<String, String>,
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

    /// Whether this entry waits for a resource lease (`calmrun hold`) instead of a command.
    pub fn is_hold(&self) -> bool {
        self.hold_ttl_ms.is_some()
    }
}

/// Resource slots held without a running command (`calmrun hold`), until released or
/// expired. No process owns a lease: it ends with `calmrun release` or at `until_ms`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Lease {
    /// Short hex id printed by `calmrun hold`.
    pub id: String,
    /// The named resources it holds one slot of each.
    pub resources: Vec<String>,
    pub label: String,
    /// Budget points it holds (0 unless `--cost` was given).
    #[serde(default)]
    pub cost: u32,
    pub cwd: String,
    /// The process that asked for it (for display; the lease outlives it).
    #[serde(default)]
    pub pid: u32,
    pub granted_ms: u64,
    /// The lease lapses at this time unless renewed.
    pub until_ms: u64,
    /// The time to live it was granted with (the default for `--renew`).
    pub ttl_ms: u64,
    /// The folder that owns the lease (`hold --for`), as given: jobs inside it use the
    /// leased resources instead of waiting for them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub for_dir: Option<String>,
    /// That folder as a path key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub for_key: Option<String>,
    /// The instance it holds of each resource that has instances.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub instances: BTreeMap<String, String>,
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
    /// Resource leases (`calmrun hold`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub leases: Vec<Lease>,
    /// The settings the queue decides with, recorded by the first job that joined it while
    /// it was empty, so every waiter uses the same budget whatever its own env says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<Settings>,
}

/// The queue-wide settings in force (see [`State::settings`]).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct Settings {
    pub budget: u32,
    pub min_free_bytes: u64,
    pub pass_limit_ms: u64,
    /// The calmrun that recorded them.
    #[serde(default)]
    pub pid: u32,
    #[serde(default)]
    pub at_ms: u64,
}

impl Settings {
    /// Whether two recordings set the same limits (who and when do not matter).
    pub fn same_limits(&self, other: &Settings) -> bool {
        (self.budget, self.min_free_bytes, self.pass_limit_ms)
            == (other.budget, other.min_free_bytes, other.pass_limit_ms)
    }
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

/// Folder of the separate queues (`--id`) under the state directory.
pub const QUEUES_DIR: &str = "queues";
/// Taken around every admission in every queue, so exclusive keys and the RAM and disk
/// checks see what runs in the other queues.
const MACHINE_LOCK: &str = "machine.lock";

/// A queue's files: `state.json`, `state.lock`, and the shared `history.jsonl`.
pub struct Store {
    dir: PathBuf,
    home: PathBuf,
    queue: Option<String>,
    history: PathBuf,
}

/// Another queue's state as read for a scheduling pass.
#[derive(Debug, Clone, Default)]
pub struct OtherQueue {
    /// `None` is the default queue.
    pub queue: Option<String>,
    pub state: State,
}

impl Store {
    /// The default queue, in `dir`.
    pub fn open(dir: &Path) -> io::Result<Store> {
        std::fs::create_dir_all(dir)?;
        Ok(Store {
            dir: dir.to_path_buf(),
            home: dir.to_path_buf(),
            queue: None,
            history: dir.join("history.jsonl"),
        })
    }

    /// A separate queue (`--id`) with its own state, sharing the history file of `home`.
    pub fn open_queue(home: &Path, id: &str) -> io::Result<Store> {
        let dir = home.join(QUEUES_DIR).join(id);
        std::fs::create_dir_all(&dir)?;
        Ok(Store {
            dir,
            home: home.to_path_buf(),
            queue: Some(id.to_string()),
            history: home.join("history.jsonl"),
        })
    }

    /// The separate queue's id (`None`: the default queue).
    pub fn queue(&self) -> Option<&str> {
        self.queue.as_deref()
    }

    fn state_path(&self) -> PathBuf {
        self.dir.join("state.json")
    }

    /// Take the machine-wide admission lock (released when the file is dropped). Lock order:
    /// this one first, then a queue's `state.lock`, never the other way round.
    pub fn machine_lock(&self) -> io::Result<File> {
        let f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.home.join(MACHINE_LOCK))?;
        f.lock()?;
        Ok(f)
    }

    /// The states of every other queue under the same state directory (the default queue
    /// included when this is a separate one). A file that cannot be read is an error, never
    /// an empty queue; a corrupt one is skipped (its own queue sets it aside).
    pub fn others(&self) -> io::Result<Vec<OtherQueue>> {
        let mut paths = Vec::new();
        if self.queue.is_some() {
            paths.push((None, self.home.join("state.json")));
        }
        if let Ok(rd) = std::fs::read_dir(self.home.join(QUEUES_DIR)) {
            for e in rd.flatten() {
                let id = e.file_name().to_string_lossy().to_string();
                if self.queue.as_deref() != Some(id.as_str()) && e.path().is_dir() {
                    paths.push((Some(id), e.path().join("state.json")));
                }
            }
        }
        paths.sort_by(|a, b| a.0.cmp(&b.0));
        let mut queues = Vec::new();
        for (queue, path) in paths {
            let bytes = read_retrying(&path)?;
            if let Ok(state) = serde_json::from_slice::<State>(&bytes) {
                queues.push(OtherQueue { queue, state });
            }
        }
        Ok(queues)
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
        read_retrying(&self.state_path())
    }

    /// Run `f` with the lock held on the state file's path and its parsed contents (`None`
    /// when there is no file); nothing is written back.
    fn with_lock_raw<R>(&self, f: impl FnOnce(&Path, Option<State>) -> R) -> io::Result<R> {
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.dir.join("state.lock"))?;
        lock.lock()?;
        let bytes = self.read_bytes()?;
        let parsed = match bytes.is_empty() {
            true => None,
            false => Some(serde_json::from_slice(&bytes).map_err(io::Error::other)?),
        };
        let out = f(&self.state_path(), parsed);
        drop(lock);
        Ok(out)
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

/// A file's bytes (empty if it does not exist). Other read errors (a scanner holding the
/// file on Windows) are retried; if they persist the call fails.
fn read_retrying(path: &Path) -> io::Result<Vec<u8>> {
    let mut attempt = 0;
    loop {
        match std::fs::read(path) {
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

/// What [`prune`] took out.
#[derive(Debug, Default)]
pub struct Pruned {
    /// Entries whose owner died (killed while waiting or running): worth a history line.
    pub dead: Vec<Job>,
    /// Expired tickets and leases.
    pub expired: usize,
}

#[cfg(test)]
impl Pruned {
    /// Everything dropped.
    pub fn count(&self) -> usize {
        self.dead.len() + self.expired
    }
}

/// Drop entries whose owner is gone (pid dead or reused by a different process) and expired
/// leases. A parked entry has no owner; it stays until its ticket expires.
pub fn prune(state: &mut State, now_ms: u64, alive: impl Fn(u32, u64) -> bool) -> Pruned {
    let mut out = Pruned::default();
    for j in std::mem::take(&mut state.jobs) {
        match j.parked_until {
            Some(until) if until > now_ms => state.jobs.push(j),
            Some(_) => out.expired += 1,
            None if alive(j.pid, j.pid_start) => state.jobs.push(j),
            None => out.dead.push(j),
        }
    }
    let leases = state.leases.len();
    state.leases.retain(|l| l.until_ms > now_ms);
    out.expired += leases - state.leases.len();
    out
}

/// Remove separate queues (`queues/<id>`) that have been empty and unwritten for `idle_ms`:
/// no jobs, no leases. Best effort; returns the ids removed.
pub fn sweep_idle_queues(home: &Path, idle_ms: u64) -> Vec<String> {
    let mut removed = Vec::new();
    let Ok(rd) = std::fs::read_dir(home.join(QUEUES_DIR)) else {
        return removed;
    };
    let now = std::time::SystemTime::now();
    for e in rd.flatten() {
        let dir = e.path();
        let touched = std::fs::metadata(dir.join("state.json"))
            .or_else(|_| std::fs::metadata(&dir))
            .and_then(|m| m.modified());
        let idle = touched
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|d| d.as_millis() >= u128::from(idle_ms));
        let id = e.file_name().to_string_lossy().to_string();
        if !idle || !dir.is_dir() {
            continue;
        }
        let Ok(store) = Store::open_queue(home, &id) else {
            continue;
        };
        // Decided under the queue's lock: only an empty (or missing) state goes.
        let empty = store.with_lock_raw(|path, s| match s {
            None => true,
            Some(s) => s.jobs.is_empty() && s.leases.is_empty() && std::fs::remove_file(path).is_ok(),
        });
        if empty.unwrap_or(false) && std::fs::remove_dir_all(&dir).is_ok() {
            removed.push(id);
        }
    }
    removed
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
            hold_ttl_ms: None,
            cwd_key: String::new(),
            use_leases: Vec::new(),
            ..Default::default()
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
        assert_eq!(removed.dead.iter().map(|j| j.id).collect::<Vec<_>>(), vec![2]);
        store
            .with_lock(|s| {
                let mut parked = job(3, 30);
                parked.parked_until = Some(500);
                s.jobs.push(parked);
            })
            .unwrap();
        assert_eq!(
            store
                .with_lock(|s| prune(s, 400, |pid, _| pid == 10))
                .unwrap()
                .count(),
            0
        );
        let expired = store.with_lock(|s| prune(s, 600, |pid, _| pid == 10)).unwrap();
        // An expired ticket is not a killed job.
        assert_eq!((expired.expired, expired.dead.len()), (1, 0));
        assert_eq!(
            store
                .with_lock(|s| s.jobs.iter().map(|j| j.id).collect::<Vec<_>>())
                .unwrap(),
            vec![1]
        );
    }

    #[test]
    fn expired_leases_are_pruned_without_an_owner_check() {
        let mut s = State::default();
        for (id, until) in [("aaaaaa", 500), ("bbbbbb", 1_500)] {
            s.leases.push(Lease {
                id: id.into(),
                until_ms: until,
                ..Lease::default()
            });
        }
        // The lease's pid is never asked about: no process owns a lease.
        assert_eq!(prune(&mut s, 1_000, |_, _| panic!("no owner check")).expired, 1);
        assert_eq!(s.leases.len(), 1);
        assert_eq!(s.leases[0].id, "bbbbbb");
    }

    #[test]
    fn a_state_without_leases_still_reads_and_writes_the_same() {
        let old = r#"{"next_id": 3, "jobs": []}"#;
        let s: State = serde_json::from_str(old).unwrap();
        assert!(s.leases.is_empty());
        assert!(!serde_json::to_string(&s).unwrap().contains("leases"));
    }

    #[test]
    fn other_queues_are_read_and_old_empty_ones_swept() {
        let tmp = tempfile::tempdir().unwrap();
        let default = Store::open(tmp.path()).unwrap();
        let a = Store::open_queue(tmp.path(), "a").unwrap();
        let b = Store::open_queue(tmp.path(), "b").unwrap();
        default.with_lock(|s| s.jobs.push(job(1, 10))).unwrap();
        a.with_lock(|s| s.jobs.push(job(2, 20))).unwrap();
        let seen = |st: &Store| -> Vec<Option<String>> {
            st.others().unwrap().into_iter().map(|o| o.queue).collect()
        };
        assert_eq!(seen(&default), vec![Some("a".to_string())]);
        assert_eq!(seen(&a), vec![None]);
        assert_eq!(seen(&b), vec![None, Some("a".to_string())]);
        drop((a, b));
        // Nothing is old enough yet.
        assert!(sweep_idle_queues(tmp.path(), 60_000).is_empty());
        // With no idle time required, only the empty queue goes; "a" still has a job.
        assert_eq!(sweep_idle_queues(tmp.path(), 0), vec!["b".to_string()]);
        assert!(tmp.path().join("queues").join("a").is_dir());
        assert!(!tmp.path().join("queues").join("b").exists());
    }

    #[test]
    fn fields_added_in_0_4_keep_the_0_3_format() {
        // A 0.3 entry (none of the 0.4 fields) still parses, and an entry that leaves them at
        // their defaults is written exactly as 0.3 wrote it, so mixed versions share the file.
        let old = r#"{"id":7,"pid":1,"pid_start":2,"label":"npx vitest","kind":"test","cost":2,
            "keys":[],"cwd":"/r","high":false,"order":3,"queued_ms":10,"started_ms":null,
            "child_pid":null}"#;
        let job: Job = serde_json::from_str(old).unwrap();
        assert!(!job.low && !job.bumped);
        assert_eq!((job.stop_ms, job.timeout_ms, job.cpu_ms), (None, None, None));
        assert_eq!(
            (job.busy_ms, job.log_path.as_deref(), job.output_ms),
            (None, None, None)
        );
        let written = serde_json::to_value(&job).unwrap();
        for k in [
            "low",
            "bumped",
            "stop_ms",
            "timeout_ms",
            "cpu_ms",
            "busy_ms",
            "log_path",
        ] {
            assert!(written.get(k).is_none(), "{k} written at its default");
        }
        assert!(written.get("output_ms").is_none());
        assert!(written.get("cpu_pct").is_none());
        // Set fields survive a round trip.
        let set = Job {
            low: true,
            bumped: true,
            stop_ms: Some(5),
            log_path: Some("l".into()),
            timeout_ms: Some(60_000),
            cpu_ms: Some(1_500),
            busy_ms: Some(9),
            cpu_pct: Some(40),
            ..job
        };
        let back: Job = serde_json::from_str(&serde_json::to_string(&set).unwrap()).unwrap();
        assert_eq!(back, set);
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
