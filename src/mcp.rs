//! `calmrun mcp`: an MCP server over stdio (newline-delimited JSON-RPC 2.0).
//!
//! Tools: `run_task` (waits in the same queue as the CLI, then runs), `queue_status`, `bump`.
//! Each `run_task` call runs on its own thread, so status calls answer while a task waits.

use crate::queue::{self, Ctx, Notify, Request};
use crate::{detect, history};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SUPPORTED_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
const TAIL_LINES: usize = 200;
const KEEP_LOGS: usize = 200;

type Out = Arc<Mutex<io::Stdout>>;

fn send(out: &Out, v: &Value) {
    let mut line = v.to_string();
    line.push('\n');
    if let Ok(mut o) = out.lock() {
        let _ = o.write_all(line.as_bytes());
        let _ = o.flush();
    }
}

fn reply(out: &Out, id: &Value, result: Value) {
    send(out, &json!({"jsonrpc": "2.0", "id": id, "result": result}));
}

fn reply_error(out: &Out, id: &Value, code: i64, message: &str) {
    send(
        out,
        &json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
    );
}

fn text_result(text: String, is_error: bool, structured: Option<Value>) -> Value {
    let mut r = json!({"content": [{"type": "text", "text": text}], "isError": is_error});
    if let Some(s) = structured {
        r["structuredContent"] = s;
    }
    r
}

fn tools() -> Value {
    json!([
        {
            "name": "run_task",
            "title": "Run a heavy command through the calmrun queue",
            "description": "Run a heavy command (tests, type checks, linters, builds, Playwright, \
    docker compose, pytest, cargo/gradle builds) through the machine-wide calmrun queue, so parallel \
    agents do not overload this computer. Waits for its turn, runs the command at low priority, and \
    returns the exit code, wait and run time, the last 200 lines of output and the path of the full \
    log. The command runs in the platform shell (sh on macOS/Linux, cmd on Windows) when it uses \
    shell syntax; plain words run the program directly.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The command line, e.g. \"npx vitest run\""},
                    "cwd": {"type": "string", "description": "Working directory (default: the server's)"},
                    "kind": {"type": "string", "description": "Job kind; detected from the command when omitted"},
                    "cost": {"type": "integer", "minimum": 1, "description": "Budget points; default by kind"},
                    "label": {"type": "string", "description": "Name shown in the queue"},
                    "timeout_s": {"type": "number", "exclusiveMinimum": 0, "description": "Stop the command after this many seconds (exit 124)"},
                    "prio": {"type": "string", "enum": ["normal", "high"]},
                    "wait_timeout_s": {"type": "number", "exclusiveMinimum": 0, "description": "Give up waiting after this many seconds; the result carries a ticket that keeps the place in line for 10 minutes"},
                    "ticket": {"type": "integer", "description": "Ticket from an earlier wait timeout: resume that place in line"}
                },
                "required": ["command"]
            }
        },
        {
            "name": "queue_status",
            "title": "Show the calmrun queue",
            "description": "Running and waiting jobs (label, kind, cost, pid, cwd, time), the point budget, \
    free RAM and warnings about jobs that have waited a long time.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "bump",
            "title": "Move waiting jobs to the front",
            "description": "Move waiting jobs whose label contains the given text to the front of the queue.",
            "inputSchema": {
                "type": "object",
                "properties": {"label": {"type": "string", "description": "Part of the label (case-insensitive)"}},
                "required": ["label"]
            }
        }
    ])
}

const INSTRUCTIONS: &str = "Use run_task for heavy commands (tests, type checks, linters, builds, \
Playwright, docker compose, pytest, cargo/gradle builds) so they wait their turn in the \
machine-wide queue instead of overloading the computer. Use queue_status to see what is running.";

/// Characters that need a shell to mean what they say.
fn needs_shell(cmd: &str) -> bool {
    let special: &[char] = if cfg!(windows) {
        &[
            '&', '|', ';', '<', '>', '(', ')', '$', '`', '*', '?', '\n', '%', '^', '!', '~',
        ]
    } else {
        &[
            '&', '|', ';', '<', '>', '(', ')', '$', '`', '*', '?', '\n', '~', '{', '[',
        ]
    };
    cmd.contains(special)
        || detect::tokenize(cmd)
            .first()
            .is_some_and(|w| w.contains('=') && !w.starts_with('-'))
}

/// argv for a command line, plus whether it must run through the shell.
pub fn command_argv(cmd: &str) -> (Vec<String>, bool) {
    if needs_shell(cmd) {
        (vec![cmd.trim().to_string()], true)
    } else {
        (detect::tokenize(cmd), false)
    }
}

struct McpNotify {
    out: Out,
    token: Option<Value>,
    count: u64,
    last_line: String,
    last_sent: Option<Instant>,
    started: Instant,
}

impl McpNotify {
    fn progress(&mut self, message: String) {
        let Some(token) = &self.token else { return };
        self.count += 1;
        send(
            &self.out,
            &json!({
                "jsonrpc": "2.0",
                "method": "notifications/progress",
                "params": {"progressToken": token, "progress": self.count, "message": message}
            }),
        );
        self.last_sent = Some(Instant::now());
    }
}

impl Notify for McpNotify {
    fn waiting(&mut self, line: &str) {
        let due = self
            .last_sent
            .is_none_or(|t| t.elapsed() >= Duration::from_secs(10));
        if line != self.last_line || due {
            self.last_line = line.to_string();
            let waited = history::human_ms(self.started.elapsed().as_millis() as u64);
            self.progress(format!("{line} (waited {waited})"));
        }
    }

    fn started(&mut self, wait_ms: u64) {
        self.progress(format!("started after waiting {}", history::human_ms(wait_ms)));
    }
}

static LOG_SEQ: AtomicU64 = AtomicU64::new(0);

fn run_task(out: &Out, args: &Value, token: Option<Value>, cancel: Arc<AtomicBool>) -> Value {
    let Some(command) = args
        .get("command")
        .and_then(Value::as_str)
        .filter(|c| !c.trim().is_empty())
    else {
        return text_result("run_task needs a non-empty \"command\" string".into(), true, None);
    };
    let cwd = args.get("cwd").and_then(Value::as_str).map(PathBuf::from);
    if let Some(d) = &cwd {
        if !d.is_dir() {
            return text_result(format!("cwd is not a directory: {}", d.display()), true, None);
        }
    }
    let mut ctx = Ctx::load();
    let log = queue::log_dir(&ctx.home).join(format!(
        "{}-{}-{}.log",
        queue::now_ms(),
        std::process::id(),
        LOG_SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    queue::prune_logs(&queue::log_dir(&ctx.home), KEEP_LOGS);
    let (argv, shell) = command_argv(command);
    let req = Request {
        argv,
        shell,
        cwd,
        kind: args.get("kind").and_then(Value::as_str).map(String::from),
        cost: args.get("cost").and_then(Value::as_u64).map(|c| c.max(1) as u32),
        high: args.get("prio").and_then(Value::as_str) == Some("high"),
        label: args.get("label").and_then(Value::as_str).map(String::from),
        timeout: args
            .get("timeout_s")
            .and_then(Value::as_f64)
            .filter(|t| *t > 0.0)
            .map(Duration::from_secs_f64),
        log: Some(log.clone()),
        cancel: Some(cancel),
        wait_timeout: args
            .get("wait_timeout_s")
            .and_then(Value::as_f64)
            .filter(|t| *t > 0.0)
            .map(Duration::from_secs_f64),
        ticket: args.get("ticket").and_then(Value::as_u64),
        rerun_hint: Some("call run_task again with the same arguments plus \"ticket\": {ticket}".into()),
    };
    let mut notify = McpNotify {
        out: out.clone(),
        token,
        count: 0,
        last_line: String::new(),
        last_sent: None,
        started: Instant::now(),
    };
    let o = queue::run(&mut ctx, req, &mut notify);
    let ran = log.exists();
    let tail = if ran {
        queue::tail_lines(&log, TAIL_LINES)
    } else {
        String::new()
    };
    let mut text = format!(
        "exit code {} ({}) · {} · kind {} · waited {} · ran {}\n",
        o.code,
        o.status,
        o.label,
        o.kind,
        history::human_ms(o.wait_ms),
        history::human_ms(o.run_ms)
    );
    if let Some(m) = &o.message {
        text.push_str(m);
        text.push('\n');
    }
    if ran {
        text.push_str(&format!("full log: {}\n", log.display()));
        text.push_str(&format!("--- last {TAIL_LINES} lines ---\n{tail}"));
    }
    let structured = json!({
        "exit_code": o.code,
        "status": o.status,
        "label": o.label,
        "kind": o.kind,
        "cost": o.cost,
        "wait_ms": o.wait_ms,
        "run_ms": o.run_ms,
        "log_path": if ran { Value::String(log.to_string_lossy().to_string()) } else { Value::Null },
        "message": o.message,
        "ticket": o.ticket,
    });
    text_result(text, o.code != 0, Some(structured))
}

fn queue_status() -> Value {
    let mut ctx = Ctx::load();
    match queue::snapshot(&mut ctx) {
        Ok(state) => {
            let v = queue::status_json(&mut ctx, &state);
            text_result(
                serde_json::to_string_pretty(&v).unwrap_or_default(),
                false,
                Some(v),
            )
        }
        Err(e) => text_result(format!("cannot read queue state: {e}"), true, None),
    }
}

fn bump(args: &Value) -> Value {
    let Some(label) = args
        .get("label")
        .and_then(Value::as_str)
        .filter(|l| !l.is_empty())
    else {
        return text_result("bump needs a \"label\" string".into(), true, None);
    };
    let mut ctx = Ctx::load();
    match queue::bump(&mut ctx, label) {
        Ok(n) => text_result(
            format!("moved {n} waiting job(s) matching \"{label}\" to the front"),
            n == 0,
            Some(json!({"moved": n})),
        ),
        Err(e) => text_result(format!("cannot lock queue state: {e}"), true, None),
    }
}

/// Serve until stdin closes. Returns the process exit code.
pub fn serve() -> i32 {
    let out: Out = Arc::new(Mutex::new(io::stdout()));
    let running: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>> = Arc::default();
    let mut threads = Vec::new();
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => {
                reply_error(&out, &Value::Null, -32700, "parse error");
                continue;
            }
        };
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let Some(id) = msg.get("id").cloned() else {
            // A notification (or a response to us). Only cancellation matters.
            if method == "notifications/cancelled" {
                if let Some(rid) = params.get("requestId") {
                    if let Some(flag) = running.lock().ok().and_then(|m| m.get(&rid.to_string()).cloned()) {
                        flag.store(true, Ordering::SeqCst);
                    }
                }
            }
            continue;
        };
        if method.is_empty() {
            continue;
        }
        match method {
            "initialize" => {
                let asked = params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let version = if SUPPORTED_VERSIONS.contains(&asked) {
                    asked
                } else {
                    SUPPORTED_VERSIONS[1]
                };
                reply(
                    &out,
                    &id,
                    json!({
                        "protocolVersion": version,
                        "capabilities": {"tools": {"listChanged": false}},
                        "serverInfo": {"name": "calmrun", "title": "calmrun", "version": env!("CARGO_PKG_VERSION")},
                        "instructions": INSTRUCTIONS,
                    }),
                );
            }
            "ping" => reply(&out, &id, json!({})),
            "tools/list" => reply(&out, &id, json!({"tools": tools()})),
            "tools/call" => {
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
                match name.as_str() {
                    "run_task" => {
                        let token = params.get("_meta").and_then(|m| m.get("progressToken")).cloned();
                        let cancel = Arc::new(AtomicBool::new(false));
                        let key = id.to_string();
                        if let Ok(mut m) = running.lock() {
                            m.insert(key.clone(), cancel.clone());
                        }
                        let out = out.clone();
                        let running = running.clone();
                        threads.push(std::thread::spawn(move || {
                            let result = run_task(&out, &args, token, cancel);
                            if let Ok(mut m) = running.lock() {
                                m.remove(&key);
                            }
                            reply(&out, &id, result);
                        }));
                    }
                    "queue_status" => reply(&out, &id, queue_status()),
                    "bump" => reply(&out, &id, bump(&args)),
                    _ => reply_error(&out, &id, -32602, &format!("unknown tool: {name}")),
                }
            }
            _ => reply_error(&out, &id, -32601, &format!("method not found: {method}")),
        }
    }
    // The client went away: stop what still runs, then exit.
    if let Ok(m) = running.lock() {
        for flag in m.values() {
            flag.store(true, Ordering::SeqCst);
        }
    }
    for t in threads {
        let _ = t.join();
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_words_run_directly() {
        assert_eq!(
            command_argv("npx vitest run"),
            (vec!["npx".into(), "vitest".into(), "run".into()], false)
        );
        let (argv, shell) = command_argv("npm ci && npm test");
        assert!(shell);
        assert_eq!(argv, vec!["npm ci && npm test".to_string()]);
        assert!(command_argv("CI=1 npm test").1);
        assert!(!command_argv("cargo test --features=x").1);
    }

    #[test]
    fn tool_list_has_three_tools() {
        let t = tools();
        let names: Vec<&str> = t
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["run_task", "queue_status", "bump"]);
    }
}
