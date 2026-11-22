mod cli;
mod config;
mod detect;
mod doctor;
mod history;
mod hook;
mod mcp;
mod memwatch;
mod queue;
mod runner;
mod scheduler;
mod setup;
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
    if let Some(q) = &cli.id {
        if !queue::valid_queue_id(q) {
            eprintln!("calmrun: invalid --id \"{q}\": use letters, digits, '-', '_' or '.'");
            std::process::exit(2);
        }
    }
    let queue_id = cli.id.clone();
    let load = move || {
        let mut c = Ctx::load();
        c.queue = queue_id.clone();
        c
    };
    let code = match cli.sub {
        Some(Sub::Status { json }) => cmd_status(load(), json),
        Some(Sub::Stats { days, json }) => cmd_stats(days, json),
        Some(Sub::Bump { ref label }) => cmd_bump(load(), label),
        Some(Sub::Doctor) => cmd_doctor(),
        Some(Sub::Hook {
            target: HookTarget::Claude,
        }) => cmd_hook(),
        Some(Sub::Init { force }) => cmd_init(force),
        Some(Sub::Mcp) => mcp::serve(),
        Some(Sub::Setup {
            write,
            remove,
            hook,
            ref agents,
            ref home,
        }) => {
            let Some(home) = home.clone().or_else(dirs::home_dir) else {
                eprintln!("calmrun: cannot find your home directory; pass --home");
                std::process::exit(1);
            };
            let opts = setup::Options {
                write,
                remove,
                hook,
                agents: agents.clone(),
                home,
            };
            let (text, code) = setup::run(&opts, &|k| std::env::var(k).ok());
            print!("{text}");
            code
        }
        Some(Sub::Guide) => {
            print!("{}", GUIDE);
            0
        }
        Some(Sub::Probe(ref a)) => cmd_probe(a),
        Some(Sub::CtrlC { pid }) => cmd_ctrl_c(pid),
        None if cli.command.is_empty() => {
            use clap::CommandFactory;
            let _ = Cli::command().print_help();
            2
        }
        None => run_queued(load(), cli),
    };
    std::process::exit(code);
}

fn run_queued(mut ctx: Ctx, cli: Cli) -> i32 {
    // Installed before anything runs so an inner (nested) calmrun also forwards signals.
    runner::install_signal_handler();
    if let Some(kind) = &cli.kind {
        if !detect::BUILTIN_KINDS.contains(&kind.as_str())
            && kind != "other"
            && !ctx.cfg.kinds.contains_key(kind)
        {
            eprintln!("calmrun: note: kind \"{kind}\" is not built in or in the config; cost defaults to 1");
        }
    }
    let req = queue::Request {
        rerun_hint: Some(rerun_hint(std::env::args().skip(1).collect())),
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
    let out = queue::run(&mut ctx, req, &mut queue::StderrNotify::new());
    if let Some(m) = &out.message {
        eprintln!("{m}");
    }
    out.code
}

const GUIDE: &str = "calmrun: a machine-wide queue for heavy commands. Several agents share this computer.

Wrap heavy commands (tests, type checks, linters, builds, Playwright, docker compose,
pytest, cargo/gradle builds):
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
Exit codes: the command's own; 75 = not started in time (place kept); 2 = refused;
127 = command not found; 130 = Ctrl+C.
MCP: `calmrun mcp` offers run_task / queue_status / bump to agents that speak MCP.
";

/// Quote a word for display so it can be pasted back into a shell.
fn shell_quote(w: &str) -> String {
    let safe = !w.is_empty()
        && w.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-\\".contains(c));
    if safe {
        w.to_string()
    } else if cfg!(windows) {
        format!("\"{}\"", w.replace('"', "\\\""))
    } else {
        format!("'{}'", w.replace('\'', "'\\''"))
    }
}

/// The command line that resumes this run with its ticket: the same arguments with
/// `--ticket {ticket}` in front (an older --ticket is dropped).
fn rerun_hint(args: Vec<String>) -> String {
    let mut out = vec![
        "calmrun".to_string(),
        "--ticket".to_string(),
        "{ticket}".to_string(),
    ];
    let mut i = 0;
    let mut in_command = false;
    while i < args.len() {
        let a = &args[i];
        if !in_command && a == "--ticket" {
            i += 2;
            continue;
        }
        if !in_command && a.starts_with("--ticket=") {
            i += 1;
            continue;
        }
        if a == "--" {
            in_command = true;
        }
        out.push(if a == "{ticket}" {
            a.clone()
        } else {
            shell_quote(a)
        });
        i += 1;
    }
    out.join(" ")
}

fn cmd_status(mut ctx: Ctx, as_json: bool) -> i32 {
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
    if let Some(q) = &ctx.queue {
        println!("queue: {q}");
    }
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

fn cmd_bump(mut ctx: Ctx, needle: &str) -> i32 {
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
    if let Some(var) = &a.print_env {
        println!("{}", std::env::var(var).unwrap_or_default());
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Vec<String> {
        s.split(' ').map(String::from).collect()
    }

    #[test]
    fn rerun_hint_keeps_args_and_replaces_ticket() {
        assert_eq!(
            rerun_hint(v("--wait-timeout 60 -- npx vitest run")),
            "calmrun --ticket {ticket} --wait-timeout 60 -- npx vitest run"
        );
        assert_eq!(
            rerun_hint(v("--ticket 4 --cost 2 -- cargo test")),
            "calmrun --ticket {ticket} --cost 2 -- cargo test"
        );
        let h = rerun_hint(vec!["--".into(), "echo".into(), "a b".into()]);
        assert!(
            h.ends_with("-- echo \"a b\"") || h.ends_with("-- echo 'a b'"),
            "{h}"
        );
    }
}
