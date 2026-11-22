mod cli;
mod config;
mod detect;
mod doctor;
mod history;
mod hook;
mod mcp;
mod queue;
mod runner;
mod scheduler;
mod state;
mod sys;

use clap::Parser;
use cli::{Cli, HookTarget, Prio, Sub};
use config::bytes_to_gb;
use queue::{now_ms, Ctx};
use state::Job;
use std::io::Read;
use std::time::Duration;

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
        Some(Sub::Mcp) => mcp::serve(),
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
    // Installed before anything runs so an inner (nested) calmrun also forwards signals.
    runner::install_signal_handler();
    let mut ctx = Ctx::load();
    if let Some(kind) = &cli.kind {
        if !detect::BUILTIN_KINDS.contains(&kind.as_str())
            && kind != "other"
            && !ctx.cfg.kinds.contains_key(kind)
        {
            eprintln!("calmrun: note: kind \"{kind}\" is not built in or in the config; cost defaults to 1");
        }
    }
    let req = queue::Request {
        argv: cli.command,
        kind: cli.kind,
        cost: cli.cost,
        high: cli.prio == Some(Prio::High),
        label: cli.label,
        ..Default::default()
    };
    let out = queue::run(&mut ctx, req, &mut queue::StderrNotify::new());
    if let Some(m) = &out.message {
        eprintln!("{m}");
    }
    out.code
}

fn cmd_status(as_json: bool) -> i32 {
    let mut ctx = Ctx::load();
    let state = match queue::snapshot(&mut ctx) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("calmrun: cannot read queue state: {e}");
            return 1;
        }
    };
    let v = queue::status_json(&mut ctx, &state);
    if as_json {
        println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
        return 0;
    }
    let now = now_ms();
    let used = v["used"].as_u64().unwrap_or(0);
    println!(
        "budget: {used} of {} points in use · free RAM {:.1} GB (starts need {:.1} GB)",
        ctx.cfg.budget,
        bytes_to_gb(v["free_ram_bytes"].as_u64().unwrap_or(0)),
        bytes_to_gb(ctx.cfg.min_free_bytes)
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
    let running: Vec<&Job> = state.jobs.iter().filter(|j| j.running()).collect();
    let waiting = scheduler::waiting_in_order(&state.jobs);
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
    for w in queue::stuck_warnings(&state, now) {
        println!("warning: {w}");
    }
    0
}

fn cmd_bump(needle: &str) -> i32 {
    let mut ctx = Ctx::load();
    match queue::bump(&mut ctx, needle) {
        Ok(0) => {
            eprintln!("calmrun: no waiting job matches \"{needle}\"; see `calmrun status`");
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
    let ctx = Ctx::load();
    print!(
        "{}",
        doctor::report(
            ctx.machine,
            &ctx.cfg,
            &ctx.home,
            &ctx.config_path,
            sys::physical_cores()
        )
    );
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
    let mut sys = sys::Sys::new();
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
