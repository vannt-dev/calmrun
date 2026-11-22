//! Regression tests for issues found in review: each test names the problem it guards.

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
        .env("CALMRUN_CRITICAL_FREE_GB", "0")
        .stdin(Stdio::null());
    c
}

fn status(home: &Path) -> serde_json::Value {
    let out = calmrun(home).args(["status", "--json"]).output().unwrap();
    serde_json::from_slice(&out.stdout).unwrap()
}

fn wait_running(home: &Path, n: usize) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let s = status(home);
        let r = &s["running"];
        if r.as_array().is_some_and(|a| a.len() == n) && (n == 0 || !r[0]["child_pid"].is_null()) {
            return s;
        }
        assert!(Instant::now() < deadline, "never {n} running: {s}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn setup_never_rewrites_a_file_it_cannot_read_as_text() {
    let home = tempfile::tempdir().unwrap();
    let md = home.path().join(".claude/CLAUDE.md");
    std::fs::create_dir_all(md.parent().unwrap()).unwrap();
    // "café" in Windows-1252: not UTF-8.
    let bytes = b"# Notes\r\ncaf\xe9\r\n".to_vec();
    std::fs::write(&md, &bytes).unwrap();
    let out = Command::new(EXE)
        .args(["setup", "--write", "--agents", "claude", "--home"])
        .arg(home.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stdout).contains("not UTF-8"));
    assert_eq!(
        std::fs::read(&md).unwrap(),
        bytes,
        "the file must be left byte for byte"
    );
}

#[test]
fn setup_backs_up_every_instruction_file_it_rewrites() {
    let home = tempfile::tempdir().unwrap();
    let md = home.path().join(".codex/AGENTS.md");
    std::fs::create_dir_all(md.parent().unwrap()).unwrap();
    std::fs::write(&md, "keep me\r\nexactly").unwrap();
    let out = Command::new(EXE)
        .args(["setup", "--write", "--agents", "codex", "--home"])
        .arg(home.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    let backup = home.path().join(".codex/AGENTS.md.calmrun.bak");
    assert_eq!(std::fs::read_to_string(backup).unwrap(), "keep me\r\nexactly");
    let text = std::fs::read_to_string(&md).unwrap();
    assert!(
        text.starts_with("keep me\r\nexactly\n\n<!-- calmrun:start"),
        "{text:?}"
    );
}

#[test]
fn watch_mode_is_refused_with_the_one_shot_command() {
    let home = tempfile::tempdir().unwrap();
    let out = calmrun(home.path())
        .args(["--", "npx", "tsc", "--watch", "-p", "."])
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("calmrun: next: calmrun -- npx tsc -p ."), "{err}");
}

#[test]
fn help_and_version_skip_the_queue() {
    let home = tempfile::tempdir().unwrap();
    let mut holder = calmrun(home.path())
        .args(["--cost", "2", "--", EXE, "__probe", "--sleep-ms", "8000"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_running(home.path(), 1);
    let started = Instant::now();
    let out = calmrun(home.path())
        .args(["--", EXE, "--version"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "waited behind the holder"
    );
    let _ = holder.kill();
    let _ = holder.wait();
}

#[test]
fn status_text_shows_places_kept_by_a_ticket() {
    let home = tempfile::tempdir().unwrap();
    let mut holder = calmrun(home.path())
        .args(["--cost", "2", "--", EXE, "__probe", "--sleep-ms", "8000"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_running(home.path(), 1);
    let out = calmrun(home.path())
        .args([
            "--cost",
            "2",
            "--wait-timeout",
            "1",
            "--label",
            "kept-job",
            "--",
            EXE,
            "__probe",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(75));
    let text = calmrun(home.path()).arg("status").output().unwrap();
    let text = String::from_utf8_lossy(&text.stdout);
    assert!(
        text.contains("kept (ticket ") && text.contains("kept-job"),
        "{text}"
    );
    let _ = holder.kill();
    let _ = holder.wait();
}

#[test]
fn hook_suggests_an_absolute_path_when_calmrun_is_not_on_path() {
    let input = serde_json::json!({
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_input": {"command": "npx vitest run"}
    })
    .to_string();
    let run = |path: &std::ffi::OsStr| {
        use std::io::Write;
        let mut c = Command::new(EXE)
            .args(["hook", "claude"])
            .env("PATH", path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        c.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
        let out = c.wait_with_output().unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        v["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let empty = tempfile::tempdir().unwrap();
    let reason = run(empty.path().as_os_str());
    let exe_name = Path::new(EXE).file_name().unwrap().to_string_lossy().to_string();
    assert!(
        reason.contains(&exe_name) && reason.contains(" -- npx vitest run"),
        "{reason}"
    );
    assert!(!reason.contains("\ncalmrun -- "), "{reason}");
    // With calmrun on PATH the plain name is used.
    let bin = tempfile::tempdir().unwrap();
    std::fs::copy(EXE, bin.path().join(&exe_name)).unwrap();
    let reason = run(bin.path().as_os_str());
    assert!(reason.contains("\ncalmrun -- npx vitest run\n"), "{reason}");
}

#[cfg(windows)]
#[test]
fn a_repository_cannot_shadow_npm_with_its_own_cmd() {
    let home = tempfile::tempdir().unwrap();
    let repo = home.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("npm.cmd"), "@echo HIJACKED\r\n").unwrap();
    let out = calmrun(home.path())
        .current_dir(&repo)
        .args(["--", "npm", "--version"])
        .stdout(Stdio::piped())
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!text.contains("HIJACKED"), "{text}");
    // An explicit relative path still works.
    let out = calmrun(home.path())
        .current_dir(&repo)
        .args(["--", ".\\npm.cmd"])
        .stdout(Stdio::piped())
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("HIJACKED"));
}

#[cfg(unix)]
#[test]
fn the_command_dies_when_calmrun_is_killed_hard() {
    let home = tempfile::tempdir().unwrap();
    let mut c = calmrun(home.path())
        .args(["--", EXE, "__probe", "--sleep-ms", "30000"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let s = wait_running(home.path(), 1);
    let child = s["running"][0]["child_pid"].as_u64().unwrap() as i32;
    // SAFETY: plain syscall on our own child.
    unsafe {
        libc::kill(c.id() as i32, libc::SIGKILL);
    }
    let _ = c.wait();
    let deadline = Instant::now() + Duration::from_secs(10);
    // SAFETY: signal 0 only checks existence; a zombie is reaped by init.
    while unsafe { libc::kill(child, 0) } == 0 {
        assert!(
            Instant::now() < deadline,
            "the command outlived a SIGKILLed calmrun"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}
