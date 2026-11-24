//! The service every agent entry point uses (CLI, MCP): submit a job, wait for its turn, run
//! it, record it. [`run`] is the whole flow; each phase is its own function below.

use crate::config::{self, bytes_to_gb, Config, LimitFrom};
use crate::detect::{self, Detected};
use crate::error::{Error, Result};
use crate::kinds::Workers;
use crate::memwatch::{self, MemWatch};
use crate::messages::{self, WaitContext};
use crate::scheduler::{self, Inputs, Limits};
use crate::state::{self, Job, Lease, OtherQueue, Settings, State, Store};
use crate::sys::{DiskCheck, Sys};
use crate::{history, joblog, pools, runner};
use std::collections::{BTreeMap, HashMap};
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
/// Publish the tree's memory and CPU to the state every this many samples.
const MEM_PUBLISH_EVERY: u64 = 2;
/// A running owner looks for a `calmrun stop` request this often.
const STOP_CHECK_EVERY: Duration = Duration::from_secs(1);
/// A waiter re-reads the usual run times from the history this often.
const USUAL_REFRESH: Duration = Duration::from_secs(300);

/// A separate queue (`--id`) left empty and unused this long is removed.
const IDLE_QUEUE_MS: u64 = 7 * 24 * 3_600_000;

/// How long a ticket keeps its place without a re-run.
pub const TICKET_TTL_MS: u64 = 10 * 60_000;
/// Not started within `--wait-timeout` (EX_TEMPFAIL).
pub const EXIT_WAIT_TIMEOUT: i32 = 75;
/// Refused (attached `docker compose up`).
pub const EXIT_REFUSED: i32 = 2;
/// Stopped by its run time limit (`--timeout`, MCP `timeout_s`, `max_run_min`).
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

    /// The scheduler limits from this process's own config.
    pub fn limits(&self) -> Limits {
        Limits {
            budget: self.cfg.budget,
            min_free_bytes: self.cfg.min_free_bytes,
            pass_limit_ms: self.cfg.pass_limit_ms,
            ram_per_point: RAM_RESERVE_PER_POINT,
            reserve_window_ms: RAM_RESERVE_WINDOW_MS,
        }
    }

    /// This process's settings, as a queue records them.
    pub fn settings(&self) -> Settings {
        Settings {
            budget: self.cfg.budget,
            min_free_bytes: self.cfg.min_free_bytes,
            pass_limit_ms: self.cfg.pass_limit_ms,
            pid: std::process::id(),
            at_ms: now_ms(),
        }
    }

    /// The files of the selected queue. Refused inside a sandbox that hides the other
    /// processes: every owner would look dead there.
    pub fn store(&self) -> Result<Store> {
        if crate::sys::in_app_container() {
            return Err(Error::Sandboxed);
        }
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

/// `own` limits with the settings the queue recorded, if any.
pub fn effective_limits(own: Limits, s: &State) -> Limits {
    match s.settings {
        Some(r) => Limits {
            budget: r.budget.max(1),
            min_free_bytes: r.min_free_bytes,
            pass_limit_ms: r.pass_limit_ms,
            ..own
        },
        None => own,
    }
}

/// Which (pid, start time) pairs are alive, read *before* taking the lock so the slow
/// process refresh never happens while other calmruns wait for it.
pub struct Liveness(HashMap<(u32, u64), bool>);

impl Liveness {
    /// Check the owners of the entries currently in `store` (an unlocked peek).
    pub fn check(store: &Store, sys: &mut Sys) -> Liveness {
        Liveness::check_with(store, &[], sys)
    }

    /// The same, plus the owners of the jobs running in `others`.
    pub fn check_with(store: &Store, others: &[OtherQueue], sys: &mut Sys) -> Liveness {
        let own = store.peek().jobs;
        let pairs: Vec<(u32, u64)> = own
            .iter()
            .filter(|j| !j.parked())
            .chain(
                others
                    .iter()
                    .flat_map(|o| o.state.jobs.iter().filter(|j| j.running())),
            )
            .map(|j| (j.pid, j.pid_start))
            .collect();
        let alive = sys.alive_set(&pairs);
        Liveness(pairs.into_iter().zip(alive).collect())
    }

    /// Whether the owner was alive at the check (an owner not checked counts as alive).
    pub fn alive(&self, pid: u32, start: u64) -> bool {
        self.0.get(&(pid, start)).copied().unwrap_or(true)
    }

    /// Drop entries whose owner died (pid gone or reused) and expired tickets. An entry that
    /// appeared after the check is kept until the next one. Returns the dead owners' entries.
    pub fn prune(&self, s: &mut State) -> Vec<Job> {
        state::prune(s, now_ms(), |pid, start| self.alive(pid, start)).dead
    }
}

/// The jobs running in other queues whose owner is alive, labelled with their queue.
pub fn foreign_running(others: &[OtherQueue], live: &Liveness) -> Vec<Job> {
    others
        .iter()
        .flat_map(|o| {
            let queue = o.queue.as_deref().unwrap_or("default");
            o.state
                .jobs
                .iter()
                .filter(|j| j.running() && live.alive(j.pid, j.pid_start))
                .map(move |j| Job {
                    label: format!("{} (queue {queue})", j.label),
                    ..j.clone()
                })
        })
        .collect()
}

/// Run `f` on the pruned state under the lock. Entries of killed owners get a history line.
pub fn locked<R>(store: &Store, sys: &mut Sys, f: impl FnOnce(&mut State) -> R) -> Result<R> {
    let live = Liveness::check(store, sys);
    let (out, dead) = store
        .with_lock(|s| {
            let dead = live.prune(s);
            (f(s), dead)
        })
        .map_err(Error::State)?;
    record_dead(store, &dead);
    Ok(out)
}

/// History lines for entries whose calmrun was killed (`taskkill /F`, `kill -9`, a closed
/// terminal): "killed" if the command was running, "abandoned" if it was still waiting.
/// Times run up to when the entry was found dead.
pub fn record_dead(store: &Store, dead: &[Job]) {
    let now = now_ms();
    for j in dead.iter().filter(|j| !j.is_hold()) {
        let (status, wait_ms, run_ms) = match j.started_ms {
            Some(s) => ("killed", s.saturating_sub(j.queued_ms), now.saturating_sub(s)),
            None => ("abandoned", now.saturating_sub(j.queued_ms), 0),
        };
        let out = Outcome {
            status: status.into(),
            label: j.label.clone(),
            kind: j.kind.clone(),
            cost: j.cost,
            wait_ms,
            run_ms,
            cwd: crate::watch::folder_key(j),
            job_id: Some(j.id),
            log: j.log_path.clone(),
            ..Outcome::default()
        };
        record(store, store.queue(), &out);
    }
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
    /// `--prio low`: after every normal and high waiter (`high` wins if both are set).
    pub low: bool,
    pub label: Option<String>,
    /// Stop the command after this long (exit 124); zero: no limit for this run. `None`
    /// takes the config's limit for the kind (queued runs only).
    pub timeout: Option<Duration>,
    /// Write stdout and stderr to this file (stdin is closed) instead of inheriting them.
    pub log: Option<PathBuf>,
    /// `calmrun --log`: copy stdout and stderr to the terminal and to this file (stdin stays).
    /// Only for a queued run; a light or nested one keeps the terminal.
    pub tee: Option<PathBuf>,
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
    /// How to show the fixed command after a refusal; `{command}` is replaced by it
    /// (default: `calmrun -- {command}`).
    pub refused_hint: Option<String>,
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
    /// The folder as a path key (for the history).
    pub cwd: String,
    /// The job's id in its queue and its log, once it ran (for `calmrun logs` afterwards).
    pub job_id: Option<u64>,
    pub log: Option<String>,
    /// The instance the job got of each resource that has instances (`android` ->
    /// `emulator-5554`).
    pub instances: BTreeMap<String, String>,
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
    /// Called once the job has its entry (and id) in the queue.
    fn queued(&mut self, _id: u64) {}
    /// Called once when the command starts.
    fn started(&mut self, _wait_ms: u64) {}
    /// A one-off note for the user.
    fn note(&mut self, line: &str) {
        eprintln!("{line}");
    }
    /// A next step that goes with the waiting line just given (a running job looks stuck).
    fn advice(&mut self, _line: &str) {}
}

/// Prints the waiting line at once (with a hint) and then every 30 s.
#[derive(Default)]
pub struct StderrNotify {
    last: Option<Instant>,
    /// The waiting line was just printed, so its advice goes with it.
    printed: bool,
}

impl Notify for StderrNotify {
    fn wants_line(&self) -> bool {
        self.last.is_none_or(|t| t.elapsed() >= MESSAGE_EVERY)
    }

    fn waiting(&mut self, line: &str) {
        self.printed = false;
        if !self.wants_line() {
            return;
        }
        eprintln!("{line}");
        if self.last.is_none() {
            eprintln!("{}", messages::STARTS_BY_ITSELF);
        }
        self.last = Some(Instant::now());
        self.printed = true;
    }

    fn advice(&mut self, line: &str) {
        if self.printed {
            eprintln!("{line}");
        }
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
    /// A `calmrun hold`: no command; admitted means a lease with this time to live.
    hold_ttl: Option<Duration>,
    /// The folder owning that lease (`hold --for`): as given, and as a path key.
    hold_for: Option<(String, String)>,
    /// `cwd` as a path key.
    cwd_key: String,
    /// The run time limit and where it comes from.
    limit: Option<(Duration, LimitFrom)>,
    /// The instances it got when admitted (resources that have instances).
    instances: BTreeMap<String, String>,
}

impl Prepared {
    fn outcome(&self) -> Outcome {
        Outcome {
            label: self.label.clone(),
            kind: self.kind.clone(),
            cost: self.cost,
            cwd: self.cwd_key.clone(),
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
    // Only a queued run gets the config's limit; a dev server or a nested run never does.
    let mut p = p;
    if p.req.timeout.is_none() {
        p.limit = config::run_limit(&ctx.cfg, &p.kind);
    }
    let store = match ctx.store() {
        Ok(s) => s,
        Err(e) => return p.outcome().error(&e),
    };
    let entry = match enqueue(ctx, &store, &p, notify) {
        Ok(e) => e,
        Err(e) => return p.outcome().error(&e),
    };
    notify.queued(entry.id);
    let started_ms = match wait_turn(ctx, &store, &p, &entry, notify) {
        Ok(Admitted::Started(t, instances)) => {
            if !instances.is_empty() {
                let env = pools::env_for(&instances, &ctx.cfg.pools);
                notify.note(&messages::instances_line(&instances, &env));
                p.req.env.extend(env);
                p.instances = instances;
            }
            t
        }
        Ok(Admitted::Leased(..)) => return p.outcome(),
        Err(out) => {
            // Ctrl+C, a cancelled MCP task or `calmrun stop` while waiting: it never started.
            if matches!(out.status.as_str(), "cancelled" | "stopped") {
                record(&store, ctx.queue.as_deref(), &out);
            }
            return *out;
        }
    };
    let out = execute(ctx, &store, &p, &entry, started_ms, notify);
    record(&store, ctx.queue.as_deref(), &out);
    state::sweep_idle_queues(&ctx.home, IDLE_QUEUE_MS);
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
            let msg = messages::refused(&reason, fixed.as_deref(), req.refused_hint.as_deref());
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
    if let Some(bad) = invalid_resource(&req.needs) {
        return Err(Box::new(Outcome::default().error(&bad)));
    }
    let limit = req
        .timeout
        .filter(|t| !t.is_zero())
        .map(|t| (t, LimitFrom::Asked));
    Ok(Prepared {
        req,
        light,
        det,
        kind,
        cost,
        label,
        cwd_key: crate::sys::path_key(&cwd),
        cwd,
        hold_ttl: None,
        hold_for: None,
        limit,
        instances: BTreeMap::new(),
    })
}

/// The first resource name that is not a plain name (same rule as queue ids).
fn invalid_resource(needs: &[String]) -> Option<Error> {
    needs
        .iter()
        .find(|n| !valid_queue_id(n))
        .map(|n| Error::InvalidResource(n.clone()))
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
        // Only a queued run (held_by) copies its output; see `Request::tee`.
        let tee = match p.req.tee.as_deref().filter(|_| held_by.is_some()) {
            Some(log) => Some(joblog::Tee::attach(&mut cmd, log)?),
            None => None,
        };
        let ended = runner::run(cmd, low_cpu, on_spawn, &mut |pid| watch.check(pid));
        if let Some(t) = tee {
            t.finish();
        }
        ended
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
        keys: match p.hold_ttl {
            Some(_) => Vec::new(),
            None => detect::exclusive_keys(&p.kind, &p.det.tool, &p.det.rest, &p.cwd),
        },
        cwd: p.cwd.to_string_lossy().to_string(),
        high: p.req.high,
        low: p.req.low && !p.req.high,
        queued_ms: now_ms(),
        needs: p.req.needs.clone(),
        hold_ttl_ms: p.hold_ttl.map(|t| t.as_millis() as u64),
        cwd_key: p.cwd_key.clone(),
        timeout_ms: p.limit.as_ref().map(|l| l.0.as_millis() as u64),
        use_leases: std::env::var("CALMRUN_LEASE")
            .unwrap_or_default()
            .split(',')
            .filter_map(crate::lease::parse_id)
            .collect(),
        ..Job::default()
    };
    let ticket = p.req.ticket;
    let mine = ctx.settings();
    let (resumed, recorded) = locked(store, &mut ctx.sys, |s| {
        // An empty queue takes the settings of the job that joins it; while it is busy every
        // waiter decides with those, whatever its own env or config says.
        if s.jobs.is_empty() || s.settings.is_none() {
            s.settings = Some(mine);
        }
        let recorded = s.settings.unwrap_or(mine);
        if let Some(j) = ticket.and_then(|t| s.jobs.iter_mut().find(|j| j.id == t && j.parked())) {
            // Resume the kept place: same id, order and queue time.
            entry.id = j.id;
            entry.order = j.order;
            entry.queued_ms = j.queued_ms;
            entry.high |= j.high;
            entry.bumped |= j.bumped;
            // A place pushed back stays at the back unless this run asks for prio high.
            entry.low = (entry.low || j.low) && !entry.high;
            *j = entry.clone();
            return (true, recorded);
        }
        s.next_id += 1;
        entry.id = s.next_id;
        entry.order = s.next_order();
        s.jobs.push(entry.clone());
        (false, recorded)
    })?;
    if let (Some(t), false) = (ticket, resumed) {
        notify.note(&format!(
            "calmrun: ticket {t} has expired or is unknown; joining the end of the line"
        ));
    }
    if !recorded.same_limits(&mine) {
        notify.note(&messages::settings_mismatch(&recorded, &mine));
    }
    Ok(entry)
}

/// How a wait ended well.
enum Admitted {
    /// The command may start: the start time and the instances it got.
    Started(u64, BTreeMap<String, String>),
    /// A hold was granted: the lease id and the instances it holds.
    Leased(String, BTreeMap<String, String>),
}

/// What one poll found.
enum Tick {
    Admitted(Admitted),
    Waiting {
        line: Option<String>,
        /// Next steps for running jobs that look stuck (with the line).
        advice: Vec<String>,
        fingerprint: (usize, usize, usize),
    },
    /// `calmrun stop` asked for this entry: it has left the queue.
    Stopped,
}

/// Poll until the scheduler admits us. Returns the start time (or a hold's lease), or the
/// outcome when the wait ends otherwise (cancelled, wait timeout, state error).
fn wait_turn(
    ctx: &mut Ctx,
    store: &Store,
    p: &Prepared,
    entry: &Job,
    notify: &mut dyn Notify,
) -> std::result::Result<Admitted, Box<Outcome>> {
    let wait_started = Instant::now();
    // A hold writes nothing, so it never waits for disk space.
    let disk_need = if p.hold_ttl.is_some() {
        0
    } else {
        ctx.cfg.min_free_disk_bytes
    };
    let mut disk = DiskCheck::new(&p.cwd, disk_need);
    let mut last_line = String::from("waiting");
    let mut poll = POLL_MIN;
    let mut last_fp = None;
    // Read outside every lock, and only when a line is wanted.
    let mut usual: Option<(Instant, history::Usual)> = None;
    // Discover and health commands also run outside every lock, refreshed every few seconds.
    let mut probe = pools::Probe::default();
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
        if want_line && usual.as_ref().is_none_or(|u| u.0.elapsed() >= USUAL_REFRESH) {
            usual = Some((Instant::now(), history::Usual::load(store.history_path())));
        }
        let short = disk.short();
        // Cut the discover and health commands short on a cancel or when the wait is over.
        let (cancel, deadline) = (p.req.cancel.clone(), p.req.wait_timeout.map(|t| wait_started + t));
        let stop = move || {
            runner::INTERRUPTED.load(Ordering::SeqCst)
                || cancel.as_ref().is_some_and(|c| c.load(Ordering::SeqCst))
                || deadline.is_some_and(|d| Instant::now() >= d)
        };
        let ready = probe.ready(&entry.needs, &ctx.cfg.pools, &stop);
        let input = TickInput {
            store,
            entry,
            disk: &disk,
            short,
            ready,
            want_line,
            hold_for: p.hold_for.as_ref(),
            usual: usual.as_ref().map(|u| &u.1),
            queue: ctx.queue.clone(),
        };
        let tick = tick(ctx, &input).map_err(|e| Box::new(p.outcome().error(&e)))?;
        match tick {
            Tick::Admitted(a) => {
                if short {
                    notify.note(&disk_warning(&disk));
                }
                return Ok(a);
            }
            Tick::Stopped => {
                let msg = "calmrun: stopped by calmrun stop while waiting; left the queue".to_string();
                return Err(Box::new(waited(p, entry).ended(
                    runner::EXIT_INTERRUPTED,
                    "stopped",
                    Some(msg),
                )));
            }
            Tick::Waiting {
                line,
                advice,
                fingerprint,
            } => {
                if let Some(l) = line {
                    notify.waiting(&l);
                    for a in &advice {
                        notify.advice(a);
                    }
                    last_line = l;
                }
                if timed_out && p.hold_ttl.is_some() {
                    return Err(Box::new(give_up_hold(store, p, entry, &last_line)));
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
    /// The ready instances of our needs that have instances, to publish in our entry.
    ready: BTreeMap<String, Vec<String>>,
    want_line: bool,
    hold_for: Option<&'a (String, String)>,
    usual: Option<&'a history::Usual>,
    /// The separate queue, for the commands in the advice.
    queue: Option<String>,
}

/// One locked look at the queue: prune, publish our disk state, read (or reuse) the shared
/// free-RAM sample, schedule. The machine lock is held throughout, so a job admitted here
/// and a job admitted in another queue never take the same exclusive key.
fn tick(ctx: &mut Ctx, t: &TickInput) -> Result<Tick> {
    let now = now_ms();
    // Owners in the other queues are checked before any lock (the process refresh is slow).
    let seen = t.store.others().unwrap_or_default();
    let live = Liveness::check_with(t.store, &seen, &mut ctx.sys);
    let _machine = t.store.machine_lock().map_err(Error::State)?;
    let foreign = foreign_running(&t.store.others().map_err(Error::State)?, &live);
    let own = ctx.limits();
    let (caps, sys, entry) = (&ctx.cfg.resources, &mut ctx.sys, t.entry);
    let mut dead = Vec::new();
    let tick = t
        .store
        .with_lock(|s| {
            dead = live.prune(s);
            let limits = effective_limits(own, s);
            match s.jobs.iter_mut().find(|j| j.id == entry.id) {
                // `calmrun stop` asked for it: leave before the scheduler can admit it.
                Some(j) if j.stop_ms.is_some() => {
                    s.jobs.retain(|j| j.id != entry.id);
                    return Tick::Stopped;
                }
                Some(j) => {
                    j.disk_short = t.short;
                    j.ready.clone_from(&t.ready);
                }
                // Our entry vanished (state file removed by hand); put it back.
                None => s.jobs.push(Job {
                    disk_short: t.short,
                    ready: t.ready.clone(),
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
                    leases: &s.leases,
                    foreign: &foreign,
                },
            );
            if plan.admits(entry.id) {
                // Our own picks, then the instances of the leases we use for the other needs.
                let mut instances = plan.instances.get(&entry.id).cloned().unwrap_or_default();
                let me = s.jobs.iter().find(|j| j.id == entry.id).unwrap_or(entry);
                for (n, i) in scheduler::lease_instances(me, &s.leases) {
                    instances.entry(n).or_insert(i);
                }
                if let Some(ttl) = entry.hold_ttl_ms {
                    // Granted in the same locked step, so the slots are never free between.
                    let lease = grant(s, entry, ttl, now, t.hold_for, &instances);
                    return Tick::Admitted(Admitted::Leased(lease, instances));
                }
                if let Some(j) = s.jobs.iter_mut().find(|j| j.id == entry.id) {
                    j.started_ms = Some(now);
                    j.disk_short = false;
                    j.ready.clear();
                    j.instances.clone_from(&instances);
                    // Again here: a 0.3 calmrun that rewrote the state meanwhile dropped it.
                    j.timeout_ms = entry.timeout_ms;
                }
                return Tick::Admitted(Admitted::Started(now, instances));
            }
            let waiting = scheduler::waiting_in_order(&s.jobs);
            let place = waiting.iter().position(|j| j.id == entry.id).unwrap_or(0);
            let running = s.jobs.len() - waiting.len();
            let line = t.want_line.then(|| {
                let c = WaitContext {
                    // As published (its ready instances), not as it joined.
                    me: s.jobs.iter().find(|j| j.id == entry.id).unwrap_or(entry),
                    budget: limits.budget,
                    disk: Some(t.disk),
                    caps,
                    now_ms: now,
                    usual: t.usual,
                };
                let line = messages::waiting_line(s, &plan.reason(entry.id), &c);
                line + &crate::logs::waiting_hint(s, t.store.queue())
            });
            let advice = match t.want_line {
                true => {
                    let flag = t.queue.as_ref().map(|q| format!(" --id {q}")).unwrap_or_default();
                    messages::stuck_running_next(s, now, t.usual, &flag)
                }
                false => Vec::new(),
            };
            Tick::Waiting {
                line,
                advice,
                fingerprint: (s.jobs.len(), running + foreign.len(), place),
            }
        })
        .map_err(Error::State)?;
    record_dead(t.store, &dead);
    Ok(tick)
}

/// Turn an admitted hold entry into a lease. Returns the new lease id.
fn grant(
    s: &mut State,
    entry: &Job,
    ttl_ms: u64,
    now: u64,
    owner: Option<&(String, String)>,
    instances: &BTreeMap<String, String>,
) -> String {
    let id = crate::lease::new_id(|id| s.leases.iter().any(|l| l.id == id));
    s.jobs.retain(|j| j.id != entry.id);
    s.leases.push(Lease {
        id: id.clone(),
        resources: entry.needs.clone(),
        label: entry.label.clone(),
        cost: entry.cost,
        cwd: entry.cwd.clone(),
        pid: entry.pid,
        granted_ms: now,
        until_ms: now.saturating_add(ttl_ms),
        ttl_ms,
        for_dir: owner.map(|o| o.0.clone()),
        for_key: owner.map(|o| o.1.clone()),
        instances: instances.clone(),
    });
    id
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

/// A hold not granted within `--wait-timeout`: leave the line (a hold has no ticket).
fn give_up_hold(store: &Store, p: &Prepared, entry: &Job, last_line: &str) -> Outcome {
    let _ = store.with_lock(|s| s.jobs.retain(|j| j.id != entry.id));
    let secs = p.req.wait_timeout.map_or(0, |t| t.as_secs());
    let msg = format!(
        "calmrun: not granted within {secs}s ({}); left the line.",
        last_line.trim_start_matches("calmrun: ")
    );
    waited(p, entry).ended(EXIT_WAIT_TIMEOUT, "wait-timeout", Some(msg))
}

/// What `calmrun hold` asks for.
#[derive(Debug, Clone, Default)]
pub struct HoldRequest {
    /// Named resources to hold one slot of each.
    pub resources: Vec<String>,
    pub ttl: Duration,
    pub label: Option<String>,
    /// Budget points to hold as well (default 0).
    pub cost: Option<u32>,
    /// Give up waiting after this long (exit 75, the place is not kept); zero tries once.
    pub wait_timeout: Option<Duration>,
    /// A folder that owns the lease: jobs inside it use its resources.
    pub for_dir: Option<PathBuf>,
}

/// Wait in the queue like a job with `--needs`, then record a lease instead of running
/// anything. Returns the lease id and the instances it holds (resources that have
/// instances), or the outcome when it was not granted.
pub fn hold(
    ctx: &mut Ctx,
    h: HoldRequest,
    notify: &mut dyn Notify,
) -> std::result::Result<(String, BTreeMap<String, String>), Box<Outcome>> {
    let needs = normalized(&h.resources);
    let bad = match needs.is_empty() {
        true => Some(Error::InvalidResource(h.resources.join(","))),
        false => invalid_resource(&needs),
    };
    if let Some(e) = bad {
        return Err(Box::new(Outcome::default().error(&e)));
    }
    let label = match h.label.as_deref().map(str::trim) {
        Some(l) if !l.is_empty() => l.to_string(),
        _ => format!("hold {}", needs.join(",")),
    };
    let cost = h.cost.unwrap_or(0).min(ctx.cfg.budget.max(1));
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let p = Prepared {
        req: Request {
            needs,
            wait_timeout: h.wait_timeout,
            ..Request::default()
        },
        light: None,
        det: Detected::default(),
        kind: "hold".into(),
        cost,
        label,
        cwd_key: crate::sys::path_key(&cwd),
        cwd,
        hold_ttl: Some(h.ttl),
        hold_for: h.for_dir.as_deref().map(|d| {
            let shown = std::path::absolute(d).unwrap_or_else(|_| d.to_path_buf());
            (shown.to_string_lossy().to_string(), crate::sys::path_key(d))
        }),
        limit: None,
        instances: BTreeMap::new(),
    };
    let store = ctx.store().map_err(|e| Box::new(p.outcome().error(&e)))?;
    let entry = enqueue(ctx, &store, &p, notify).map_err(|e| Box::new(p.outcome().error(&e)))?;
    notify.queued(entry.id);
    match wait_turn(ctx, &store, &p, &entry, notify)? {
        Admitted::Leased(id, instances) => Ok((id, instances)),
        // Not reachable for a hold entry; leave the line rather than hold nothing.
        Admitted::Started(..) => {
            let _ = store.with_lock(|s| s.jobs.retain(|j| j.id != entry.id));
            Err(Box::new(p.outcome().ended(
                1,
                "error",
                Some("calmrun: hold was not granted".into()),
            )))
        }
    }
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
    Timeout(Duration, LimitFrom),
    Memory(memwatch::Stop),
    /// `calmrun stop` asked for it.
    Stopped,
}

/// Asked every 250 ms while the command runs: cancel, run time limit, `calmrun stop`,
/// memory. It also publishes the tree's memory and CPU for `status`.
struct Watcher<'a> {
    cancel: Option<&'a AtomicBool>,
    limit: Option<(Duration, LimitFrom)>,
    started: Instant,
    mem: Option<MemWatch>,
    /// Where to publish the tree's memory and CPU (not for nested runs).
    store: Option<&'a Store>,
    id: u64,
    samples: u64,
    /// The last sample in which the tree used a noticeable share of CPU.
    busy_ms: Option<u64>,
    stop_checked: Option<Instant>,
    reason: Option<StopReason>,
    /// The job's log, watched for new output (`output_ms`).
    output: Option<joblog::Output>,
    /// The instances the job got, published again with each sample.
    instances: &'a BTreeMap<String, String>,
}

impl<'a> Watcher<'a> {
    fn new(p: &'a Prepared, mem: Option<MemWatch>, store: Option<&'a Store>, id: u64) -> Watcher<'a> {
        Watcher {
            cancel: p.req.cancel.as_deref(),
            limit: p.limit.clone(),
            started: Instant::now(),
            mem,
            store,
            id,
            samples: 0,
            busy_ms: None,
            stop_checked: None,
            reason: None,
            output: job_log(&p.req).map(joblog::Output::new),
            instances: &p.instances,
        }
    }

    fn check(&mut self, pid: u32) -> bool {
        let over = self
            .limit
            .as_ref()
            .filter(|l| self.started.elapsed() > l.0)
            .cloned();
        if self.cancel.is_some_and(|c| c.load(Ordering::SeqCst)) {
            self.reason = Some(StopReason::Cancelled);
        } else if let Some((t, from)) = over {
            self.reason = Some(StopReason::Timeout(t, from));
        } else if self.stop_requested() {
            self.reason = Some(StopReason::Stopped);
        } else if let Some(stop) = self.check_tree(pid) {
            self.reason = Some(StopReason::Memory(stop));
        }
        self.check_output();
        self.reason.is_some()
    }

    /// Whether `calmrun stop` marked our entry (an unlocked read, once a second).
    fn stop_requested(&mut self) -> bool {
        let Some(store) = self.store else {
            return false;
        };
        if self.stop_checked.is_some_and(|t| t.elapsed() < STOP_CHECK_EVERY) {
            return false;
        }
        self.stop_checked = Some(Instant::now());
        store
            .peek()
            .jobs
            .iter()
            .any(|j| j.id == self.id && j.stop_ms.is_some())
    }

    fn check_tree(&mut self, pid: u32) -> Option<memwatch::Stop> {
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
            if mem.busy() {
                self.busy_ms = Some(now_ms());
            }
            if self.samples % MEM_PUBLISH_EVERY == 1 {
                let (used, cpu_ms, cpu_pct, busy_ms) = (mem.used, mem.cpu_ms, mem.cpu_pct, self.busy_ms);
                let instances = self.instances;
                // The limit too: a 0.3 calmrun that rewrote the state dropped it.
                let timeout_ms = self.limit.as_ref().map(|l| l.0.as_millis() as u64);
                // New output rides along with the memory sample.
                let output = self.output.as_mut().and_then(joblog::Output::poll);
                let _ = store.map(|st| {
                    st.with_lock(|s| {
                        if let Some(j) = s.jobs.iter_mut().find(|j| j.id == id) {
                            j.mem_bytes = Some(used);
                            j.cpu_ms = Some(cpu_ms);
                            j.cpu_pct = Some(cpu_pct);
                            j.busy_ms = busy_ms;
                            j.timeout_ms = timeout_ms;
                            j.output_ms = output.or(j.output_ms);
                            // So are the instances: without them another job could get ours.
                            if j.instances.is_empty() {
                                j.instances.clone_from(instances);
                            }
                        }
                    })
                });
            }
        }
        stop
    }

    /// Without memory sampling, publish new output by itself now and then.
    fn check_output(&mut self) {
        let (None, Some(store)) = (&self.mem, self.store) else {
            return;
        };
        let id = self.id;
        if let Some(t) = self.output.as_mut().and_then(joblog::Output::poll_now_and_then) {
            let _ = store.with_lock(|s| {
                if let Some(j) = s.jobs.iter_mut().find(|j| j.id == id) {
                    j.output_ms = Some(t);
                }
            });
        }
    }
}

/// The file a run's output goes to, if any (MCP log or `--log`).
fn job_log(req: &Request) -> Option<&Path> {
    req.log.as_deref().or(req.tee.as_deref())
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
    let log = job_log(&p.req).map(|l| l.to_string_lossy().to_string());
    let set_child = |child: u32| {
        let _ = store.with_lock(|s| {
            if let Some(j) = s.jobs.iter_mut().find(|j| j.id == id) {
                j.child_pid = Some(child);
                j.log_path = log.clone();
            }
        });
    };
    let mut out = spawn_and_finish(p, watch, cfg.low_cpu, Some(std::process::id()), set_child);
    let _ = store.with_lock(|s| s.jobs.retain(|j| j.id != id));
    out.job_id = Some(id);
    out.log = log;
    out.instances = p.instances.clone();
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
        (Ok(e), Some(StopReason::Timeout(t, from))) if e.stopped => {
            out.ended(EXIT_TIMEOUT, "timeout", Some(timeout_message(label, t, &from)))
        }
        (Ok(e), Some(StopReason::Stopped)) if e.stopped => out.ended(
            runner::EXIT_INTERRUPTED,
            "stopped",
            Some(format!(
                "calmrun: stopped by calmrun stop: {label} and its process tree were stopped"
            )),
        ),
        (Ok(e), _) if runner::INTERRUPTED.load(Ordering::SeqCst) => out.ended(e.code, "interrupted", None),
        (Ok(e), _) => out.ended(e.code, &e.code.to_string(), None),
    }
}

/// The message after a run time limit stopped `label`, with how to change the limit.
fn timeout_message(label: &str, t: Duration, from: &LimitFrom) -> String {
    let what = match from {
        LimitFrom::Asked => format!("the {}s timeout", t.as_secs()),
        LimitFrom::Kind(k) => format!(
            "its {} run time limit ([kinds.{k}] max_run_min)",
            crate::watch::short_ms(t.as_millis() as u64)
        ),
        LimitFrom::Global => format!(
            "its {} run time limit (max_run_min)",
            crate::watch::short_ms(t.as_millis() as u64)
        ),
        LimitFrom::Builtin(k) => format!(
            "its {} run time limit (the default for kind {k})",
            crate::watch::short_ms(t.as_millis() as u64)
        ),
    };
    format!(
        "calmrun: stopped {label}: still running after {what}\ncalmrun: next: if it was still \
         working, {}",
        from.how_to_change()
    )
}

/// Append the run to `history.jsonl` (best effort: history is a convenience).
pub fn record(store: &Store, queue: Option<&str>, out: &Outcome) {
    let _ = history::append_rotating(
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
            cwd: (!out.cwd.is_empty()).then(|| out.cwd.clone()),
            id: out.job_id,
            log: out.log.clone(),
        },
        history::ROTATE_BYTES,
    );
}

/// Directory for captured logs (MCP runs, `--log`).
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
    fn timeout_messages_name_the_setting_to_change() {
        let hour = Duration::from_secs(3600);
        let m = timeout_message("npx vitest", hour, &LimitFrom::Builtin("vitest".into()));
        assert!(
            m.starts_with(
                "calmrun: stopped npx vitest: still running after its 1h run time limit (the \
                 default for kind vitest)\ncalmrun: next: "
            ),
            "{m}"
        );
        assert!(
            m.contains("[kinds.vitest] max_run_min") && m.contains("--timeout"),
            "{m}"
        );
        let m = timeout_message("x", Duration::from_secs(5), &LimitFrom::Asked);
        assert!(
            m.contains("after the 5s timeout") && m.contains("--timeout"),
            "{m}"
        );
        let m = timeout_message("x", hour, &LimitFrom::Global);
        assert!(
            m.contains("(max_run_min)") && m.contains("CALMRUN_MAX_RUN_MIN"),
            "{m}"
        );
        assert!(m.is_ascii());
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
        assert!(invalid_resource(&n).is_none());
        let bad = invalid_resource(&["db".into(), "bad name!".into()]).unwrap();
        assert_eq!(bad.exit_code(), 2);
        assert!(bad.to_string().contains("\"bad name!\""), "{bad}");
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
