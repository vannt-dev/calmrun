//! The service every agent entry point uses (CLI, MCP): submit a job, wait for its turn, run
//! it, record it. [`run`] is the whole flow; each phase is its own function below.

use crate::config::{self, bytes_to_gb, Config};
use crate::detect::{self, Detected};
use crate::error::{Error, Result};
use crate::kinds::Workers;
use crate::memwatch::{self, MemWatch};
use crate::messages::{self, WaitContext};
use crate::scheduler::{self, Inputs, Limits};
use crate::state::{self, Job, State, Store};
use crate::sys::{DiskCheck, Sys};
use crate::{history, runner};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// First poll interval while waiting; it grows while nothing changes.
const POLL_MIN: Duration = Duration::from_millis(250);
/// Longest poll interval while waiting.
const POLL_MAX: Duration = Duration::from_millis(1500);
/// Cancellation is noticed within this, even during a long poll interval.
const CANCEL_CHECK: Duration = Duration::from_millis(100);
/// The CLI repeats the waiting line this often.
const MESSAGE_EVERY: Duration = Duration::from_secs(30);
/// A free-RAM reading in the state is reused for this long by every waiter.
const RAM_SAMPLE_TTL_MS: u64 = 1_000;
/// RAM a just-started job is assumed to take per point until it shows in the reading.
const RAM_RESERVE_PER_POINT: u64 = 1 << 30;
/// How long that assumption fades out over.
const RAM_RESERVE_WINDOW_MS: u64 = 20_000;
/// Publish the tree's memory to the state every this many samples.
const MEM_PUBLISH_EVERY: u64 = 2;

/// How long a ticket keeps its place without a re-run.
pub const TICKET_TTL_MS: u64 = 10 * 60_000;
/// Not started within `--wait-timeout` (EX_TEMPFAIL).
pub const EXIT_WAIT_TIMEOUT: i32 = 75;
/// Refused (attached `docker compose up`).
pub const EXIT_REFUSED: i32 = 2;
/// Stopped by the MCP `timeout_s`.
pub const EXIT_TIMEOUT: i32 = 124;

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Loaded settings plus a system-info handle.
pub struct Ctx {
    pub cfg: Config,
    pub home: PathBuf,
    pub config_path: PathBuf,
    pub machine: config::Machine,
    pub sys: Sys,
    /// A separate queue (`--id`); `None` is the default machine-wide queue.
    pub queue: Option<String>,
}

/// A queue id may hold letters, digits, `-`, `_` and `.` (not leading).
pub fn valid_queue_id(id: &str) -> bool {
    const MAX_LEN: usize = 64;
    !id.is_empty()
        && id.len() <= MAX_LEN
        && !id.starts_with('.')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

impl Ctx {
    /// Read the config (a broken file is reported and ignored) and the machine.
    pub fn load() -> Result<Ctx> {
        let mut sys = Sys::new()?;
        let home = config::home_dir();
        let config_path = config::config_path(&home);
        let machine = sys.machine();
        let file = config::read_file_config(&config_path).unwrap_or_else(|e| {
            eprintln!("calmrun: ignoring config file: {e}");
            config::FileConfig::default()
        });
        let cfg = config::resolve(machine, file, &|k| std::env::var(k).ok());
        Ok(Ctx {
            cfg,
            home,
            config_path,
            machine,
            sys,
            queue: None,
        })
    }

    /// Use a separate queue.
    pub fn with_queue(mut self, queue: Option<String>) -> Result<Ctx> {
        if let Some(q) = queue.as_deref().filter(|q| !valid_queue_id(q)) {
            return Err(Error::InvalidQueue(q.to_string()));
        }
        self.queue = queue;
        Ok(self)
    }

    /// The scheduler limits from the config.
    pub fn limits(&self) -> Limits {
        Limits {
            budget: self.cfg.budget,
            min_free_bytes: self.cfg.min_free_bytes,
            pass_limit_ms: self.cfg.pass_limit_ms,
            ram_per_point: RAM_RESERVE_PER_POINT,
            reserve_window_ms: RAM_RESERVE_WINDOW_MS,
        }
    }

    /// The files of the selected queue.
    pub fn store(&self) -> Result<Store> {
        let opened = match &self.queue {
            Some(id) => Store::open_queue(&self.home, id),
            None => Store::open(&self.home),
        };
        opened.map_err(|source| Error::StateDir {
            path: self.home.clone(),
            source,
        })
    }
}

/// Which (pid, start time) pairs are alive, read *before* taking the lock so the slow
/// process refresh never happens while other calmruns wait for it.
pub struct Liveness(HashMap<(u32, u64), bool>);

impl Liveness {
    /// Check the owners of the entries currently in `store` (an unlocked peek).
    pub fn check(store: &Store, sys: &mut Sys) -> Liveness {
        let pairs: Vec<(u32, u64)> = store
            .peek()
            .jobs
            .iter()
            .filter(|j| !j.parked())
            .map(|j| (j.pid, j.pid_start))
            .collect();
        let alive = sys.alive_set(&pairs);
        Liveness(pairs.into_iter().zip(alive).collect())
    }

    /// Drop entries whose owner died (pid gone or reused) and expired tickets. An entry that
    /// appeared after the check is kept until the next one.
    pub fn prune(&self, s: &mut State) {
        state::prune(s, now_ms(), |pid, start| {
            self.0.get(&(pid, start)).copied().unwrap_or(true)
        });
    }
}

/// Run `f` on the pruned state under the lock.
pub fn locked<R>(store: &Store, sys: &mut Sys, f: impl FnOnce(&mut State) -> R) -> Result<R> {
    let live = Liveness::check(store, sys);
    store
        .with_lock(|s| {
            live.prune(s);
            f(s)
        })
        .map_err(Error::State)
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
    /// Extra environment for the command.
    pub env: Vec<(String, String)>,
    /// Named resources to hold one slot of each (`--needs db`).
    pub needs: Vec<String>,
}

/// What happened.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Outcome {
    pub code: i32,
    /// Exit code as text, or a word: refused, interrupted, cancelled, timeout, wait-timeout,
    /// ceiling, low-memory, spawn-failed, error.
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

impl Outcome {
    fn ended(mut self, code: i32, status: &str, message: Option<String>) -> Outcome {
        self.code = code;
        self.status = status.to_string();
        self.message = message;
        self
    }

    fn error(self, e: &Error) -> Outcome {
        self.ended(e.exit_code(), e.status(), Some(format!("calmrun: {e}")))
    }
}

/// Progress callbacks.
pub trait Notify {
    /// Whether [`Notify::waiting`] would use a line now (so it is not built for nothing).
    fn wants_line(&self) -> bool {
        true
    }
    /// The current waiting line.
    fn waiting(&mut self, line: &str);
    /// Called once when the command starts.
    fn started(&mut self, _wait_ms: u64) {}
    /// A one-off note for the user.
    fn note(&mut self, line: &str) {
        eprintln!("{line}");
    }
}

/// Prints the waiting line at once (with a hint) and then every 30 s.
#[derive(Default)]
pub struct StderrNotify {
    last: Option<Instant>,
}

impl Notify for StderrNotify {
    fn wants_line(&self) -> bool {
        self.last.is_none_or(|t| t.elapsed() >= MESSAGE_EVERY)
    }

    fn waiting(&mut self, line: &str) {
        if !self.wants_line() {
            return;
        }
        eprintln!("{line}");
        if self.last.is_none() {
            eprintln!("{}", messages::STARTS_BY_ITSELF);
        }
        self.last = Some(Instant::now());
    }
}

/// A request after detection: what will run and how it is booked.
struct Prepared {
    req: Request,
    /// Runs at once without the queue (dev server, help output): why.
    light: Option<&'static str>,
    det: Detected,
    kind: String,
    cost: u32,
    label: String,
    cwd: PathBuf,
}

impl Prepared {
    fn outcome(&self) -> Outcome {
        Outcome {
            label: self.label.clone(),
            kind: self.kind.clone(),
            cost: self.cost,
            ..Outcome::default()
        }
    }

    fn cancelled(&self) -> bool {
        runner::INTERRUPTED.load(Ordering::SeqCst) || self.cancel_requested()
    }

    fn cancel_requested(&self) -> bool {
        self.req.cancel.as_ref().is_some_and(|c| c.load(Ordering::SeqCst))
    }
}

/// Run the command for `req` through the queue.
pub fn run(ctx: &mut Ctx, req: Request, notify: &mut dyn Notify) -> Outcome {
    let p = match prepare(ctx, req) {
        Ok(p) => p,
        Err(out) => return *out,
    };
    if let Some(what) = p.light {
        notify.note(&format!(
            "calmrun: {} ({what}): runs now, outside the queue",
            p.label
        ));
        return run_nested(&p);
    }
    if held_by_outer(ctx) {
        return run_nested(&p);
    }
    let store = match ctx.store() {
        Ok(s) => s,
        Err(e) => return p.outcome().error(&e),
    };
    let entry = match enqueue(ctx, &store, &p, notify) {
        Ok(e) => e,
        Err(e) => return p.outcome().error(&e),
    };
    let started_ms = match wait_turn(ctx, &store, &p, &entry, notify) {
        Ok(t) => t,
        Err(out) => return *out,
    };
    let out = execute(ctx, &store, &p, &entry, started_ms, notify);
    record(&store, ctx.queue.as_deref(), &out);
    out
}

/// Refusals, detection, hints and booking details.
fn prepare(ctx: &Ctx, mut req: Request) -> std::result::Result<Prepared, Box<Outcome>> {
    let cfg = &ctx.cfg;
    // Only the CLI shares our stdin with the command (and the MCP server holds its lock).
    let tty = req.log.is_none() && std::io::IsTerminal::is_terminal(&std::io::stdin());
    let mut light = None;
    match detect::disposition(&req.argv, tty) {
        detect::Disposition::Queue => {}
        detect::Disposition::Light(what) => light = Some(what),
        detect::Disposition::Refuse { reason, fixed } => {
            // The same one-shot form the agent hook suggests (`npx vitest run`).
            let fixed = fixed.map(|f| detect::accepted_form(f.clone()).unwrap_or(f));
            let msg = messages::refused(&reason, fixed.as_deref(), &runner::program_name());
            return Err(Box::new(Outcome::default().ended(
                EXIT_REFUSED,
                "refused",
                Some(msg),
            )));
        }
    }
    let det = detect::detect(&req.argv, &cfg.kinds);
    let kind = req.kind.clone().unwrap_or_else(|| det.kind_name().to_string());
    let rule = crate::kinds::find_rule(&det.tool, &det.rest);
    let cost = scheduler::effective_cost(
        req.cost
            .unwrap_or_else(|| crate::kinds::job_cost(&kind, rule, &cfg.kinds)),
        cfg.budget,
    );
    let label = match req.label.as_deref().map(str::trim) {
        Some(l) if !l.is_empty() => l.to_string(),
        _ if det.label.is_empty() => "command".into(),
        _ => det.label.clone(),
    };
    let workers = |w: Workers| match w {
        Workers::Vitest => cfg.vitest_workers,
        Workers::CoreShare => tool_workers(ctx.machine.logical_cores, cost, cfg.budget),
    };
    let cwd = req
        .cwd
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    let hints = detect::apply_hints(&req.argv, &det, &cwd, &workers, &|v| std::env::var(v).ok());
    // A dev server or app run keeps its arguments (`turbo run dev` refuses a concurrency
    // below its number of persistent tasks); environment hints still apply.
    if let Some(a) = hints.argv.filter(|_| light.is_none()) {
        req.argv = a;
    }
    req.env.extend(hints.env);
    // Resources the tool itself needs (an Android device test needs the emulator).
    if let Some(r) = rule.filter(|r| r.kind == kind) {
        req.needs.extend(r.needs.iter().map(|n| n.to_string()));
    }
    req.needs = normalized(&req.needs);
    Ok(Prepared {
        req,
        light,
        det,
        kind,
        cost,
        label,
        cwd,
    })
}

fn normalized(needs: &[String]) -> Vec<String> {
    let mut n: Vec<String> = needs
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    n.sort();
    n.dedup();
    n
}

/// Worker count for a tool: the job's share of the logical cores.
pub fn tool_workers(cores: usize, cost: u32, budget: u32) -> u32 {
    ((cores as u64 * cost as u64) / budget.max(1) as u64).max(1) as u32
}

/// True when an outer calmrun (still alive) already holds points for us.
fn held_by_outer(ctx: &mut Ctx) -> bool {
    std::env::var("CALMRUN_HELD")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .is_some_and(|held| held != std::process::id() && ctx.sys.start_time(held).is_some())
}

/// Nested run: no queueing, no history (the outer run records it).
fn run_nested(p: &Prepared) -> Outcome {
    let t0 = Instant::now();
    let mut out = spawn_and_finish(p, Watcher::new(p, None, None, 0), false, None, |_| {});
    out.run_ms = t0.elapsed().as_millis() as u64;
    out
}

/// Spawn the command, watch it, and turn how it ended into an outcome. `held_by` marks the
/// command as running inside our points (`CALMRUN_HELD`).
fn spawn_and_finish(
    p: &Prepared,
    mut watch: Watcher,
    low_cpu: bool,
    held_by: Option<u32>,
    on_spawn: impl FnOnce(u32),
) -> Outcome {
    let result = command(p).and_then(|mut cmd| {
        if let Some(pid) = held_by {
            cmd.env("CALMRUN_HELD", pid.to_string());
        }
        runner::run(cmd, low_cpu, on_spawn, &mut |pid| watch.check(pid))
    });
    finish(p, result, watch.reason)
}

/// The command, ready to spawn.
fn command(p: &Prepared) -> std::io::Result<Command> {
    let req = &p.req;
    let mut cmd = match (req.shell, req.argv.first()) {
        (true, Some(line)) => runner::shell_command(line),
        _ => runner::build(&req.argv, &p.cwd)?,
    };
    if let Some(d) = &req.cwd {
        cmd.current_dir(d);
    }
    for (k, v) in &req.env {
        cmd.env(k, v);
    }
    if let Some(log) = &req.log {
        if let Some(parent) = log.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let f = std::fs::File::create(log)?;
        cmd.stdin(Stdio::null()).stdout(f.try_clone()?).stderr(f);
    }
    Ok(cmd)
}

/// Add our entry, or take back a parked one with `--ticket`.
fn enqueue(ctx: &mut Ctx, store: &Store, p: &Prepared, notify: &mut dyn Notify) -> Result<Job> {
    let me = std::process::id();
    let mut entry = Job {
        pid: me,
        pid_start: ctx.sys.start_time(me).unwrap_or(0),
        label: p.label.clone(),
        kind: p.kind.clone(),
        cost: p.cost,
        keys: detect::exclusive_keys(&p.kind, &p.det.tool, &p.det.rest, &p.cwd),
        cwd: p.cwd.to_string_lossy().to_string(),
        high: p.req.high,
        queued_ms: now_ms(),
        needs: p.req.needs.clone(),
        ..Job::default()
    };
    let ticket = p.req.ticket;
    let resumed = locked(store, &mut ctx.sys, |s| {
        if let Some(j) = ticket.and_then(|t| s.jobs.iter_mut().find(|j| j.id == t && j.parked())) {
            // Resume the kept place: same id, order and queue time.
            entry.id = j.id;
            entry.order = j.order;
            entry.queued_ms = j.queued_ms;
            entry.high |= j.high;
            *j = entry.clone();
            return true;
        }
        s.next_id += 1;
        entry.id = s.next_id;
        entry.order = s.next_order();
        s.jobs.push(entry.clone());
        false
    })?;
    if let (Some(t), false) = (ticket, resumed) {
        notify.note(&format!(
            "calmrun: ticket {t} has expired or is unknown; joining the end of the line"
        ));
    }
    Ok(entry)
}

/// What one poll found.
enum Tick {
    Started(u64),
    Waiting {
        line: Option<String>,
        fingerprint: (usize, usize, usize),
    },
}

/// Poll until the scheduler admits us. Returns the start time, or the outcome when the wait
/// ends otherwise (cancelled, wait timeout, state error).
fn wait_turn(
    ctx: &mut Ctx,
    store: &Store,
    p: &Prepared,
    entry: &Job,
    notify: &mut dyn Notify,
) -> std::result::Result<u64, Box<Outcome>> {
    let wait_started = Instant::now();
    let mut disk = DiskCheck::new(&p.cwd, ctx.cfg.min_free_disk_bytes);
    let mut last_line = String::from("waiting");
    let mut poll = POLL_MIN;
    let mut last_fp = None;
    loop {
        if p.cancelled() {
            let _ = store.with_lock(|s| s.jobs.retain(|j| j.id != entry.id));
            let msg = "calmrun: cancelled while waiting; left the queue".to_string();
            return Err(Box::new(waited(p, entry).ended(
                runner::EXIT_INTERRUPTED,
                "cancelled",
                Some(msg),
            )));
        }
        let timed_out = p.req.wait_timeout.is_some_and(|t| wait_started.elapsed() >= t);
        let want_line = timed_out || notify.wants_line();
        let short = disk.short();
        let input = TickInput {
            store,
            entry,
            disk: &disk,
            short,
            want_line,
        };
        let tick = tick(ctx, &input).map_err(|e| Box::new(p.outcome().error(&e)))?;
        match tick {
            Tick::Started(t) => {
                if short {
                    notify.note(&disk_warning(&disk));
                }
                return Ok(t);
            }
            Tick::Waiting { line, fingerprint } => {
                if let Some(l) = line {
                    notify.waiting(&l);
                    last_line = l;
                }
                if timed_out {
                    return Err(Box::new(park(store, p, entry, &last_line)));
                }
                poll = if last_fp == Some(fingerprint) {
                    (poll * 3 / 2).min(POLL_MAX)
                } else {
                    POLL_MIN
                };
                last_fp = Some(fingerprint);
            }
        }
        sleep_unless_cancelled(poll, p);
    }
}

/// What one poll needs besides the context.
struct TickInput<'a> {
    store: &'a Store,
    entry: &'a Job,
    disk: &'a DiskCheck,
    short: bool,
    want_line: bool,
}

/// One locked look at the queue: prune, publish our disk state, read (or reuse) the shared
/// free-RAM sample, schedule.
fn tick(ctx: &mut Ctx, t: &TickInput) -> Result<Tick> {
    let now = now_ms();
    let limits = ctx.limits();
    let live = Liveness::check(t.store, &mut ctx.sys);
    let (caps, sys, entry) = (&ctx.cfg.resources, &mut ctx.sys, t.entry);
    t.store
        .with_lock(|s| {
            live.prune(s);
            match s.jobs.iter_mut().find(|j| j.id == entry.id) {
                Some(j) => j.disk_short = t.short,
                // Our entry vanished (state file removed by hand); put it back.
                None => s.jobs.push(Job {
                    disk_short: t.short,
                    ..entry.clone()
                }),
            }
            let free_ram = shared_ram_sample(s, sys, now);
            let plan = scheduler::schedule(
                &s.jobs,
                &Inputs {
                    now_ms: now,
                    free_ram,
                    limits,
                    caps,
                },
            );
            if plan.admits(entry.id) {
                if let Some(j) = s.jobs.iter_mut().find(|j| j.id == entry.id) {
                    j.started_ms = Some(now);
                    j.disk_short = false;
                }
                return Tick::Started(now);
            }
            let waiting = scheduler::waiting_in_order(&s.jobs);
            let place = waiting.iter().position(|j| j.id == entry.id).unwrap_or(0);
            let running = s.jobs.len() - waiting.len();
            let line = t.want_line.then(|| {
                let c = WaitContext {
                    me: entry,
                    budget: limits.budget,
                    disk: t.disk,
                    caps,
                    now_ms: now,
                };
                messages::waiting_line(s, &plan.reason(entry.id), &c)
            });
            Tick::Waiting {
                line,
                fingerprint: (s.jobs.len(), running, place),
            }
        })
        .map_err(Error::State)
}

/// The free-RAM reading every waiter uses: refreshed (under the lock) when older than a
/// second, so waiters polling at the same time decide on the same number.
fn shared_ram_sample(s: &mut State, sys: &mut Sys, now: u64) -> u64 {
    match s.ram_sample {
        Some(r) if now.saturating_sub(r.at_ms) < RAM_SAMPLE_TTL_MS => r.free,
        _ => {
            let free = sys.free_ram();
            s.ram_sample = Some(state::RamSample { free, at_ms: now });
            free
        }
    }
}

fn sleep_unless_cancelled(total: Duration, p: &Prepared) {
    let end = Instant::now() + total;
    while Instant::now() < end && !p.cancelled() {
        std::thread::sleep(CANCEL_CHECK.min(end.saturating_duration_since(Instant::now())));
    }
}

fn waited(p: &Prepared, entry: &Job) -> Outcome {
    Outcome {
        wait_ms: now_ms().saturating_sub(entry.queued_ms),
        ..p.outcome()
    }
}

/// `--wait-timeout` passed: keep the place for a ticket and say how to resume.
fn park(store: &Store, p: &Prepared, entry: &Job, last_line: &str) -> Outcome {
    let until = now_ms() + TICKET_TTL_MS;
    let _ = store.with_lock(|s| {
        if let Some(j) = s.jobs.iter_mut().find(|j| j.id == entry.id) {
            j.parked_until = Some(until);
        }
    });
    let secs = p.req.wait_timeout.map_or(0, |t| t.as_secs());
    let msg = messages::wait_timeout(
        secs,
        last_line,
        p.req.rerun_hint.as_deref(),
        entry.id,
        TICKET_TTL_MS / 60_000,
    );
    let mut out = waited(p, entry).ended(EXIT_WAIT_TIMEOUT, "wait-timeout", Some(msg));
    out.ticket = Some(entry.id);
    out
}

fn disk_warning(disk: &DiskCheck) -> String {
    format!(
        "calmrun: warning: only {:.1} GB free on {} (wanted {:.1} GB); starting anyway because \
         nothing else is running",
        bytes_to_gb(disk.free),
        disk.mount,
        bytes_to_gb(disk.need)
    )
}

/// Why the watcher stopped the command.
enum StopReason {
    Cancelled,
    Timeout(Duration),
    Memory(memwatch::Stop),
}

/// Asked every 250 ms while the command runs: cancel, timeout, memory.
struct Watcher<'a> {
    cancel: Option<&'a AtomicBool>,
    timeout: Option<Duration>,
    started: Instant,
    mem: Option<MemWatch>,
    /// Where to publish the tree's memory (not for nested runs).
    store: Option<&'a Store>,
    id: u64,
    samples: u64,
    reason: Option<StopReason>,
}

impl<'a> Watcher<'a> {
    fn new(p: &'a Prepared, mem: Option<MemWatch>, store: Option<&'a Store>, id: u64) -> Watcher<'a> {
        Watcher {
            cancel: p.req.cancel.as_deref(),
            timeout: p.req.timeout,
            started: Instant::now(),
            mem,
            store,
            id,
            samples: 0,
            reason: None,
        }
    }

    fn check(&mut self, pid: u32) -> bool {
        if self.cancel.is_some_and(|c| c.load(Ordering::SeqCst)) {
            self.reason = Some(StopReason::Cancelled);
        } else if let Some(t) = self.timeout.filter(|t| self.started.elapsed() > *t) {
            self.reason = Some(StopReason::Timeout(t));
        } else if let Some(stop) = self.check_memory(pid) {
            self.reason = Some(StopReason::Memory(stop));
        }
        self.reason.is_some()
    }

    fn check_memory(&mut self, pid: u32) -> Option<memwatch::Stop> {
        let mem = self.mem.as_mut()?;
        let (store, id) = (self.store, self.id);
        // Only the biggest running job is stopped when free RAM is critical.
        let mut biggest = |mine: u64| {
            store.is_none_or(|st| {
                st.with_lock(|s| {
                    s.jobs
                        .iter()
                        .filter(|j| j.running() && j.id != id)
                        .all(|j| beats(mine, id, j.mem_bytes.unwrap_or(0), j.id))
                })
                .unwrap_or(true)
            })
        };
        let (fresh, stop) = mem.check(pid, &mut biggest);
        if fresh {
            self.samples += 1;
            if self.samples % MEM_PUBLISH_EVERY == 1 {
                let used = mem.used;
                let _ = store.map(|st| {
                    st.with_lock(|s| {
                        if let Some(j) = s.jobs.iter_mut().find(|j| j.id == id) {
                            j.mem_bytes = Some(used);
                        }
                    })
                });
            }
        }
        stop
    }
}

/// Whether a job (`mine`, `id`) is bigger than another one: only the single biggest job is
/// stopped for low free RAM, and on a tie the newer one (higher id) goes.
fn beats(mine: u64, id: u64, other: u64, other_id: u64) -> bool {
    other < mine || (other == mine && other_id < id)
}

/// Run the admitted command, then leave the queue.
fn execute(
    ctx: &Ctx,
    store: &Store,
    p: &Prepared,
    entry: &Job,
    started_ms: u64,
    notify: &mut dyn Notify,
) -> Outcome {
    let wait_ms = started_ms.saturating_sub(entry.queued_ms);
    notify.started(wait_ms);
    let cfg = &ctx.cfg;
    let mem = MemWatch::new(memwatch::Limits {
        ceiling: cfg.ceiling_bytes,
        floor: cfg.critical_free_bytes,
        min_stop: memwatch::min_stop_bytes(ctx.machine.total_ram),
    });
    let watch = Watcher::new(p, mem, Some(store), entry.id);
    let id = entry.id;
    let set_child = |child: u32| {
        let _ = store.with_lock(|s| {
            if let Some(j) = s.jobs.iter_mut().find(|j| j.id == id) {
                j.child_pid = Some(child);
            }
        });
    };
    let mut out = spawn_and_finish(p, watch, cfg.low_cpu, Some(std::process::id()), set_child);
    let _ = store.with_lock(|s| s.jobs.retain(|j| j.id != id));
    out.wait_ms = wait_ms;
    out.run_ms = now_ms().saturating_sub(started_ms);
    out
}

/// Turn how the command ended into an outcome.
fn finish(p: &Prepared, result: std::io::Result<runner::Ended>, reason: Option<StopReason>) -> Outcome {
    let out = p.outcome();
    let label = &p.label;
    match (result, reason) {
        (Err(source), _) => {
            let program = p.req.argv.first().cloned().unwrap_or_default();
            out.error(&Error::Spawn { program, source })
        }
        (Ok(e), Some(StopReason::Memory(stop))) if e.stopped => {
            out.ended(e.code, stop.status(), Some(stop.message(label)))
        }
        (Ok(e), Some(StopReason::Cancelled)) if e.stopped => out.ended(
            runner::EXIT_INTERRUPTED,
            "cancelled",
            Some(format!("calmrun: stopped {label}: the request was cancelled")),
        ),
        (Ok(e), Some(StopReason::Timeout(t))) if e.stopped => out.ended(
            EXIT_TIMEOUT,
            "timeout",
            Some(format!(
                "calmrun: stopped {label}: still running after the {}s timeout",
                t.as_secs()
            )),
        ),
        (Ok(e), _) if runner::INTERRUPTED.load(Ordering::SeqCst) => out.ended(e.code, "interrupted", None),
        (Ok(e), _) => out.ended(e.code, &e.code.to_string(), None),
    }
}

/// Append the run to `history.jsonl` (best effort: history is a convenience).
fn record(store: &Store, queue: Option<&str>, out: &Outcome) {
    let _ = history::append(
        store.history_path(),
        &history::Record {
            at: history::format_utc(now_ms()),
            label: out.label.clone(),
            kind: out.kind.clone(),
            cost: out.cost,
            wait_ms: out.wait_ms,
            run_ms: out.run_ms,
            status: out.status.clone(),
            queue: queue.map(str::to_string),
        },
    );
}

/// Directory for captured logs (MCP runs).
pub fn log_dir(home: &Path) -> PathBuf {
    home.join("logs")
}

/// Keep the newest `keep` log files.
pub fn prune_logs(dir: &Path, keep: usize) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
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
    lines[lines.len().saturating_sub(n)..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_workers_share_cores() {
        assert_eq!(tool_workers(20, 2, 3), 13);
        assert_eq!(tool_workers(8, 1, 6), 1);
        assert_eq!(tool_workers(2, 1, 4), 1);
    }

    #[test]
    fn only_the_single_biggest_job_counts_as_biggest() {
        assert!(beats(5, 2, 4, 1));
        assert!(!beats(4, 2, 5, 1));
        // A tie: the newer job (higher id) is the one to stop, never both.
        assert!(beats(5, 2, 5, 1));
        assert!(!beats(5, 1, 5, 2));
    }

    #[test]
    fn queue_ids() {
        assert!(valid_queue_id("e2e-runs_1.x"));
        for bad in ["", ".hidden", "../x", "a/b", "a b"] {
            assert!(!valid_queue_id(bad), "{bad}");
        }
    }

    #[test]
    fn needs_are_trimmed_sorted_unique() {
        let n = normalized(&[" db".into(), "gpu".into(), "db".into(), "".into()]);
        assert_eq!(n, vec!["db".to_string(), "gpu".to_string()]);
    }

    #[test]
    fn logs_tail_and_prune() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("a.log");
        std::fs::write(&f, "1\n2\n3\n4\n").unwrap();
        assert_eq!(tail_lines(&f, 2), "3\n4");
        for i in 0..5 {
            std::fs::write(tmp.path().join(format!("{i}.log")), "x").unwrap();
        }
        prune_logs(tmp.path(), 3);
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 3);
    }
}
