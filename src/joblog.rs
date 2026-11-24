//! Job logs: where they live, copying a command's output to the terminal and a log at once
//! (`calmrun --log`), and noticing when a log last grew (`output_ms` in the state).
//!
//! MCP tasks write their output straight into the log file. A CLI run with `--log` keeps
//! the terminal: its stdout and stderr go through pipes, and one thread per stream copies
//! every chunk to the matching terminal stream and to the log as it arrives.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Log files kept in `logs/` (the newest).
pub const KEEP_LOGS: usize = 200;
/// After the command exits, how long the copy threads may still read the pipes. A process
/// the command left running (a daemon) may hold them open; calmrun does not wait for it.
/// Time spent waiting for our own reader (a paused `| less`) does not count.
const DRAIN_GRACE: Duration = Duration::from_secs(2);
/// How often `finish` looks at the copy threads.
const DRAIN_CHECK: Duration = Duration::from_millis(20);
/// With no memory sampling to ride along, the log is looked at this often.
const OUTPUT_CHECK_EVERY: Duration = Duration::from_secs(5);
/// Bytes read from a pipe at a time.
const CHUNK: usize = 8 * 1024;

static LOG_SEQ: AtomicU64 = AtomicU64::new(0);

/// A new log file path in `<home>/logs` (not created yet). Old logs past [`KEEP_LOGS`] are
/// removed first.
pub fn new_log(home: &Path) -> PathBuf {
    let dir = crate::queue::log_dir(home);
    crate::queue::prune_logs(&dir, KEEP_LOGS);
    dir.join(format!(
        "{}-{}-{}.log",
        crate::queue::now_ms(),
        std::process::id(),
        LOG_SEQ.fetch_add(1, Ordering::SeqCst)
    ))
}

/// Copies of the command's stdout and stderr, running while it runs.
pub struct Tee {
    done: mpsc::Receiver<()>,
    threads: usize,
    /// Copy threads inside a terminal write right now.
    writing: Arc<AtomicUsize>,
}

impl Tee {
    /// Point `cmd`'s stdout and stderr at pipes and start copying them to our own stdout and
    /// stderr and to a new file at `log`. stdin is left as it is.
    pub fn attach(cmd: &mut Command, log: &Path) -> io::Result<Tee> {
        if let Some(parent) = log.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = Arc::new(Mutex::new(File::create(log)?));
        let (out_r, out_w) = io::pipe()?;
        let (err_r, err_w) = io::pipe()?;
        cmd.stdout(out_w).stderr(err_w);
        let (tx, done) = mpsc::channel();
        let writing = Arc::new(AtomicUsize::new(0));
        let streams: [(io::PipeReader, Box<dyn Write + Send>, bool); 2] = [
            (
                out_r,
                Box::new(io::stdout()),
                io::IsTerminal::is_terminal(&io::stdout()),
            ),
            (
                err_r,
                Box::new(io::stderr()),
                io::IsTerminal::is_terminal(&io::stderr()),
            ),
        ];
        for (reader, term, tty) in streams {
            let (file, tx, busy) = (file.clone(), tx.clone(), writing.clone());
            std::thread::spawn(move || {
                copy(reader, Terminal::new(term, tty), &file, &busy);
                let _ = tx.send(());
            });
        }
        Ok(Tee {
            done,
            threads: 2,
            writing,
        })
    }

    /// Wait for the copies to reach the end of the output. While a copy waits for our own
    /// reader the wait goes on (without `--log` the command would wait the same way); only
    /// the time spent reading counts toward a short grace, so a daemon holding the pipes
    /// does not keep calmrun.
    pub fn finish(self) {
        let mut left = self.threads;
        let mut reading = Duration::ZERO;
        while left > 0 && reading < DRAIN_GRACE {
            let t = Instant::now();
            if self.done.recv_timeout(DRAIN_CHECK).is_ok() {
                left -= 1;
            }
            if self.writing.load(Ordering::SeqCst) == 0 {
                reading += t.elapsed();
            }
        }
    }
}

/// Copy one stream until it ends. A failing terminal (a closed pipe after `| head`) or log
/// never stops the draining: the command would block on a full pipe.
fn copy(mut reader: io::PipeReader, mut term: Terminal, file: &Mutex<File>, writing: &AtomicUsize) {
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        };
        // The log first: it must not lose what a slow terminal holds back.
        if let Ok(mut f) = file.lock() {
            let _ = f.write_all(&buf[..n]);
        }
        writing.fetch_add(1, Ordering::SeqCst);
        term.write(&buf[..n]);
        writing.fetch_sub(1, Ordering::SeqCst);
    }
}

/// One of our terminal streams, written chunk by chunk and flushed each time.
pub struct Terminal {
    out: Option<Box<dyn Write + Send>>,
    /// On a Windows console, text is decoded as UTF-8 (Rust's console writer accepts
    /// nothing else); the incomplete end of a character waits here for the next chunk.
    decode: Option<Vec<u8>>,
}

impl Terminal {
    /// `tty`: the stream is a terminal (only matters on Windows).
    pub fn new(out: Box<dyn Write + Send>, tty: bool) -> Terminal {
        Terminal {
            out: Some(out),
            decode: (cfg!(windows) && tty).then(Vec::new),
        }
    }

    /// Our stdout.
    pub fn stdout() -> Terminal {
        Terminal::new(Box::new(io::stdout()), io::IsTerminal::is_terminal(&io::stdout()))
    }

    /// Write and flush; after the first failure the stream is dropped.
    pub fn write(&mut self, bytes: &[u8]) {
        let text;
        let bytes = match &mut self.decode {
            Some(carry) => {
                text = utf8_lossy_carry(carry, bytes);
                text.as_bytes()
            }
            None => bytes,
        };
        if let Some(out) = &mut self.out {
            if out.write_all(bytes).and_then(|_| out.flush()).is_err() {
                self.out = None;
            }
        }
    }
}

/// Decode `carry` + `chunk` as UTF-8, replacing invalid bytes; an incomplete character at the
/// end stays in `carry` for the next chunk.
fn utf8_lossy_carry(carry: &mut Vec<u8>, chunk: &[u8]) -> String {
    carry.extend_from_slice(chunk);
    let mut out = String::new();
    let mut rest: &[u8] = carry;
    loop {
        match std::str::from_utf8(rest) {
            Ok(s) => {
                out.push_str(s);
                rest = &[];
                break;
            }
            Err(e) => {
                let (valid, after) = rest.split_at(e.valid_up_to());
                out.push_str(std::str::from_utf8(valid).unwrap_or_default());
                match e.error_len() {
                    Some(n) => {
                        out.push(char::REPLACEMENT_CHARACTER);
                        rest = &after[n..];
                    }
                    None => {
                        rest = after;
                        break;
                    }
                }
            }
        }
    }
    *carry = rest.to_vec();
    out
}

/// Watches a running job's log for new output.
pub struct Output {
    path: PathBuf,
    len: u64,
    /// When the log last grew (ms since the epoch).
    seen_ms: Option<u64>,
    /// What was last written to the state.
    published_ms: Option<u64>,
    checked: Option<Instant>,
}

impl Output {
    pub fn new(path: &Path) -> Output {
        Output {
            path: path.to_path_buf(),
            len: 0,
            seen_ms: None,
            published_ms: None,
            checked: None,
        }
    }

    /// Look at the log now. Returns the time of the latest output when it is newer than what
    /// was last returned (so the caller writes the state only then).
    pub fn poll(&mut self) -> Option<u64> {
        self.checked = Some(Instant::now());
        let len = std::fs::metadata(&self.path).map_or(0, |m| m.len());
        if len > self.len {
            self.len = len;
            self.seen_ms = Some(crate::queue::now_ms());
        }
        if self.seen_ms == self.published_ms {
            return None;
        }
        self.published_ms = self.seen_ms;
        self.seen_ms
    }

    /// [`Output::poll`], at most every few seconds (for when nothing else writes the state).
    pub fn poll_now_and_then(&mut self) -> Option<u64> {
        if self.checked.is_some_and(|t| t.elapsed() < OUTPUT_CHECK_EVERY) {
            return None;
        }
        self.poll()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_split_across_chunks_is_kept_whole() {
        let mut carry = Vec::new();
        let s = "a\u{e9}\u{2713}b".as_bytes();
        // Cut inside the two-byte and the three-byte character.
        assert_eq!(utf8_lossy_carry(&mut carry, &s[..2]), "a");
        assert_eq!(utf8_lossy_carry(&mut carry, &s[2..5]), "\u{e9}");
        assert_eq!(utf8_lossy_carry(&mut carry, &s[5..]), "\u{2713}b");
        assert!(carry.is_empty());
        // Invalid bytes become the replacement character, the rest stays.
        assert_eq!(utf8_lossy_carry(&mut carry, b"x\xffy"), "x\u{fffd}y");
    }

    #[test]
    fn output_reports_growth_once() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("a.log");
        let mut o = Output::new(&f);
        assert_eq!(o.poll(), None, "no file yet");
        std::fs::write(&f, "x").unwrap();
        let t = o.poll().expect("grew");
        assert_eq!(o.poll(), None, "no new output");
        assert_eq!(o.poll_now_and_then(), None, "checked just now");
        std::thread::sleep(Duration::from_millis(5));
        std::fs::write(&f, "xy").unwrap();
        assert!(o.poll().unwrap() >= t);
    }

    #[test]
    fn new_logs_are_distinct_and_in_logs() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (new_log(tmp.path()), new_log(tmp.path()));
        assert_ne!(a, b);
        assert_eq!(a.parent().unwrap(), tmp.path().join("logs"));
    }
}
