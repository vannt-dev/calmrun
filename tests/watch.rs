//! Working or hung: CPU and usual-time hints, the run time limit, and `calmrun stop`, with
//! real calmrun processes against a temporary state dir.

use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const EXE: &str = env!("CARGO_BIN_EXE_calmrun");

fn calmrun(home: &Path) -> Command {
    let mut c = Command::new(EXE);
    for (k, _) in std::env::vars() {
        if k.starts_with("CALMRUN_") {
            c.env_remove(k);
        }
    }
    c.env("CALMRUN_HOME", home)
        .env("CALMRUN_BUDGET", "2")
        .env("CALMRUN_MIN_FREE_GB", "0")
        .env("CALMRUN_MIN_FREE_DISK_GB", "0")
        .env("CALMRUN_CRITICAL_FREE_GB", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    c
}

/// `calmrun <flags> -- calmrun __probe <probe>`, with stderr captured.
fn job(home: &Path, flags: &[&str], probe: &[&str]) -> Child {
    calmrun(home)
        .args(flags)
        .args(["--", EXE, "__probe"])
        .args(probe)
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn run(home: &Path, args: &[&str]) -> Output {
    calmrun(home)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap()
}

fn status(home: &Path) -> serde_json::Value {
    serde_json::from_slice(&run(home, &["status", "--json"]).stdout).unwrap()
}

/// Poll `status --json` until `ok` holds for it.
fn wait_for(home: &Path, what: &str, ok: impl Fn(&serde_json::Value) -> bool) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let s = status(home);
        if ok(&s) {
            return s;
        }
        assert!(Instant::now() < deadline, "never {what}: {s}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn len(v: &serde_json::Value) -> usize {
    v.as_array().map_or(0, Vec::len)
}

/// Exit code and stderr of a finished child.
fn finish(c: Child, limit: Duration) -> (i32, String) {
    let deadline = Instant::now() + limit;
    let mut c = c;
    loop {
        if c.try_wait().unwrap().is_some() {
            let out = c.wait_with_output().unwrap();
            return (
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr).to_string(),
            );
        }
        if Instant::now() > deadline {
            let _ = c.kill();
            panic!("did not finish in {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn history(home: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(home.join("history.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).to_string()
}

fn alive(pid: u32) -> bool {
    let mut sys = sysinfo::System::new().unwrap();
    let p = sysinfo::Pid::from_u32(pid);
    sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[p]), true);
    sys.process(p).is_some_and(|pr| {
        !matches!(
            pr.status(),
            sysinfo::ProcessStatus::Zombie | sysinfo::ProcessStatus::Dead
        )
    })
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
}

#[test]
fn stop_ends_a_running_job_and_its_calmrun_exits_130() {
    let home = tempfile::tempdir().unwrap();
    let owner = job(home.path(), &["--label", "long"], &["--sleep-ms", "30000"]);
    let s = wait_for(home.path(), "running", |s| {
        len(&s["running"]) == 1 && !s["running"][0]["child_pid"].is_null()
    });
    let id = s["running"][0]["id"].as_u64().unwrap().to_string();
    // The text status has an id column.
    let text = String::from_utf8_lossy(&run(home.path(), &["status"]).stdout).to_string();
    assert!(text.contains("   id label"), "{text}");
    let started = Instant::now();
    let out = run(home.path(), &["stop", &id]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("its calmrun stopped the command"),
        "{}",
        stderr(&out)
    );
    let (code, err) = finish(owner, Duration::from_secs(20));
    assert_eq!(code, 130, "{err}");
    assert!(err.contains("calmrun: stopped by calmrun stop"), "{err}");
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(len(&status(home.path())["running"]), 0);
    let h = history(home.path());
    assert_eq!(h.last().unwrap()["status"], "stopped", "{h:?}");
    assert!(h.last().unwrap()["cwd"].is_string(), "{h:?}");
}

#[test]
fn stop_takes_a_waiting_job_out_of_line() {
    let home = tempfile::tempdir().unwrap();
    let holder = job(home.path(), &["--cost", "2"], &["--sleep-ms", "6000"]);
    wait_for(home.path(), "holder running", |s| len(&s["running"]) == 1);
    let waiter = job(
        home.path(),
        &["--cost", "2", "--label", "behind"],
        &["--sleep-ms", "10"],
    );
    wait_for(home.path(), "a waiter", |s| len(&s["waiting"]) == 1);
    let out = run(home.path(), &["stop", "--label", "behind"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let (code, err) = finish(waiter, Duration::from_secs(20));
    assert_eq!(code, 130, "{err}");
    assert!(err.contains("stopped by calmrun stop while waiting"), "{err}");
    assert_eq!(len(&status(home.path())["waiting"]), 0);
    let stopped: Vec<_> = history(home.path())
        .into_iter()
        .filter(|r| r["status"] == "stopped")
        .collect();
    assert_eq!(stopped.len(), 1, "{stopped:?}");
    assert_eq!(
        (stopped[0]["label"].as_str(), stopped[0]["run_ms"].as_u64()),
        (Some("behind"), Some(0))
    );
    assert_eq!(finish(holder, Duration::from_secs(30)).0, 0);
}

#[test]
fn stop_needs_a_label_that_names_one_job() {
    let home = tempfile::tempdir().unwrap();
    let a = job(
        home.path(),
        &["--cost", "2", "--label", "same"],
        &["--sleep-ms", "5000"],
    );
    wait_for(home.path(), "running", |s| len(&s["running"]) == 1);
    let b = job(
        home.path(),
        &["--cost", "2", "--label", "same"],
        &["--sleep-ms", "10"],
    );
    wait_for(home.path(), "waiting", |s| len(&s["waiting"]) == 1);
    let out = run(home.path(), &["stop", "--label", "same"]);
    assert_eq!(out.status.code(), Some(2));
    let err = stderr(&out);
    assert!(
        err.contains("2 jobs are labelled \"same\"") && err.contains("calmrun stop <id>"),
        "{err}"
    );
    let out = run(home.path(), &["stop", "999"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("no job with id 999"), "{}", stderr(&out));
    assert_eq!(finish(a, Duration::from_secs(30)).0, 0);
    assert_eq!(finish(b, Duration::from_secs(30)).0, 0);
}

/// A fake owner: a process that never reads the state (like a calmrun older than 0.4), with
/// a child of its own. Returns it, the child's pid and its own start time.
fn fake_owner(home: &Path) -> (Child, u32, u64) {
    let pidfile = home.join("daemon.pid");
    let fake = Command::new(EXE)
        .args(["__probe", "--sleep-ms", "30000", "--spawn-daemon"])
        .arg(&pidfile)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let daemon: u32 = loop {
        if let Some(p) = std::fs::read_to_string(&pidfile)
            .ok()
            .and_then(|s| s.trim().parse().ok())
        {
            break p;
        }
        assert!(Instant::now() < deadline, "no daemon pid");
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut sys = sysinfo::System::new().unwrap();
    let pid = sysinfo::Pid::from_u32(fake.id());
    sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
    let start = sys.process(pid).unwrap().start_time();
    (fake, daemon, start)
}

#[test]
fn stop_kills_an_owner_that_does_not_react() {
    let home = tempfile::tempdir().unwrap();
    let (mut fake, daemon, start) = fake_owner(home.path());
    let now = now_ms();
    let state = serde_json::json!({
        "next_id": 7,
        "jobs": [{
            "id": 7, "pid": fake.id(), "pid_start": start, "label": "fake owner", "kind": "test",
            "cost": 1, "keys": [], "cwd": home.path(), "high": false, "order": 7,
            "queued_ms": now, "started_ms": now, "child_pid": daemon
        }]
    });
    std::fs::write(home.path().join("state.json"), state.to_string()).unwrap();
    let out = run(home.path(), &["stop", "7", "--wait", "1"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("did not react within 1s"),
        "{}",
        stderr(&out)
    );
    let _ = fake.wait();
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(daemon) {
        assert!(Instant::now() < deadline, "the owner's child survived");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(len(&status(home.path())["running"]), 0);
    let h = history(home.path());
    assert_eq!(h.len(), 1, "one history line, not a second 'killed': {h:?}");
    assert_eq!(h[0]["status"], "stopped");
}

#[test]
fn a_run_time_limit_stops_with_124_and_says_how_to_change_it() {
    let home = tempfile::tempdir().unwrap();
    // From the config (here the env): 0.02 min, about a second.
    let (code, err) = finish(
        calmrun(home.path())
            .env("CALMRUN_MAX_RUN_MIN", "0.02")
            .args(["--kind", "test", "--", EXE, "__probe", "--sleep-ms", "20000"])
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
        Duration::from_secs(20),
    );
    assert_eq!(code, 124, "{err}");
    assert!(
        err.contains("run time limit (max_run_min)") && err.contains("--timeout"),
        "{err}"
    );
    // --timeout 0: no limit for this run, whatever the config says.
    let (code, err) = finish(
        calmrun(home.path())
            .env("CALMRUN_MAX_RUN_MIN", "0.02")
            .args([
                "--timeout",
                "0",
                "--kind",
                "test",
                "--",
                EXE,
                "__probe",
                "--sleep-ms",
                "2000",
            ])
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
        Duration::from_secs(20),
    );
    assert_eq!(code, 0, "{err}");
    // --timeout SECS for one run; status shows the limit meanwhile.
    let c = job(home.path(), &["--timeout", "3"], &["--sleep-ms", "20000"]);
    let s = wait_for(home.path(), "running", |s| len(&s["running"]) == 1);
    assert_eq!(s["running"][0]["timeout_ms"], 3000, "{s}");
    let (code, err) = finish(c, Duration::from_secs(20));
    assert_eq!(code, 124, "{err}");
    assert!(err.contains("after the 3s timeout"), "{err}");
    let statuses: Vec<String> = history(home.path())
        .iter()
        .map(|r| r["status"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(statuses, ["timeout", "0", "timeout"]);
}

#[test]
fn a_busy_job_shows_its_cpu_and_the_usual_run_time() {
    let home = tempfile::tempdir().unwrap();
    // Five earlier successful runs of the label: a usual run time of 1 s.
    let line = r#"{"at":"2026-11-20T10:00:00Z","label":"busy","kind":"test","cost":1,"wait_ms":0,"run_ms":1000,"status":"0"}"#;
    std::fs::write(
        home.path().join("history.jsonl"),
        format!("{}\n", [line; 5].join("\n")),
    )
    .unwrap();
    let busy = job(
        home.path(),
        &["--cost", "2", "--label", "busy"],
        &["--burn-ms", "8000"],
    );
    let s = wait_for(home.path(), "CPU samples", |s| {
        s["running"][0]["cpu_ms"].as_u64().is_some_and(|ms| ms > 0)
            && s["running"][0]["cpu_pct"].as_u64().is_some_and(|p| p >= 2)
    });
    let r = &s["running"][0];
    assert_eq!(r["usual_ms"], 1000, "{s}");
    assert_eq!(r["idle_ms"], serde_json::Value::Null, "{s}");
    assert!(r["hint"].as_str().unwrap().contains("usually 1s"), "{s}");
    let text = String::from_utf8_lossy(&run(home.path(), &["status"]).stdout).to_string();
    assert!(
        text.contains("now: cpu ") && text.contains("usually 1s"),
        "{text}"
    );
    // A job waiting behind it hears how the running one is doing.
    let waiter = job(home.path(), &["--cost", "2"], &["--sleep-ms", "10"]);
    let (code, err) = finish(waiter, Duration::from_secs(40));
    assert_eq!(code, 0, "{err}");
    assert!(
        err.contains("running: busy in ") && err.contains("usually 1s"),
        "{err}"
    );
    assert_eq!(finish(busy, Duration::from_secs(30)).0, 0);
}

#[test]
fn stop_never_kills_an_owner_that_runs_other_jobs_too() {
    let home = tempfile::tempdir().unwrap();
    // One fake owner with two entries, like an MCP server running two tasks.
    let (mut fake, daemon, start) = fake_owner(home.path());
    let now = now_ms();
    let entry = |id: u64, running: bool| {
        serde_json::json!({
            "id": id, "pid": fake.id(), "pid_start": start, "label": format!("task {id}"),
            "kind": "test", "cost": 1, "keys": [], "cwd": home.path(), "high": false,
            "order": id, "queued_ms": now,
            "started_ms": if running { Some(now) } else { None },
            "child_pid": if running { Some(daemon) } else { None }
        })
    };
    let state = serde_json::json!({"next_id": 8, "jobs": [entry(7, true), entry(8, false)]});
    std::fs::write(home.path().join("state.json"), state.to_string()).unwrap();
    // The waiting one cannot be stopped without killing the owner: refused, entry kept.
    let out = run(home.path(), &["stop", "8", "--wait", "1"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("also owns jobs 7"), "{}", stderr(&out));
    assert_eq!(len(&status(home.path())["waiting"]), 1);
    // The running one: only its command goes; the owner lives on.
    let out = run(home.path(), &["stop", "7", "--wait", "1"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("only the command was killed (1 process(es))"),
        "{}",
        stderr(&out)
    );
    assert!(!alive(daemon), "the command survived");
    assert!(alive(fake.id()), "the shared owner was killed");
    let s = status(home.path());
    assert_eq!((len(&s["running"]), len(&s["waiting"])), (0, 1), "{s}");
    let _ = fake.kill();
    let _ = fake.wait();
}

#[test]
fn the_usual_run_time_is_per_folder() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("proj");
    std::fs::create_dir_all(&dir).unwrap();
    // Three runs here are enough (five would be needed across folders).
    for _ in 0..3 {
        let c = calmrun(home.path())
            .current_dir(&dir)
            .args(["--label", "quick", "--", EXE, "__probe", "--sleep-ms", "10"])
            .status()
            .unwrap();
        assert!(c.success());
    }
    let c = calmrun(home.path())
        .current_dir(&dir)
        .args(["--label", "quick", "--", EXE, "__probe", "--sleep-ms", "4000"])
        .spawn()
        .unwrap();
    let s = wait_for(home.path(), "running", |s| len(&s["running"]) == 1);
    assert!(s["running"][0]["usual_ms"].as_u64().is_some(), "{s}");
    assert_eq!(finish(c, Duration::from_secs(20)).0, 0);
}
