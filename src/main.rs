mod cli;
mod config;
mod detect;
mod doctor;
mod history;
mod hook;
mod runner;
mod scheduler;
mod state;
mod sys;

use clap::Parser;
use cli::{Cli, HookTarget, Prio, Sub};
use config::{bytes_to_gb, Config};
use scheduler::{Block, Limits};
use serde_json::json;
use state::{Job, State, Store};
use std::collections::HashSet;
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use sys::Sys;

const POLL: Duration = Duration::from_millis(500);
const MESSAGE_EVERY: Duration = Duration::from_secs(30);
const STUCK_MS: u64 = 30 * 60_000;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn load_config(sys: &mut Sys) -> (Config, PathBuf, PathBuf, config::Machine) {
    let home = config::home_dir();
    let path = config::config_path(&home);
    let m = sys.machine();
    let file = config::read_file_config(&path).unwrap_or_else(|e| {
        eprintln!("calmrun: ignoring config file: {e}");
        config::FileConfig::default()
    });
    let cfg = config::resolve(m, file, &|k| std::env::var(k).ok());
    (cfg, home, path, m)
}

fn limits(cfg: &Config) -> Limits {
    Limits {
        budget: cfg.budget,
        min_free_bytes: cfg.min_free_bytes,
        pass_limit_ms: cfg.pass_limit_ms,
    }
}

fn prune_dead(s: &mut State, sys: &mut Sys) {
    let pairs: Vec<(u32, u64)> = s.jobs.iter().map(|j| (j.pid, j.pid_start)).collect();
    let alive: HashSet<(u32, u64)> = pairs
        .iter()
        .zip(sys.alive_set(&pairs))
        .filter_map(|(p, ok)| ok.then_some(*p))
        .collect();
    state::prune(s, |pid, start| alive.contains(&(pid, start)));
}

fn ordinal(n: usize) -> String {
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

fn main() {
    let cli = Cli::parse();
    let code = match cli.sub {
        Some(Sub::Status { json }) => cmd_status(json),
        Some(Sub::Stats { days, json }) => cmd_stats(days, json),
        Some(Sub::Bump { ref label }) => cmd_bump(label),
        Some(Sub::Doctor) => cmd_doctor(),
        Some(Sub::Hook {
            target: HookTarget::Claude,
        }) => cmd_hook(),
        Some(Sub::Init { force }) => cmd_init(force),
        Some(Sub::Probe(ref a)) => cmd_probe(a),
        Some(Sub::CtrlC { pid }) => cmd_ctrl_c(pid),
        None if cli.command.is_empty() => {
            use clap::CommandFactory;
            let _ = Cli::command().print_help();
            2
        }
        None => run_queued(cli),
    };
    std::process::exit(code);
}

fn run_queued(cli: Cli) -> i32 {
    let mut sys = Sys::new();
    let (cfg, home, _, _) = load_config(&mut sys);
    let mut argv = cli.command;
    if let Some(reason) = detect::refuse_reason(&argv) {
        eprintln!("calmrun: refused: {reason}");
        return 2;
    }
    if let Some(a) = detect::vitest_args(&argv, cfg.vitest_workers) {
        argv = a;
    }

    // Installed before the nested shortcut so an inner calmrun also forwards signals.
    runner::install_signal_handler();
    // Nested: an outer calmrun already holds points for us.
    if let Some(held) = std::env::var("CALMRUN_HELD")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
    {
        if held != std::process::id() && sys.start_time(held).is_some() {
            return match runner::build(&argv) {
                Ok(cmd) => runner::run(cmd, false, |_| {}, &mut |_| false)
                    .map(|e| e.code)
                    .unwrap_or_else(|e| spawn_error(&argv, e)),
                Err(e) => spawn_error(&argv, e),
            };
        }
    }

    let det = detect::detect(&argv, &cfg.kinds);
    let kind = cli.kind.clone().unwrap_or_else(|| det.kind_name().to_string());
    if !detect::BUILTIN_KINDS.contains(&kind.as_str()) && kind != "other" && !cfg.kinds.contains_key(&kind) {
        eprintln!("calmrun: note: kind \"{kind}\" is not built in or in the config; cost defaults to 1");
    }
    let cost = scheduler::effective_cost(
        cli.cost.unwrap_or_else(|| detect::kind_cost(&kind, &cfg.kinds)),
        cfg.budget,
    );
    let label = cli
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
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let keys = detect::exclusive_keys(
        &detect::Detected {
            kind: Some(kind.clone()),
            ..det.clone()
        },
        &cwd,
    );

    let store = match Store::open(&home) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("calmrun: cannot open state dir {}: {e}", home.display());
            return 1;
        }
    };
    let me = std::process::id();
    let my_start = sys.start_time(me).unwrap_or(0);
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
        high: cli.prio == Some(Prio::High),
        order: 0,
        queued_ms,
        started_ms: None,
        child_pid: None,
    };
    let enq = store.with_lock(|s| {
        prune_dead(s, &mut sys);
        s.next_id += 1;
        entry.id = s.next_id;
        entry.order = s.next_order();
        s.jobs.push(entry.clone());
        entry.clone()
    });
    let entry = match enq {
        Ok(e) => e,
        Err(e) => {
            eprintln!("calmrun: cannot lock queue state: {e}");
            return 1;
        }
    };
    let id = entry.id;
    let lim = limits(&cfg);

    // Wait for our turn.
    let mut last_msg: Option<Instant> = None;
    let started_ms = loop {
        if runner::INTERRUPTED.load(Ordering::SeqCst) {
            let _ = store.with_lock(|s| s.jobs.retain(|j| j.id != id));
            eprintln!("calmrun: cancelled while waiting");
            return 130;
        }
        let free = sys.free_ram();
        let now = now_ms();
        let step = store.with_lock(|s| {
            prune_dead(s, &mut sys);
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
            let used = scheduler::used_points(&s.jobs, lim.budget);
            let place = scheduler::waiting_in_order(&s.jobs)
                .iter()
                .position(|j| j.id == id)
                .unwrap_or(0)
                + 1;
            let why = blocked
                .into_iter()
                .find(|(b, _)| *b == id)
                .map(|(_, r)| r)
                .unwrap_or(Block::Order);
            let msg = match why {
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
            };
            Err(msg)
        });
        match step {
            Ok(Ok(t)) => break t,
            Ok(Err(msg)) => {
                if last_msg.is_none_or(|t| t.elapsed() >= MESSAGE_EVERY) {
                    eprintln!("{msg}");
                    last_msg = Some(Instant::now());
                }
            }
            Err(e) => {
                eprintln!("calmrun: cannot lock queue state: {e}");
                return 1;
            }
        }
        std::thread::sleep(POLL);
    };

    let wait_ms = started_ms.saturating_sub(queued_ms);
    let result = runner::build(&argv).and_then(|mut cmd| {
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
            &mut |_| false,
        )
    });
    let run_ms = now_ms().saturating_sub(started_ms);
    let _ = store.with_lock(|s| s.jobs.retain(|j| j.id != id));
    let (code, status) = match result {
        Ok(e) if runner::INTERRUPTED.load(Ordering::SeqCst) => (e.code, "interrupted".to_string()),
        Ok(e) => (e.code, e.code.to_string()),
        Err(e) => (spawn_error(&argv, e), "spawn-failed".to_string()),
    };
    let _ = history::append(
        &store.history_path(),
        &history::Record {
            at: history::format_utc(now_ms()),
            label,
            kind,
            cost,
            wait_ms,
            run_ms,
            status,
        },
    );
    code
}

fn spawn_error(argv: &[String], e: std::io::Error) -> i32 {
    eprintln!(
        "calmrun: cannot run {}: {e}",
        argv.first().map(String::as_str).unwrap_or("")
    );
    if e.kind() == std::io::ErrorKind::NotFound {
        127
    } else {
        126
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
        "cwd": j.cwd,
        "high": j.high,
        "since_ms": j.started_ms.unwrap_or(j.queued_ms),
        "for_ms": now.saturating_sub(j.started_ms.unwrap_or(j.queued_ms)),
        "place": place,
    })
}

fn cmd_status(as_json: bool) -> i32 {
    let mut sys = Sys::new();
    let (cfg, home, _, _) = load_config(&mut sys);
    let state = match Store::open(&home).and_then(|st| {
        st.with_lock(|s| {
            prune_dead(s, &mut sys);
            s.clone()
        })
    }) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("calmrun: cannot read queue state: {e}");
            return 1;
        }
    };
    let now = now_ms();
    let free = sys.free_ram();
    let used = scheduler::used_points(&state.jobs, cfg.budget);
    let running: Vec<&Job> = state.jobs.iter().filter(|j| j.running()).collect();
    let waiting = scheduler::waiting_in_order(&state.jobs);
    let warnings: Vec<String> = waiting
        .iter()
        .filter(|j| now.saturating_sub(j.queued_ms) > STUCK_MS)
        .map(|j| {
            format!(
                "\"{}\" (pid {}) has waited {} — is something stuck? See the running jobs above, or `calmrun bump`",
                j.label,
                j.pid,
                history::human_ms(now - j.queued_ms)
            )
        })
        .collect();
    if as_json {
        let out = json!({
            "budget": cfg.budget,
            "used": used,
            "free_ram_bytes": free,
            "min_free_bytes": cfg.min_free_bytes,
            "running": running.iter().map(|j| job_json(j, now, None)).collect::<Vec<_>>(),
            "waiting": waiting.iter().enumerate().map(|(i, j)| job_json(j, now, Some(i + 1))).collect::<Vec<_>>(),
            "warnings": warnings,
        });
        println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
        return 0;
    }
    println!(
        "budget: {used} of {} points in use · free RAM {:.1} GB (starts need {:.1} GB)",
        cfg.budget,
        bytes_to_gb(free),
        bytes_to_gb(cfg.min_free_bytes)
    );
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
    println!("running ({}):", running.len());
    if !running.is_empty() {
        println!(
            "  {:<4} {:<24} {:<8} {:>4} {:>7}  {:>7}  cwd",
            "", "label", "kind", "cost", "pid", "for"
        );
    }
    for j in &running {
        row(j, String::new());
    }
    println!("waiting ({}):", waiting.len());
    if !waiting.is_empty() {
        println!(
            "  {:<4} {:<24} {:<8} {:>4} {:>7}  {:>7}  cwd",
            "#", "label", "kind", "cost", "pid", "waited"
        );
    }
    for (i, j) in waiting.iter().enumerate() {
        row(j, format!("{}{}", i + 1, if j.high { "!" } else { "" }));
    }
    for w in &warnings {
        println!("warning: {w}");
    }
    0
}

fn cmd_stats(days: u32, as_json: bool) -> i32 {
    let home = config::home_dir();
    let text = std::fs::read_to_string(home.join("history.jsonl")).unwrap_or_default();
    let since = now_ms().saturating_sub(days as u64 * 86_400_000);
    let rows = history::summarize(&text, since);
    if as_json {
        println!("{}", serde_json::to_string_pretty(&rows).unwrap_or_default());
        return 0;
    }
    if rows.is_empty() {
        println!("no runs in the last {days} days");
        return 0;
    }
    println!("last {days} days:");
    println!(
        "  {:<24} {:<8} {:>5} {:>5} {:>9} {:>9} {:>9}",
        "label", "kind", "runs", "fail", "avg wait", "max wait", "avg run"
    );
    for r in rows {
        println!(
            "  {:<24} {:<8} {:>5} {:>5} {:>9} {:>9} {:>9}",
            r.label,
            r.kind,
            r.runs,
            r.failures,
            history::human_ms(r.avg_wait_ms),
            history::human_ms(r.max_wait_ms),
            history::human_ms(r.avg_run_ms)
        );
    }
    0
}

fn cmd_bump(needle: &str) -> i32 {
    let mut sys = Sys::new();
    let home = config::home_dir();
    let needle = needle.to_lowercase();
    let res = Store::open(&home).and_then(|st| {
        st.with_lock(|s| {
            prune_dead(s, &mut sys);
            let min = s.jobs.iter().map(|j| j.order).min().unwrap_or(0);
            let mut ids: Vec<u64> = scheduler::waiting_in_order(&s.jobs)
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
            ids.drain(..).count()
        })
    });
    match res {
        Ok(0) => {
            eprintln!("calmrun: no waiting job matches \"{needle}\"");
            1
        }
        Ok(n) => {
            println!("moved {n} waiting job(s) to the front");
            0
        }
        Err(e) => {
            eprintln!("calmrun: cannot lock queue state: {e}");
            1
        }
    }
}

fn cmd_doctor() -> i32 {
    let mut sys = Sys::new();
    let (cfg, home, path, m) = load_config(&mut sys);
    print!("{}", doctor::report(m, &cfg, &home, &path, sys::physical_cores()));
    0
}

fn cmd_hook() -> i32 {
    // Fail open: any problem means no output and exit 0.
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return 0;
    }
    let home = config::home_dir();
    let kinds = config::read_file_config(&config::config_path(&home))
        .map(|f| f.kinds)
        .unwrap_or_default();
    if let Some(out) = hook::handle(&input, hook::Mode::from_env(), &kinds) {
        println!("{out}");
    }
    0
}

fn cmd_init(force: bool) -> i32 {
    let mut sys = Sys::new();
    let home = config::home_dir();
    let path = config::config_path(&home);
    if path.exists() && !force {
        println!("{}", path.display());
        eprintln!("calmrun: config already exists (use --force to overwrite)");
        return 0;
    }
    let m = sys.machine();
    let res = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|_| std::fs::write(&path, config::default_config_text(m)));
    match res {
        Ok(()) => {
            println!("{}", path.display());
            0
        }
        Err(e) => {
            eprintln!("calmrun: cannot write {}: {e}", path.display());
            1
        }
    }
}

fn cmd_probe(a: &cli::ProbeArgs) -> i32 {
    use std::io::Write;
    if a.print_args {
        println!("{}", serde_json::to_string(&a.rest).unwrap_or_default());
    }
    if let Some(pidfile) = &a.spawn_daemon {
        let mut c = std::process::Command::new(std::env::current_exe().unwrap_or_else(|_| "calmrun".into()));
        c.args(["__probe", "--sleep-ms", "6000"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            c.process_group(0);
        }
        if let Ok(child) = c.spawn() {
            let _ = std::fs::write(pidfile, child.id().to_string());
        }
    }
    let mark = |what: &str| {
        if let Some(p) = &a.log {
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
                // One write per line: concurrent appends must not interleave.
                let _ = f.write_all(format!("{what} {} {}\n", a.id, now_ms()).as_bytes());
            }
        }
    };
    mark("start");
    std::thread::sleep(Duration::from_millis(a.sleep_ms));
    mark("end");
    a.exit
}

fn cmd_ctrl_c(pid: u32) -> i32 {
    #[cfg(windows)]
    let ok = runner::win::send_ctrl_c(pid);
    #[cfg(unix)]
    // SAFETY: plain syscall.
    let ok = unsafe { libc::kill(pid as i32, libc::SIGINT) == 0 };
    if ok {
        0
    } else {
        1
    }
}
