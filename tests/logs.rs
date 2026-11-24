//! `calmrun --log` (output copied to the terminal and a log) and `calmrun logs`.

use serde_json::Value;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const EXE: &str = env!("CARGO_BIN_EXE_calmrun");

struct Env {
    home: tempfile::TempDir,
    extra: Vec<(&'static str, &'static str)>,
}

impl Env {
    fn new() -> Env {
        Env {
            home: tempfile::tempdir().unwrap(),
            extra: Vec::new(),
        }
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(EXE);
        for (k, _) in std::env::vars() {
            if k.starts_with("CALMRUN_") {
                c.env_remove(k);
            }
        }
        c.env("CALMRUN_HOME", self.home.path())
            .env("CALMRUN_BUDGET", "2")
            .env("CALMRUN_MIN_FREE_GB", "0")
            .env("CALMRUN_MIN_FREE_DISK_GB", "0")
            .env("CALMRUN_CRITICAL_FREE_GB", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in &self.extra {
            c.env(k, v);
        }
        c
    }

    /// `calmrun <flags> -- calmrun __probe <probe>`, in the background.
    fn start(&self, flags: &[&str], probe: &[&str]) -> Child {
        self.cmd()
            .args(flags)
            .args(["--", EXE, "__probe"])
            .args(probe)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd().args(args).output().unwrap()
    }

    fn status(&self) -> Value {
        serde_json::from_slice(&self.run(&["status", "--json"]).stdout).unwrap()
    }

    /// Poll the status until `f` finds something in it.
    fn wait_for<T>(&self, what: &str, f: impl Fn(&Value) -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let s = self.status();
            if let Some(t) = f(&s) {
                return t;
            }
            assert!(Instant::now() < deadline, "never saw {what}: {s}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// The running job with this label, once it has a log.
    fn running_with_log(&self, label: &str) -> Value {
        self.wait_for(&format!("{label} running with a log"), |s| {
            s["running"]
                .as_array()?
                .iter()
                .find(|j| j["label"] == label && j["log_path"].is_string())
                .cloned()
        })
    }

    fn logs(&self) -> Vec<PathBuf> {
        std::fs::read_dir(self.home.path().join("logs"))
            .map(|rd| rd.flatten().map(|e| e.path()).collect())
            .unwrap_or_default()
    }
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).replace("\r\n", "\n")
}

fn ticks(n: u32) -> String {
    (0..n).map(|i| format!("tick {i}\n")).collect()
}

fn wait_exit(c: &mut Child, limit: Duration) -> i32 {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(s) = c.try_wait().unwrap() {
            return s.code().unwrap_or(-1);
        }
        if Instant::now() > deadline {
            let _ = c.kill();
            panic!("did not finish in {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn log_copies_both_streams_and_keeps_the_exit_code() {
    let env = Env::new();
    let out = env.run(&[
        "--log",
        "--",
        EXE,
        "__probe",
        "--ticks",
        "300",
        "--stderr",
        "to stderr",
        "--exit",
        "7",
    ]);
    assert_eq!(out.status.code(), Some(7), "{out:?}");
    // Each stream reaches the terminal whole and in order.
    assert_eq!(text(&out.stdout), ticks(300));
    assert!(text(&out.stderr).contains("to stderr\n"), "{out:?}");
    let logs = env.logs();
    assert_eq!(logs.len(), 1, "{logs:?}");
    let log = text(&std::fs::read(&logs[0]).unwrap());
    assert!(log.contains("to stderr"), "{log}");
    // Both streams share the file; a write of one may land inside a line of the other.
    let stdout_only: String = log
        .replace("to stderr", "")
        .split('\n')
        .filter(|l| !l.is_empty())
        .map(|l| format!("{l}\n"))
        .collect();
    assert_eq!(stdout_only, ticks(300), "{log}");
    assert_eq!(log.len(), ticks(300).len() + "to stderr\n".len(), "{log}");
    // The history remembers the log, so `calmrun logs <id>` finds it after the end.
    let hist = std::fs::read_to_string(env.home.path().join("history.jsonl")).unwrap();
    let rec: Value = serde_json::from_str(hist.lines().last().unwrap()).unwrap();
    assert_eq!(rec["log"], logs[0].to_string_lossy().as_ref());
    let id = rec["id"].as_u64().unwrap().to_string();
    let shown = env.run(&["logs", &id, "--tail", "2"]);
    assert_eq!(shown.status.code(), Some(0), "{shown:?}");
    // The last two lines of the file, whichever stream wrote them (stderr may come last).
    let lines: Vec<&str> = log.lines().collect();
    let last_two: String = lines[lines.len() - 2..]
        .iter()
        .map(|l| format!("{l}\n"))
        .collect();
    assert_eq!(text(&shown.stdout), last_two);
}

#[test]
fn a_slow_reader_loses_nothing() {
    let env = Env::new();
    // More than a pipe buffer of output, read only after a pause.
    let c = env
        .cmd()
        .args(["--log", "--", EXE, "__probe", "--ticks", "9000"])
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(4));
    let out = c.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{:?}", text(&out.stderr));
    assert!(text(&out.stdout) == ticks(9000), "stdout incomplete");
    let log = text(&std::fs::read(&env.logs()[0]).unwrap());
    assert!(log == ticks(9000), "log incomplete: {} bytes", log.len());
}

#[test]
fn log_keeps_stdin_and_without_it_nothing_is_written() {
    let env = Env::new();
    let mut c = env
        .cmd()
        .args(["--log", "--", EXE, "__probe", "--cat"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        let mut stdin = c.stdin.take().unwrap();
        stdin.write_all(b"from stdin\n").unwrap();
    }
    let out = c.wait_with_output().unwrap();
    assert_eq!(text(&out.stdout), "from stdin\n", "{out:?}");
    assert_eq!(env.logs().len(), 1);
    let out = env.run(&["--", EXE, "__probe", "--print-args", "x"]);
    assert_eq!(text(&out.stdout), "[\"x\"]\n");
    assert_eq!(env.logs().len(), 1, "no --log, no log file");
}

/// `output_ms` appears once the job prints and moves on with later output, with the memory
/// ceiling on and off.
fn output_ms_moves(ceiling: &'static str) {
    let mut env = Env::new();
    env.extra.push(("CALMRUN_CEILING_GB", ceiling));
    let mut job = env.start(
        &["--log", "--label", "ticker"],
        &["--ticks", "120", "--tick-ms", "500"],
    );
    let first = env.wait_for("output_ms", |s| s["running"][0]["output_ms"].as_u64());
    let later = env.wait_for("a later output_ms", |s| {
        s["running"][0]["output_ms"].as_u64().filter(|t| *t > first)
    });
    assert!(later > first);
    let s = env.status();
    let j = &s["running"][0];
    assert!(j["output_ago_ms"].as_u64().is_some(), "{s}");
    assert!(j["log_path"].as_str().unwrap().ends_with(".log"), "{s}");
    let text_status = text(&env.run(&["status"]).stdout);
    assert!(text_status.contains("last output "), "{text_status}");
    assert!(
        text_status.contains(&format!("calmrun logs {} --follow", j["id"])),
        "{text_status}"
    );
    let _ = job.kill();
    let _ = job.wait();
}

#[test]
fn output_ms_rides_along_with_the_memory_sample() {
    output_ms_moves("8");
}

#[test]
fn output_ms_moves_with_the_memory_ceiling_off() {
    output_ms_moves("0");
}

#[test]
fn logs_tail_and_follow_until_the_job_ends() {
    let env = Env::new();
    let mut job = env.start(
        &["--log", "--label", "ticker"],
        &["--ticks", "12", "--tick-ms", "400"],
    );
    let j = env.running_with_log("ticker");
    let id = j["id"].to_string();
    // Wait for a few lines, then the tail shows the newest one(s) only.
    let log = PathBuf::from(j["log_path"].as_str().unwrap());
    let deadline = Instant::now() + Duration::from_secs(20);
    while text(&std::fs::read(&log).unwrap_or_default()).lines().count() < 3 {
        assert!(Instant::now() < deadline, "no output");
        std::thread::sleep(Duration::from_millis(50));
    }
    let out = env.run(&["logs", &id, "--tail", "1"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let shown = text(&out.stdout);
    assert_eq!(shown.lines().count(), 1, "{shown}");
    assert!(shown.starts_with("tick "), "{shown}");
    // --follow prints up to the last line and exits 0 once the job has ended.
    let mut f = env
        .cmd()
        .args(["logs", &id, "--follow", "--tail", "100"])
        .spawn()
        .unwrap();
    let code = wait_exit(&mut f, Duration::from_secs(30));
    let out = f.wait_with_output().unwrap();
    assert_eq!(code, 0, "{out:?}");
    assert_eq!(text(&out.stdout), ticks(12));
    assert!(text(&out.stderr).contains("has ended"), "{out:?}");
    assert_eq!(wait_exit(&mut job, Duration::from_secs(10)), 0);
}

#[test]
fn logs_by_label_needs_exactly_one_match() {
    let env = Env::new();
    let mut a = env.start(&["--log", "--label", "e2e-a"], &["--sleep-ms", "20000"]);
    let mut b = env.start(&["--label", "e2e-b"], &["--sleep-ms", "20000"]);
    env.running_with_log("e2e-a");
    env.wait_for("e2e-b running", |s| {
        s["running"].as_array().filter(|r| r.len() == 2).map(|_| ())
    });
    let out = env.run(&["logs", "--label", "E2E"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let err = text(&out.stderr);
    assert!(err.contains("2 jobs match"), "{err}");
    assert!(
        err.contains("e2e-a (running, has a log)") && err.contains("e2e-b (running, no log)"),
        "{err}"
    );
    assert_eq!(env.run(&["logs", "--label", "e2e-a"]).status.code(), Some(0));
    let out = env.run(&["logs", "--label", "nothing"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    // A job without a log says how to get one.
    let out = env.run(&["logs", "--label", "e2e-b"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(text(&out.stderr).contains("calmrun --log --"), "{out:?}");
    // An id that is not in the queue.
    assert_eq!(env.run(&["logs", "999"]).status.code(), Some(1));
    for c in [&mut a, &mut b] {
        let _ = c.kill();
        let _ = c.wait();
    }
}

#[test]
fn a_waiting_line_points_to_the_running_jobs_log() {
    let env = Env::new();
    let mut a = env.start(
        &["--log", "--cost", "2", "--label", "big"],
        &["--sleep-ms", "20000"],
    );
    let j = env.running_with_log("big");
    let out = env.run(&["--wait-timeout", "1", "--", EXE, "__probe"]);
    assert_eq!(out.status.code(), Some(75), "{out:?}");
    let err = text(&out.stderr);
    assert!(
        err.contains(&format!("; output of big: calmrun logs {}", j["id"])),
        "{err}"
    );
    let _ = a.kill();
    let _ = a.wait();
}

#[test]
fn log_on_by_config_and_off_by_flag() {
    let env = Env::new();
    std::fs::write(env.home.path().join("config.toml"), "log = true\n").unwrap();
    let out = env.run(&["--", EXE, "__probe", "--print-args", "x"]);
    assert_eq!(text(&out.stdout), "[\"x\"]\n", "{out:?}");
    assert_eq!(env.logs().len(), 1);
    env.run(&["--no-log", "--", EXE, "__probe"]);
    assert_eq!(env.logs().len(), 1);
}
