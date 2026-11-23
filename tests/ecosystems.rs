//! Fake build tools on a temporary PATH: calmrun must pass them a worker count, leave an
//! explicit choice alone, refuse watch modes, run dev servers unchanged, and do the same
//! through the MCP server.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const EXE: &str = env!("CARGO_BIN_EXE_calmrun");

/// Variables a hint sets or reads; the test process may carry them (from lane or calmrun).
const HINT_VARS: &[&str] = &[
    "MAKEFLAGS",
    "CMAKE_BUILD_PARALLEL_LEVEL",
    "CTEST_PARALLEL_LEVEL",
    "MESON_TESTTHREADS",
    "GOFLAGS",
    "CARGO_BUILD_JOBS",
    "NEXTEST_TEST_THREADS",
    "PARALLEL_WORKERS",
    "PARALLEL_TEST_PROCESSORS",
    "DENO_JOBS",
    "MOON_CONCURRENCY",
];

/// A fake `name` in `dir` that runs calmrun's probe with `probe` (printing its arguments or
/// an environment variable).
fn fake_tool(dir: &Path, name: &str, probe: &str) {
    if cfg!(windows) {
        let body = format!("@\"{EXE}\" __probe {probe} -- %*\r\n");
        std::fs::write(dir.join(format!("{name}.cmd")), body).unwrap();
    } else {
        let path = dir.join(name);
        std::fs::write(
            &path,
            format!("#!/bin/sh\nexec \"{EXE}\" __probe {probe} -- \"$@\"\n"),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
}

struct Fakes {
    home: tempfile::TempDir,
    bin: PathBuf,
}

impl Fakes {
    /// Fakes that print their arguments as JSON.
    fn new(tools: &[&str]) -> Fakes {
        let f = Fakes::empty();
        for t in tools {
            fake_tool(&f.bin, t, "--print-args");
        }
        f
    }

    fn empty() -> Fakes {
        let home = tempfile::tempdir().unwrap();
        let bin = home.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        Fakes { home, bin }
    }

    fn path(&self) -> std::ffi::OsString {
        let old = std::env::var_os("PATH").unwrap_or_default();
        let mut dirs = vec![self.bin.clone()];
        dirs.extend(std::env::split_paths(&old));
        std::env::join_paths(dirs).unwrap()
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(EXE);
        for (k, _) in std::env::vars() {
            if k.starts_with("CALMRUN_") {
                c.env_remove(k);
            }
        }
        for v in HINT_VARS {
            c.env_remove(v);
        }
        c.env("CALMRUN_HOME", self.home.path())
            .env("CALMRUN_BUDGET", "4")
            .env("CALMRUN_MIN_FREE_GB", "0")
            .env("CALMRUN_MIN_FREE_DISK_GB", "0")
            .env("CALMRUN_CRITICAL_FREE_GB", "0")
            .env("PATH", self.path())
            .current_dir(self.home.path())
            .stdin(Stdio::null());
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd()
            .arg("--")
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .unwrap()
    }

    /// Run `calmrun -- <args>` and return the arguments the fake tool received.
    fn args_seen(&self, args: &[&str]) -> Vec<String> {
        let out = self.run(args);
        assert!(out.status.success(), "{args:?}: {out:?}");
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {out:?}"))
    }
}

fn workers_arg(seen: &[String], flag: &str) -> Option<u32> {
    if let Some(i) = seen.iter().position(|a| a == flag) {
        return seen.get(i + 1).and_then(|n| n.parse().ok());
    }
    seen.iter()
        .find_map(|a| a.strip_prefix(flag).and_then(|n| n.parse().ok()))
}

/// (command, the worker flag calmrun must add) for one tool of each ecosystem.
const HINTED: &[(&str, &str)] = &[
    ("ninja -C out", "-j"),
    ("cmake --build build", "--parallel"),
    ("meson test -C build", "--num-processes"),
    ("bazel build //...", "--jobs="),
    ("buck2 build //app:app", "-j"),
    ("gradlew assembleDebug", "--max-workers="),
    ("mvnd package", "-T"),
    ("dotnet build", "-m:"),
    ("go test ./...", "-p"),
    ("swift build", "-j"),
    ("xcodebuild -scheme App build", "-jobs"),
    ("flutter test", "--concurrency="),
    ("paratest", "--processes="),
    ("mix test", "--max-cases"),
    ("stack build", "-j"),
    ("cabal build", "-j"),
    ("dune build", "-j"),
    ("zig build", "-j"),
    ("turbo run build", "--concurrency="),
    ("nx run-many -t test", "--parallel="),
];

#[test]
fn build_tools_of_every_ecosystem_get_a_worker_count() {
    let mut names: Vec<&str> = HINTED.iter().map(|(c, _)| c.split(' ').next().unwrap()).collect();
    names.sort();
    names.dedup();
    let f = Fakes::new(&names);
    for (cmd, flag) in HINTED {
        let words: Vec<&str> = cmd.split(' ').collect();
        let seen = f.args_seen(&words);
        assert!(
            workers_arg(&seen, flag).is_some_and(|n| n >= 1),
            "{cmd}: {seen:?}"
        );
        // The user's own arguments are all still there, in order.
        let mine: Vec<&str> = seen
            .iter()
            .map(String::as_str)
            .filter(|a| words[1..].contains(a))
            .collect();
        assert_eq!(mine, words[1..], "{cmd}: {seen:?}");
    }
}

#[test]
fn serial_tools_are_left_serial() {
    let f = Fakes::new(&["make", "mvn", "ctest", "msbuild"]);
    assert_eq!(f.args_seen(&["make", "all"]), ["all"]);
    assert_eq!(f.args_seen(&["mvn", "package"]), ["package"]);
    assert_eq!(f.args_seen(&["ctest", "--test-dir", "b"]), ["--test-dir", "b"]);
    assert_eq!(f.args_seen(&["msbuild", "App.sln"]), ["App.sln"]);
}

#[test]
fn an_explicit_choice_is_left_alone() {
    let f = Fakes::new(&["ninja", "mvnd", "go", "cmake"]);
    assert_eq!(f.args_seen(&["ninja", "-j2"]), ["-j2"]);
    assert_eq!(
        f.args_seen(&["mvnd", "-T", "1C", "package"]),
        ["-T", "1C", "package"]
    );
    assert_eq!(
        f.args_seen(&["go", "test", "-p", "1", "./..."]),
        ["test", "-p", "1", "./..."]
    );
    let out = f
        .cmd()
        .env("CMAKE_BUILD_PARALLEL_LEVEL", "3")
        .args(["--", "cmake", "--build", "b"])
        .stdout(Stdio::piped())
        .output()
        .unwrap();
    let seen: Vec<String> = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(seen, ["--build", "b"]);
}

#[test]
fn environment_hints_reach_the_tool() {
    let f = Fakes::empty();
    for (tool, var) in [
        ("cargo", "CARGO_BUILD_JOBS"),
        ("rails", "PARALLEL_WORKERS"),
        ("deno", "DENO_JOBS"),
        ("moon", "MOON_CONCURRENCY"),
    ] {
        fake_tool(&f.bin, tool, &format!("--print-env {var}"));
    }
    for cmd in ["cargo build", "rails test", "deno test", "moon run :build"] {
        let words: Vec<&str> = cmd.split(' ').collect();
        let out = f.run(&words);
        assert!(out.status.success(), "{cmd}: {out:?}");
        let n: u32 = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .unwrap_or_else(|e| panic!("{cmd}: {e}: {out:?}"));
        assert!(n >= 1, "{cmd}");
    }
}

#[test]
fn watch_modes_are_refused_and_dev_servers_run_unchanged() {
    let f = Fakes::new(&["dotnet", "turbo", "gradlew"]);
    for (cmd, fix) in [
        ("dotnet watch test", "dotnet test"),
        ("turbo watch build", "turbo run build"),
        ("gradlew build --continuous", "gradlew build"),
    ] {
        let words: Vec<&str> = cmd.split(' ').collect();
        let out = f.run(&words);
        assert!(!out.status.success(), "{cmd} must be refused: {out:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains(fix), "{cmd}: {err}");
    }
    // A dev server runs at once, without a worker flag.
    assert_eq!(f.args_seen(&["turbo", "run", "dev"]), ["run", "dev"]);
    assert_eq!(f.args_seen(&["gradlew", "bootRun"]), ["bootRun"]);
}

#[test]
fn the_mcp_server_uses_the_same_table() {
    use std::io::{BufRead, BufReader, Write};
    let f = Fakes::new(&["ninja"]);
    let mut child = f
        .cmd()
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let reader = BufReader::new(child.stdout.take().unwrap());
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{}}}}"#
    )
    .unwrap();
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"run_task","arguments":{{"command":"ninja all"}}}}}}"#
    )
    .unwrap();
    stdin.flush().unwrap();
    let mut result = None;
    for line in reader.lines() {
        let v: serde_json::Value = serde_json::from_str(&line.unwrap()).unwrap();
        if v["id"] == 2 {
            result = Some(v);
            break;
        }
    }
    drop(stdin);
    let _ = child.wait();
    let r = result.expect("no run_task result");
    let s = &r["result"]["structuredContent"];
    assert_eq!(s["kind"], "build", "{r}");
    let tail = s["output_tail"].as_str().unwrap();
    assert!(tail.contains("\"all\"") && tail.contains("\"-j"), "{tail}");
}
