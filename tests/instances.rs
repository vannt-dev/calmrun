//! Resources with instances: each job (or lease) gets its own free, healthy instance.

use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const EXE: &str = env!("CARGO_BIN_EXE_calmrun");

/// calmrun against a temporary state dir with this config.
struct Env {
    home: tempfile::TempDir,
}

impl Env {
    fn new(config: &str) -> Env {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("config.toml"), config).unwrap();
        Env { home }
    }

    fn dir(&self) -> &Path {
        self.home.path()
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(EXE);
        for (k, _) in std::env::vars() {
            if k.starts_with("CALMRUN_") || k == "ANDROID_SERIAL" {
                c.env_remove(k);
            }
        }
        c.env("CALMRUN_HOME", self.dir())
            .env("CALMRUN_BUDGET", "4")
            .env("CALMRUN_MIN_FREE_GB", "0")
            .env("CALMRUN_MIN_FREE_DISK_GB", "0")
            .env("CALMRUN_CRITICAL_FREE_GB", "0")
            .stdin(Stdio::null());
        c
    }

    /// `calmrun --needs <res> -- calmrun __probe --print-env <var> --sleep-ms <ms>`.
    fn job(&self, flags: &[&str], var: &str, ms: u64) -> Command {
        let mut c = self.cmd();
        c.args(flags).args([
            "--",
            EXE,
            "__probe",
            "--print-env",
            var,
            "--sleep-ms",
            &ms.to_string(),
        ]);
        c
    }

    fn spawn(&self, flags: &[&str], var: &str, ms: u64) -> Child {
        self.job(flags, var, ms)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd().args(args).output().unwrap()
    }

    fn status(&self) -> serde_json::Value {
        serde_json::from_slice(&self.run(&["status", "--json"]).stdout).unwrap()
    }

    /// Poll until `n` jobs run.
    fn wait_running(&self, n: usize) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let s = self.status();
            if s["running"].as_array().map_or(0, Vec::len) == n {
                return s;
            }
            assert!(Instant::now() < deadline, "never {n} running: {s}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).to_string()
}

/// A TOML literal string holding a command that runs the probe.
fn probe_line(args: &str) -> String {
    format!("'\"{EXE}\" __probe {args}'")
}

const TWO: &str = "[resources]\nandroid = [\"emulator-5554\", \"emulator-5556\"]\n";

#[test]
fn parallel_jobs_get_different_instances() {
    let env = Env::new(TWO);
    let a = env.spawn(&["--needs", "android"], "ANDROID_SERIAL", 5000);
    let b = env.spawn(&["--needs", "android"], "ANDROID_SERIAL", 5000);
    // Both start at once (two instances), each shown with its instance.
    let s = env.wait_running(2);
    let mut shown: Vec<String> = s["running"]
        .as_array()
        .unwrap()
        .iter()
        .map(|j| j["instances"]["android"].as_str().unwrap_or_default().to_string())
        .collect();
    shown.sort();
    assert_eq!(shown, ["emulator-5554", "emulator-5556"], "{s}");
    let text = out(&env.run(&["status"]));
    assert!(text.contains("uses: android=emulator-555"), "{text}");
    // A third waits for one of them.
    let third = env.run(&[
        "--needs",
        "android",
        "--wait-timeout",
        "0.3",
        "--",
        EXE,
        "__probe",
    ]);
    assert_eq!(third.status.code(), Some(75), "{}", err(&third));
    let e = err(&third);
    assert!(
        e.contains("all 2 ready instance(s) in use by calmrun (emulator-555"),
        "{e}"
    );
    let (a, b) = (a.wait_with_output().unwrap(), b.wait_with_output().unwrap());
    assert!(a.status.success() && b.status.success(), "{}{}", err(&a), err(&b));
    let mut got = [out(&a), out(&b)];
    got.sort();
    assert_eq!(got, ["emulator-5554", "emulator-5556"]);
    assert!(
        err(&a).contains("calmrun: using android emulator-555"),
        "{}",
        err(&a)
    );
    // The generic variable too, and the instance is free again afterwards.
    let o = env
        .job(&["--needs", "android"], "CALMRUN_ANDROID", 0)
        .output()
        .unwrap();
    assert_eq!(out(&o), "emulator-5554");
}

#[test]
fn unhealthy_instances_are_skipped() {
    let env = Env::new("");
    let dir = env.dir().display().to_string();
    let health = probe_line(&format!("--exists \"{dir}/{{instance}}.ok\""));
    std::fs::write(
        env.dir().join("config.toml"),
        format!("[resources.dev]\ninstances = [\"a\", \"b\"]\nhealth = {health}\nenv = \"DEV_SERIAL\"\n"),
    )
    .unwrap();
    std::fs::write(env.dir().join("b.ok"), "").unwrap();
    let o = env.job(&["--needs", "dev"], "DEV_SERIAL", 0).output().unwrap();
    assert!(o.status.success(), "{}", err(&o));
    assert_eq!(out(&o), "b", "a fails its health check");
    // No healthy instance: it waits, and says why.
    std::fs::remove_file(env.dir().join("b.ok")).unwrap();
    let o = env
        .job(&["--needs", "dev", "--wait-timeout", "1"], "DEV_SERIAL", 0)
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(75), "{}", err(&o));
    assert!(err(&o).contains("a ready instance of \"dev\""), "{}", err(&o));
}

#[test]
fn instances_are_discovered_by_a_command() {
    let env = Env::new("");
    let list = env.dir().join("devices.txt");
    std::fs::write(
        &list,
        "List of devices attached\nemulator-5560\toffline\nemulator-5562\tdevice\n\n",
    )
    .unwrap();
    let discover = probe_line(&format!("--print-file \"{}\"", list.display()));
    std::fs::write(
        env.dir().join("config.toml"),
        format!("[resources.emulator]\ndiscover = {discover}\n"),
    )
    .unwrap();
    let o = env
        .job(&["--needs", "emulator"], "ANDROID_SERIAL", 0)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", err(&o));
    assert_eq!(out(&o), "emulator-5562", "the offline one is skipped");
}

#[test]
fn a_lease_holds_an_instance_and_hands_it_to_its_jobs() {
    let env = Env::new(TWO);
    let o = env.run(&["hold", "android", "--label", "device test"]);
    assert!(o.status.success(), "{}", err(&o));
    let id = out(&o);
    assert_eq!(id.lines().count(), 1, "stdout stays the bare id: {id:?}");
    assert!(err(&o).contains("holding android=emulator-5554"), "{}", err(&o));
    assert!(err(&o).contains("ANDROID_SERIAL=emulator-5554"), "{}", err(&o));
    let lease = &env.status()["leases"][0];
    assert_eq!(lease["instances"]["android"], "emulator-5554", "{lease}");
    // A job of the lease gets its instance; any other job the other one.
    let o = env
        .job(&["--needs", "android"], "ANDROID_SERIAL", 0)
        .env("CALMRUN_LEASE", &id)
        .output()
        .unwrap();
    assert_eq!(out(&o), "emulator-5554", "{}", err(&o));
    let o = env
        .job(&["--needs", "android"], "ANDROID_SERIAL", 0)
        .output()
        .unwrap();
    assert_eq!(out(&o), "emulator-5556", "{}", err(&o));
    // --print-env: NAME=VALUE lines for a script.
    let o = env.run(&["hold", "android", "--print-env"]);
    assert!(o.status.success(), "{}", err(&o));
    let lines = out(&o);
    let lines: Vec<&str> = lines.lines().collect();
    assert!(lines[0].starts_with("CALMRUN_LEASE="), "{lines:?}");
    assert!(lines.contains(&"ANDROID_SERIAL=emulator-5556"), "{lines:?}");
    assert!(lines.contains(&"CALMRUN_ANDROID=emulator-5556"), "{lines:?}");
    // Both held: a third hold is not granted.
    let o = env.run(&["hold", "android", "--wait-timeout", "0"]);
    assert_eq!(o.status.code(), Some(75), "{}", err(&o));
    assert!(env.run(&["release", &id]).status.success());
    let o = env
        .job(&["--needs", "android"], "ANDROID_SERIAL", 0)
        .output()
        .unwrap();
    assert_eq!(out(&o), "emulator-5554", "released: free again");
}

#[test]
fn count_only_resources_are_unchanged() {
    let env = Env::new("[resources]\ndb = 1\n");
    let o = env.job(&["--needs", "db"], "CALMRUN_DB", 0).output().unwrap();
    assert!(o.status.success(), "{}", err(&o));
    assert_eq!(out(&o), "", "no instance, no variable");
    assert!(!err(&o).contains("using"), "{}", err(&o));
    let bad = Env::new("[resources.android]\nhealth = \"true\"\n");
    let o = bad.run(&["status"]);
    let e = err(&o);
    assert!(
        e.contains("ignoring config file") && e.contains("at least one instance"),
        "{e}"
    );
}
