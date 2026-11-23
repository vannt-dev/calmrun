//! `calmrun hold` / `calmrun release`: resource leases without a running command.

use std::path::Path;
use std::process::{Command, Output, Stdio};
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
        .env("CALMRUN_CRITICAL_FREE_GB", "0")
        .stdin(Stdio::null());
    c
}

fn run(home: &Path, args: &[&str]) -> Output {
    calmrun(home).args(args).output().unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).to_string()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).to_string()
}

/// `calmrun hold …` that must succeed; returns the lease id.
fn hold(home: &Path, args: &[&str]) -> String {
    let mut all = vec!["hold"];
    all.extend_from_slice(args);
    let o = run(home, &all);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let out = stdout(&o);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 1, "exactly one line on stdout: {out:?}");
    let id = lines[0].to_string();
    assert!(id.len() == 6 && id.chars().all(|c| c.is_ascii_hexdigit()), "{id}");
    id
}

fn status(home: &Path, queue: Option<&str>) -> serde_json::Value {
    let mut args = vec!["status", "--json"];
    if let Some(q) = queue {
        args.extend(["--id", q]);
    }
    serde_json::from_slice(&run(home, &args).stdout).unwrap()
}

fn leases(home: &Path, queue: Option<&str>) -> Vec<serde_json::Value> {
    status(home, queue)["leases"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

/// A queued probe job with these flags; returns its exit code.
fn job(home: &Path, flags: &[&str], sleep_ms: u64) -> i32 {
    calmrun(home)
        .args(flags)
        .args(["--", EXE, "__probe", "--sleep-ms", &sleep_ms.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .code()
        .unwrap_or(-1)
}

#[test]
fn a_lease_is_held_listed_renewed_and_released() {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    let id = hold(h, &["emulator", "--label", "lane calmrun-4", "--ttl", "600"]);
    let l = leases(h, None);
    assert_eq!(l.len(), 1);
    assert_eq!(l[0]["lease"], id.as_str());
    assert_eq!(l[0]["resources"], serde_json::json!(["emulator"]));
    assert_eq!(l[0]["label"], "lane calmrun-4");
    assert_eq!(l[0]["cost"], 0);
    let left = l[0]["expires_in_ms"].as_u64().unwrap();
    assert!(left > 590_000 && left <= 600_000, "{left}");
    // Leases are listed apart from jobs, and take no points.
    let s = status(h, None);
    assert_eq!(s["running"].as_array().unwrap().len(), 0);
    assert_eq!(s["used"], 0);
    let text = stdout(&run(h, &["status"]));
    assert!(text.contains("leases (1):"), "{text}");
    assert!(text.contains(&id) && text.contains("emulator"), "{text}");

    // Renew: a new expiry from now, and the id again on stdout.
    let o = run(h, &["hold", "--renew", &id, "--ttl", "7200"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(stdout(&o).trim(), id);
    let left = leases(h, None)[0]["expires_in_ms"].as_u64().unwrap();
    assert!(left > 7_100_000, "{left}");

    // Release, then release again: both fine, the second with a note.
    let o = run(h, &["release", &id]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert!(leases(h, None).is_empty());
    let o = run(h, &["release", &id]);
    assert_eq!(o.status.code(), Some(0));
    assert!(
        stderr(&o).contains("already released or expired"),
        "{}",
        stderr(&o)
    );
    // Renewing a lease that is gone fails.
    assert_eq!(run(h, &["hold", "--renew", &id]).status.code(), Some(1));
    // Ids of the wrong shape are usage errors.
    for bad in ["xyz", "1234567", "zzzzzz"] {
        assert_eq!(run(h, &["release", bad]).status.code(), Some(2), "{bad}");
        assert_eq!(run(h, &["hold", "--renew", bad]).status.code(), Some(2), "{bad}");
    }
    // A hold needs a resource unless it renews.
    assert_eq!(run(h, &["hold"]).status.code(), Some(2));
}

#[test]
fn an_expired_lease_frees_its_slot() {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    hold(h, &["emulator", "--ttl", "1"]);
    assert_eq!(leases(h, None).len(), 1);
    std::thread::sleep(Duration::from_millis(1_300));
    // Any calmrun that reads the state drops it.
    assert!(leases(h, None).is_empty());
    let t0 = Instant::now();
    hold(h, &["emulator"]);
    assert!(t0.elapsed() < Duration::from_secs(5));
}

#[test]
fn jobs_needing_a_leased_resource_wait_and_others_do_not() {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    let id = hold(h, &["emulator"]);
    // The emulator's only slot is leased: a job that needs it does not start.
    assert_eq!(job(h, &["--needs", "emulator", "--wait-timeout", "1"], 10), 75);
    // A job that does not need it runs as usual, even a full-budget one.
    assert_eq!(job(h, &["--cost", "2"], 10), 0);
    // A second hold of it waits too: --wait-timeout exits 75 and leaves the line.
    let o = run(h, &["hold", "emulator", "--wait-timeout", "1"]);
    assert_eq!(o.status.code(), Some(75), "{}", stderr(&o));
    assert!(stdout(&o).is_empty());
    assert!(stderr(&o).contains(&format!("lease {id}")), "{}", stderr(&o));
    let s = status(h, None);
    assert!(s["waiting"].as_array().unwrap().is_empty(), "{s}");
    // Only the --needs job above kept a ticket; a hold never does.
    assert!(
        s["parked"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["kind"] != "hold"),
        "no ticket for a hold: {s}"
    );
    // Released: the job gets the emulator.
    assert_eq!(run(h, &["release", &id]).status.code(), Some(0));
    assert_eq!(job(h, &["--needs", "emulator", "--wait-timeout", "10"], 10), 0);
}

#[test]
fn a_hold_waits_for_a_running_job_that_has_the_resource() {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    let mut runner = calmrun(h)
        .args(["--needs", "emulator", "--", EXE, "__probe", "--sleep-ms", "1500"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while status(h, None)["running"].as_array().unwrap().is_empty() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(50));
    }
    let t0 = Instant::now();
    hold(h, &["emulator"]);
    assert!(
        t0.elapsed() >= Duration::from_millis(500),
        "granted while the job held it"
    );
    assert!(runner.wait().unwrap().success());
}

#[test]
fn a_hold_with_cost_takes_points() {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    let id = hold(h, &["db", "--cost", "2"]);
    assert_eq!(status(h, None)["used"], 2);
    assert_eq!(job(h, &["--cost", "1", "--wait-timeout", "1"], 10), 75);
    run(h, &["release", &id]);
    assert_eq!(job(h, &["--cost", "1", "--wait-timeout", "10"], 10), 0);
}

#[test]
fn leases_belong_to_their_queue() {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    let a = hold(h, &["emulator", "--id", "a"]);
    // Another queue has its own emulator slot.
    let b = hold(h, &["emulator", "--id", "b"]);
    assert!(leases(h, None).is_empty());
    assert_eq!(leases(h, Some("a")).len(), 1);
    // Releasing in the wrong queue finds nothing (and says so); the lease stays.
    let o = run(h, &["release", &a, "--id", "b"]);
    assert_eq!(o.status.code(), Some(0));
    assert!(stderr(&o).contains("already released"), "{}", stderr(&o));
    assert_eq!(leases(h, Some("a")).len(), 1);
    assert_eq!(run(h, &["release", &a, "--id", "a"]).status.code(), Some(0));
    assert_eq!(run(h, &["release", &b, "--id", "b"]).status.code(), Some(0));
    assert!(leases(h, Some("a")).is_empty() && leases(h, Some("b")).is_empty());
}

#[test]
fn resource_names_must_be_plain_names() {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    for bad in ["bad name!", "../x", ".hidden", ","] {
        let o = run(h, &["hold", bad]);
        assert_eq!(o.status.code(), Some(2), "hold {bad}: {}", stderr(&o));
        assert!(stdout(&o).is_empty());
    }
    assert!(stderr(&run(h, &["hold", "a b"])).contains("invalid resource name"));
    assert_eq!(job(h, &["--needs", "bad name!"], 10), 2);
    assert_eq!(job(h, &["--needs", "db.main,gpu-1"], 10), 0);
    assert!(leases(h, None).is_empty());
}
