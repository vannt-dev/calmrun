//! Running one job through the queue. Shared by the CLI and the MCP server.

use crate::config::{self, bytes_to_gb, Config};
use crate::scheduler::{self, Block, Limits};
use crate::state::{self, Job, State, Store};
use crate::sys::Sys;
use crate::{detect, history, memwatch, runner};
use serde_json::json;
use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const POLL: Duration = Duration::from_millis(500);
pub const STUCK_MS: u64 = 30 * 60_000;

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Loaded settings plus a system-info handle.
pub struct Ctx {
    pub cfg: Config,
    pub home: PathBuf,
    pub config_path: PathBuf,
    pub machine: config::Machine,
    pub sys: Sys,
}

impl Ctx {
    pub fn load() -> Ctx {
        let mut sys = Sys::new();
        let home = config::home_dir();
        let config_path = config::config_path(&home);
        let machine = sys.machine();
        let file = config::read_file_config(&config_path).unwrap_or_else(|e| {
            eprintln!("calmrun: ignoring config file: {e}");
            config::FileConfig::default()
        });
        let cfg = config::resolve(machine, file, &|k| std::env::var(k).ok());
        Ctx {
            cfg,
            home,
            config_path,
            machine,
            sys,
        }
    }

    pub fn limits(&self) -> Limits {
        Limits {
            budget: self.cfg.budget,
            min_free_bytes: self.cfg.min_free_bytes,
            pass_limit_ms: self.cfg.pass_limit_ms,
        }
    }

    pub fn store(&self) -> io::Result<Store> {
        Store::open(&self.home)
    }
}

pub fn prune_dead(s: &mut State, sys: &mut Sys) {
    let pairs: Vec<(u32, u64)> = s
        .jobs
        .iter()
        .filter(|j| !j.parked())
        .map(|j| (j.pid, j.pid_start))
        .collect();
    let alive: HashSet<(u32, u64)> = pairs
        .iter()
        .zip(sys.alive_set(&pairs))
        .filter_map(|(p, ok)| ok.then_some(*p))
        .collect();
    state::prune(s, now_ms(), |pid, start| alive.contains(&(pid, start)));
}

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

/// One job to run.
#[derive(Debug, Clone, Default)]
pub struct Request {
    pub argv: Vec<String>,
    /// Run `argv[0]` as one shell line (`sh -c` / `cmd /c`) even without spaces.
    pub shell: bool,
    pub cwd: Option<PathBuf>,
    pub kind: Option<String>,
    pub cost: Option<u32>,
    pub high: bool,
    pub label: Option<String>,
    /// Stop the command after this long (exit 124).
    pub timeout: Option<Duration>,
    /// Write stdout and stderr to this file (stdin is closed) instead of inheriting them.
    pub log: Option<PathBuf>,
    /// Set by the caller to give up (waiting) or stop the command (running).
    pub cancel: Option<Arc<AtomicBool>>,
    /// Give up waiting after this long (exit 75) but keep the place for a ticket.
    pub wait_timeout: Option<Duration>,
    /// Resume the place of an earlier `--wait-timeout` run.
    pub ticket: Option<u64>,
    /// How to show the re-run command; `{ticket}` is replaced by the ticket id.
    pub rerun_hint: Option<String>,
}

/// How long a ticket keeps its place without a re-run.
pub const TICKET_TTL_MS: u64 = 10 * 60_000;
pub const EXIT_WAIT_TIMEOUT: i32 = 75;

/// What happened.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub code: i32,
    /// Exit code as text, or a word: refused, interrupted, cancelled, timeout, spawn-failed.
    pub status: String,
    pub label: String,
    pub kind: String,
    pub cost: u32,
    pub wait_ms: u64,
    pub run_ms: u64,
    /// A line for the user when calmrun itself has something to say (refusal, error).
    pub message: Option<String>,
    /// Ticket id when the wait timed out and the place is kept.
    pub ticket: Option<u64>,
}

/// Progress callbacks.
pub trait Notify {
    /// Called on every poll while waiting, with the current reason line.
    fn waiting(&mut self, line: &str);
    /// Called once when the command starts.
    fn started(&mut self, _wait_ms: u64) {}
    /// A one-off note for the user.
    fn note(&mut self, line: &str) {
        eprintln!("{line}");
    }
}

/// Prints the waiting line at once and then every 30 s.
pub struct StderrNotify {
    last: Option<Instant>,
}

impl StderrNotify {
    pub fn new() -> StderrNotify {
        StderrNotify { last: None }
    }
}

impl Notify for StderrNotify {
    fn waiting(&mut self, line: &str) {
        if self.last.is_none() {
            eprintln!("{line}");
            eprintln!("calmrun: it starts by itself when there is room; `calmrun status` shows the queue");
            self.last = Some(Instant::now());
        } else if self.last.is_some_and(|t| t.elapsed() >= Duration::from_secs(30)) {
            eprintln!("{line}");
            self.last = Some(Instant::now());
        }
    }
}

fn cancelled(req: &Request) -> bool {
    runner::INTERRUPTED.load(Ordering::SeqCst)
        || req.cancel.as_ref().is_some_and(|c| c.load(Ordering::SeqCst))
}

fn spawn_error(argv: &[String], e: &io::Error) -> (i32, String) {
    let code = if e.kind() == io::ErrorKind::NotFound {
        127
    } else {
        126
    };
    let name = argv.first().map(String::as_str).unwrap_or("");
    let hint = if code == 127 {
        "\ncalmrun: next: check the name and PATH; a package's own bin may need `npx`, \
         `pnpm exec` or `uv run` in front"
    } else {
        ""
    };
    let msg = format!("calmrun: cannot run {name}: {e}{hint}");
    (code, msg)
}

fn prepare(req: &Request, cfg: &Config) -> io::Result<std::process::Command> {
    let mut cmd = match (req.shell, req.argv.first()) {
        (true, Some(line)) => runner::shell_command(line),
        _ => runner::build(&req.argv)?,
    };
    if let Some(d) = &req.cwd {
        cmd.current_dir(d);
    }
    if let Some(log) = &req.log {
        if let Some(parent) = log.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let f = std::fs::File::create(log)?;
        cmd.stdin(Stdio::null()).stdout(f.try_clone()?).stderr(f);
    }
    let _ = cfg;
    Ok(cmd)
}

/// Run the command for `req` through the queue.
pub fn run(ctx: &mut Ctx, mut req: Request, notify: &mut dyn Notify) -> Outcome {
    let cfg = ctx.cfg.clone();
    let mut out = Outcome {
        code: 0,
        status: String::new(),
        label: String::new(),
        kind: String::new(),
        cost: 0,
        wait_ms: 0,
        run_ms: 0,
        message: None,
        ticket: None,
    };
    if let Some(reason) = detect::refuse_reason(&req.argv) {
        out.code = 2;
        out.status = "refused".into();
        let next = match detect::detached_compose(&req.argv) {
            Some(fixed) => format!("calmrun -- {}", fixed.join(" ")),
            None => "add -d after `up`, or run `docker compose up` outside calmrun".to_string(),
        };
        out.message = Some(format!("calmrun: refused: {reason}.\ncalmrun: next: {next}"));
        return out;
    }
    if let Some(a) = detect::vitest_args(&req.argv, cfg.vitest_workers) {
        req.argv = a;
    }
    let timeout = req.timeout;
    let started_at = std::cell::Cell::new(None::<Instant>);
    let mut watch = |_pid: u32| -> bool {
        if req.cancel.as_ref().is_some_and(|c| c.load(Ordering::SeqCst)) {
            return true;
        }
        matches!((timeout, started_at.get()), (Some(t), Some(s)) if s.elapsed() > t)
    };

    // Nested: an outer calmrun already holds points for us.
    if let Some(held) = std::env::var("CALMRUN_HELD")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
    {
        if held != std::process::id() && ctx.sys.start_time(held).is_some() {
            let t0 = Instant::now();
            started_at.set(Some(t0));
            match prepare(&req, &cfg).and_then(|c| runner::run(c, false, |_| {}, &mut watch)) {
                Ok(e) => {
                    out.code = e.code;
                    out.status = e.code.to_string();
                }
                Err(e) => {
                    let (c, m) = spawn_error(&req.argv, &e);
                    out.code = c;
                    out.message = Some(m);
                    out.status = "spawn-failed".into();
                }
            }
            out.run_ms = t0.elapsed().as_millis() as u64;
            return out;
        }
    }

    let det = detect::detect(&req.argv, &cfg.kinds);
    let kind = req.kind.clone().unwrap_or_else(|| det.kind_name().to_string());
    let cost = scheduler::effective_cost(
        req.cost.unwrap_or_else(|| detect::kind_cost(&kind, &cfg.kinds)),
        cfg.budget,
    );
    let label = req
        .label
        .clone()
        .filter(|l| !l.trim().is_empty())
        .unwrap_or_else(|| {
            if det.label.is_empty() {
                "command".into()
            } else {
                det.label.clone()
            }
        });
    out.label = label.clone();
    out.kind = kind.clone();
    out.cost = cost;
    let cwd = req
        .cwd
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    let keys = detect::exclusive_keys(
        &detect::Detected {
            kind: Some(kind.clone()),
            ..det.clone()
        },
        &cwd,
    );

    let store = match ctx.store() {
        Ok(s) => s,
        Err(e) => {
            out.code = 1;
            out.status = "error".into();
            out.message = Some(format!(
                "calmrun: cannot open state dir {}: {e}",
                ctx.home.display()
            ));
            return out;
        }
    };
    let me = std::process::id();
    let my_start = ctx.sys.start_time(me).unwrap_or(0);
    let queued_ms = now_ms();
    let mut entry = Job {
        id: 0,
        pid: me,
        pid_start: my_start,
        label: label.clone(),
        kind: kind.clone(),
        cost,
        keys,
        cwd: cwd.to_string_lossy().to_string(),
        high: req.high,
        order: 0,
        queued_ms,
        started_ms: None,
        child_pid: None,
        parked_until: None,
        mem_bytes: None,
    };
    let sys = &mut ctx.sys;
    let ticket = req.ticket;
    let enq = store.with_lock(|s| {
        prune_dead(s, sys);
        if let Some(t) = ticket {
            if let Some(j) = s.jobs.iter_mut().find(|j| j.id == t && j.parked()) {
                // Resume the kept place: same id, order and queue time.
                entry.id = j.id;
                entry.order = j.order;
                entry.queued_ms = j.queued_ms;
                entry.high = entry.high || j.high;
                *j = entry.clone();
                return (entry.clone(), true);
            }
        }
        s.next_id += 1;
        entry.id = s.next_id;
        entry.order = s.next_order();
        s.jobs.push(entry.clone());
        (entry.clone(), false)
    });
    let entry = match enq {
        Ok((e, resumed)) => {
            if let (Some(t), false) = (ticket, resumed) {
                notify.note(&format!(
                    "calmrun: ticket {t} has expired or is unknown; joining the end of the line"
                ));
            }
            e
        }
        Err(e) => {
            out.code = 1;
            out.status = "error".into();
            out.message = Some(format!("calmrun: cannot lock queue state: {e}"));
            return out;
        }
    };
    let id = entry.id;
    let queued_ms = entry.queued_ms;
    let lim = ctx.limits();
    let wait_started = Instant::now();

    // Wait for our turn.
    let mut last_line = String::from("waiting");
    let started_ms = loop {
        if cancelled(&req) {
            let _ = store.with_lock(|s| s.jobs.retain(|j| j.id != id));
            out.code = 130;
            out.status = "cancelled".into();
            out.wait_ms = now_ms().saturating_sub(queued_ms);
            out.message = Some("calmrun: cancelled while waiting; left the queue".into());
            return out;
        }
        if req.wait_timeout.is_some_and(|t| wait_started.elapsed() >= t) {
            let until = now_ms() + TICKET_TTL_MS;
            let _ = store.with_lock(|s| {
                if let Some(j) = s.jobs.iter_mut().find(|j| j.id == id) {
                    j.parked_until = Some(until);
                }
            });
            let secs = req.wait_timeout.map_or(0, |t| t.as_secs());
            let mut msg = format!(
                "calmrun: not started within {secs}s ({}); your place in line is kept for 10 min.",
                last_line.trim_start_matches("calmrun: ")
            );
            if let Some(h) = &req.rerun_hint {
                msg.push_str(&format!(
                    "\ncalmrun: next: {}",
                    h.replace("{ticket}", &id.to_string())
                ));
            }
            out.code = EXIT_WAIT_TIMEOUT;
            out.status = "wait-timeout".into();
            out.wait_ms = now_ms().saturating_sub(queued_ms);
            out.message = Some(msg);
            out.ticket = Some(id);
            return out;
        }
        let free = ctx.sys.free_ram();
        let now = now_ms();
        let sys = &mut ctx.sys;
        let step = store.with_lock(|s| {
            prune_dead(s, sys);
            if !s.jobs.iter().any(|j| j.id == id) {
                // Our entry vanished (state file removed by hand); put it back.
                s.jobs.push(entry.clone());
            }
            let (admitted, blocked) = scheduler::plan(&s.jobs, now, free, lim);
            if admitted.contains(&id) {
                if let Some(j) = s.jobs.iter_mut().find(|j| j.id == id) {
                    j.started_ms = Some(now);
                }
                return Ok(now);
            }
            Err(waiting_line(s, id, &entry, &blocked, lim, cost))
        });
        match step {
            Ok(Ok(t)) => break t,
            Ok(Err(line)) => {
                notify.waiting(&line);
                last_line = line;
            }
            Err(e) => {
                out.code = 1;
                out.status = "error".into();
                out.message = Some(format!("calmrun: cannot lock queue state: {e}"));
                return out;
            }
        }
        std::thread::sleep(POLL);
    };

    out.wait_ms = started_ms.saturating_sub(queued_ms);
    notify.started(out.wait_ms);
    started_at.set(Some(Instant::now()));
    let mut mem = memwatch::MemWatch::new(cfg.ceiling_bytes, cfg.critical_free_bytes);
    let stop_reason: std::cell::RefCell<Option<memwatch::Stop>> = std::cell::RefCell::new(None);
    let mut samples = 0u64;
    let mut full_watch = |pid: u32| -> bool {
        if watch(pid) {
            return true;
        }
        let Some(m) = mem.as_mut() else { return false };
        // Only the biggest running job is stopped when free RAM is critical.
        let mut biggest = |mine: u64| {
            store
                .with_lock(|s| {
                    s.jobs
                        .iter()
                        .filter(|j| j.running() && j.id != id)
                        .all(|j| j.mem_bytes.unwrap_or(0) <= mine)
                })
                .unwrap_or(true)
        };
        let (fresh, stop) = m.check(pid, &mut biggest);
        if fresh {
            samples += 1;
            if samples % 2 == 1 {
                let used = m.used;
                let _ = store.with_lock(|s| {
                    if let Some(j) = s.jobs.iter_mut().find(|j| j.id == id) {
                        j.mem_bytes = Some(used);
                    }
                });
            }
        }
        match stop {
            Some(r) => {
                *stop_reason.borrow_mut() = Some(r);
                true
            }
            None => false,
        }
    };
    let result = prepare(&req, &cfg).and_then(|mut cmd| {
        cmd.env("CALMRUN_HELD", me.to_string());
        runner::run(
            cmd,
            cfg.low_cpu,
            |child| {
                let _ = store.with_lock(|s| {
                    if let Some(j) = s.jobs.iter_mut().find(|j| j.id == id) {
                        j.child_pid = Some(child);
                    }
                });
            },
            &mut full_watch,
        )
    });
    let stop_reason = stop_reason.into_inner();
    out.run_ms = now_ms().saturating_sub(started_ms);
    let _ = store.with_lock(|s| s.jobs.retain(|j| j.id != id));
    match result {
        Ok(e) if e.stopped && stop_reason.is_some() => {
            let r = stop_reason.as_ref().expect("checked");
            out.code = e.code;
            out.status = r.status().into();
            out.message = Some(r.message(&label));
        }
        Ok(e) if e.stopped && req.cancel.as_ref().is_some_and(|c| c.load(Ordering::SeqCst)) => {
            out.code = 130;
            out.status = "cancelled".into();
            out.message = Some(format!("calmrun: stopped {label}: the request was cancelled"));
        }
        Ok(e) if e.stopped => {
            out.code = 124;
            out.status = "timeout".into();
            out.message = Some(format!(
                "calmrun: stopped {label}: still running after the {}s timeout",
                timeout.map_or(0, |t| t.as_secs())
            ));
        }
        Ok(e) if runner::INTERRUPTED.load(Ordering::SeqCst) => {
            out.code = e.code;
            out.status = "interrupted".into();
        }
        Ok(e) => {
            out.code = e.code;
            out.status = e.code.to_string();
        }
        Err(e) => {
            let (c, m) = spawn_error(&req.argv, &e);
            out.code = c;
            out.message = Some(m);
            out.status = "spawn-failed".into();
        }
    }
    let _ = history::append(
        &store.history_path(),
        &history::Record {
            at: history::format_utc(now_ms()),
            label,
            kind,
            cost,
            wait_ms: out.wait_ms,
            run_ms: out.run_ms,
            status: out.status.clone(),
        },
    );
    out
}

fn waiting_line(s: &State, id: u64, entry: &Job, blocked: &[(u64, Block)], lim: Limits, cost: u32) -> String {
    let used = scheduler::used_points(&s.jobs, lim.budget);
    let place = scheduler::waiting_in_order(&s.jobs)
        .iter()
        .position(|j| j.id == id)
        .unwrap_or(0)
        + 1;
    let why = blocked
        .iter()
        .find(|(b, _)| *b == id)
        .map(|(_, r)| r.clone())
        .unwrap_or(Block::Order);
    match why {
        Block::Ram { free, needs } => format!(
            "calmrun: waiting for RAM: {:.1} GB free, needs {:.1} GB",
            bytes_to_gb(free),
            bytes_to_gb(needs)
        ),
        Block::Key => {
            let holder = s
                .jobs
                .iter()
                .find(|j| j.running() && j.keys.iter().any(|k| entry.keys.contains(k)))
                .map(|j| j.label.clone())
                .unwrap_or_default();
            format!(
                "calmrun: waiting — same dependency folder is in use by \"{holder}\", {} in line",
                ordinal(place)
            )
        }
        Block::Points { .. } | Block::Order => format!(
            "calmrun: waiting — {used} of {} points in use, {} in line (needs {cost})",
            lim.budget,
            ordinal(place)
        ),
    }
}

fn job_json(j: &Job, now: u64, place: Option<usize>) -> serde_json::Value {
    json!({
        "id": j.id,
        "label": j.label,
        "kind": j.kind,
        "cost": j.cost,
        "pid": j.pid,
        "child_pid": j.child_pid,
        "mem_bytes": j.mem_bytes,
        "cwd": j.cwd,
        "high": j.high,
        "since_ms": j.started_ms.unwrap_or(j.queued_ms),
        "for_ms": now.saturating_sub(j.started_ms.unwrap_or(j.queued_ms)),
        "place": place,
    })
}

/// A pruned snapshot of the queue.
pub fn snapshot(ctx: &mut Ctx) -> io::Result<State> {
    let store = ctx.store()?;
    let sys = &mut ctx.sys;
    store.with_lock(|s| {
        prune_dead(s, sys);
        s.clone()
    })
}

/// The data behind `status --json` and the MCP `queue_status` tool.
pub fn status_json(ctx: &mut Ctx, state: &State) -> serde_json::Value {
    let now = now_ms();
    let free = ctx.sys.free_ram();
    let budget = ctx.cfg.budget;
    let used = scheduler::used_points(&state.jobs, budget);
    let running: Vec<&Job> = state.jobs.iter().filter(|j| j.running()).collect();
    let waiting = scheduler::waiting_in_order(&state.jobs);
    json!({
        "budget": budget,
        "used": used,
        "free_ram_bytes": free,
        "min_free_bytes": ctx.cfg.min_free_bytes,
        "running": running.iter().map(|j| job_json(j, now, None)).collect::<Vec<_>>(),
        "waiting": waiting.iter().enumerate().map(|(i, j)| job_json(j, now, Some(i + 1))).collect::<Vec<_>>(),
        "parked": state.jobs.iter().filter(|j| j.parked()).map(|j| json!({
            "ticket": j.id,
            "label": j.label,
            "kind": j.kind,
            "cost": j.cost,
            "cwd": j.cwd,
            "expires_in_ms": j.parked_until.unwrap_or(0).saturating_sub(now),
        })).collect::<Vec<_>>(),
        "warnings": stuck_warnings(state, now),
    })
}

pub fn stuck_warnings(state: &State, now: u64) -> Vec<String> {
    scheduler::waiting_in_order(&state.jobs)
        .iter()
        .filter(|j| now.saturating_sub(j.queued_ms) > STUCK_MS)
        .map(|j| {
            format!(
                "\"{}\" (pid {}) has waited {} — is something stuck? Check the running jobs \
                 with `calmrun status`, or move it forward with `calmrun bump {}`",
                j.label,
                j.pid,
                history::human_ms(now - j.queued_ms),
                j.label
            )
        })
        .collect()
}

/// Move waiting jobs whose label contains `needle` to the front. Returns how many moved.
pub fn bump(ctx: &mut Ctx, needle: &str) -> io::Result<usize> {
    let needle = needle.to_lowercase();
    let store = ctx.store()?;
    let sys = &mut ctx.sys;
    store.with_lock(|s| {
        prune_dead(s, sys);
        let min = s.jobs.iter().map(|j| j.order).min().unwrap_or(0);
        let ids: Vec<u64> = scheduler::waiting_in_order(&s.jobs)
            .into_iter()
            .filter(|j| j.label.to_lowercase().contains(&needle))
            .map(|j| j.id)
            .collect();
        let n = ids.len() as i64;
        for (i, id) in ids.iter().enumerate() {
            if let Some(j) = s.jobs.iter_mut().find(|j| j.id == *id) {
                j.high = true;
                j.order = min - n + i as i64;
            }
        }
        ids.len()
    })
}

/// Directory for captured logs (MCP runs).
pub fn log_dir(home: &Path) -> PathBuf {
    home.join("logs")
}

/// Keep the newest `keep` log files.
pub fn prune_logs(dir: &Path, keep: usize) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<(SystemTime, PathBuf)> = rd
        .flatten()
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    if files.len() <= keep {
        return;
    }
    files.sort_by_key(|f| std::cmp::Reverse(f.0));
    for (_, p) in files.into_iter().skip(keep) {
        let _ = std::fs::remove_file(p);
    }
}

/// The last `n` lines of a file (lossy UTF-8).
pub fn tail_lines(path: &Path, n: usize) -> String {
    let bytes = std::fs::read(path).unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}
