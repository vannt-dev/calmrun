//! Working or hung? Hints about a running job from its CPU samples, its usual run time and its
//! run time limit. Hints only: nothing here stops a job. A tree that waits for docker, an
//! emulator or the network uses no CPU of its own and may still be doing its work.

use crate::history::{human_ms, Usual};
use crate::state::Job;
use serde_json::{json, Value};
use std::path::Path;

/// A tree that has used no noticeable CPU (see [`crate::memwatch::BUSY_PCT`]) for this long
/// shows `idle`.
pub const IDLE_AFTER_MS: u64 = 2 * 60_000;
/// "Longer than usual": a run time above max(3 x the usual one, the usual one + 2 min).
const LONGER_FACTOR: u64 = 3;
const LONGER_MARGIN_MS: u64 = 2 * 60_000;

/// What can be said about one job right now.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Hints {
    /// Time since it started (0 while waiting).
    pub run_ms: u64,
    /// CPU over the last sample, in percent of one core; `None` when its owner publishes no
    /// samples (a calmrun older than 0.4).
    pub cpu_pct: Option<u32>,
    /// CPU time the tree used so far.
    pub cpu_ms: Option<u64>,
    /// How long the tree has used no noticeable CPU, once that is at least [`IDLE_AFTER_MS`].
    pub idle_ms: Option<u64>,
    /// The usual run time of this label from the history.
    pub usual_ms: Option<u64>,
    /// It has run much longer than usual.
    pub longer: bool,
    /// Its run time limit.
    pub limit_ms: Option<u64>,
    /// `calmrun stop` asked its owner to stop it.
    pub stop_requested: bool,
}

/// The folder key the history matches a job on.
pub fn folder_key(j: &Job) -> String {
    match j.cwd_key.is_empty() {
        true => crate::sys::path_key(Path::new(&j.cwd)),
        false => j.cwd_key.clone(),
    }
}

/// Whether a run of `run_ms` is much longer than a usual one of `usual_ms`.
pub fn longer_than_usual(run_ms: u64, usual_ms: u64) -> bool {
    run_ms > (usual_ms.saturating_mul(LONGER_FACTOR)).max(usual_ms + LONGER_MARGIN_MS)
}

/// The hints for `j` at `now`.
pub fn hints(j: &Job, now: u64, usual: Option<&Usual>) -> Hints {
    let started = j.started_ms;
    let run_ms = started.map_or(0, |s| now.saturating_sub(s));
    let idle_ms = match (started, j.cpu_ms) {
        // Idle since the last busy sample, or since the start when it never was busy.
        (Some(s), Some(_)) => Some(now.saturating_sub(j.busy_ms.unwrap_or(0).max(s))),
        _ => None,
    }
    .filter(|ms| *ms >= IDLE_AFTER_MS);
    let usual_ms = usual
        .filter(|_| started.is_some())
        .and_then(|u| u.median(&j.label, &folder_key(j)));
    Hints {
        run_ms,
        cpu_pct: j.cpu_pct.filter(|_| started.is_some() && j.cpu_ms.is_some()),
        cpu_ms: j.cpu_ms.filter(|_| started.is_some()),
        idle_ms,
        usual_ms,
        longer: usual_ms.is_some_and(|u| longer_than_usual(run_ms, u)),
        limit_ms: j.timeout_ms,
        stop_requested: j.stop_ms.is_some(),
    }
}

/// A short duration: `40s`, `12m`, `2h`, `2h05m`.
pub fn short_ms(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        _ if s % 3600 < 60 => format!("{}h", s / 3600),
        _ => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
    }
}

/// A run time limit: whole minutes as `60m`, else [`short_ms`].
fn limit_text(ms: u64) -> String {
    match ms % 60_000 {
        0 if ms > 0 => format!("{}m", ms / 60_000),
        _ => short_ms(ms),
    }
}

impl Hints {
    /// Whether the job looks stuck: idle, or much longer than usual.
    pub fn worrying(&self) -> bool {
        self.idle_ms.is_some() || self.longer
    }

    /// The parts a waiting line adds after the run time: `usually 40s`, `longer than usual`,
    /// `idle 5m`. Empty when there is nothing to say.
    pub fn brief(&self) -> Vec<String> {
        let mut parts = Vec::new();
        if let Some(u) = self.usual_ms {
            parts.push(format!("usually {}", short_ms(u)));
        }
        if self.longer {
            parts.push("longer than usual".to_string());
        }
        if let Some(i) = self.idle_ms {
            parts.push(format!("idle {}", short_ms(i)));
        }
        parts
    }

    /// The line under a running job in `calmrun status`: `cpu 85%, usually 40s, limit 60m`.
    pub fn full(&self) -> String {
        let mut parts = Vec::new();
        if let Some(p) = self.cpu_pct {
            parts.push(format!("cpu {p}%"));
        }
        parts.extend(self.brief());
        if let Some(l) = self.limit_ms {
            parts.push(format!("limit {}", limit_text(l)));
        }
        if self.stop_requested {
            parts.push("stop requested".to_string());
        }
        parts.join(", ")
    }

    /// The same as JSON fields (`status --json`, MCP).
    pub fn json(&self) -> Value {
        json!({
            "run_ms": self.run_ms,
            "cpu_pct": self.cpu_pct,
            "cpu_ms": self.cpu_ms,
            "idle_ms": self.idle_ms,
            "usual_ms": self.usual_ms,
            "longer_than_usual": self.longer,
            "timeout_ms": self.limit_ms,
            "stop_requested": self.stop_requested,
            "hint": self.full(),
        })
    }
}

/// The next step for a running job that looks stuck, for the people waiting behind it.
pub fn next_step(j: &Job, h: &Hints, queue_flag: &str) -> String {
    let why = match (h.idle_ms, h.usual_ms) {
        (Some(i), _) => format!("has used no CPU for {}", short_ms(i)),
        (None, Some(u)) => format!("has run {} (usually {})", human_ms(h.run_ms), short_ms(u)),
        _ => "may hang".to_string(),
    };
    format!(
        "calmrun: next: \"{}\" (job {}) {why}; check it with `calmrun status{queue_flag}`, and \
         if it hangs stop it with `calmrun stop {}{queue_flag}`",
        j.label, j.id, j.id
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn running(started: u64) -> Job {
        Job {
            id: 4,
            label: "npx vitest".into(),
            cwd: "/w".into(),
            cwd_key: "/w".into(),
            started_ms: Some(started),
            ..Job::default()
        }
    }

    fn usual(ms: u64, n: usize) -> Usual {
        let line = format!(
            r#"{{"at":"2026-11-20T10:00:00Z","label":"npx vitest","kind":"vitest","cost":2,"wait_ms":0,"run_ms":{ms},"status":"0","cwd":"/w"}}"#
        );
        Usual::from_text(&vec![line; n].join("\n"))
    }

    #[test]
    fn idle_needs_samples_and_two_minutes() {
        let now = 1_000_000;
        // A 0.3 owner publishes nothing: no CPU or idle hint at all.
        let old = running(now - 600_000);
        assert_eq!(
            hints(&old, now, None),
            Hints {
                run_ms: 600_000,
                ..Hints::default()
            }
        );
        let mut j = running(now - 600_000);
        j.cpu_ms = Some(40);
        j.cpu_pct = Some(0);
        // Never busy since the start ten minutes ago.
        assert_eq!(hints(&j, now, None).idle_ms, Some(600_000));
        j.busy_ms = Some(now - 300_000);
        let h = hints(&j, now, None);
        assert_eq!((h.idle_ms, h.cpu_pct), (Some(300_000), Some(0)));
        assert_eq!(h.full(), "cpu 0%, idle 5m");
        j.busy_ms = Some(now - 60_000);
        assert_eq!(hints(&j, now, None).idle_ms, None);
        assert!(!hints(&j, now, None).worrying());
    }

    #[test]
    fn usual_and_longer_than_usual() {
        let now = 1_000_000;
        let mut j = running(now - 12 * 60_000);
        j.timeout_ms = Some(3_600_000);
        let h = hints(&j, now, Some(&usual(40_000, 3)));
        assert_eq!(h.usual_ms, Some(40_000));
        assert!(h.longer && h.worrying());
        assert_eq!(h.brief(), vec!["usually 40s", "longer than usual"]);
        assert_eq!(h.full(), "usually 40s, longer than usual, limit 60m");
        assert!(next_step(&j, &h, " --id q").contains("`calmrun stop 4 --id q`"));
        // Too few runs: no hint. A short job is longer only past usual + 2 min.
        assert_eq!(hints(&j, now, Some(&usual(40_000, 2))).usual_ms, None);
        assert!(!longer_than_usual(150_000, 40_000));
        assert!(longer_than_usual(170_000, 40_000));
        assert!(!longer_than_usual(2_900_000, 1_000_000));
        assert!(longer_than_usual(3_100_000, 1_000_000));
    }

    #[test]
    fn durations_and_text_are_short_and_ascii() {
        assert_eq!(short_ms(40_500), "40s");
        assert_eq!(short_ms(12 * 60_000 + 5_000), "12m");
        assert_eq!(short_ms(2 * 3_600_000), "2h");
        assert_eq!(short_ms(2 * 3_600_000 + 5 * 60_000), "2h05m");
        assert_eq!(limit_text(90_000), "1m");
        assert_eq!(limit_text(30_000), "30s");
        let h = Hints {
            run_ms: 1,
            cpu_pct: Some(85),
            idle_ms: Some(IDLE_AFTER_MS),
            usual_ms: Some(1),
            longer: true,
            limit_ms: Some(1),
            stop_requested: true,
            ..Hints::default()
        };
        assert!(h.full().is_ascii());
        assert!(next_step(&running(0), &h, "").is_ascii());
    }
}
