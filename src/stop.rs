//! `calmrun stop`: ask the calmrun that owns a job to stop it, and end that owner directly
//! when it does not react in time (a calmrun older than 0.4 ignores the request; a process
//! may be wedged). CLI only: an agent must not clear other agents' jobs out of its way.

use crate::error::Result;
use crate::memwatch::tree_members;
use crate::queue::{self, locked, now_ms, Ctx, Outcome};
use crate::state::{Job, State, Store};
use std::time::{Duration, Instant};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

/// How often `stop` looks whether the owner has acted, and renews its request.
const POLL: Duration = Duration::from_millis(100);
const RENEW_EVERY: Duration = Duration::from_secs(1);
/// Start times of the same process read twice may differ by rounding (as in `sys`).
const START_TIME_SLACK_S: u64 = 1;

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
    /// The owner was already gone; the entry was removed.
    OwnerGone,
    /// A place kept for a ticket (no owner): dropped.
    Ticket,
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
    let how = kill_owner(&job);
    // Gone either way now: take the entry out and record the stop (nobody else will).
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

/// End the owner of `job` and every process of its tree, after checking the pid still
/// belongs to that owner (same start time); a reused pid is never touched.
fn kill_owner(job: &Job) -> Stopped {
    let Ok(mut sys) = System::new() else {
        return Stopped::OwnerGone;
    };
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().without_tasks(),
    );
    let same = |pid: u32, start: u64| {
        sys.process(Pid::from_u32(pid))
            .is_some_and(|p| p.start_time().abs_diff(start) <= START_TIME_SLACK_S)
    };
    if !same(job.pid, job.pid_start) {
        return Stopped::OwnerGone;
    }
    let links: Vec<(u32, Option<u32>)> = sys
        .processes()
        .iter()
        .map(|(pid, p)| (pid.as_u32(), p.parent().map(Pid::as_u32)))
        .collect();
    // The tree before the owner goes: afterwards its children have a new parent.
    let tree: Vec<(u32, u64)> = tree_members(&links, job.pid)
        .into_iter()
        .filter(|pid| *pid != job.pid)
        .filter_map(|pid| sys.process(Pid::from_u32(pid)).map(|p| (pid, p.start_time())))
        .collect();
    // The owner first, so it cannot record the killed command as a normal end.
    if let Some(p) = sys.process(Pid::from_u32(job.pid)) {
        p.kill();
    }
    let mut killed = 0;
    for (pid, start) in &tree {
        if let Some(p) = sys
            .process(Pid::from_u32(*pid))
            .filter(|p| p.start_time() == *start)
        {
            if p.kill() {
                killed += 1;
            }
        }
    }
    Stopped::Killed { processes: killed }
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
