//! Ctrl+C handling and process-tree cleanup, with real calmrun processes.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const EXE: &str = env!("CARGO_BIN_EXE_calmrun");

fn calmrun(home: &Path, budget: u32) -> Command {
    let mut c = Command::new(EXE);
    for (k, _) in std::env::vars() {
        if k.starts_with("CALMRUN_") {
            c.env_remove(k);
        }
    }
    c.env("CALMRUN_HOME", home)
        .env("CALMRUN_BUDGET", budget.to_string())
        .env("CALMRUN_MIN_FREE_GB", "0")
        .env("CALMRUN_MIN_FREE_DISK_GB", "0")
        .env("CALMRUN_CRITICAL_FREE_GB", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    c
}

/// On Windows, give the process its own console so a Ctrl+C event can target it alone.
fn own_console(c: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_CONSOLE: u32 = 0x10;
        c.creation_flags(CREATE_NEW_CONSOLE);
    }
    c
}

fn status(home: &Path) -> serde_json::Value {
    let out = calmrun(home, 2)
        .args(["status", "--json"])
        .stdout(Stdio::piped())
        .output()
        .unwrap();
    serde_json::from_slice(&out.stdout).unwrap()
}

fn wait_queue(home: &Path, running: usize, waiting: usize) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let s = status(home);
        let r = s["running"].as_array().map_or(0, Vec::len);
        let w = s["waiting"].as_array().map_or(0, Vec::len);
        if r == running && w == waiting && (running == 0 || !s["running"][0]["child_pid"].is_null()) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "queue never reached {running}/{waiting}: {s}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_code(mut c: Child, limit: Duration) -> i32 {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(s) = c.try_wait().unwrap() {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                if let Some(sig) = s.signal() {
                    return 128 + sig;
                }
            }
            return s.code().unwrap_or(-1);
        }
        if Instant::now() > deadline {
            let _ = c.kill();
            panic!("process did not finish in {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn ctrl_c(home: &Path, pid: u32) {
    let ok = calmrun(home, 2)
        .args(["__ctrl_c", &pid.to_string()])
        .status()
        .unwrap();
    assert!(ok.success(), "could not deliver Ctrl+C to {pid}");
}

fn alive(pid: u32) -> bool {
    let mut sys = sysinfo::System::new().unwrap();
    let p = sysinfo::Pid::from_u32(pid);
    sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[p]), true);
    sys.process(p).is_some_and(|pr| {
        // A zombie still has a table entry on Unix; it is not alive.
        !matches!(
            pr.status(),
            sysinfo::ProcessStatus::Zombie | sysinfo::ProcessStatus::Dead
        )
    })
}

fn read_pid(file: &Path) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(pid) = std::fs::read_to_string(file)
            .ok()
            .and_then(|s| s.trim().parse().ok())
        {
            return pid;
        }
        assert!(Instant::now() < deadline, "daemon pid never written");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn kill_pid(pid: u32) {
    let mut sys = sysinfo::System::new().unwrap();
    let p = sysinfo::Pid::from_u32(pid);
    sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[p]), true);
    if let Some(pr) = sys.process(p) {
        pr.kill();
    }
}

#[test]
fn ctrl_c_while_running_reaches_the_command() {
    let home = tempfile::tempdir().unwrap();
    let mut c = calmrun(home.path(), 2);
    own_console(&mut c).args(["--cost", "2", "--", EXE, "__probe", "--sleep-ms", "20000"]);
    let child = c.spawn().unwrap();
    let pid = child.id();
    wait_queue(home.path(), 1, 0);
    let started = Instant::now();
    ctrl_c(home.path(), pid);
    assert_eq!(wait_code(child, Duration::from_secs(15)), 130);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the command did not stop on Ctrl+C"
    );
    wait_queue(home.path(), 0, 0);
    let hist = std::fs::read_to_string(home.path().join("history.jsonl")).unwrap();
    assert!(hist.contains("\"status\":\"interrupted\""), "{hist}");
}

#[test]
fn ctrl_c_while_waiting_leaves_the_queue() {
    let home = tempfile::tempdir().unwrap();
    let holder = calmrun(home.path(), 2)
        .args(["--cost", "2", "--", EXE, "__probe", "--sleep-ms", "6000"])
        .spawn()
        .unwrap();
    wait_queue(home.path(), 1, 0);
    let mut c = calmrun(home.path(), 2);
    own_console(&mut c).args(["--cost", "2", "--", EXE, "__probe"]);
    let waiter = c.spawn().unwrap();
    let pid = waiter.id();
    wait_queue(home.path(), 1, 1);
    ctrl_c(home.path(), pid);
    assert_eq!(wait_code(waiter, Duration::from_secs(10)), 130);
    let s = status(home.path());
    assert_eq!(s["waiting"].as_array().unwrap().len(), 0, "{s}");
    assert_eq!(wait_code(holder, Duration::from_secs(20)), 0);
}

#[test]
fn daemon_survives_a_normal_exit() {
    let home = tempfile::tempdir().unwrap();
    let pidfile = home.path().join("daemon.pid");
    let code = calmrun(home.path(), 2)
        .args(["--", EXE, "__probe", "--spawn-daemon"])
        .arg(&pidfile)
        .status()
        .unwrap()
        .code();
    assert_eq!(code, Some(0));
    let daemon = read_pid(&pidfile);
    std::thread::sleep(Duration::from_millis(700));
    let survived = alive(daemon);
    kill_pid(daemon);
    assert!(survived, "a daemon left by a normally exiting command was killed");
}

#[cfg(windows)]
#[test]
fn killing_calmrun_ends_the_whole_tree() {
    let home = tempfile::tempdir().unwrap();
    let pidfile = home.path().join("daemon.pid");
    let mut child = calmrun(home.path(), 2)
        .args(["--", EXE, "__probe", "--sleep-ms", "20000", "--spawn-daemon"])
        .arg(&pidfile)
        .spawn()
        .unwrap();
    let daemon = read_pid(&pidfile);
    assert!(alive(daemon));
    child.kill().unwrap();
    let _ = child.wait();
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(daemon) {
        assert!(Instant::now() < deadline, "tree survived calmrun being killed");
        std::thread::sleep(Duration::from_millis(100));
    }
}
