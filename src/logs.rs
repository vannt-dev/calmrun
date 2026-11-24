//! `calmrun logs <id> [--follow] [--tail N]`: the output of a job that has a log (a run with
//! `--log`, or an MCP task), and the hints that point to it in `status` and waiting lines.
//! Read-only: it never changes the queue.

use crate::cli::LogsArgs;
use crate::history::{self, Record};
use crate::joblog::Terminal;
use crate::queue::Ctx;
use crate::state::{Job, State, Store};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How often `--follow` looks for new output.
const FOLLOW_EVERY: Duration = Duration::from_millis(250);
/// How often `--follow` checks that the job still runs (and its owner lives).
const ALIVE_EVERY: Duration = Duration::from_secs(1);
/// Bytes read at a time when looking for the last lines.
const BLOCK: u64 = 64 * 1024;

/// `calmrun logs 5`, with `--id q` for a separate queue.
fn command_for(id: u64, queue: Option<&str>) -> String {
    match queue {
        Some(q) => format!("calmrun logs {id} --id {q}"),
        None => format!("calmrun logs {id}"),
    }
}

/// The extra `status` line under a running job that has a log.
pub fn status_line(j: &Job, now: u64, queue: Option<&str>) -> Option<String> {
    let log = j.log_path.as_deref()?;
    let output = match j.output_ms {
        Some(t) => format!("last output {} ago", history::human_ms(now.saturating_sub(t))),
        None => "no output yet".to_string(),
    };
    Some(format!(
        "log: {log}, {output} ({} --follow)",
        command_for(j.id, queue)
    ))
}

/// Appended to a waiting line when a running job has a log: how to watch it. Empty otherwise,
/// so the line stays as it was.
pub fn waiting_hint(s: &State, queue: Option<&str>) -> String {
    s.jobs
        .iter()
        .filter(|j| j.running() && j.log_path.is_some())
        .min_by_key(|j| j.started_ms)
        .map_or_else(String::new, |j| {
            format!("; output of {}: {}", j.label, command_for(j.id, queue))
        })
}

/// What `logs` shows.
enum Target {
    /// A job in the queue (running, or waiting to start).
    Live(Box<Job>),
    /// A finished run whose log may still exist.
    Finished {
        id: Option<u64>,
        label: String,
        log: PathBuf,
    },
}

/// `calmrun logs ...`.
pub fn run(mut ctx: Ctx, a: &LogsArgs) -> i32 {
    let store = match ctx.store() {
        Ok(s) => s,
        Err(e) => return crate::commands::fail(&e),
    };
    // Read without the lock and without pruning: `logs` never changes the queue. Entries
    // whose owner died are left out here instead.
    let mut state = store.peek();
    let pairs: Vec<(u32, u64)> = state.jobs.iter().map(|j| (j.pid, j.pid_start)).collect();
    let alive = ctx.sys.alive_set(&pairs);
    let mut alive = alive.into_iter();
    state.jobs.retain(|_| alive.next().unwrap_or(true));
    let queue = ctx.queue.clone();
    let target = match find(&state, a, &store, queue.as_deref()) {
        Ok(t) => t,
        Err(code) => return code,
    };
    match target {
        Target::Finished { id, label, log } => {
            let name = id.map_or_else(|| label.clone(), |i| format!("job {i} ({label})"));
            if !log.is_file() {
                eprintln!(
                    "calmrun: the log of {name} is gone (only the newest {} logs are kept)",
                    crate::joblog::KEEP_LOGS
                );
                return 1;
            }
            eprintln!("calmrun: {name} has ended; its log: {}", log.display());
            print_tail(&log, a.tail);
            0
        }
        Target::Live(job) => live(&mut ctx, &store, *job, a, queue.as_deref()),
    }
}

/// A job still in the queue: print its tail, then follow it while it runs.
fn live(ctx: &mut Ctx, store: &Store, mut job: Job, a: &LogsArgs, queue: Option<&str>) -> i32 {
    let name = format!("job {} ({})", job.id, job.label);
    if !job.running() {
        if !a.follow {
            eprintln!(
                "calmrun: {name} has not started yet, so it has no output; `{} --follow` waits for it",
                command_for(job.id, queue)
            );
            return 1;
        }
        eprintln!("calmrun: {name} has not started yet; waiting for it");
        match wait_until_started(ctx, store, &job) {
            Some(j) => job = j,
            None => {
                eprintln!("calmrun: {name} left the queue without starting");
                return 1;
            }
        }
    }
    let Some(log) = job.log_path.clone().map(PathBuf::from) else {
        eprintln!(
            "calmrun: {name} has no log: it runs with the terminal as its output. To keep a log, \
             run it with `calmrun --log -- <command>` (or set `log = true` in the config), or \
             through the MCP tool run_task"
        );
        return 1;
    };
    eprintln!("calmrun: {name}: {}", log.display());
    let offset = print_tail(&log, a.tail);
    if !a.follow {
        return 0;
    }
    follow(ctx, store, &job, &log, offset);
    eprintln!("calmrun: {name} has ended");
    0
}

/// Pick the job: by id, or by a label that matches exactly one job. Finished runs are
/// found in the history. `Err` is the exit code after the message.
fn find(state: &State, a: &LogsArgs, store: &Store, queue: Option<&str>) -> Result<Target, i32> {
    let in_queue = |j: &&Job| !j.parked() && !j.is_hold();
    let finished = |pick: &dyn Fn(&Record) -> bool| {
        newest_finished(store, pick).map(|r| Target::Finished {
            id: r.id,
            label: r.label.clone(),
            log: PathBuf::from(r.log.unwrap_or_default()),
        })
    };
    if let Some(id) = a.job {
        if let Some(j) = state.jobs.iter().filter(in_queue).find(|j| j.id == id) {
            return Ok(Target::Live(Box::new(j.clone())));
        }
        return finished(&|r| r.id == Some(id)).ok_or_else(|| {
            eprintln!(
                "calmrun: no job {id} in {}; `calmrun status{}` lists the jobs and their ids",
                queue.map_or_else(|| "the queue".to_string(), |q| format!("queue {q}")),
                queue.map_or_else(String::new, |q| format!(" --id {q}"))
            );
            1
        });
    }
    let needle = a.label.clone().unwrap_or_default().to_lowercase();
    let matches: Vec<&Job> = state
        .jobs
        .iter()
        .filter(in_queue)
        .filter(|j| j.label.to_lowercase().contains(&needle))
        .collect();
    // A label that is exactly the text wins over labels that only contain it.
    let exact: Vec<&Job> = matches
        .iter()
        .copied()
        .filter(|j| j.label.to_lowercase() == needle)
        .collect();
    let matches = if exact.len() == 1 { exact } else { matches };
    match matches.as_slice() {
        [j] => Ok(Target::Live(Box::new((*j).clone()))),
        [] => finished(&|r| r.label.to_lowercase().contains(&needle)).ok_or_else(|| {
            eprintln!("calmrun: no job matches \"{needle}\"");
            list(state.jobs.iter().filter(in_queue).collect(), queue);
            2
        }),
        many => {
            eprintln!(
                "calmrun: {} jobs match \"{needle}\"; pick one by its id:",
                many.len()
            );
            list(many.to_vec(), queue);
            Err(2)
        }
    }
}

/// The candidates, one per line on stderr.
fn list(jobs: Vec<&Job>, queue: Option<&str>) {
    for j in jobs {
        let what = match (j.running(), j.log_path.is_some()) {
            (true, true) => "running, has a log",
            (true, false) => "running, no log",
            (false, _) => "waiting",
        };
        eprintln!("  {:<28} {} ({what})", command_for(j.id, queue), j.label);
    }
}

/// The newest finished run in this queue with a log that `pick` accepts.
fn newest_finished(store: &Store, pick: &dyn Fn(&Record) -> bool) -> Option<Record> {
    history::read_all(store.history_path())
        .lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<Record>(l).ok())
        .find(|r| r.log.is_some() && r.queue.as_deref() == store.queue() && pick(r))
}

/// Print the last `n` lines of `log`; returns the offset printed up to.
fn print_tail(log: &Path, n: usize) -> u64 {
    let Ok(mut f) = File::open(log) else {
        return 0;
    };
    let len = f.metadata().map_or(0, |m| m.len());
    let start = tail_start(&mut f, len, n);
    copy_range(&mut f, start, len, &mut Terminal::stdout());
    len
}

/// Where the last `n` lines of a file of `len` bytes start.
fn tail_start(f: &mut File, len: u64, n: usize) -> u64 {
    if n == 0 {
        return len;
    }
    let mut found = 0;
    let mut end = len;
    // A final newline ends the last line; it does not start another one.
    let mut skip_last = true;
    while end > 0 {
        let start = end.saturating_sub(BLOCK);
        let mut buf = vec![0u8; (end - start) as usize];
        if f.seek(SeekFrom::Start(start)).is_err() || f.read_exact(&mut buf).is_err() {
            return 0;
        }
        for (i, b) in buf.iter().enumerate().rev() {
            if skip_last {
                skip_last = false;
                if start + i as u64 == len - 1 && *b == b'\n' {
                    continue;
                }
            }
            if *b == b'\n' {
                found += 1;
                if found == n {
                    return start + i as u64 + 1;
                }
            }
        }
        end = start;
    }
    0
}

/// Print bytes `from..to` of `f`.
fn copy_range(f: &mut File, from: u64, to: u64, term: &mut Terminal) {
    if to <= from || f.seek(SeekFrom::Start(from)).is_err() {
        return;
    }
    let mut left = to - from;
    let mut buf = vec![0u8; BLOCK as usize];
    while left > 0 {
        let want = left.min(BLOCK) as usize;
        match f.read(&mut buf[..want]) {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                term.write(&buf[..n]);
                left -= n as u64;
            }
        }
    }
}

/// Poll until the waiting job starts; `None` when it leaves the queue first.
fn wait_until_started(ctx: &mut Ctx, store: &Store, job: &Job) -> Option<Job> {
    loop {
        let j = still_queued(ctx, store, job)?;
        if j.running() {
            return Some(j);
        }
        std::thread::sleep(ALIVE_EVERY);
    }
}

/// The job's entry if it is still in the queue and its owner lives.
fn still_queued(ctx: &mut Ctx, store: &Store, job: &Job) -> Option<Job> {
    let j = store
        .peek()
        .jobs
        .into_iter()
        .find(|j| j.id == job.id && j.pid == job.pid)?;
    let alive = ctx.sys.alive_set(&[(j.pid, j.pid_start)]);
    alive.first().copied().unwrap_or(false).then_some(j)
}

/// Print new output as it arrives until the job leaves the queue (or its owner dies).
fn follow(ctx: &mut Ctx, store: &Store, job: &Job, log: &Path, mut offset: u64) {
    let mut term = Terminal::stdout();
    let mut checked = Instant::now();
    loop {
        let ended = checked.elapsed() >= ALIVE_EVERY && {
            checked = Instant::now();
            still_queued(ctx, store, job).is_none_or(|j| !j.running())
        };
        if let Ok(mut f) = File::open(log) {
            let len = f.metadata().map_or(0, |m| m.len());
            // A shorter file was replaced; start again from its beginning.
            offset = offset.min(len);
            copy_range(&mut f, offset, len, &mut term);
            offset = len;
        }
        if ended {
            return;
        }
        std::thread::sleep(FOLLOW_EVERY);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tail(text: &str, n: usize) -> String {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("a.log");
        std::fs::write(&p, text).unwrap();
        let mut f = File::open(&p).unwrap();
        let start = tail_start(&mut f, text.len() as u64, n) as usize;
        text[start..].to_string()
    }

    #[test]
    fn tail_finds_the_last_lines() {
        assert_eq!(tail("a\nb\nc\n", 2), "b\nc\n");
        assert_eq!(tail("a\nb\nc", 2), "b\nc");
        assert_eq!(tail("a\nb\nc\n", 9), "a\nb\nc\n");
        assert_eq!(tail("a\nb\n", 0), "");
        assert_eq!(tail("", 3), "");
        assert_eq!(tail("\n\n", 1), "\n");
        // Across read blocks.
        let long: String = (0..30_000).map(|i| format!("line {i}\n")).collect();
        assert_eq!(tail(&long, 2), "line 29998\nline 29999\n");
    }

    #[test]
    fn hints_name_the_job_and_queue() {
        let mut s = State::default();
        assert_eq!(waiting_hint(&s, None), "");
        s.jobs.push(Job {
            id: 4,
            label: "npx vitest".into(),
            started_ms: Some(10),
            ..Job::default()
        });
        assert_eq!(waiting_hint(&s, None), "", "no log, no hint");
        s.jobs[0].log_path = Some("/l/a.log".into());
        assert_eq!(
            waiting_hint(&s, Some("e2e")),
            "; output of npx vitest: calmrun logs 4 --id e2e"
        );
        let j = &mut s.jobs[0];
        assert_eq!(
            status_line(j, 1_000, None).unwrap(),
            "log: /l/a.log, no output yet (calmrun logs 4 --follow)"
        );
        j.output_ms = Some(1_000);
        assert!(status_line(j, 361_000, None)
            .unwrap()
            .contains("last output 6m00s ago"));
    }
}
