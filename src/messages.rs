//! The lines calmrun prints for people and agents. Every line says what is happening and,
//! where there is something to do, the exact next command.

use crate::config::bytes_to_gb;
use crate::scheduler::{self, Block};
use crate::state::{Job, State};
use crate::sys::DiskCheck;
use std::collections::BTreeMap;
use std::path::Path;

/// How many running jobs the waiting line names before "+N more".
const RUNNING_SHOWN: usize = 3;
/// The first waiting line is followed by this hint.
pub const STARTS_BY_ITSELF: &str =
    "calmrun: it starts by itself when there is room; `calmrun status` shows the queue";

/// `1st`, `2nd`, `3rd`, `4th`, `11th`, …
pub fn ordinal(n: usize) -> String {
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

/// What a waiting line needs to know besides the state.
pub struct WaitContext<'a> {
    pub me: &'a Job,
    pub budget: u32,
    pub disk: &'a DiskCheck,
    pub caps: &'a BTreeMap<String, u32>,
    pub now_ms: u64,
}

/// "calmrun: waiting — 4 of 6 points in use, 2nd in line (needs 2); running: …".
pub fn waiting_line(s: &State, why: &Block, c: &WaitContext) -> String {
    let line = waiting_reason(s, why, c);
    match running_summary(s, c.now_ms) {
        r if r.is_empty() => line,
        r => format!("{line}; running: {r}"),
    }
}

fn waiting_reason(s: &State, why: &Block, c: &WaitContext) -> String {
    let place = scheduler::waiting_in_order(&s.jobs)
        .iter()
        .position(|j| j.id == c.me.id)
        .map_or(1, |p| p + 1);
    let running = || s.jobs.iter().filter(|j| j.running());
    match why {
        Block::Ram { free, needs } => format!(
            "calmrun: waiting for RAM: {:.1} GB free, needs {:.1} GB",
            bytes_to_gb(*free),
            bytes_to_gb(*needs)
        ),
        Block::Resource(name) => {
            let holders: Vec<String> = running()
                .filter(|j| j.needs.contains(name))
                .map(|j| j.label.clone())
                .chain(
                    s.leases
                        .iter()
                        .filter(|l| l.resources.contains(name))
                        .map(|l| format!("{} (lease {})", l.label, l.id)),
                )
                .collect();
            format!(
                "calmrun: waiting for resource \"{name}\" ({} of {} slot(s) held by {}), {} in line",
                holders.len(),
                scheduler::capacity(c.caps, name),
                holders.join(", "),
                ordinal(place)
            )
        }
        Block::Disk => format!(
            "calmrun: waiting for disk space: {:.1} GB free on {}, needs {:.1} GB",
            bytes_to_gb(c.disk.free),
            c.disk.mount,
            bytes_to_gb(c.disk.need)
        ),
        Block::Key => {
            let holder = running()
                .find(|j| j.keys.iter().any(|k| c.me.keys.contains(k)))
                .map_or("", |j| j.label.as_str());
            format!(
                "calmrun: waiting — same dependency folder is in use by \"{holder}\", {} in line",
                ordinal(place)
            )
        }
        Block::Points { .. } | Block::Order => format!(
            "calmrun: waiting — {} of {} points in use, {} in line (needs {})",
            scheduler::used_points(&s.jobs, c.budget) + scheduler::leased_points(&s.leases, c.budget),
            c.budget,
            ordinal(place),
            scheduler::job_points(c.me, c.budget)
        ),
    }
}

/// "npx vitest in web (3m10s), cargo build in api (1m02s)".
pub fn running_summary(s: &State, now: u64) -> String {
    let mut running: Vec<&Job> = s.jobs.iter().filter(|j| j.running()).collect();
    running.sort_by_key(|j| j.started_ms);
    let mut out = running
        .iter()
        .take(RUNNING_SHOWN)
        .map(|j| {
            let dir = Path::new(&j.cwd)
                .file_name()
                .map_or_else(|| j.cwd.clone(), |n| n.to_string_lossy().to_string());
            let since = now.saturating_sub(j.started_ms.unwrap_or(now));
            format!("{} in {dir} ({})", j.label, crate::history::human_ms(since))
        })
        .collect::<Vec<_>>()
        .join(", ");
    if running.len() > RUNNING_SHOWN {
        out.push_str(&format!(", +{} more", running.len() - RUNNING_SHOWN));
    }
    out
}

/// The message after `--wait-timeout`, with the command that resumes the place.
pub fn wait_timeout(
    secs: u64,
    last_line: &str,
    rerun_hint: Option<&str>,
    ticket: u64,
    ttl_min: u64,
) -> String {
    let mut msg = format!(
        "calmrun: not started within {secs}s ({}); your place in line is kept for {ttl_min} min.",
        last_line.trim_start_matches("calmrun: ")
    );
    if let Some(h) = rerun_hint {
        msg.push_str(&format!(
            "\ncalmrun: next: {}",
            h.replace("{ticket}", &ticket.to_string())
        ));
    }
    msg
}

/// The refusal for an attached `docker compose up`, with the fixed command when possible.
/// `hint` says how to run that command (`{command}` is replaced; default: the CLI form).
pub fn refused(reason: &str, fixed: Option<&[String]>, hint: Option<&str>) -> String {
    let next = match fixed {
        Some(f) => hint
            .unwrap_or("calmrun -- {command}")
            .replace("{command}", &f.join(" ")),
        None => "run it outside calmrun".to_string(),
    };
    format!("calmrun: refused: {reason}.\ncalmrun: next: {next}")
}

/// `kept (ticket 17, expires in 9:12) npx vitest run  /src/web`.
pub fn kept_line(j: &Job, now: u64) -> String {
    let left = j.parked_until.unwrap_or(now).saturating_sub(now) / 1000;
    format!(
        "kept (ticket {}, expires in {}:{:02}) {}  {}",
        j.id,
        left / 60,
        left % 60,
        j.label,
        j.cwd
    )
}

/// Warnings for waiters older than `stuck_ms`.
pub fn stuck_warnings(state: &State, now: u64, stuck_ms: u64) -> Vec<String> {
    scheduler::waiting_in_order(&state.jobs)
        .iter()
        .filter(|j| now.saturating_sub(j.queued_ms) > stuck_ms)
        .map(|j| {
            format!(
                "\"{}\" (pid {}) has waited {} — is something stuck? Check the running jobs \
                 with `calmrun status`, or move it forward with `calmrun bump {}`",
                j.label,
                j.pid,
                crate::history::human_ms(now - j.queued_ms),
                j.label
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(id: u64, label: &str, cwd: &str, started: u64) -> Job {
        Job {
            id,
            label: label.into(),
            cwd: cwd.into(),
            started_ms: Some(started),
            ..Default::default()
        }
    }

    #[test]
    fn running_summary_names_jobs_and_folders() {
        let s = State {
            ram_sample: None,
            leases: Vec::new(),
            next_id: 9,
            jobs: vec![
                job(1, "npx vitest", "/src/web", 10_000),
                job(2, "cargo build", "/src/api", 60_000),
                Job {
                    id: 3,
                    label: "waiting".into(),
                    ..Default::default()
                },
            ],
        };
        assert_eq!(
            running_summary(&s, 200_000),
            "npx vitest in web (3m10s), cargo build in api (2m20s)"
        );
    }

    #[test]
    fn kept_line_shows_ticket_and_time_left() {
        let j = Job {
            id: 17,
            label: "npx vitest run".into(),
            cwd: "/w".into(),
            parked_until: Some(553_000),
            ..Default::default()
        };
        assert_eq!(
            kept_line(&j, 1_000),
            "kept (ticket 17, expires in 9:12) npx vitest run  /w"
        );
    }

    #[test]
    fn resource_line_names_leases_as_holders() {
        let me = Job {
            id: 2,
            order: 2,
            needs: vec!["emulator".into()],
            ..Default::default()
        };
        let s = State {
            jobs: vec![me.clone()],
            leases: vec![crate::state::Lease {
                id: "abc123".into(),
                resources: vec!["emulator".into()],
                label: "lane calmrun-4".into(),
                until_ms: u64::MAX,
                ..Default::default()
            }],
            ..State::default()
        };
        let disk = DiskCheck::new(Path::new("."), 0);
        let caps = BTreeMap::new();
        let c = WaitContext {
            me: &me,
            budget: 2,
            disk: &disk,
            caps: &caps,
            now_ms: 1_000,
        };
        assert_eq!(
            waiting_line(&s, &Block::Resource("emulator".into()), &c),
            "calmrun: waiting for resource \"emulator\" (1 of 1 slot(s) held by lane calmrun-4 \
             (lease abc123)), 1st in line"
        );
    }

    #[test]
    fn ordinals() {
        let got: Vec<String> = [1, 2, 3, 4, 11, 12, 13, 21, 22, 101]
            .iter()
            .map(|n| ordinal(*n))
            .collect();
        assert_eq!(
            got,
            ["1st", "2nd", "3rd", "4th", "11th", "12th", "13th", "21st", "22nd", "101st"]
        );
    }

    #[test]
    fn points_line_names_place_and_need() {
        let me = Job {
            id: 2,
            cost: 2,
            order: 2,
            ..Default::default()
        };
        let s = State {
            ram_sample: None,
            leases: Vec::new(),
            next_id: 2,
            jobs: vec![job(1, "npx vitest", "/w/web", 0), me.clone()],
        };
        let disk = DiskCheck::new(Path::new("."), 0);
        let caps = BTreeMap::new();
        let c = WaitContext {
            me: &me,
            budget: 2,
            disk: &disk,
            caps: &caps,
            now_ms: 1_000,
        };
        let s0 = State {
            jobs: vec![
                Job {
                    cost: 2,
                    ..s.jobs[0].clone()
                },
                me.clone(),
            ],
            ..s
        };
        assert_eq!(
            waiting_line(&s0, &Block::Points { used: 2, needs: 2 }, &c),
            "calmrun: waiting — 2 of 2 points in use, 1st in line (needs 2); running: npx vitest in web (1.0s)"
        );
    }
}
