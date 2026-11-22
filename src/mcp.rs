//! `calmrun mcp`: an MCP server over stdio (newline-delimited JSON-RPC 2.0).
//!
//! Tools are entries in [`TOOLS`]: a name, a schema and a handler. `run_task` waits in the
//! same queue as the CLI (through [`crate::queue::run`]) on its own thread, so status calls
//! answer while a task waits. Adding a tool = one entry plus its handler.

use crate::error::Error;
use crate::queue::{self, Ctx, Notify, Request};
use crate::{history, runner, shell, status};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Protocol versions we accept from the client; we answer with the client's if listed.
const SUPPORTED_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
/// Answer for a client asking for a version we do not list.
const DEFAULT_VERSION: &str = "2025-06-18";
/// Lines of output returned by `run_task`.
const TAIL_LINES: usize = 200;
/// Log files kept in the state directory.
const KEEP_LOGS: usize = 200;
/// A progress notification is repeated at least this often while the line stays the same.
const PROGRESS_EVERY: Duration = Duration::from_secs(10);
/// JSON-RPC error codes.
const PARSE_ERROR: i64 = -32700;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

const INSTRUCTIONS: &str = "Use run_task for heavy commands (tests, type checks, linters, builds, \
Playwright, docker compose, pytest, cargo/gradle builds) so they wait their turn in the \
machine-wide queue instead of overloading the computer. Use queue_status to see what is running.";

type Out = Arc<Mutex<io::Stdout>>;

/// One MCP tool.
struct Tool {
    name: &'static str,
    schema: fn() -> Value,
    /// Long-running tools run on their own thread and can be cancelled.
    background: bool,
    call: fn(&Call) -> Value,
}

/// What a tool handler gets.
struct Call<'a> {
    out: &'a Out,
    args: &'a Value,
    progress_token: Option<Value>,
    cancel: Arc<AtomicBool>,
}

const TOOLS: &[Tool] = &[
    Tool {
        name: "run_task",
        schema: run_task_schema,
        background: true,
        call: run_task,
    },
    Tool {
        name: "queue_status",
        schema: queue_status_schema,
        background: false,
        call: queue_status,
    },
    Tool {
        name: "bump",
        schema: bump_schema,
        background: false,
        call: bump,
    },
];

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

fn error_result(e: &Error) -> Value {
    text_result(e.to_string(), true, None)
}

const QUEUE_PROP: &str = "A separate queue id instead of the machine-wide one";

fn run_task_schema() -> Value {
    json!({
        "name": "run_task",
        "title": "Run a heavy command through the calmrun queue",
        "description": "Run a heavy command (tests, type checks, linters, builds, Playwright, docker \
            compose, pytest, cargo/gradle builds) through the machine-wide calmrun queue, so parallel \
            agents do not overload this computer. Waits for its turn, runs the command at low \
            priority, and returns the exit code, wait and run time, the last 200 lines of output and \
            the path of the full log. The command runs in the platform shell (sh on macOS/Linux, cmd \
            on Windows) when it uses shell syntax; plain words run the program directly.",
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
                "ticket": {"type": "integer", "description": "Ticket from an earlier wait timeout: resume that place in line"},
                "needs": {"type": "array", "items": {"type": "string"}, "description": "Named resources to hold a slot of, e.g. [\"db\"]"},
                "queue": {"type": "string", "description": QUEUE_PROP}
            },
            "required": ["command"]
        }
    })
}

fn queue_status_schema() -> Value {
    json!({
        "name": "queue_status",
        "title": "Show the calmrun queue",
        "description": "Running and waiting jobs (label, kind, cost, pid, cwd, time), the point \
            budget, free RAM and warnings about jobs that have waited a long time.",
        "inputSchema": {"type": "object", "properties": {"queue": {"type": "string", "description": QUEUE_PROP}}}
    })
}

fn bump_schema() -> Value {
    json!({
        "name": "bump",
        "title": "Move waiting jobs to the front",
        "description": "Move waiting jobs whose label contains the given text to the front of the queue.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "label": {"type": "string", "description": "Part of the label (case-insensitive)"},
                "queue": {"type": "string", "description": QUEUE_PROP}
            },
            "required": ["label"]
        }
    })
}

/// Whether a command line needs the shell to mean what it says.
fn needs_shell(cmd: &str) -> bool {
    cmd.contains(runner::SHELL_CHARS)
        || shell::tokenize(cmd)
            .first()
            .is_some_and(|w| w.contains('=') && !w.starts_with('-'))
}

/// argv for a command line, plus whether it must run through the shell.
pub fn command_argv(cmd: &str) -> (Vec<String>, bool) {
    if needs_shell(cmd) {
        (vec![cmd.trim().to_string()], true)
    } else {
        (shell::tokenize(cmd), false)
    }
}

/// Sends `notifications/progress` while a task waits, if the client gave a token.
struct McpNotify<'a> {
    out: &'a Out,
    token: Option<Value>,
    count: u64,
    last_line: String,
    last_sent: Option<Instant>,
    started: Instant,
}

impl McpNotify<'_> {
    fn progress(&mut self, message: String) {
        let Some(token) = &self.token else {
            return;
        };
        self.count += 1;
        let params = json!({"progressToken": token, "progress": self.count, "message": message});
        send(
            self.out,
            &json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": params}),
        );
        self.last_sent = Some(Instant::now());
    }
}

impl Notify for McpNotify<'_> {
    fn wants_line(&self) -> bool {
        self.token.is_some()
    }

    fn waiting(&mut self, line: &str) {
        let due = self.last_sent.is_none_or(|t| t.elapsed() >= PROGRESS_EVERY);
        if line != self.last_line || due {
            self.last_line = line.to_string();
            let waited = history::human_ms(self.started.elapsed().as_millis() as u64);
            self.progress(format!("{line} (waited {waited})"));
        }
    }

    fn started(&mut self, wait_ms: u64) {
        self.progress(format!("started after waiting {}", history::human_ms(wait_ms)));
    }

    /// Notes go to stderr: stdout carries the protocol.
    fn note(&mut self, line: &str) {
        eprintln!("{line}");
    }
}

/// Settings for the queue named in the arguments.
fn ctx_for(args: &Value) -> Result<Ctx, Value> {
    let queue = args
        .get("queue")
        .and_then(Value::as_str)
        .filter(|q| !q.is_empty())
        .map(String::from);
    Ctx::load()
        .and_then(|c| c.with_queue(queue))
        .map_err(|e| error_result(&e))
}

fn secs(args: &Value, key: &str) -> Option<Duration> {
    args.get(key)
        .and_then(Value::as_f64)
        .filter(|t| *t > 0.0)
        .map(Duration::from_secs_f64)
}

fn string(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(String::from)
}

static LOG_SEQ: AtomicU64 = AtomicU64::new(0);

fn request_for(args: &Value, command: &str, log: PathBuf, cancel: Arc<AtomicBool>) -> Request {
    let (argv, shell) = command_argv(command);
    Request {
        argv,
        shell,
        cwd: string(args, "cwd").map(PathBuf::from),
        kind: string(args, "kind"),
        cost: args.get("cost").and_then(Value::as_u64).map(|c| c.max(1) as u32),
        high: args.get("prio").and_then(Value::as_str) == Some("high"),
        label: string(args, "label"),
        timeout: secs(args, "timeout_s"),
        log: Some(log),
        cancel: Some(cancel),
        wait_timeout: secs(args, "wait_timeout_s"),
        ticket: args.get("ticket").and_then(Value::as_u64),
        rerun_hint: Some("call run_task again with the same arguments plus \"ticket\": {ticket}".into()),
        env: Vec::new(),
        needs: args
            .get("needs")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect())
            .unwrap_or_default(),
    }
}

fn run_task(c: &Call) -> Value {
    let Some(command) = c
        .args
        .get("command")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
    else {
        return text_result("run_task needs a non-empty \"command\" string".into(), true, None);
    };
    if let Some(d) = string(c.args, "cwd").map(PathBuf::from).filter(|d| !d.is_dir()) {
        return text_result(format!("cwd is not a directory: {}", d.display()), true, None);
    }
    let mut ctx = match ctx_for(c.args) {
        Ok(ctx) => ctx,
        Err(e) => return e,
    };
    let logs = queue::log_dir(&ctx.home);
    queue::prune_logs(&logs, KEEP_LOGS);
    let name = format!(
        "{}-{}-{}.log",
        queue::now_ms(),
        std::process::id(),
        LOG_SEQ.fetch_add(1, Ordering::SeqCst)
    );
    let log = logs.join(name);
    let req = request_for(c.args, command, log.clone(), c.cancel.clone());
    let mut notify = McpNotify {
        out: c.out,
        token: c.progress_token.clone(),
        count: 0,
        last_line: String::new(),
        last_sent: None,
        started: Instant::now(),
    };
    let o = queue::run(&mut ctx, req, &mut notify);
    task_result(&o, log.exists().then_some(log))
}

fn task_result(o: &queue::Outcome, log: Option<PathBuf>) -> Value {
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
    if let Some(l) = &log {
        text.push_str(&format!("full log: {}\n", l.display()));
        text.push_str(&format!(
            "--- last {TAIL_LINES} lines ---\n{}",
            queue::tail_lines(l, TAIL_LINES)
        ));
    }
    let structured = json!({
        "exit_code": o.code,
        "status": o.status,
        "label": o.label,
        "kind": o.kind,
        "cost": o.cost,
        "wait_ms": o.wait_ms,
        "run_ms": o.run_ms,
        "log_path": log.map(|l| l.to_string_lossy().to_string()),
        "message": o.message,
        "ticket": o.ticket,
    });
    text_result(text, o.code != 0, Some(structured))
}

fn queue_status(c: &Call) -> Value {
    let mut ctx = match ctx_for(c.args) {
        Ok(ctx) => ctx,
        Err(e) => return e,
    };
    match status::snapshot(&mut ctx) {
        Ok(state) => {
            let v = status::status_json(&mut ctx, &state);
            text_result(
                serde_json::to_string_pretty(&v).unwrap_or_default(),
                false,
                Some(v),
            )
        }
        Err(e) => error_result(&e),
    }
}

fn bump(c: &Call) -> Value {
    let Some(label) = c
        .args
        .get("label")
        .and_then(Value::as_str)
        .filter(|l| !l.is_empty())
    else {
        return text_result("bump needs a \"label\" string".into(), true, None);
    };
    let mut ctx = match ctx_for(c.args) {
        Ok(ctx) => ctx,
        Err(e) => return e,
    };
    match status::bump(&mut ctx, label) {
        Ok(n) => text_result(
            format!("moved {n} waiting job(s) matching \"{label}\" to the front"),
            n == 0,
            Some(json!({"moved": n})),
        ),
        Err(e) => error_result(&e),
    }
}

/// The server: tracks running background calls so they can be cancelled.
struct Server {
    out: Out,
    /// Background calls by request id. Taking an entry out (on cancel) also means: no reply.
    running: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
    threads: Vec<JoinHandle<()>>,
}

impl Server {
    fn handle_line(&mut self, line: &str) {
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => return reply_error(&self.out, &Value::Null, PARSE_ERROR, "parse error"),
        };
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        match msg.get("id").cloned() {
            // A notification (or a response to us). Only cancellation matters.
            None if method == "notifications/cancelled" => self.cancel(&params),
            None => {}
            Some(_) if method.is_empty() => {}
            Some(id) => self.request(id, method, params),
        }
    }

    fn cancel(&mut self, params: &Value) {
        let Some(rid) = params.get("requestId") else {
            return;
        };
        // Per the MCP spec a cancelled request gets no response.
        if let Some(flag) = self
            .running
            .lock()
            .ok()
            .and_then(|mut m| m.remove(&rid.to_string()))
        {
            flag.store(true, Ordering::SeqCst);
        }
    }

    fn request(&mut self, id: Value, method: &str, params: Value) {
        match method {
            "initialize" => {
                let asked = params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let version = SUPPORTED_VERSIONS
                    .iter()
                    .find(|v| **v == asked)
                    .copied()
                    .unwrap_or(DEFAULT_VERSION);
                let result = json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": "calmrun", "title": "calmrun", "version": env!("CARGO_PKG_VERSION")},
                    "instructions": INSTRUCTIONS,
                });
                reply(&self.out, &id, result);
            }
            "ping" => reply(&self.out, &id, json!({})),
            "tools/list" => {
                let tools: Vec<Value> = TOOLS.iter().map(|t| (t.schema)()).collect();
                reply(&self.out, &id, json!({"tools": tools}));
            }
            "tools/call" => self.call(id, params),
            _ => reply_error(
                &self.out,
                &id,
                METHOD_NOT_FOUND,
                &format!("method not found: {method}"),
            ),
        }
    }

    fn call(&mut self, id: Value, params: Value) {
        let name = params.get("name").and_then(Value::as_str).unwrap_or("");
        let Some(tool) = TOOLS.iter().find(|t| t.name == name) else {
            return reply_error(&self.out, &id, INVALID_PARAMS, &format!("unknown tool: {name}"));
        };
        let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
        let progress_token = params.get("_meta").and_then(|m| m.get("progressToken")).cloned();
        let cancel = Arc::new(AtomicBool::new(false));
        if !tool.background {
            let c = Call {
                out: &self.out,
                args: &args,
                progress_token,
                cancel,
            };
            return reply(&self.out, &id, (tool.call)(&c));
        }
        let key = id.to_string();
        if let Ok(mut m) = self.running.lock() {
            m.insert(key.clone(), cancel.clone());
        }
        let (out, running, call) = (self.out.clone(), self.running.clone(), tool.call);
        self.threads.push(std::thread::spawn(move || {
            let result = call(&Call {
                out: &out,
                args: &args,
                progress_token,
                cancel,
            });
            let still_wanted = running
                .lock()
                .map(|mut m| m.remove(&key).is_some())
                .unwrap_or(true);
            if still_wanted {
                reply(&out, &id, result);
            }
        }));
    }

    /// The client went away: stop what still runs, then wait for it.
    fn shutdown(self) {
        if let Ok(m) = self.running.lock() {
            m.values().for_each(|f| f.store(true, Ordering::SeqCst));
        }
        for t in self.threads {
            let _ = t.join();
        }
    }
}

/// Serve until stdin closes. Returns the process exit code.
pub fn serve() -> i32 {
    let mut server = Server {
        out: Arc::new(Mutex::new(io::stdout())),
        running: Arc::default(),
        threads: Vec::new(),
    };
    for line in io::stdin().lock().lines() {
        let Ok(line) = line else {
            break;
        };
        if !line.trim().is_empty() {
            server.handle_line(&line);
        }
    }
    server.shutdown();
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
    fn every_tool_has_a_matching_schema() {
        let names: Vec<&str> = TOOLS.iter().map(|t| t.name).collect();
        assert_eq!(names, vec!["run_task", "queue_status", "bump"]);
        for t in TOOLS {
            let s = (t.schema)();
            assert_eq!(s["name"], t.name);
            assert_eq!(s["inputSchema"]["type"], "object");
        }
    }
}
