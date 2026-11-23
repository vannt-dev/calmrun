//! The CLI subcommands. Each returns the process exit code.

use crate::cli::{Cli, HookTarget, Prio, ProbeArgs};
use crate::config::{self, bytes_to_gb};
use crate::error::Error;
use crate::lease::{self, HoldArgs, ReleaseArgs};
use crate::queue::{self, now_ms, Ctx};
use crate::state::{Job, State};
use crate::{doctor, history, hook, kinds, runner, scheduler, setup, status, sys};
use std::io::Read;
use std::time::Duration;

/// Instructions for agents (`calmrun guide`).
pub const GUIDE: &str = "\
calmrun: a machine-wide queue for heavy commands. Several agents share this computer.

Wrap heavy commands (tests, type checks, linters, builds, Playwright, docker compose,
pytest, cargo/gradle/make/dotnet/go builds):
    calmrun -- npx vitest run
    calmrun -- cargo test
    calmrun \"npm ci && npm test\"     (one quoted string runs in the shell)
Do not wrap light commands (git, ls, cat, package installs).

Long runs: start them in the background and check back, or cap the wait:
    calmrun --wait-timeout 120 -- npx vitest run
If it has not started in 120 s it exits 75 and prints the exact command that resumes
the same place in line (calmrun --ticket <id> -- ...). The place is kept for 10 minutes.

See the queue:        calmrun status          (calmrun status --json for tools)
Move a job forward:   calmrun bump <label>
Hold a device/resource without running anything (prints a lease id; default 4 h):
    calmrun hold emulator --label \"my task\"   ...   calmrun release <lease>
Exit codes: the command's own; 75 = not started in time (place kept); 2 = refused;
127 = command not found; 130 = Ctrl+C; 137 = stopped by the memory ceiling.
MCP: `calmrun mcp` offers run_task (returns a task_id when still busy after wait_seconds),
task_status, cancel_task, queue_status and bump to agents that speak MCP.
";

/// Print an error the way every subcommand does and return its exit code.
pub fn fail(e: &Error) -> i32 {
    eprintln!("calmrun: {e}");
    e.exit_code()
}

/// `calmrun [flags] -- <command>`.
pub fn run_queued(mut ctx: Ctx, cli: Cli) -> i32 {
    // Installed before anything runs so an inner (nested) calmrun also forwards signals.
    runner::install_signal_handler();
    if let Some(kind) = &cli.kind {
        let known =
            kinds::builtin_names().any(|k| k == kind) || kind == "other" || ctx.cfg.kinds.contains_key(kind);
        if !known {
            eprintln!(
                "calmrun: note: kind \"{kind}\" is not built in or in the config; cost defaults to {}",
                kinds::UNKNOWN_COST
            );
        }
    }
    let req = queue::Request {
        rerun_hint: Some(rerun_hint(
            &runner::program_name(),
            std::env::args().skip(1).collect(),
        )),
        wait_timeout: cli.wait_timeout.filter(|t| *t > 0.0).map(Duration::from_secs_f64),
        ticket: cli.ticket,
        needs: cli.needs,
        argv: cli.command,
        kind: cli.kind,
        cost: cli.cost,
        high: cli.prio == Some(Prio::High),
        label: cli.label,
        ..Default::default()
    };
    let out = queue::run(&mut ctx, req, &mut queue::StderrNotify::default());
    if let Some(m) = &out.message {
        eprintln!("{m}");
    }
    out.code
}

/// The command line that resumes this run with its ticket: the same arguments with
/// `--ticket {ticket}` in front (an older --ticket is dropped). `program` starts calmrun.
pub fn rerun_hint(program: &str, args: Vec<String>) -> String {
    let mut out = vec![
        program.to_string(),
        "--ticket".to_string(),
        "{ticket}".to_string(),
    ];
    let mut in_command = false;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        if !in_command && a == "--ticket" {
            it.next();
            continue;
        }
        if !in_command && a.starts_with("--ticket=") {
            continue;
        }
        in_command |= a == "--";
        out.push(runner::quote_for_display(&a));
    }
    out.join(" ")
}

/// `calmrun status [--json]`.
pub fn status(mut ctx: Ctx, as_json: bool) -> i32 {
    let state = match status::snapshot(&mut ctx) {
        Ok(s) => s,
        Err(e) => return fail(&e),
    };
    let v = status::status_json(&mut ctx, &state);
    if as_json {
        println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
        return 0;
    }
    if let Some(q) = &ctx.queue {
        println!("queue: {q}");
    }
    let num = |k: &str| v[k].as_u64().unwrap_or(0);
    println!(
        "budget: {} of {} points in use, free RAM {:.1} GB (starts need {:.1} GB)",
        num("used"),
        num("budget"),
        bytes_to_gb(num("free_ram_bytes")),
        bytes_to_gb(num("min_free_bytes"))
    );
    let why = |id: u64| {
        v["waiting"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|w| w["id"].as_u64() == Some(id))
            .and_then(|w| w["why"].as_str())
            .map(str::to_string)
    };
    print_jobs(&state, now_ms(), &why);
    for w in v["warnings"].as_array().into_iter().flatten() {
        println!("warning: {}", w.as_str().unwrap_or_default());
    }
    0
}

fn print_jobs(state: &State, now: u64, why: &dyn Fn(u64) -> Option<String>) {
    let header = |first: &str, time: &str| {
        println!(
            "  {first:<4} {:<24} {:<8} {:>4} {:>7}  {time:>7}  cwd",
            "label", "kind", "cost", "pid"
        );
    };
    let row = |j: &Job, first: String| {
        let since = now.saturating_sub(j.started_ms.unwrap_or(j.queued_ms));
        println!(
            "  {first:<4} {:<24} {:<8} {:>4} {:>7}  {:>7}  {}",
            j.label,
            j.kind,
            j.cost,
            j.pid,
            history::human_ms(since),
            j.cwd
        );
    };
    let running: Vec<&Job> = state.jobs.iter().filter(|j| j.running()).collect();
    let waiting = scheduler::waiting_in_order(&state.jobs);
    println!("running ({}):", running.len());
    if !running.is_empty() {
        header("", "for");
    }
    running.iter().for_each(|j| row(j, String::new()));
    println!("waiting ({}):", waiting.len());
    if !waiting.is_empty() {
        header("#", "waited");
    }
    for (i, j) in waiting.iter().enumerate() {
        row(j, format!("{}{}", i + 1, if j.high { "!" } else { "" }));
        if let Some(w) = why(j.id) {
            println!("       waits for: {w}");
        }
    }
    if !state.leases.is_empty() {
        println!("leases ({}):", state.leases.len());
        println!(
            "  {:<6}  {:<16} {:<24} {:>7}  {:>10}  for",
            "lease", "resources", "label", "age", "expires in"
        );
    }
    for l in &state.leases {
        println!(
            "  {:<6}  {:<16} {:<24} {:>7}  {:>10}  {}",
            l.id,
            l.resources.join(","),
            l.label,
            history::human_ms(now.saturating_sub(l.granted_ms)),
            history::human_ms(l.until_ms.saturating_sub(now)),
            l.for_dir.as_deref().unwrap_or("-")
        );
    }
    let kept: Vec<&Job> = state.jobs.iter().filter(|j| j.parked()).collect();
    if !kept.is_empty() {
        println!(
            "kept for a ticket ({}; `calmrun release --ticket <id>` gives one up):",
            kept.len()
        );
    }
    for j in kept {
        println!("  {}", crate::messages::kept_line(j, now));
    }
}

/// `calmrun hold <resource>[,…]` and `calmrun hold --renew <lease>`.
pub fn hold(mut ctx: Ctx, a: &HoldArgs) -> i32 {
    let ttl = a.ttl.map(Duration::from_secs);
    if let Some(raw) = &a.renew {
        let Some(id) = lease::parse_id(raw) else {
            return bad_lease_id(raw);
        };
        return match lease::renew(&mut ctx, &id, ttl) {
            Ok(Some(l)) => {
                println!("{id}");
                eprintln!("calmrun: renewed {}", lease::line(&l, now_ms()));
                0
            }
            Ok(None) => {
                eprintln!(
                    "calmrun: lease {id} is gone (released or expired); take a new one with `{} hold`",
                    runner::program_name()
                );
                1
            }
            Err(e) => fail(&e),
        };
    }
    if let Some(d) = a.for_dir.as_ref().filter(|d| !d.is_dir()) {
        eprintln!("calmrun: --for {}: not a folder", d.display());
        return 2;
    }
    runner::install_signal_handler();
    let req = queue::HoldRequest {
        resources: a.resources.clone(),
        ttl: ttl.unwrap_or(lease::DEFAULT_TTL),
        label: a.label.clone(),
        cost: a.cost,
        // 0 (or less) tries once.
        wait_timeout: a.wait_timeout.map(|t| Duration::from_secs_f64(t.clamp(0.0, 1e9))),
        for_dir: a.for_dir.clone(),
    };
    let (what, for_ms) = (req.resources.join(","), req.ttl.as_millis() as u64);
    match queue::hold(&mut ctx, req, &mut queue::StderrNotify::default()) {
        Ok(id) => {
            println!("{id}");
            eprintln!(
                "calmrun: holding {what} for {} (lease {id}); give it back with `{} release {id}{}`",
                history::human_ms(for_ms),
                runner::program_name(),
                queue_flag(&ctx)
            );
            0
        }
        Err(out) => {
            if let Some(m) = &out.message {
                eprintln!("{m}");
            }
            out.code
        }
    }
}

/// `calmrun release <lease>` and `calmrun release --ticket <id>`.
pub fn release(mut ctx: Ctx, a: &ReleaseArgs) -> i32 {
    if let Some(t) = a.ticket {
        return match lease::drop_ticket(&mut ctx, t) {
            Ok(Some(j)) => {
                eprintln!(
                    "calmrun: dropped ticket {t} ({}); its place in line is gone",
                    j.label
                );
                0
            }
            Ok(None) => {
                eprintln!(
                    "calmrun: note: no place is kept for ticket {t} (expired, resumed or unknown);                      `calmrun status{}` lists the kept ones",
                    queue_flag(&ctx)
                );
                0
            }
            Err(e) => fail(&e),
        };
    }
    let raw = a.lease.as_deref().unwrap_or_default();
    let Some(id) = lease::parse_id(raw) else {
        return bad_lease_id(raw);
    };
    match lease::release(&mut ctx, &id) {
        Ok(Some(l)) => {
            eprintln!("calmrun: released {}", lease::line(&l, now_ms()));
            0
        }
        Ok(None) => {
            eprintln!("calmrun: note: lease {id} was already released or expired");
            0
        }
        Err(e) => fail(&e),
    }
}

fn bad_lease_id(raw: &str) -> i32 {
    eprintln!(
        "calmrun: \"{raw}\" is not a lease id ({} hex digits, as printed by `calmrun hold`)",
        lease::ID_LEN
    );
    2
}

fn queue_flag(ctx: &Ctx) -> String {
    ctx.queue
        .as_ref()
        .map(|q| format!(" --id {q}"))
        .unwrap_or_default()
}

/// `calmrun bump <label>`.
pub fn bump(mut ctx: Ctx, needle: &str) -> i32 {
    match status::bump(&mut ctx, needle) {
        Ok(0) => {
            eprintln!("calmrun: no waiting job matches \"{needle}\"; see `calmrun status`");
            1
        }
        Ok(n) => {
            println!("moved {n} waiting job(s) to the front");
            0
        }
        Err(e) => fail(&e),
    }
}

/// `calmrun doctor`.
pub fn doctor(mut ctx: Ctx) -> i32 {
    let free_now = ctx.sys.free_ram();
    print!(
        "{}",
        doctor::report(
            ctx.machine,
            &ctx.cfg,
            &ctx.home,
            &ctx.config_path,
            sys::physical_cores(),
            free_now
        )
    );
    0
}

/// `calmrun stats [--days N] [--json]`.
pub fn stats(days: u32, as_json: bool) -> i32 {
    const DAY_MS: u64 = 86_400_000;
    let home = config::home_dir();
    let text = history::read_all(&home.join("history.jsonl"));
    let rows = history::summarize(&text, now_ms().saturating_sub(days as u64 * DAY_MS));
    if as_json {
        println!("{}", serde_json::to_string_pretty(&rows).unwrap_or_default());
        return 0;
    }
    if rows.is_empty() {
        println!("no runs in the last {days} days");
        return 0;
    }
    println!("last {days} days:");
    let line = |a: &str, b: &str, cols: [String; 6]| {
        let [c, d, e, f, g, h] = cols;
        println!("  {a:<24} {b:<8} {c:>5} {d:>5} {e:>9} {f:>9} {g:>9} {h:>7}");
    };
    line(
        "label",
        "kind",
        ["runs", "fail", "avg wait", "max wait", "avg run", "gave up"].map(String::from),
    );
    for r in rows {
        line(
            &r.label,
            &r.kind,
            [
                r.runs.to_string(),
                r.failures.to_string(),
                history::human_ms(r.avg_wait_ms),
                history::human_ms(r.max_wait_ms),
                history::human_ms(r.avg_run_ms),
                r.gave_up.to_string(),
            ],
        );
    }
    0
}

/// `calmrun hook claude`. Fails open: any problem means no output and exit 0.
pub fn hook(target: &HookTarget) -> i32 {
    let agent = match target {
        HookTarget::Claude => hook::Agent::Claude,
        HookTarget::Codex => hook::Agent::Codex,
        HookTarget::Antigravity => hook::Agent::Antigravity,
        HookTarget::Opencode => hook::Agent::OpenCode,
    };
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return 0;
    }
    let home = config::home_dir();
    let kinds = config::read_file_config(&config::config_path(&home))
        .map(|f| f.kinds)
        .unwrap_or_default();
    let program = hook::program_name();
    let opts = hook::Opts {
        mode: hook::Mode::from_env(),
        kinds: &kinds,
        program: &program,
        load: &queue_load,
    };
    if let Some(out) = hook::run(agent, &input, &opts) {
        println!("{out}");
    }
    0
}

/// One line on the machine-wide queue for an agent that is told to wait in it:
/// `Queue now: 3 of 3 points in use, 1 waiting.` `None` when the state cannot be read.
fn queue_load() -> Option<String> {
    let mut ctx = Ctx::load().ok()?;
    let state = status::snapshot(&mut ctx).ok()?;
    let budget = queue::effective_limits(ctx.limits(), &state).budget;
    let used = scheduler::used_points(&state.jobs, budget) + scheduler::leased_points(&state.leases, budget);
    let waiting = scheduler::waiting_in_order(&state.jobs).len();
    Some(format!(
        "Queue now: {used} of {budget} points in use, {waiting} waiting."
    ))
}

/// `calmrun init [--force]`.
pub fn init(force: bool) -> i32 {
    let home = config::home_dir();
    let path = config::config_path(&home);
    if path.exists() && !force {
        println!("{}", path.display());
        eprintln!("calmrun: config already exists (use --force to overwrite)");
        return 0;
    }
    let machine = match sys::Sys::new() {
        Ok(mut s) => s.machine(),
        Err(e) => return fail(&e),
    };
    let res = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|_| std::fs::write(&path, config::default_config_text(machine)));
    match res {
        Ok(()) => {
            println!("{}", path.display());
            0
        }
        Err(source) => fail(&Error::Write { path, source }),
    }
}

/// `calmrun setup …`.
pub fn setup(opts: setup::Options) -> i32 {
    let (text, code) = setup::run(&opts, &|k| std::env::var(k).ok());
    print!("{text}");
    code
}

/// Hidden test helper: sleep, log start/end, allocate, print, spawn a daemon.
pub fn probe(a: &ProbeArgs) -> i32 {
    /// How long the probe's daemon lives.
    const DAEMON_MS: &str = "6000";
    if let Some(var) = &a.print_env {
        println!("{}", std::env::var(var).unwrap_or_default());
    }
    if a.print_args {
        println!("{}", serde_json::to_string(&a.rest).unwrap_or_default());
    }
    if let Some(pidfile) = &a.spawn_daemon {
        let exe = std::env::current_exe().unwrap_or_else(|_| "calmrun".into());
        let mut c = std::process::Command::new(exe);
        c.args(["__probe", "--sleep-ms", DAEMON_MS])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        // Same process group as this probe on Unix, like a real build daemon before it
        // detaches, so the test shows a normal exit leaves the group alone.
        if let Ok(child) = c.spawn() {
            let _ = std::fs::write(pidfile, child.id().to_string());
        }
    }
    let mark = |what: &str| {
        use std::io::Write;
        if let Some(p) = &a.log {
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
                // One write per line: concurrent appends must not interleave.
                let _ = f.write_all(format!("{what} {} {}\n", a.name, now_ms()).as_bytes());
            }
        }
    };
    mark("start");
    // Touch every page so the memory is really resident.
    let hog: Vec<u8> = vec![1u8; a.alloc_mb * 1024 * 1024];
    std::thread::sleep(Duration::from_millis(a.sleep_ms));
    std::hint::black_box(&hog);
    mark("end");
    a.exit
}

/// Hidden test helper: Ctrl+C to a process.
pub fn ctrl_c(pid: u32) -> i32 {
    i32::from(!runner::send_ctrl_c(pid))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Vec<String> {
        s.split(' ').map(String::from).collect()
    }

    #[test]
    fn rerun_hint_keeps_args_and_replaces_ticket() {
        assert_eq!(
            rerun_hint("calmrun", v("--wait-timeout 60 -- npx vitest run")),
            "calmrun --ticket {ticket} --wait-timeout 60 -- npx vitest run"
        );
        assert_eq!(
            rerun_hint("calmrun", v("--ticket 4 --cost 2 -- cargo test")),
            "calmrun --ticket {ticket} --cost 2 -- cargo test"
        );
        let h = rerun_hint("calmrun", vec!["--".into(), "echo".into(), "a b".into()]);
        assert!(
            h.ends_with("-- echo \"a b\"") || h.ends_with("-- echo 'a b'"),
            "{h}"
        );
    }
}
