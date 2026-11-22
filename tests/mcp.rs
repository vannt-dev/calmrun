//! Speak JSON-RPC to `calmrun mcp` like an MCP client would.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
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
        .env("CALMRUN_CRITICAL_FREE_GB", "0");
    c
}

struct Client {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Value>,
    /// Messages read while looking for something else.
    seen: Vec<Value>,
}

impl Client {
    fn start(home: &Path) -> Client {
        let mut child = calmrun(home)
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Ok(v) = serde_json::from_str::<Value>(&line) {
                    if tx.send(v).is_err() {
                        break;
                    }
                }
            }
        });
        Client {
            child,
            stdin,
            rx,
            seen: Vec::new(),
        }
    }

    fn send(&mut self, v: Value) {
        writeln!(self.stdin, "{v}").unwrap();
        self.stdin.flush().unwrap();
    }

    /// Wait for the response with this id; other messages are kept in `seen`.
    fn response(&mut self, id: i64, limit: Duration) -> Value {
        let deadline = Instant::now() + limit;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let v = self.rx.recv_timeout(left).expect("no response in time");
            if v.get("id") == Some(&json!(id)) && v.get("method").is_none() {
                return v;
            }
            self.seen.push(v);
        }
    }

    fn call(&mut self, id: i64, name: &str, args: Value, meta: Option<Value>) -> Value {
        let mut params = json!({"name": name, "arguments": args});
        if let Some(m) = meta {
            params["_meta"] = m;
        }
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": params}));
        self.response(id, Duration::from_secs(60))
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn init(c: &mut Client) {
    c.send(json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}
    }));
    let r = c.response(1, Duration::from_secs(10));
    assert_eq!(r["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(r["result"]["serverInfo"]["name"], "calmrun");
    assert!(r["result"]["capabilities"]["tools"].is_object());
    c.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
}

fn probe_cmd(extra: &str) -> String {
    // Quote the binary path in case it contains spaces.
    format!("\"{EXE}\" __probe {extra}")
}

#[test]
fn lists_tools_and_runs_a_task() {
    let home = tempfile::tempdir().unwrap();
    let mut c = Client::start(home.path());
    init(&mut c);
    c.send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let r = c.response(2, Duration::from_secs(10));
    let names: Vec<&str> = r["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["run_task", "queue_status", "bump"]);

    let r = c.call(
        3,
        "run_task",
        json!({"command": probe_cmd("--exit 3 --print-args hello world"), "label": "probe"}),
        None,
    );
    let res = &r["result"];
    assert_eq!(res["isError"], true);
    let s = &res["structuredContent"];
    assert_eq!(s["exit_code"], 3);
    assert_eq!(s["label"], "probe");
    let text = res["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("[\"hello\",\"world\"]"), "{text}");
    let log = s["log_path"].as_str().unwrap();
    assert!(std::fs::read_to_string(log).unwrap().contains("hello"));

    let r = c.call(4, "queue_status", json!({}), None);
    assert_eq!(r["result"]["structuredContent"]["budget"], 2);
    let r = c.call(5, "bump", json!({"label": "nothing-matches"}), None);
    assert_eq!(r["result"]["structuredContent"]["moved"], 0);

    c.send(json!({"jsonrpc": "2.0", "id": 6, "method": "no/such"}));
    assert_eq!(c.response(6, Duration::from_secs(10))["error"]["code"], -32601);
}

#[test]
fn waits_in_the_shared_queue_and_reports_progress() {
    let home = tempfile::tempdir().unwrap();
    // A CLI job holds the whole budget for a while.
    let mut holder = calmrun(home.path())
        .args(["--cost", "2", "--", EXE, "__probe", "--sleep-ms", "2500"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let out = calmrun(home.path()).args(["status", "--json"]).output().unwrap();
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        if v["running"].as_array().is_some_and(|r| r.len() == 1) {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut c = Client::start(home.path());
    init(&mut c);
    let r = c.call(
        2,
        "run_task",
        json!({"command": probe_cmd("--sleep-ms 10"), "cost": 2}),
        Some(json!({"progressToken": "tok-1"})),
    );
    let s = &r["result"]["structuredContent"];
    assert_eq!(s["exit_code"], 0);
    assert!(s["wait_ms"].as_u64().unwrap() > 300, "{s}");
    let progress: Vec<&Value> = c
        .seen
        .iter()
        .filter(|m| m["method"] == "notifications/progress")
        .collect();
    assert!(!progress.is_empty(), "no progress notifications: {:?}", c.seen);
    assert_eq!(progress[0]["params"]["progressToken"], "tok-1");
    assert!(progress[0]["params"]["message"]
        .as_str()
        .unwrap()
        .contains("waiting"));
    assert!(holder.wait().unwrap().success());
}

#[test]
fn timeout_stops_the_task() {
    let home = tempfile::tempdir().unwrap();
    let mut c = Client::start(home.path());
    init(&mut c);
    let r = c.call(
        2,
        "run_task",
        json!({"command": probe_cmd("--sleep-ms 30000"), "timeout_s": 1}),
        None,
    );
    let s = &r["result"]["structuredContent"];
    assert_eq!(s["exit_code"], 124, "{s}");
    assert_eq!(s["status"], "timeout");
    assert!(s["run_ms"].as_u64().unwrap() < 15_000);
}

#[test]
fn cancellation_stops_the_task_without_a_reply() {
    let home = tempfile::tempdir().unwrap();
    let mut c = Client::start(home.path());
    init(&mut c);
    c.send(json!({
        "jsonrpc": "2.0", "id": 7, "method": "tools/call",
        "params": {"name": "run_task", "arguments": {"command": probe_cmd("--sleep-ms 30000"), "label": "long"}}
    }));
    // Wait until it runs, then cancel it.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let out = calmrun(home.path()).args(["status", "--json"]).output().unwrap();
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        if v["running"].as_array().is_some_and(|r| r.len() == 1) {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(50));
    }
    c.send(json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 7}}));
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let out = calmrun(home.path()).args(["status", "--json"]).output().unwrap();
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        if v["running"].as_array().is_some_and(|r| r.is_empty()) {
            break;
        }
        assert!(Instant::now() < deadline, "task kept running after cancel");
        std::thread::sleep(Duration::from_millis(100));
    }
    // The server still answers, and never answered the cancelled request.
    c.send(json!({"jsonrpc": "2.0", "id": 8, "method": "ping"}));
    c.response(8, Duration::from_secs(10));
    std::thread::sleep(Duration::from_millis(300));
    while let Ok(v) = c.rx.try_recv() {
        c.seen.push(v);
    }
    assert!(
        c.seen.iter().all(|m| m.get("id") != Some(&json!(7))),
        "{:?}",
        c.seen
    );
    let hist = std::fs::read_to_string(home.path().join("history.jsonl")).unwrap();
    assert!(hist.contains("\"status\":\"cancelled\""), "{hist}");
}

#[cfg(windows)]
#[test]
fn relative_program_resolves_in_the_task_cwd() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("proj");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("tool.cmd"),
        format!("@\"{EXE}\" __probe --print-args -- %*\r\n"),
    )
    .unwrap();
    let mut c = Client::start(home.path());
    init(&mut c);
    for (i, cmd) in ["./tool x", "tool x", ".\\tool x"].iter().enumerate() {
        let r = c.call(
            10 + i as i64,
            "run_task",
            json!({"command": cmd, "cwd": dir.to_string_lossy()}),
            None,
        );
        let s = &r["result"]["structuredContent"];
        assert_eq!(s["exit_code"], 0, "{cmd}: {r}");
        let text = r["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("[\"x\"]"), "{cmd}: {text}");
    }
}
