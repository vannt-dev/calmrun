//! Memory ceiling: a job whose process tree grows too large is stopped.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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
        // The critical-free-RAM guard depends on the machine; keep it out of these tests.
        .env("CALMRUN_CRITICAL_FREE_GB", "0")
        .stdin(Stdio::null());
    c
}

#[test]
fn tree_above_the_ceiling_is_stopped() {
    let home = tempfile::tempdir().unwrap();
    let started = Instant::now();
    let out = calmrun(home.path())
        .env("CALMRUN_CEILING_GB", "0.2")
        // A nested calmrun in between: the ceiling counts the whole tree.
        .args([
            "--",
            EXE,
            "--",
            EXE,
            "__probe",
            "--alloc-mb",
            "400",
            "--sleep-ms",
            "30000",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(137));
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "took {:?}",
        started.elapsed()
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("above the 0.2 GB ceiling"), "{err}");
    let hist = std::fs::read_to_string(home.path().join("history.jsonl")).unwrap();
    assert!(hist.contains("\"status\":\"ceiling\""), "{hist}");
}

#[test]
fn status_shows_tree_memory() {
    let home = tempfile::tempdir().unwrap();
    let mut child = calmrun(home.path())
        .args(["--", EXE, "__probe", "--alloc-mb", "120", "--sleep-ms", "6000"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let out = calmrun(home.path()).args(["status", "--json"]).output().unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        let mem = v["running"][0]["mem_bytes"].as_u64().unwrap_or(0);
        if mem >= 100 * 1024 * 1024 {
            break;
        }
        assert!(Instant::now() < deadline, "mem_bytes never reached 100 MB: {v}");
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(child.wait().unwrap().success());
}
