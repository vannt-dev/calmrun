//! End-to-end tests: several real calmrun processes against one temporary state dir.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const EXE: &str = env!("CARGO_BIN_EXE_calmrun");

struct Env {
    home: tempfile::TempDir,
    log: PathBuf,
}

impl Env {
    fn new() -> Env {
        let home = tempfile::tempdir().unwrap();
        let log = home.path().join("probe.log");
        Env { home, log }
    }

    fn cmd(&self, budget: u32) -> Command {
        let mut c = Command::new(EXE);
        for (k, _) in std::env::vars() {
            if k.starts_with("CALMRUN_") {
                c.env_remove(k);
            }
        }
        c.env("CALMRUN_HOME", self.home.path())
            .env("CALMRUN_BUDGET", budget.to_string())
            .env("CALMRUN_MIN_FREE_GB", "0")
            .env("CALMRUN_MIN_FREE_DISK_GB", "0")
            .env("CALMRUN_CRITICAL_FREE_GB", "0")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        c
    }

    /// `calmrun <flags> -- calmrun __probe --log L --id ID --sleep-ms MS`
    fn probe(&self, budget: u32, flags: &[&str], id: &str, ms: u64, cwd: Option<&Path>) -> Child {
        let mut c = self.cmd(budget);
        c.args(flags)
            .arg("--")
            .arg(EXE)
            .args(["__probe", "--name", id, "--sleep-ms", &ms.to_string(), "--log"])
            .arg(&self.log);
        if let Some(d) = cwd {
            c.current_dir(d);
        }
        c.spawn().unwrap()
    }

    /// (id, start_ms, end_ms) per probe.
    fn intervals(&self) -> Vec<(String, u64, u64)> {
        let text = std::fs::read_to_string(&self.log).unwrap_or_default();
        let mut starts = std::collections::HashMap::new();
        let mut out = Vec::new();
        for line in text.lines() {
            let p: Vec<&str> = line.split_whitespace().collect();
            let t: u64 = p[2].parse().unwrap();
            match p[0] {
                "start" => {
                    starts.insert(p[1].to_string(), t);
                }
                _ => out.push((p[1].to_string(), starts[p[1]], t)),
            }
        }
        out.sort_by_key(|x| x.1);
        out
    }
}

impl Env {
    fn status(&self) -> serde_json::Value {
        let out = self
            .cmd(2)
            .args(["status", "--json"])
            .stdout(Stdio::piped())
            .output()
            .unwrap();
        serde_json::from_slice(&out.stdout).unwrap()
    }

    /// Poll `status --json` until `running` and `waiting` have the given lengths.
    fn wait_queue(&self, running: usize, waiting: usize) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let s = self.status();
            let r = s["running"].as_array().map_or(0, Vec::len);
            let w = s["waiting"].as_array().map_or(0, Vec::len);
            if r == running && w == waiting {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "queue never reached {running}/{waiting}: {s}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn wait_all(children: Vec<Child>, limit: Duration) -> Vec<i32> {
    let deadline = Instant::now() + limit;
    children
        .into_iter()
        .map(|mut c| loop {
            if let Some(s) = c.try_wait().unwrap() {
                break s.code().unwrap_or(-1);
            }
            if Instant::now() > deadline {
                let _ = c.kill();
                panic!("calmrun did not finish in {limit:?}");
            }
            std::thread::sleep(Duration::from_millis(50));
        })
        .collect()
}

/// Highest number of overlapping intervals, weighted by cost.
fn max_overlap(iv: &[(String, u64, u64)], cost: impl Fn(&str) -> u32) -> u32 {
    let mut events: Vec<(u64, i64)> = Vec::new();
    for (id, s, e) in iv {
        events.push((*s, cost(id) as i64));
        events.push((*e, -(cost(id) as i64)));
    }
    // Ends sort before starts at the same millisecond.
    events.sort_by_key(|&(t, d)| (t, d));
    let (mut cur, mut max) = (0i64, 0i64);
    for (_, d) in events {
        cur += d;
        max = max.max(cur);
    }
    max as u32
}

#[test]
fn concurrency_never_exceeds_budget() {
    let env = Env::new();
    let kids: Vec<Child> = (0..5)
        .map(|i| env.probe(2, &["--cost", "1", "--kind", "test"], &format!("p{i}"), 700, None))
        .collect();
    assert!(wait_all(kids, Duration::from_secs(60)).iter().all(|&c| c == 0));
    let iv = env.intervals();
    assert_eq!(iv.len(), 5);
    let max = max_overlap(&iv, |_| 1);
    assert!(max <= 2, "overlap {max} exceeds budget 2: {iv:?}");
    assert_eq!(max, 2, "expected two jobs to run side by side: {iv:?}");
}

#[test]
fn cost_two_jobs_run_one_at_a_time() {
    let env = Env::new();
    let kids: Vec<Child> = (0..3)
        .map(|i| env.probe(2, &["--cost", "2"], &format!("c{i}"), 400, None))
        .collect();
    assert!(wait_all(kids, Duration::from_secs(60)).iter().all(|&c| c == 0));
    assert_eq!(max_overlap(&env.intervals(), |_| 1), 1);
}

#[test]
fn exclusive_node_modules_key() {
    let env = Env::new();
    let proj = env.home.path().join("proj");
    std::fs::create_dir_all(proj.join("node_modules")).unwrap();
    std::fs::create_dir_all(proj.join("pkg")).unwrap();
    let other = env.home.path().join("other");
    std::fs::create_dir_all(other.join("node_modules")).unwrap();
    let mut kids = Vec::new();
    for i in 0..3 {
        let dir = if i == 1 { proj.join("pkg") } else { proj.clone() };
        kids.push(env.probe(
            8,
            &["--kind", "vitest", "--cost", "1"],
            &format!("same{i}"),
            400,
            Some(&dir),
        ));
    }
    kids.push(env.probe(
        8,
        &["--kind", "vitest", "--cost", "1"],
        "other",
        400,
        Some(&other),
    ));
    assert!(wait_all(kids, Duration::from_secs(60)).iter().all(|&c| c == 0));
    let iv = env.intervals();
    let same: Vec<_> = iv.iter().filter(|x| x.0.starts_with("same")).cloned().collect();
    assert_eq!(same.len(), 3);
    assert_eq!(
        max_overlap(&same, |_| 1),
        1,
        "same node_modules overlapped: {same:?}"
    );
}

#[test]
fn first_come_first_served() {
    let env = Env::new();
    let a = env.probe(2, &["--cost", "2"], "a", 3000, None);
    env.wait_queue(1, 0);
    let b = env.probe(2, &["--cost", "2"], "b", 200, None);
    env.wait_queue(1, 1);
    let c = env.probe(2, &["--cost", "2"], "c", 200, None);
    assert!(wait_all(vec![a, b, c], Duration::from_secs(60))
        .iter()
        .all(|&x| x == 0));
    let order: Vec<String> = env.intervals().into_iter().map(|x| x.0).collect();
    assert_eq!(order, vec!["a", "b", "c"]);
}

#[test]
fn prio_high_jumps_the_queue() {
    let env = Env::new();
    let a = env.probe(2, &["--cost", "2"], "a", 3000, None);
    env.wait_queue(1, 0);
    let b = env.probe(2, &["--cost", "2"], "b", 200, None);
    env.wait_queue(1, 1);
    let c = env.probe(2, &["--cost", "2", "--prio", "high"], "c", 200, None);
    assert!(wait_all(vec![a, b, c], Duration::from_secs(60))
        .iter()
        .all(|&x| x == 0));
    let order: Vec<String> = env.intervals().into_iter().map(|x| x.0).collect();
    assert_eq!(order, vec!["a", "c", "b"]);
}

#[test]
fn exit_code_passes_through() {
    let env = Env::new();
    let code = env
        .cmd(2)
        .args(["--", EXE, "__probe", "--exit", "7"])
        .status()
        .unwrap()
        .code();
    assert_eq!(code, Some(7));
    let hist = std::fs::read_to_string(env.home.path().join("history.jsonl")).unwrap();
    assert!(hist.contains("\"status\":\"7\""), "{hist}");
}

#[test]
fn missing_command_is_127() {
    let env = Env::new();
    let code = env
        .cmd(2)
        .args(["--", "calmrun-no-such-command-xyz"])
        .status()
        .unwrap()
        .code();
    assert_eq!(code, Some(127));
}

#[test]
fn nested_run_does_not_deadlock() {
    let env = Env::new();
    let mut c = env.cmd(2);
    c.args([
        "--cost", "2", "--", EXE, "--cost", "2", "--", EXE, "__probe", "--name", "inner", "--log",
    ])
    .arg(&env.log);
    let child = c.spawn().unwrap();
    assert_eq!(wait_all(vec![child], Duration::from_secs(20)), vec![0]);
    assert_eq!(env.intervals().len(), 1);
}

#[test]
fn killed_waiter_leaves_the_queue() {
    let env = Env::new();
    let a = env.probe(2, &["--cost", "2"], "a", 4000, None);
    env.wait_queue(1, 0);
    let mut b = env.probe(2, &["--cost", "2", "--label", "victim"], "b", 100, None);
    env.wait_queue(1, 1);
    let s = env.status();
    assert_eq!(s["running"].as_array().unwrap().len(), 1);
    assert_eq!(s["waiting"][0]["label"], "victim");
    b.kill().unwrap();
    let _ = b.wait();
    let s = env.status();
    assert_eq!(s["waiting"].as_array().unwrap().len(), 0, "{s}");
    assert_eq!(wait_all(vec![a], Duration::from_secs(20)), vec![0]);
}

#[test]
fn bump_moves_a_waiter_forward() {
    let env = Env::new();
    let a = env.probe(2, &["--cost", "2"], "a", 3000, None);
    env.wait_queue(1, 0);
    let b = env.probe(2, &["--cost", "2", "--label", "first"], "b", 200, None);
    env.wait_queue(1, 1);
    let c = env.probe(2, &["--cost", "2", "--label", "urgent-e2e"], "c", 200, None);
    env.wait_queue(1, 2);
    let out = env.cmd(2).args(["bump", "URGENT"]).status().unwrap();
    assert!(out.success());
    assert!(wait_all(vec![a, b, c], Duration::from_secs(60))
        .iter()
        .all(|&x| x == 0));
    let order: Vec<String> = env.intervals().into_iter().map(|x| x.0).collect();
    assert_eq!(order, vec!["a", "c", "b"]);
}

#[test]
fn hook_denies_heavy_and_allows_light() {
    let env = Env::new();
    let run = |cmd: &str| -> String {
        let input = serde_json::json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": cmd}
        });
        let mut child = env
            .cmd(2)
            .args(["hook", "claude"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.to_string().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap()
    };
    let denied = run("npx vitest run");
    let v: serde_json::Value = serde_json::from_str(&denied).unwrap();
    assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
    assert_eq!(run("git status"), "");
}

#[cfg(windows)]
#[test]
fn windows_cmd_shim_arguments_survive() {
    let env = Env::new();
    let dir = env.home.path().join("bin");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("echoargs.cmd"),
        format!("@\"{EXE}\" __probe --print-args -- %*\r\n"),
    )
    .unwrap();
    let args = [
        "a b", "x&y", "100%", "%PATH%", "q\"t", "c^d", "p|q", "(paren)", "tr\\",
    ];
    let path = format!("{};{}", dir.display(), std::env::var("PATH").unwrap_or_default());
    let out = env
        .cmd(2)
        .env("PATH", path)
        .arg("--")
        .arg("echoargs")
        .args(args)
        .stdout(Stdio::piped())
        .output()
        .unwrap();
    assert!(out.status.success());
    let got: Vec<String> = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(got, args);
}

#[test]
fn wait_timeout_keeps_the_place_with_a_ticket() {
    let env = Env::new();
    let holder = env.probe(2, &["--cost", "2"], "holder", 6000, None);
    env.wait_queue(1, 0);
    // B gives up after 1 s and gets a ticket.
    let out = env
        .cmd(2)
        .args([
            "--cost",
            "2",
            "--wait-timeout",
            "1",
            "--",
            EXE,
            "__probe",
            "--name",
            "b",
            "--log",
        ])
        .arg(&env.log)
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(75));
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    let next = err
        .lines()
        .find_map(|l| l.strip_prefix("calmrun: next: "))
        .unwrap_or_else(|| panic!("no next line in: {err}"))
        .to_string();
    assert!(next.starts_with("calmrun --ticket "), "{next}");
    let ticket = next.split_whitespace().nth(2).unwrap().to_string();
    let s = env.status();
    assert_eq!(s["parked"][0]["ticket"].to_string(), ticket);
    assert_eq!(s["waiting"].as_array().unwrap().len(), 0);
    // C queues normally; then B comes back with its ticket and is still ahead of C.
    let c = env.probe(2, &["--cost", "2"], "c", 200, None);
    env.wait_queue(1, 1);
    let b = env.probe(2, &["--cost", "2", "--ticket", &ticket], "b", 200, None);
    env.wait_queue(1, 2);
    assert!(wait_all(vec![holder, b, c], Duration::from_secs(60))
        .iter()
        .all(|&x| x == 0));
    let order: Vec<String> = env.intervals().into_iter().map(|x| x.0).collect();
    assert_eq!(order, vec!["holder", "b", "c"]);
    assert_eq!(env.status()["parked"].as_array().unwrap().len(), 0);
}

#[test]
fn refusal_prints_the_next_command() {
    let env = Env::new();
    let out = env
        .cmd(2)
        .args(["--", "docker", "compose", "up"])
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("calmrun: next: calmrun -- docker compose up -d"),
        "{err}"
    );
}

#[test]
fn guide_prints_instructions() {
    let env = Env::new();
    let out = env.cmd(2).arg("guide").stdout(Stdio::piped()).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("calmrun -- npx vitest run") && text.contains("--wait-timeout"));
}

#[test]
fn short_disk_waits_and_says_why() {
    let env = Env::new();
    // Ask for far more free disk than any machine has.
    let mut holder_cmd = env.cmd(2);
    holder_cmd
        .env("CALMRUN_MIN_FREE_DISK_GB", "999999")
        .args([
            "--cost",
            "1",
            "--label",
            "holder",
            "--",
            EXE,
            "__probe",
            "--sleep-ms",
            "5000",
        ])
        .stderr(Stdio::piped());
    let holder = holder_cmd.spawn().unwrap();
    env.wait_queue(1, 0);
    let out = env
        .cmd(2)
        .env("CALMRUN_MIN_FREE_DISK_GB", "999999")
        .args(["--cost", "1", "--wait-timeout", "2", "--", EXE, "__probe"])
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(75));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("waiting for disk space"), "{err}");
    assert!(err.contains("; running: holder in "), "{err}");
    let h = holder.wait_with_output().unwrap();
    assert!(h.status.success());
    let herr = String::from_utf8_lossy(&h.stderr);
    assert!(herr.contains("starting anyway"), "{herr}");
}

#[test]
fn cargo_gets_a_job_count() {
    let env = Env::new();
    let dir = env.home.path().join("bin");
    std::fs::create_dir_all(&dir).unwrap();
    let fake = dir.join(if cfg!(windows) { "cargo.exe" } else { "cargo" });
    std::fs::copy(EXE, &fake).unwrap();
    let out = env
        .cmd(4)
        .env_remove("CARGO_BUILD_JOBS")
        .arg("--")
        .arg(&fake)
        .args(["__probe", "--print-env", "CARGO_BUILD_JOBS"])
        .stdout(Stdio::piped())
        .output()
        .unwrap();
    assert!(out.status.success());
    let n: u32 = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap();
    assert!(n >= 1);
    // An explicit setting wins.
    let out = env
        .cmd(4)
        .env("CARGO_BUILD_JOBS", "7")
        .arg("--")
        .arg(&fake)
        .args(["__probe", "--print-env", "CARGO_BUILD_JOBS"])
        .stdout(Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "7");
}

#[test]
fn named_resource_is_held_by_one_job_at_a_time() {
    let env = Env::new();
    let kids = vec![
        env.probe(4, &["--cost", "1", "--needs", "db"], "db1", 600, None),
        env.probe(4, &["--cost", "1", "--needs", "db"], "db2", 600, None),
        env.probe(4, &["--cost", "1"], "free", 600, None),
    ];
    assert!(wait_all(kids, Duration::from_secs(60)).iter().all(|&c| c == 0));
    let iv = env.intervals();
    let db: Vec<_> = iv.iter().filter(|x| x.0.starts_with("db")).cloned().collect();
    assert_eq!(max_overlap(&db, |_| 1), 1, "db holders overlapped: {db:?}");
    assert!(
        max_overlap(&iv, |_| 1) >= 2,
        "the job without needs should run alongside: {iv:?}"
    );
}

#[test]
fn separate_queues_do_not_share_points() {
    let env = Env::new();
    let a = env.probe(2, &["--cost", "2", "--id", "alpha"], "a", 1500, None);
    let b = env.probe(2, &["--cost", "2", "--id", "beta"], "b", 1500, None);
    // Each queue has its own budget, so both run at once.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let out = env
            .cmd(2)
            .args(["status", "--json", "--id", "alpha"])
            .stdout(Stdio::piped())
            .output()
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        if v["running"].as_array().is_some_and(|r| r.len() == 1) {
            assert_eq!(v["queue"], "alpha");
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(wait_all(vec![a, b], Duration::from_secs(60))
        .iter()
        .all(|&c| c == 0));
    assert_eq!(max_overlap(&env.intervals(), |_| 1), 2);
    // The default queue never saw them.
    assert_eq!(env.status()["running"].as_array().unwrap().len(), 0);
    let bad = env
        .cmd(2)
        .args(["--id", "../x", "--", EXE, "__probe"])
        .status()
        .unwrap();
    assert_eq!(bad.code(), Some(2));
}

#[cfg(windows)]
#[test]
fn relative_cmd_path_runs_from_its_folder() {
    let env = Env::new();
    let dir = env.home.path().join("proj");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("gradlew.bat"),
        format!("@\"{EXE}\" __probe --print-args -- %*\r\n"),
    )
    .unwrap();
    let out = env
        .cmd(2)
        .current_dir(&dir)
        .args(["--", "./gradlew", "build"])
        .stdout(Stdio::piped())
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    // The gradle rule adds --max-workers to a direct gradlew call.
    let got: Vec<String> = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(got[0], "build");
    assert!(got[1].starts_with("--max-workers="), "{got:?}");
}
