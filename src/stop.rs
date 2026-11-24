//! `calmrun stop`: ask the calmrun that owns a job to stop it, and end that owner directly
//! when it does not react in time (a calmrun older than 0.4 ignores the request; a process
//! may be wedged). CLI only: an agent must not clear other agents' jobs out of its way.

use crate::error::Result;
use crate::queue::{self, locked, now_ms, Ctx, Outcome};
use crate::state::{Job, State, Store};
use std::time::{Duration, Instant};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

/// How often `stop` looks whether the owner has acted, and renews its request.
const POLL: Duration = Duration::from_millis(100);
const RENEW_EVERY: Duration = Duration::from_secs(1);
/// Start times of the same process read twice may differ by rounding (as in `sys`).
const START_TIME_SLACK_S: u64 = 1;
/// How long a killed process may take to disappear.
const KILL_WAIT: Duration = Duration::from_secs(5);
/// How long an owner that keeps running (it owns other jobs) gets to clean up after its
/// command was killed.
const OWNER_CLEANUP: Duration = Duration::from_secs(5);

/// Which job to stop.
#[derive(Debug, Clone)]
pub enum Selector {
    Id(u64),
    /// The exact label; it must name one job.
    Label(String),
}

/// What happened to the selected job.
#[derive(Debug, Clone, PartialEq)]
pub enum Stopped {
    /// Its owner stopped it (running) or took it out of the line (waiting).
    ByOwner,
    /// The owner did not react: it was killed with this many processes of its tree.
    Killed { processes: usize },
    /// The owner did not react but also owns other jobs (an MCP server): only this job's
    /// command tree was killed, this many processes; the owner lives on.
    CommandKilled { processes: usize },
    /// The owner was already gone; the entry was removed.
    OwnerGone,
    /// A place kept for a ticket (no owner): dropped.
    Ticket,
    /// The owner did not react and could not be ended safely: why (the entry is left).
    NotKilled(String),
}

/// The selection failed: the text to print (exit 2).
#[derive(Debug, Clone, PartialEq)]
pub struct NoMatch(pub String);

fn describe(j: &Job, now: u64) -> String {
    let state = match () {
        _ if j.parked() => "kept".to_string(),
        _ if j.is_hold() && !j.running() => "waiting (hold)".to_string(),
        _ if j.running() => format!(
            "running {}",
            crate::history::human_ms(now.saturating_sub(j.started_ms.unwrap_or(now)))
        ),
        _ => "waiting".to_string(),
    };
    format!("  id {:<5} {:<16} {}  {}", j.id, state, j.label, j.cwd)
}

/// The job `sel` names in `s`, or why there is none.
pub fn select(s: &State, sel: &Selector, queue_flag: &str) -> std::result::Result<Job, NoMatch> {
    let now = now_ms();
    let list = |jobs: &[&Job]| {
        jobs.iter()
            .map(|j| describe(j, now))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let all: Vec<&Job> = s.jobs.iter().collect();
    let none = |what: String| {
        let tail = match all.is_empty() {
            true => format!("the queue is empty (`calmrun status{queue_flag}`)"),
            false => format!("jobs in this queue:\n{}", list(&all)),
        };
        NoMatch(format!("calmrun: {what}; {tail}"))
    };
    match sel {
        Selector::Id(id) => s
            .jobs
            .iter()
            .find(|j| j.id == *id)
            .cloned()
            .ok_or_else(|| none(format!("no job with id {id}"))),
        Selector::Label(l) => {
            let hits: Vec<&Job> = s.jobs.iter().filter(|j| j.label == *l).collect();
            match hits.len() {
                1 => Ok(hits[0].clone()),
                0 => Err(none(format!("no job is labelled \"{l}\""))),
                n => Err(NoMatch(format!(
                    "calmrun: {n} jobs are labelled \"{l}\"; stop one by its id \
                     (`calmrun stop <id>{queue_flag}`):\n{}",
                    list(&hits)
                ))),
            }
        }
    }
}

/// Mark the job for its owner (or drop a kept ticket). Returns the job as it was marked, and
/// whether it was a ticket.
fn mark(s: &mut State, sel: &Selector, queue_flag: &str) -> std::result::Result<(Job, bool), NoMatch> {
    let job = select(s, sel, queue_flag)?;
    if job.parked() {
        s.jobs.retain(|j| j.id != job.id);
        return Ok((job, true));
    }
    let now = now_ms();
    if let Some(j) = s.jobs.iter_mut().find(|j| j.id == job.id) {
        j.stop_ms.get_or_insert(now);
    }
    Ok((job, false))
}

/// Stop the selected job: ask its owner, wait up to `wait` for it to act, then end the owner
/// and its process tree directly. `say` gets progress lines.
pub fn stop(
    ctx: &mut Ctx,
    sel: &Selector,
    wait: Duration,
    say: &mut dyn FnMut(&str),
) -> Result<std::result::Result<(Job, Stopped), NoMatch>> {
    let flag = ctx
        .queue
        .as_ref()
        .map(|q| format!(" --id {q}"))
        .unwrap_or_default();
    let store = ctx.store()?;
    let (job, ticket) = match locked(&store, &mut ctx.sys, |s| mark(s, sel, &flag))? {
        Ok(m) => m,
        Err(e) => return Ok(Err(e)),
    };
    if ticket {
        return Ok(Ok((job, Stopped::Ticket)));
    }
    say(&format!(
        "calmrun: asked the calmrun of job {} ({}, pid {}) to stop it; waiting up to {}s",
        job.id,
        job.label,
        job.pid,
        wait.as_secs()
    ));
    let deadline = Instant::now() + wait;
    let mut renewed = Instant::now();
    while Instant::now() < deadline {
        std::thread::sleep(POLL);
        if !store.peek().jobs.iter().any(|j| j.id == job.id) {
            return Ok(Ok((job, Stopped::ByOwner)));
        }
        // A 0.3 calmrun rewriting the state drops the request; ask again.
        if renewed.elapsed() >= RENEW_EVERY {
            renewed = Instant::now();
            let _ = store.with_lock(|s| {
                if let Some(j) = s.jobs.iter_mut().find(|j| j.id == job.id) {
                    j.stop_ms.get_or_insert_with(now_ms);
                }
            });
        }
    }
    let how = match kill_owner(&store, &job) {
        Ok(h) => h,
        Err(why) => return Ok(Ok((job, Stopped::NotKilled(why)))),
    };
    if matches!(how, Stopped::CommandKilled { .. }) {
        // The owner sees its command end and leaves the queue itself, recording the run.
        let until = Instant::now() + OWNER_CLEANUP;
        while Instant::now() < until && store.peek().jobs.iter().any(|j| j.id == job.id) {
            std::thread::sleep(POLL);
        }
    }
    // Gone now: take the entry out and record the stop (nobody else will).
    let current = store
        .with_lock(|s| {
            let found = s.jobs.iter().find(|j| j.id == job.id).cloned();
            s.jobs.retain(|j| j.id != job.id);
            found
        })
        .map_err(crate::error::Error::State)?;
    // The owner may have finished on its own just now and recorded the run itself.
    if let Some(j) = current {
        record_stopped(&store, &j);
    }
    Ok(Ok((job, how)))
}

/// The processes below `root` (pid, start time). On Windows a process keeps the pid of its
/// parent even after that parent died and the pid was reused, so a link only counts when the
/// child started no earlier than its parent.
fn descendants(sys: &System, root: u32) -> Vec<(u32, u64)> {
    let start = |pid: u32| sys.process(Pid::from_u32(pid)).map(|p| p.start_time());
    let mut children: std::collections::HashMap<u32, Vec<u32>> = Default::default();
    for (pid, p) in sys.processes() {
        if let Some(pp) = p.parent().map(Pid::as_u32).filter(|pp| *pp != pid.as_u32()) {
            children.entry(pp).or_default().push(pid.as_u32());
        }
    }
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::from([root]);
    let mut stack = vec![root];
    while let Some(parent) = stack.pop() {
        let Some(parent_start) = start(parent) else {
            continue;
        };
        for &c in children.get(&parent).into_iter().flatten() {
            let Some(cs) = start(c) else { continue };
            if cs + START_TIME_SLACK_S >= parent_start && seen.insert(c) {
                out.push((c, cs));
                stack.push(c);
            }
        }
    }
    out
}

/// Kill `(pid, start)` processes that still have that start time; returns how many.
fn kill_all(sys: &System, procs: &[(u32, u64)]) -> usize {
    procs
        .iter()
        .filter(|(pid, start)| {
            sys.process(Pid::from_u32(*pid))
                .filter(|p| p.start_time() == *start)
                .is_some_and(|p| p.kill())
        })
        .count()
}

/// Whether `pid` with start time `start` is gone, waiting up to [`KILL_WAIT`].
fn gone(sys: &mut System, pid: u32, start: u64) -> bool {
    let until = Instant::now() + KILL_WAIT;
    let p = Pid::from_u32(pid);
    loop {
        sys.refresh_processes_specifics(ProcessesToUpdate::Some(&[p]), true, ProcessRefreshKind::nothing());
        let alive = sys.process(p).is_some_and(|pr| {
            pr.start_time().abs_diff(start) <= START_TIME_SLACK_S
                && !matches!(
                    pr.status(),
                    sysinfo::ProcessStatus::Zombie | sysinfo::ProcessStatus::Dead
                )
        });
        if !alive {
            return true;
        }
        if Instant::now() >= until {
            return false;
        }
        std::thread::sleep(POLL);
    }
}

/// End the owner of `job` and every process of its tree, after checking the pid still
/// belongs to that owner (same start time); a reused pid is never touched. An owner that also
/// owns other jobs (an MCP server runs every task under one pid) is not killed: only this
/// job's command tree is, and a waiting job of such an owner is refused. `Err` says why
/// nothing was done (the entry is left as it is).
fn kill_owner(store: &Store, job: &Job) -> std::result::Result<Stopped, String> {
    let Ok(mut sys) = System::new() else {
        return Err("calmrun: cannot read the process list to stop the job's calmrun".into());
    };
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().without_tasks(),
    );
    let same = |sys: &System, pid: u32, start: u64| {
        sys.process(Pid::from_u32(pid))
            .is_some_and(|p| p.start_time().abs_diff(start) <= START_TIME_SLACK_S)
    };
    if !same(&sys, job.pid, job.pid_start) {
        return Ok(Stopped::OwnerGone);
    }
    let others: Vec<u64> = store
        .peek()
        .jobs
        .into_iter()
        .chain(
            store
                .others()
                .unwrap_or_default()
                .into_iter()
                .flat_map(|o| o.state.jobs),
        )
        .filter(|j| j.id != job.id || j.queued_ms != job.queued_ms)
        .filter(|j| j.pid == job.pid && j.pid_start == job.pid_start && !j.parked())
        .map(|j| j.id)
        .collect();
    if !others.is_empty() {
        let list = others.iter().map(u64::to_string).collect::<Vec<_>>().join(", ");
        let child = job.child_pid.filter(|_| job.running());
        let Some(child) = child else {
            return Err(format!(
                "calmrun: job {} did not leave the line: its calmrun (pid {}) did not react, and \
                 it also owns jobs {list} (an MCP server?), which killing it would stop too; \
                 cancel the task there or restart that server",
                job.id, job.pid
            ));
        };
        // Only the command, if it is still the owner's own child.
        let child_start = sys
            .process(Pid::from_u32(child))
            .filter(|p| p.parent().map(Pid::as_u32) == Some(job.pid))
            .map(|p| p.start_time())
            .filter(|cs| cs + START_TIME_SLACK_S >= job.pid_start);
        let Some(child_start) = child_start else {
            return Err(format!(
                "calmrun: job {}'s calmrun (pid {}) did not react and also owns jobs {list}; its \
                 command (pid {child}) is no longer its child, so nothing was killed",
                job.id, job.pid
            ));
        };
        let mut tree = vec![(child, child_start)];
        tree.extend(descendants(&sys, child));
        return Ok(Stopped::CommandKilled {
            processes: kill_all(&sys, &tree),
        });
    }
    // The tree before the owner goes: afterwards its children have a new parent.
    let tree = descendants(&sys, job.pid);
    // The owner first, so it cannot record the killed command as a normal end.
    let killed = sys.process(Pid::from_u32(job.pid)).is_some_and(|p| p.kill());
    if !killed || !gone(&mut sys, job.pid, job.pid_start) {
        return Err(format!(
            "calmrun: job {}'s calmrun (pid {}) did not react and could not be killed (another \
             user's process?); the entry is left as it is",
            job.id, job.pid
        ));
    }
    Ok(Stopped::Killed {
        processes: kill_all(&sys, &tree),
    })
}

/// The history line for a job `stop` ended itself.
fn record_stopped(store: &Store, j: &Job) {
    let now = now_ms();
    let (wait_ms, run_ms) = match j.started_ms {
        Some(s) => (s.saturating_sub(j.queued_ms), now.saturating_sub(s)),
        None => (now.saturating_sub(j.queued_ms), 0),
    };
    if j.is_hold() {
        return;
    }
    let out = Outcome {
        status: "stopped".into(),
        label: j.label.clone(),
        kind: j.kind.clone(),
        cost: j.cost,
        wait_ms,
        run_ms,
        cwd: crate::watch::folder_key(j),
        ..Outcome::default()
    };
    queue::record(store, store.queue(), &out);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(id: u64, label: &str) -> Job {
        Job {
            id,
            label: label.into(),
            cwd: "/w".into(),
            ..Job::default()
        }
    }

    #[test]
    fn a_label_must_name_exactly_one_job() {
        let s = State {
            jobs: vec![job(1, "npx vitest"), job(2, "npx vitest"), job(3, "cargo test")],
            ..State::default()
        };
        assert_eq!(
            select(&s, &Selector::Label("cargo test".into()), "").unwrap().id,
            3
        );
        assert_eq!(select(&s, &Selector::Id(2), "").unwrap().id, 2);
        let many = select(&s, &Selector::Label("npx vitest".into()), " --id q")
            .unwrap_err()
            .0;
        assert!(
            many.starts_with("calmrun: 2 jobs are labelled \"npx vitest\""),
            "{many}"
        );
        assert!(
            many.contains("`calmrun stop <id> --id q`") && many.contains("id 1 "),
            "{many}"
        );
        // Not a substring match: a stop must not hit the wrong job.
        let none = select(&s, &Selector::Label("vitest".into()), "").unwrap_err().0;
        assert!(
            none.contains("no job is labelled") && none.contains("id 3 "),
            "{none}"
        );
        let empty = select(&State::default(), &Selector::Id(9), "").unwrap_err().0;
        assert!(empty.contains("the queue is empty"), "{empty}");
        assert!(many.is_ascii() && none.is_ascii() && empty.is_ascii());
    }

    #[test]
    fn marking_sets_the_request_once_and_drops_a_ticket() {
        let mut parked = job(5, "npx jest");
        parked.parked_until = Some(u64::MAX);
        let mut s = State {
            jobs: vec![job(4, "npx vitest"), parked],
            ..State::default()
        };
        let (j, ticket) = mark(&mut s, &Selector::Id(4), "").unwrap();
        assert!(!ticket && j.stop_ms.is_none());
        let first = s.jobs[0].stop_ms.expect("marked");
        mark(&mut s, &Selector::Id(4), "").unwrap();
        assert_eq!(s.jobs[0].stop_ms, Some(first));
        let (_, ticket) = mark(&mut s, &Selector::Id(5), "").unwrap();
        assert!(ticket);
        assert_eq!(s.jobs.len(), 1);
    }
}
