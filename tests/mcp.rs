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
    stdin: Option<ChildStdin>,
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
            stdin: Some(stdin),
            rx,
            seen: Vec::new(),
        }
    }

    fn send(&mut self, v: Value) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{v}").unwrap();
        stdin.flush().unwrap();
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
    assert_eq!(
        names,
        vec!["run_task", "task_status", "cancel_task", "queue_status", "bump"]
    );

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
    // The output also travels in structuredContent, for clients that show only that.
    let tail = s["output_tail"].as_str().unwrap();
    assert!(tail.contains("[\"hello\",\"world\"]"), "{tail}");
    // Finished within wait_seconds: the result itself, with the task id for reference.
    assert_eq!(s["state"], "done");
    assert!(s["task_id"].as_str().unwrap().starts_with('t'));

    let r = c.call(4, "queue_status", json!({}), None);
    assert_eq!(r["result"]["structuredContent"]["budget"], 2);
    let r = c.call(5, "bump", json!({"label": "nothing-matches"}), None);
    assert_eq!(r["result"]["structuredContent"]["moved"], 0);
    // Nothing matching is a normal answer, not a tool error.
    assert_eq!(r["result"]["structuredContent"]["matched"], 0);
    assert_eq!(r["result"]["isError"], false);

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
    for (i, cmd) in ["./tool x", ".\\tool x"].iter().enumerate() {
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
    // A bare name is never taken from the working directory (it could be a planted shim).
    let r = c.call(
        20,
        "run_task",
        json!({"command": "tool x", "cwd": dir.to_string_lossy()}),
        None,
    );
    assert_eq!(r["result"]["structuredContent"]["exit_code"], 127, "{r}");
}

#[test]
fn leading_env_assignments_work_on_every_os() {
    let home = tempfile::tempdir().unwrap();
    let mut c = Client::start(home.path());
    init(&mut c);
    let cmd = format!(
        "CALMRUN_PROBE_VALUE=hi-there {}",
        probe_cmd("--print-env CALMRUN_PROBE_VALUE")
    );
    let r = c.call(2, "run_task", json!({"command": cmd}), None);
    let s = &r["result"]["structuredContent"];
    assert_eq!(s["exit_code"], 0, "{r}");
    assert!(s["output_tail"].as_str().unwrap().contains("hi-there"), "{r}");
}

#[cfg(unix)]
#[test]
fn sigterm_stops_the_server_and_its_tasks() {
    let home = tempfile::tempdir().unwrap();
    let mut c = Client::start(home.path());
    init(&mut c);
    c.send(json!({
        "jsonrpc": "2.0", "id": 9, "method": "tools/call",
        "params": {"name": "run_task", "arguments": {"command": probe_cmd("--sleep-ms 60000")}}
    }));
    let deadline = Instant::now() + Duration::from_secs(20);
    let child = loop {
        let out = calmrun(home.path()).args(["status", "--json"]).output().unwrap();
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        if let Some(p) = v["running"][0]["child_pid"].as_u64() {
            break p as i32;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(50));
    };
    // SAFETY: plain syscalls on processes this test started.
    unsafe {
        libc::kill(c.child.id() as i32, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    while c.child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "the server ignored SIGTERM");
        std::thread::sleep(Duration::from_millis(100));
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while unsafe { libc::kill(child, 0) } == 0 {
        assert!(Instant::now() < deadline, "the task outlived the server");
        std::thread::sleep(Duration::from_millis(100));
    }
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

fn wait_gone(pid: u32, limit: Duration, what: &str) {
    let deadline = Instant::now() + limit;
    while alive(pid) {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn queue_json(home: &Path) -> Value {
    let out = calmrun(home).args(["status", "--json"]).output().unwrap();
    serde_json::from_slice(&out.stdout).unwrap()
}

/// The child pid of the only running job, once it runs.
fn running_child(home: &Path) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(p) = queue_json(home)["running"][0]["child_pid"].as_u64() {
            return p as u32;
        }
        assert!(Instant::now() < deadline, "the task never started");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn structured(r: &Value) -> &Value {
    &r["result"]["structuredContent"]
}

#[test]
fn a_long_task_returns_a_handle_and_is_followed_with_offsets() {
    let home = tempfile::tempdir().unwrap();
    let mut c = Client::start(home.path());
    init(&mut c);
    let r = c.call(
        2,
        "run_task",
        json!({"command": probe_cmd("--sleep-ms 2500 --print-args first-line"), "wait_seconds": 0.5}),
        None,
    );
    assert_eq!(r["result"]["isError"], false, "{r}");
    let s = structured(&r);
    let id = s["task_id"].as_str().unwrap().to_string();
    assert!(
        ["waiting", "running"].contains(&s["state"].as_str().unwrap()),
        "{s}"
    );
    let text = r["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("task_status"), "{text}");

    // The output so far, then nothing new from next_offset on.
    let deadline = Instant::now() + Duration::from_secs(20);
    let next = loop {
        let r = c.call(3, "task_status", json!({"task_id": id}), None);
        let s = structured(&r);
        if s["output"].as_str().unwrap_or("").contains("first-line") {
            assert_eq!(s["state"], "running", "{s}");
            assert!(s["run_ms"].as_u64().is_some());
            break s["next_offset"].as_u64().unwrap();
        }
        assert!(Instant::now() < deadline, "no output: {s}");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(next > 0);
    let r = c.call(
        4,
        "task_status",
        json!({"task_id": id, "output_offset": next}),
        None,
    );
    assert_eq!(structured(&r)["output"], "", "{r}");
    assert_eq!(structured(&r)["next_offset"], next);

    // Waiting for the end inside task_status.
    let r = c.call(5, "task_status", json!({"task_id": id, "wait_seconds": 20}), None);
    let s = structured(&r);
    assert_eq!(s["state"], "done", "{s}");
    assert_eq!(s["exit_code"], 0);
    assert!(s["run_ms"].as_u64().unwrap() >= 2_000, "{s}");
    // Finished tasks stay queryable; cancelling one changes nothing.
    let r = c.call(6, "cancel_task", json!({"task_id": id}), None);
    assert_eq!(structured(&r)["state"], "done");
    assert_eq!(structured(&r)["exit_code"], 0);
    let r = c.call(7, "task_status", json!({"task_id": "t000000"}), None);
    assert_eq!(r["result"]["isError"], true);
}

#[test]
fn cancel_task_frees_a_waiting_place() {
    let home = tempfile::tempdir().unwrap();
    // A CLI job holds the whole budget.
    let mut holder = calmrun(home.path())
        .args(["--cost", "2", "--", EXE, "__probe", "--sleep-ms", "30000"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    running_child(home.path());
    let mut c = Client::start(home.path());
    init(&mut c);
    let r = c.call(
        2,
        "run_task",
        json!({"command": probe_cmd("--sleep-ms 10"), "cost": 2, "label": "behind", "wait_seconds": 1}),
        None,
    );
    let s = structured(&r);
    assert_eq!(s["state"], "waiting", "{s}");
    assert_eq!(s["place"], 1, "{s}");
    assert_eq!(
        (s["used"].as_u64(), s["budget"].as_u64()),
        (Some(2), Some(2)),
        "{s}"
    );
    assert!(s["waiting_line"].as_str().unwrap().contains("points"), "{s}");
    let id = s["task_id"].as_str().unwrap().to_string();
    assert_eq!(queue_json(home.path())["waiting"].as_array().unwrap().len(), 1);

    let r = c.call(3, "cancel_task", json!({"task_id": id}), None);
    let s = structured(&r);
    assert_eq!(s["state"], "done", "{s}");
    assert_eq!(s["status"], "cancelled", "{s}");
    assert!(s["note"].as_str().unwrap().contains("left the queue"), "{s}");
    assert!(queue_json(home.path())["waiting"].as_array().unwrap().is_empty());
    let _ = holder.kill();
    let _ = holder.wait();
}

#[test]
fn cancel_task_stops_the_whole_tree() {
    let home = tempfile::tempdir().unwrap();
    let pidfile = home.path().join("daemon.pid");
    let mut c = Client::start(home.path());
    init(&mut c);
    let cmd = probe_cmd(&format!(
        "--sleep-ms 60000 --spawn-daemon \"{}\"",
        pidfile.display()
    ));
    let r = c.call(2, "run_task", json!({"command": cmd, "wait_seconds": 0}), None);
    let id = structured(&r)["task_id"].as_str().unwrap().to_string();
    let child = running_child(home.path());
    let deadline = Instant::now() + Duration::from_secs(10);
    let daemon: u32 = loop {
        if let Some(p) = std::fs::read_to_string(&pidfile)
            .ok()
            .and_then(|s| s.trim().parse().ok())
        {
            break p;
        }
        assert!(Instant::now() < deadline, "daemon pid never written");
        std::thread::sleep(Duration::from_millis(50));
    };
    let r = c.call(3, "cancel_task", json!({"task_id": id}), None);
    let s = structured(&r);
    assert_eq!(s["status"], "cancelled", "{s}");
    assert_eq!(
        r["result"]["isError"], false,
        "a cancel that worked is no tool error"
    );
    assert!(s["note"].as_str().unwrap().contains("process tree"), "{s}");
    wait_gone(child, Duration::from_secs(5), "the command outlived cancel_task");
    // The daemon would end by itself after 6 s; it must go sooner.
    wait_gone(
        daemon,
        Duration::from_secs(3),
        "the command's child outlived cancel_task",
    );
    assert!(queue_json(home.path())["running"].as_array().unwrap().is_empty());
    let hist = std::fs::read_to_string(home.path().join("history.jsonl")).unwrap();
    assert!(hist.contains("\"status\":\"cancelled\""), "{hist}");
}

#[test]
fn closing_the_server_stops_its_background_tasks() {
    let home = tempfile::tempdir().unwrap();
    let mut c = Client::start(home.path());
    init(&mut c);
    let r = c.call(
        2,
        "run_task",
        json!({"command": probe_cmd("--sleep-ms 60000"), "wait_seconds": 0}),
        None,
    );
    assert!(structured(&r)["task_id"].is_string());
    let child = running_child(home.path());
    // The client goes away: stdin closes.
    c.stdin = None;
    let deadline = Instant::now() + Duration::from_secs(15);
    while c.child.try_wait().unwrap().is_none() {
        assert!(
            Instant::now() < deadline,
            "the server did not exit after stdin closed"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    wait_gone(child, Duration::from_secs(10), "the task outlived its server");
    let q = queue_json(home.path());
    assert!(q["running"].as_array().unwrap().is_empty(), "{q}");
    let hist = std::fs::read_to_string(home.path().join("history.jsonl")).unwrap();
    assert!(hist.contains("\"status\":\"cancelled\""), "{hist}");
}

#[test]
fn a_killed_server_leaves_no_task_or_queue_entry_behind() {
    let home = tempfile::tempdir().unwrap();
    let mut c = Client::start(home.path());
    init(&mut c);
    // One running task and one waiting behind it (budget 2).
    c.call(
        2,
        "run_task",
        json!({"command": probe_cmd("--sleep-ms 60000"), "cost": 2, "wait_seconds": 0}),
        None,
    );
    let child = running_child(home.path());
    let r = c.call(
        3,
        "run_task",
        json!({"command": probe_cmd("--sleep-ms 10"), "cost": 2, "wait_seconds": 0.5}),
        None,
    );
    assert_eq!(structured(&r)["state"], "waiting", "{r}");
    c.child.kill().unwrap();
    let _ = c.child.wait();
    wait_gone(
        child,
        Duration::from_secs(10),
        "the task outlived a killed server",
    );
    // The dead server's entries are dropped by the next calmrun that looks.
    let q = queue_json(home.path());
    assert!(q["running"].as_array().unwrap().is_empty(), "{q}");
    assert!(q["waiting"].as_array().unwrap().is_empty(), "{q}");
}

#[test]
fn refusals_and_lost_tickets_speak_to_the_agent() {
    let home = tempfile::tempdir().unwrap();
    let mut c = Client::start(home.path());
    init(&mut c);
    // A refusal names the next run_task call, not the CLI form, and has no empty fields.
    let r = c.call(2, "run_task", json!({"command": "docker compose up"}), None);
    let text = r["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("exit code 2 (refused)\n"), "{text}");
    assert!(
        text.contains("next: call run_task with \"command\" set to: docker compose up -d"),
        "{text}"
    );
    assert!(!text.contains("calmrun -- "), "{text}");
    // A ticket that is unknown: the agent learns it lost its place.
    let r = c.call(
        3,
        "run_task",
        json!({"command": probe_cmd("--sleep-ms 10"), "ticket": 999}),
        None,
    );
    let s = structured(&r);
    assert_eq!(s["exit_code"], 0, "{r}");
    let notes = s["notes"].as_array().unwrap();
    assert!(notes[0].as_str().unwrap().contains("ticket 999"), "{s}");
    let text = r["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("joining the end of the line"), "{text}");
    // A plain success shows no status word that only repeats the code.
    assert!(text.contains("exit code 0, "), "{text}");
    assert!(!text.contains("exit code 0 (0)"), "{text}");
    // Resource names are plain names.
    let r = c.call(
        4,
        "run_task",
        json!({"command": probe_cmd("--sleep-ms 10"), "needs": ["bad name!"]}),
        None,
    );
    assert_eq!(structured(&r)["exit_code"], 2, "{r}");
    let text = r["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("invalid resource name"), "{text}");
}
