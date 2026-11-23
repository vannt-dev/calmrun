//! Background tasks of the MCP server. `run_task` hands its job to a worker thread that
//! lives until the job ends, and answers within `wait_seconds`: with the result when the job
//! finished by then, else with a task id that `task_status` and `cancel_task` follow. Tasks
//! live in the server process: when it exits they are cancelled (their process trees
//! stopped), and if it is killed, the queue drops its entries like any dead owner's.

use crate::queue::{now_ms, Outcome};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How long `run_task` waits for the job before it answers with a task id.
pub const DEFAULT_WAIT: Duration = Duration::from_secs(25);
/// Finished tasks kept for `task_status`.
const KEEP_FINISHED: usize = 100;
/// Most output one `task_status` returns (the newest part when there is more).
pub const OUTPUT_MAX_BYTES: u64 = 32 * 1024;
/// How often a waiting caller looks at its own cancel flag.
const CHECK_EVERY: Duration = Duration::from_millis(100);

/// Where a task is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Waiting,
    Running,
    Done,
}

impl Phase {
    pub fn name(self) -> &'static str {
        match self {
            Phase::Waiting => "waiting",
            Phase::Running => "running",
            Phase::Done => "done",
        }
    }
}

#[derive(Debug, Default)]
struct Inner {
    /// The job's id in the queue, once it has its entry.
    job_id: Option<u64>,
    /// The latest waiting line.
    line: Option<String>,
    /// When the command started (ms since the epoch).
    started_ms: Option<u64>,
    outcome: Option<Outcome>,
    /// One-off notes for the agent (an expired ticket, a disk warning, …).
    notes: Vec<String>,
}

/// What a task looks like right now.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub phase: Phase,
    pub job_id: Option<u64>,
    pub line: Option<String>,
    pub created_ms: u64,
    pub started_ms: Option<u64>,
    pub outcome: Option<Outcome>,
    pub notes: Vec<String>,
}

impl Snapshot {
    /// Time spent waiting so far (or in total, once started).
    pub fn wait_ms(&self, now: u64) -> u64 {
        match (&self.outcome, self.started_ms) {
            (Some(o), _) => o.wait_ms,
            (None, Some(s)) => s.saturating_sub(self.created_ms),
            (None, None) => now.saturating_sub(self.created_ms),
        }
    }

    /// Time the command has run so far (or in total, once done).
    pub fn run_ms(&self, now: u64) -> u64 {
        match (&self.outcome, self.started_ms) {
            (Some(o), _) => o.run_ms,
            (None, Some(s)) => now.saturating_sub(s),
            (None, None) => 0,
        }
    }
}

/// One job handed to a worker thread.
pub struct Task {
    pub id: String,
    /// The queue it waits in (`None`: the machine-wide one).
    pub queue: Option<String>,
    /// Its captured output.
    pub log: PathBuf,
    /// Set to leave the queue or stop the command (its whole process tree).
    pub cancel: Arc<AtomicBool>,
    created_ms: u64,
    inner: Mutex<Inner>,
    changed: Condvar,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl Task {
    fn update(&self, f: impl FnOnce(&mut Inner)) {
        if let Ok(mut i) = self.inner.lock() {
            f(&mut i);
        }
        self.changed.notify_all();
    }

    pub fn set_job(&self, id: u64) {
        self.update(|i| i.job_id = Some(id));
    }

    pub fn set_line(&self, line: &str) {
        self.update(|i| i.line = Some(line.to_string()));
    }

    pub fn add_note(&self, line: &str) {
        self.update(|i| i.notes.push(line.to_string()));
    }

    pub fn set_started(&self) {
        self.update(|i| i.started_ms = Some(now_ms()));
    }

    fn finish(&self, o: Outcome) {
        self.update(|i| i.outcome = Some(o));
    }

    pub fn snapshot(&self) -> Snapshot {
        let i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let phase = match (&i.outcome, i.started_ms) {
            (Some(_), _) => Phase::Done,
            (None, Some(_)) => Phase::Running,
            (None, None) => Phase::Waiting,
        };
        Snapshot {
            phase,
            job_id: i.job_id,
            line: i.line.clone(),
            created_ms: self.created_ms,
            started_ms: i.started_ms,
            outcome: i.outcome.clone(),
            notes: i.notes.clone(),
        }
    }

    pub fn is_done(&self) -> bool {
        self.inner.lock().map_or(true, |i| i.outcome.is_some())
    }

    /// Wait until the task is done, `limit` passes, or `stop` says to give up. Returns
    /// whether it is done.
    pub fn wait(&self, limit: Duration, stop: &dyn Fn() -> bool) -> bool {
        let deadline = Instant::now() + limit;
        let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if i.outcome.is_some() {
                return true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || stop() {
                return false;
            }
            i = match self.changed.wait_timeout(i, left.min(CHECK_EVERY)) {
                Ok((g, _)) => g,
                Err(e) => e.into_inner().0,
            };
        }
    }
}

/// The tasks of one server.
#[derive(Default)]
pub struct Tasks {
    list: Mutex<Vec<Arc<Task>>>,
}

impl Tasks {
    /// Start `run` on a worker thread that lives until the job ends (on Linux the command's
    /// parent-death signal is tied to the thread that spawned it).
    pub fn start(
        &self,
        queue: Option<String>,
        log: PathBuf,
        cancel: Arc<AtomicBool>,
        run: impl FnOnce(Arc<Task>) -> Outcome + Send + 'static,
    ) -> Arc<Task> {
        let mut list = self.list.lock().unwrap_or_else(|e| e.into_inner());
        let id = format!(
            "t{}",
            crate::lease::new_id(|id| list.iter().any(|t| t.id[1..] == *id))
        );
        let task = Arc::new(Task {
            id,
            queue,
            log,
            cancel,
            created_ms: now_ms(),
            inner: Mutex::default(),
            changed: Condvar::new(),
            worker: Mutex::new(None),
        });
        let t = task.clone();
        let handle = std::thread::spawn(move || {
            let o = run(t.clone());
            t.finish(o);
        });
        if let Ok(mut w) = task.worker.lock() {
            *w = Some(handle);
        }
        list.push(task.clone());
        forget_old(&mut list);
        task
    }

    pub fn get(&self, id: &str) -> Option<Arc<Task>> {
        let list = self.list.lock().ok()?;
        list.iter().find(|t| t.id == id.trim()).cloned()
    }

    /// Ask every unfinished task to stop.
    pub fn cancel_all(&self) {
        if let Ok(list) = self.list.lock() {
            list.iter().for_each(|t| t.cancel.store(true, Ordering::SeqCst));
        }
    }

    pub fn all_done(&self) -> bool {
        self.list.lock().map_or(true, |l| l.iter().all(|t| t.is_done()))
    }

    /// Wait for every worker thread to end.
    pub fn join_all(&self) {
        let handles: Vec<JoinHandle<()>> = match self.list.lock() {
            Ok(list) => list
                .iter()
                .filter_map(|t| t.worker.lock().ok().and_then(|mut w| w.take()))
                .collect(),
            Err(_) => Vec::new(),
        };
        for h in handles {
            let _ = h.join();
        }
    }
}

/// Keep at most [`KEEP_FINISHED`] finished tasks (the newest); unfinished ones always stay.
fn forget_old(list: &mut Vec<Arc<Task>>) {
    let finished = list.iter().filter(|t| t.is_done()).count();
    let mut extra = finished.saturating_sub(KEEP_FINISHED);
    list.retain(|t| {
        if extra > 0 && t.is_done() {
            extra -= 1;
            return false;
        }
        true
    });
}

/// Output written since `offset`: at most `max` bytes (the newest), the offset to ask from
/// next time, and how many bytes were skipped to stay within `max`. A log that does not exist
/// yet (the command has not started) reads as empty.
pub fn read_output(path: &Path, offset: u64, max: u64) -> (String, u64, u64) {
    let Ok(mut f) = std::fs::File::open(path) else {
        return (String::new(), offset, 0);
    };
    let len = f.metadata().map_or(0, |m| m.len());
    let offset = offset.min(len);
    let start = offset.max(len.saturating_sub(max));
    let mut buf = Vec::new();
    if f.seek(SeekFrom::Start(start)).is_ok() {
        let _ = (&mut f).take(len - start).read_to_end(&mut buf);
    }
    // Start on a whole character when the cut fell inside one.
    let skip = if start > offset {
        buf.iter().take(3).take_while(|b| (**b & 0xC0) == 0x80).count()
    } else {
        0
    };
    let text = String::from_utf8_lossy(&buf[skip..]).to_string();
    (text, start + buf.len() as u64, start - offset + skip as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_since_an_offset_is_bounded_to_the_newest_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("a.log");
        assert_eq!(read_output(&f, 0, 10), (String::new(), 0, 0));
        std::fs::write(&f, "0123456789abcdef").unwrap();
        assert_eq!(read_output(&f, 0, 100), ("0123456789abcdef".into(), 16, 0));
        assert_eq!(read_output(&f, 10, 100), ("abcdef".into(), 16, 0));
        // More than the cap: only the newest bytes, and the skipped count says so.
        assert_eq!(read_output(&f, 2, 4), ("cdef".into(), 16, 10));
        // An offset past the end (a log replaced by hand) reads nothing.
        assert_eq!(read_output(&f, 99, 4), (String::new(), 16, 0));
        // A cut inside a multi-byte character starts at the next whole one.
        std::fs::write(&f, "a\u{e9}").unwrap();
        assert_eq!(read_output(&f, 0, 1), (String::new(), 3, 3));
    }

    #[test]
    fn a_task_reports_its_phases_and_can_be_waited_for() {
        let tasks = Tasks::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let gate = Arc::new(AtomicBool::new(false));
        let g = gate.clone();
        let t = tasks.start(None, PathBuf::from("x.log"), cancel, move |t| {
            t.set_job(7);
            t.set_line("waiting");
            while !g.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
            t.set_started();
            Outcome {
                code: 3,
                ..Outcome::default()
            }
        });
        assert!(t.id.starts_with('t') && t.id.len() == 7, "{}", t.id);
        assert!(!t.wait(Duration::from_millis(50), &|| false));
        assert_eq!(t.snapshot().phase, Phase::Waiting);
        assert!(!t.wait(Duration::from_secs(5), &|| true), "stop ends the wait");
        gate.store(true, Ordering::SeqCst);
        assert!(t.wait(Duration::from_secs(5), &|| false));
        let s = t.snapshot();
        assert_eq!(
            (s.phase, s.job_id, s.outcome.map(|o| o.code)),
            (Phase::Done, Some(7), Some(3))
        );
        assert!(tasks.get(&t.id).is_some());
        assert!(tasks.all_done());
        tasks.join_all();
    }
}
