//! `calmrun mcp`: an MCP server over stdio (newline-delimited JSON-RPC 2.0).
//!
//! Tools are entries in [`TOOLS`]: a name, a schema and a handler. `run_task` hands its job to
//! a background task ([`crate::tasks`]) that waits in the same queue as the CLI (through
//! [`crate::queue::run`]); the call answers with the result when the job ends within
//! `wait_seconds`, else with a task id for `task_status` / `cancel_task`, so agents' tool
//! timeouts never cost the place in line. Adding a tool = one entry plus its handler.

use crate::error::Error;
use crate::queue::{self, Ctx, Notify, Request};
use crate::tasks::{self, Phase, Task, Tasks};
use crate::{history, runner, scheduler, shell, status};
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
/// How long `cancel_task` waits for the task to confirm it stopped.
const CANCEL_WAIT: Duration = Duration::from_secs(15);
/// JSON-RPC error codes.
const PARSE_ERROR: i64 = -32700;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

const INSTRUCTIONS: &str = "Use run_task for heavy commands (tests, type checks, linters, builds, \
Playwright, docker compose, pytest, cargo/gradle/make/dotnet/go builds) so they wait their turn in the \
machine-wide queue instead of overloading the computer. When the job is not done within wait_seconds \
(default 25), run_task returns a task_id at once and the job keeps its place: follow it with \
task_status (output_offset for new output only), stop it with cancel_task. Use queue_status to see \
what is running.";

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
    /// Set when this request is cancelled (or the server stops).
    cancel: Arc<AtomicBool>,
    tasks: &'a Arc<Tasks>,
}

const TOOLS: &[Tool] = &[
    Tool {
        name: "run_task",
        schema: run_task_schema,
        background: true,
        call: run_task,
    },
    Tool {
        name: "task_status",
        schema: task_status_schema,
        background: true,
        call: task_status,
    },
    Tool {
        name: "cancel_task",
        schema: cancel_task_schema,
        background: true,
        call: cancel_task,
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
            compose, pytest, cargo/gradle/make/dotnet/go builds) through the machine-wide calmrun queue, so parallel \
            agents do not overload this computer. Waits for its turn, runs the command at low \
            priority, and returns the exit code, wait and run time, the last 200 lines of output and \
            the path of the full log. If the job has not finished within wait_seconds (default 25), \
            it returns at once with a task_id and state (waiting or running) while the job keeps its \
            place and goes on in the background: call task_status with the task_id (and \
            output_offset) until state is done, or cancel_task to stop it. The command runs in the \
            platform shell (sh on macOS/Linux, cmd on Windows) when it uses shell syntax; plain words \
            run the program directly.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "The command line, e.g. \"npx vitest run\""},
                "cwd": {"type": "string", "description": "Working directory (default: the server's)"},
                "kind": {"type": "string", "description": "Job kind; detected from the command when omitted"},
                "cost": {"type": "integer", "minimum": 1, "description": "Budget points; default by kind"},
                "label": {"type": "string", "description": "Name shown in the queue"},
                "timeout_s": {"type": "number", "exclusiveMinimum": 0, "description": "Stop the command after this many seconds (exit 124)"},
                "prio": {"type": "string", "enum": ["normal", "high", "low"], "description": "high goes before every normal job, low after every normal one (default normal)"},
                "wait_timeout_s": {"type": "number", "exclusiveMinimum": 0, "description": "Give up waiting after this many seconds; the result carries a ticket that keeps the place in line for 10 minutes"},
                "ticket": {"type": "integer", "description": "Ticket from an earlier wait timeout: resume that place in line"},
                "needs": {"type": "array", "items": {"type": "string"}, "description": "Named resources to hold a slot of, e.g. [\"db\"]"},
                "wait_seconds": {"type": "number", "minimum": 0, "description": "Answer with the result if the job ends within this many seconds, else with a task_id at once (default 25; 0 = always a task_id; a large value blocks until the end, as before)"},
                "queue": {"type": "string", "description": QUEUE_PROP}
            },
            "required": ["command"]
        }
    })
}

fn task_status_schema() -> Value {
    json!({
        "name": "task_status",
        "title": "Follow a run_task task",
        "description": "State of a task started by run_task: waiting (with place in line and points), \
            running, or done (with exit code and status); wait and run time; and the output written since \
            output_offset (at most the newest 32 KB) with next_offset to pass next time. With \
            wait_seconds it waits up to that long for the task to finish first.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "task_id": {"type": "string", "description": "The task_id run_task returned"},
                "output_offset": {"type": "integer", "minimum": 0, "description": "Byte offset into the output to read from (default 0; use next_offset from the last answer)"},
                "wait_seconds": {"type": "number", "minimum": 0, "description": "Wait up to this long for the task to finish before answering (default 0)"}
            },
            "required": ["task_id"]
        }
    })
}

fn cancel_task_schema() -> Value {
    json!({
        "name": "cancel_task",
        "title": "Cancel a run_task task",
        "description": "Stop a task started by run_task: a waiting task leaves the queue, a running \
            command is stopped with its whole process tree. Returns how it ended. A task that has \
            already finished is left as it is.",
        "inputSchema": {
            "type": "object",
            "properties": {"task_id": {"type": "string", "description": "The task_id run_task returned"}},
            "required": ["task_id"]
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
        "title": "Move waiting jobs to the front (or the back)",
        "description": "Move waiting jobs to the front of the queue, picked by exactly one of label, pos, \
            ticket or dir. Later jobs may not pass them while they wait for points or RAM. With back \
            set, move them to the end of the line instead, with prio low.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "label": {"type": "string", "description": "Part of the label (case-insensitive)"},
                "pos": {"type": "integer", "minimum": 1, "description": "Place in the waiting line (place in queue_status)"},
                "ticket": {"type": "integer", "description": "The job id (id in queue_status, or a wait-timeout ticket)"},
                "dir": {"type": "string", "description": "Jobs whose folder is this one or inside it"},
                "back": {"type": "boolean", "description": "Move them to the end of the line instead, with prio low"},
                "queue": {"type": "string", "description": QUEUE_PROP}
            }
        }
    })
}

/// Whether a command line needs the shell to mean what it says.
fn needs_shell(cmd: &str) -> bool {
    cmd.contains(runner::SHELL_CHARS)
}

/// A command line split for spawning.
#[derive(Debug, PartialEq)]
pub struct CommandLine {
    pub argv: Vec<String>,
    /// Run `argv[0]` through the platform shell.
    pub shell: bool,
    /// Leading `VAR=value` words, set for the command instead of passed to a shell (so
    /// `CI=1 npm test` works on Windows too, where cmd has no such syntax).
    pub env: Vec<(String, String)>,
}

/// Split a command line: plain words run the program directly, shell syntax runs in the shell.
pub fn command_argv(cmd: &str) -> CommandLine {
    if needs_shell(cmd) {
        return CommandLine {
            argv: vec![cmd.trim().to_string()],
            shell: true,
            env: Vec::new(),
        };
    }
    let mut words = shell::tokenize(cmd);
    let assignments = words.iter().take_while(|w| is_assignment(w)).count();
    let env = words
        .drain(..assignments)
        .filter_map(|w| w.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
        .collect();
    CommandLine {
        argv: words,
        shell: false,
        env,
    }
}

fn is_assignment(w: &str) -> bool {
    w.split_once('=').is_some_and(|(k, _)| {
        !k.is_empty()
            && !k.starts_with(|c: char| c.is_ascii_digit())
            && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// Sends `notifications/progress` while a task waits, if the client gave a token.
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
        let Some(token) = &self.token else {
            return;
        };
        self.count += 1;
        let params = json!({"progressToken": token, "progress": self.count, "message": message});
        send(
            &self.out,
            &json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": params}),
        );
        self.last_sent = Some(Instant::now());
    }
}

impl Notify for McpNotify {
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

/// Records a task's progress, and forwards it as progress notifications while the
/// `run_task` request is still open (`open`; never after its answer).
struct TaskNotify {
    task: Arc<Task>,
    progress: McpNotify,
    open: Arc<Mutex<bool>>,
}

impl TaskNotify {
    fn forward(&mut self, f: impl FnOnce(&mut McpNotify)) {
        if let Ok(open) = self.open.lock() {
            if *open {
                f(&mut self.progress);
            }
        }
    }
}

impl Notify for TaskNotify {
    fn waiting(&mut self, line: &str) {
        self.task.set_line(line);
        self.forward(|p| p.waiting(line));
    }

    fn queued(&mut self, id: u64) {
        self.task.set_job(id);
    }

    fn started(&mut self, wait_ms: u64) {
        self.task.set_started();
        self.forward(|p| p.started(wait_ms));
    }

    /// Notes reach the agent in the tool result (and the server's stderr).
    fn note(&mut self, line: &str) {
        self.task.add_note(line);
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

/// Longest wait or timeout taken from arguments (larger values mean "as long as it takes").
const MAX_SECS: f64 = 30.0 * 86_400.0;

fn secs(args: &Value, key: &str) -> Option<Duration> {
    args.get(key)
        .and_then(Value::as_f64)
        .filter(|t| *t > 0.0)
        .map(|t| Duration::from_secs_f64(t.min(MAX_SECS)))
}

fn string(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(String::from)
}

static LOG_SEQ: AtomicU64 = AtomicU64::new(0);

fn request_for(args: &Value, command: &str, log: PathBuf, cancel: Arc<AtomicBool>) -> Request {
    let line = command_argv(command);
    Request {
        argv: line.argv,
        shell: line.shell,
        cwd: string(args, "cwd").map(PathBuf::from),
        kind: string(args, "kind"),
        cost: args.get("cost").and_then(Value::as_u64).map(|c| c.max(1) as u32),
        high: args.get("prio").and_then(Value::as_str) == Some("high"),
        low: args.get("prio").and_then(Value::as_str) == Some("low"),
        label: string(args, "label"),
        timeout: secs(args, "timeout_s"),
        log: Some(log),
        cancel: Some(cancel),
        wait_timeout: secs(args, "wait_timeout_s"),
        ticket: args.get("ticket").and_then(Value::as_u64),
        rerun_hint: Some("call run_task again with the same arguments plus \"ticket\": {ticket}".into()),
        refused_hint: Some("call run_task with \"command\" set to: {command}".into()),
        env: line.env,
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
    // Absent: the default; 0 (or less): answer with a task id at once.
    let wait = match c.args.get("wait_seconds").and_then(Value::as_f64) {
        Some(_) => secs(c.args, "wait_seconds").unwrap_or(Duration::ZERO),
        None => tasks::DEFAULT_WAIT,
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let req = request_for(c.args, command, log.clone(), cancel.clone());
    let open = Arc::new(Mutex::new(true));
    let progress = McpNotify {
        out: c.out.clone(),
        token: c.progress_token.clone(),
        count: 0,
        last_line: String::new(),
        last_sent: None,
        started: Instant::now(),
    };
    let forward = open.clone();
    let task = c.tasks.start(ctx.queue.clone(), log, cancel, move |task| {
        let mut notify = TaskNotify {
            task,
            progress,
            open: forward,
        };
        queue::run(&mut ctx, req, &mut notify)
    });
    let request_cancelled = || c.cancel.load(Ordering::SeqCst);
    if !task.wait(wait, &request_cancelled) && request_cancelled() {
        // The request itself was cancelled (or the server stops) while it waited: stop the
        // task, as a cancelled run_task always did.
        task.cancel.store(true, Ordering::SeqCst);
        task.wait(CANCEL_WAIT, &|| false);
    }
    // No progress for this request after its answer.
    if let Ok(mut o) = open.lock() {
        *o = false;
    }
    task_answer(&task, 0, None)
}

/// The answer about a task: its result once done, else where it is.
fn task_answer(task: &Task, offset: u64, note: Option<&str>) -> Value {
    let snap = task.snapshot();
    let log = task.log.exists().then(|| task.log.clone());
    let mut v = match &snap.outcome {
        Some(o) if offset == 0 => task_result(o, log),
        Some(o) => {
            let mut v = task_result(o, log);
            add_output(&mut v, task, offset);
            v
        }
        None => pending_result(task, &snap, offset),
    };
    let s = &mut v["structuredContent"];
    s["task_id"] = json!(task.id);
    s["state"] = json!(snap.phase.name());
    if !snap.notes.is_empty() {
        s["notes"] = json!(snap.notes);
    }
    if let Some(n) = note {
        s["note"] = json!(n);
    }
    // Notes go first in the text too, so an agent reading only the text sees them.
    let lead: Vec<&str> = snap.notes.iter().map(String::as_str).chain(note).collect();
    if let (false, Some(t)) = (lead.is_empty(), v["content"][0]["text"].as_str()) {
        v["content"][0]["text"] = json!(format!("{}\n{t}", lead.join("\n")));
    }
    v
}

/// New output since `offset` in a finished task's answer (for `task_status`).
fn add_output(v: &mut Value, task: &Task, offset: u64) {
    let (text, next, skipped) = tasks::read_output(&task.log, offset, tasks::OUTPUT_MAX_BYTES);
    let s = &mut v["structuredContent"];
    s["output"] = json!(text);
    s["next_offset"] = json!(next);
    s["skipped_bytes"] = json!(skipped);
}

/// A task that is still waiting or running: place in line, times, and output so far.
fn pending_result(task: &Task, snap: &tasks::Snapshot, offset: u64) -> Value {
    let now = queue::now_ms();
    let (output, next, skipped) = tasks::read_output(&task.log, offset, tasks::OUTPUT_MAX_BYTES);
    let place = queue_place(task, snap.job_id);
    let state = snap.phase.name();
    let mut text = match snap.phase {
        Phase::Waiting => format!(
            "task {} is waiting for its turn ({}), waited {}",
            task.id,
            snap.line
                .as_deref()
                .unwrap_or("joining the queue")
                .trim_start_matches("calmrun: "),
            history::human_ms(snap.wait_ms(now))
        ),
        _ => format!(
            "task {} is running (waited {}, running for {})",
            task.id,
            history::human_ms(snap.wait_ms(now)),
            history::human_ms(snap.run_ms(now))
        ),
    };
    text.push_str(&format!(
        "\nIt keeps its place and goes on in the background: call task_status with \
         {{\"task_id\": \"{}\", \"output_offset\": {next}}} to follow it, or cancel_task to stop it.\n",
        task.id
    ));
    if !output.is_empty() {
        text.push_str(&format!("--- output so far ---\n{output}"));
    }
    let mut structured = json!({
        "task_id": task.id,
        "state": state,
        "waiting_line": snap.line,
        "wait_ms": snap.wait_ms(now),
        "run_ms": snap.run_ms(now),
        "log_path": task.log.exists().then(|| task.log.to_string_lossy().to_string()),
        "output": output,
        "output_tail": queue::tail_lines(&task.log, TAIL_LINES),
        "next_offset": next,
        "skipped_bytes": skipped,
    });
    if let Some(p) = place {
        for (k, v) in p.as_object().into_iter().flatten() {
            structured[k] = v.clone();
        }
    }
    text_result(text, false, Some(structured))
}

/// Place in line and points of the task's job, read from its queue.
fn queue_place(task: &Task, job_id: Option<u64>) -> Option<Value> {
    let id = job_id?;
    let mut ctx = Ctx::load().and_then(|c| c.with_queue(task.queue.clone())).ok()?;
    let s = status::snapshot(&mut ctx).ok()?;
    let budget = crate::queue::effective_limits(ctx.limits(), &s).budget;
    let waiting = scheduler::waiting_in_order(&s.jobs);
    let me = s.jobs.iter().find(|j| j.id == id)?;
    Some(json!({
        "job_id": id,
        "label": me.label,
        "kind": me.kind,
        "cost": me.cost,
        "place": waiting.iter().position(|j| j.id == id).map(|p| p + 1),
        "waiting": waiting.len(),
        "running": s.jobs.iter().filter(|j| j.running()).count(),
        "used": scheduler::used_points(&s.jobs, budget) + scheduler::leased_points(&s.leases, budget),
        "budget": budget,
    }))
}

fn task_arg(c: &Call) -> Result<Arc<Task>, Value> {
    let Some(id) = c.args.get("task_id").and_then(Value::as_str) else {
        return Err(text_result(
            "needs a \"task_id\" string from run_task".into(),
            true,
            None,
        ));
    };
    c.tasks.get(id).ok_or_else(|| {
        text_result(
            format!(
                "unknown task_id \"{id}\": tasks live as long as this MCP server, and only the last \
                 100 finished ones are kept"
            ),
            true,
            None,
        )
    })
}

fn task_status(c: &Call) -> Value {
    let task = match task_arg(c) {
        Ok(t) => t,
        Err(e) => return e,
    };
    if let Some(w) = secs(c.args, "wait_seconds") {
        task.wait(w, &|| c.cancel.load(Ordering::SeqCst));
    }
    let offset = c.args.get("output_offset").and_then(Value::as_u64).unwrap_or(0);
    let mut v = task_answer(&task, offset, None);
    if task.is_done() && offset == 0 {
        add_output(&mut v, &task, 0);
    }
    v
}

fn cancel_task(c: &Call) -> Value {
    let task = match task_arg(c) {
        Ok(t) => t,
        Err(e) => return e,
    };
    if task.is_done() {
        return task_answer(&task, 0, Some("already finished; nothing to cancel"));
    }
    let was = task.snapshot().phase;
    task.cancel.store(true, Ordering::SeqCst);
    if !task.wait(CANCEL_WAIT, &|| c.cancel.load(Ordering::SeqCst)) {
        return task_answer(&task, 0, Some("cancel requested; the task is still stopping"));
    }
    let note = match was {
        Phase::Waiting => "cancelled while waiting: it left the queue",
        _ => "cancelled: the command and its process tree were stopped",
    };
    let mut v = task_answer(&task, 0, Some(note));
    // The cancel worked: not a tool error, even though the job's exit code is 130.
    if v["structuredContent"]["status"] == "cancelled" {
        v["isError"] = json!(false);
    }
    v
}

/// `exit code 1, npx vitest, kind test, waited 2.0s, ran 31s`; the status word only
/// when it says more than the code, and no job details for a refusal or an error before
/// the job was set up.
fn summary_line(o: &queue::Outcome) -> String {
    let mut parts = vec![if o.status == o.code.to_string() || o.status.is_empty() {
        format!("exit code {}", o.code)
    } else {
        format!("exit code {} ({})", o.code, o.status)
    }];
    if !o.label.is_empty() {
        parts.push(o.label.clone());
    }
    if !o.kind.is_empty() {
        parts.push(format!("kind {}", o.kind));
        parts.push(format!("waited {}", history::human_ms(o.wait_ms)));
        parts.push(format!("ran {}", history::human_ms(o.run_ms)));
    }
    parts.join(", ")
}

fn task_result(o: &queue::Outcome, log: Option<PathBuf>) -> Value {
    let mut text = summary_line(o);
    text.push('\n');
    if let Some(m) = &o.message {
        text.push_str(m);
        text.push('\n');
    }
    let tail = log.as_ref().map(|l| queue::tail_lines(l, TAIL_LINES));
    if let (Some(l), Some(t)) = (&log, &tail) {
        text.push_str(&format!("full log: {}\n", l.display()));
        text.push_str(&format!("--- last {TAIL_LINES} lines of output ---\n{t}"));
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
        // Clients that show structured content instead of the text still get the output.
        "output_tail": tail,
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

/// The one selector of a `bump` call: exactly one of `label`, `pos`, `ticket`, `dir`, each
/// of the right type.
fn bump_selector(args: &Value) -> Result<status::Selector, String> {
    use status::Selector;
    const KEYS: [&str; 4] = ["label", "pos", "ticket", "dir"];
    let given: Vec<&str> = KEYS
        .into_iter()
        .filter(|k| args.get(k).is_some_and(|v| !v.is_null()))
        .collect();
    let [key] = given[..] else {
        return Err(format!(
            "bump needs exactly one of \"label\", \"pos\", \"ticket\" or \"dir\" (got {})",
            if given.is_empty() {
                "none".to_string()
            } else {
                given.join(", ")
            }
        ));
    };
    let v = &args[key];
    let text = || v.as_str().filter(|t| !t.is_empty()).map(String::from);
    let sel = match key {
        "label" => text().map(Selector::Label),
        "pos" => v.as_u64().filter(|p| *p >= 1).map(|p| Selector::Pos(p as usize)),
        "ticket" => v.as_u64().map(Selector::Ticket),
        _ => text().map(|d| Selector::dir(std::path::Path::new(&d))),
    };
    sel.ok_or_else(|| {
        let want = match key {
            "label" | "dir" => "a non-empty string",
            "pos" => "an integer of at least 1",
            _ => "an integer",
        };
        format!("bump: \"{key}\" must be {want}")
    })
}

fn bump(c: &Call) -> Value {
    let sel = match bump_selector(c.args) {
        Ok(sel) => sel,
        Err(e) => return text_result(e, true, None),
    };
    let back = c.args.get("back").and_then(Value::as_bool).unwrap_or(false);
    let mut ctx = match ctx_for(c.args) {
        Ok(ctx) => ctx,
        Err(e) => return e,
    };
    match status::bump(&mut ctx, &sel, back) {
        Ok(n) => {
            let to = if back { "the end (prio low)" } else { "the front" };
            text_result(
                format!("moved {n} waiting job(s) matching {} to {to}", sel.describe()),
                false,
                Some(json!({"moved": n, "matched": n, "back": back})),
            )
        }
        Err(e) => error_result(&e),
    }
}

/// The server: tracks running background calls so they can be cancelled.
struct Server {
    out: Out,
    /// Background calls by request id. Taking an entry out (on cancel) also means: no reply.
    running: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
    threads: Vec<JoinHandle<()>>,
    /// Jobs started by `run_task`, which may outlive their request.
    tasks: Arc<Tasks>,
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
                tasks: &self.tasks,
            };
            return reply(&self.out, &id, (tool.call)(&c));
        }
        let key = id.to_string();
        if let Ok(mut m) = self.running.lock() {
            m.insert(key.clone(), cancel.clone());
        }
        let (out, running, call) = (self.out.clone(), self.running.clone(), tool.call);
        let tasks = self.tasks.clone();
        self.threads.push(std::thread::spawn(move || {
            let result = call(&Call {
                out: &out,
                args: &args,
                progress_token,
                cancel,
                tasks: &tasks,
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

    /// The client went away: stop what still runs (calls and tasks), then wait for it.
    fn shutdown(self) {
        if let Ok(m) = self.running.lock() {
            m.values().for_each(|f| f.store(true, Ordering::SeqCst));
        }
        self.tasks.cancel_all();
        for t in self.threads {
            let _ = t.join();
        }
        self.tasks.join_all();
    }
}

/// How long running tasks get to stop after SIGTERM/Ctrl+C before the server exits.
const SIGNAL_GRACE: Duration = Duration::from_secs(5);

/// On Ctrl+C / SIGTERM: cancel every call and task (their process trees are stopped like
/// any cancelled task), give them a moment, then exit.
fn stop_tasks_on_signal(running: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>, tasks: Arc<Tasks>) {
    let _ = ctrlc::set_handler(move || {
        if let Ok(m) = running.lock() {
            m.values().for_each(|f| f.store(true, Ordering::SeqCst));
        }
        tasks.cancel_all();
        let deadline = Instant::now() + SIGNAL_GRACE;
        let busy = || running.lock().is_ok_and(|m| !m.is_empty()) || !tasks.all_done();
        while Instant::now() < deadline && busy() {
            std::thread::sleep(Duration::from_millis(100));
        }
        std::process::exit(runner::EXIT_INTERRUPTED);
    });
}

/// Serve until stdin closes. Returns the process exit code.
pub fn serve() -> i32 {
    let mut server = Server {
        out: Arc::new(Mutex::new(io::stdout())),
        running: Arc::default(),
        threads: Vec::new(),
        tasks: Arc::default(),
    };
    stop_tasks_on_signal(server.running.clone(), server.tasks.clone());
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
    fn bump_takes_exactly_one_selector() {
        use status::Selector;
        let sel = |v: Value| bump_selector(&v);
        assert_eq!(sel(json!({"label": "e2e"})), Ok(Selector::Label("e2e".into())));
        assert_eq!(sel(json!({"pos": 2, "back": true})), Ok(Selector::Pos(2)));
        assert_eq!(sel(json!({"ticket": 7, "label": null})), Ok(Selector::Ticket(7)));
        assert!(matches!(sel(json!({"dir": "."})), Ok(Selector::Dir(_))));
        let err = sel(json!({})).unwrap_err();
        assert!(err.contains("exactly one") && err.contains("none"), "{err}");
        let err = sel(json!({"label": "a", "pos": 1})).unwrap_err();
        assert!(err.contains("label, pos"), "{err}");
        for bad in [
            json!({"pos": 0}),
            json!({"pos": "2"}),
            json!({"label": ""}),
            json!({"ticket": -1}),
        ] {
            assert!(sel(bad.clone()).unwrap_err().contains("must be"), "{bad}");
        }
    }

    #[test]
    fn plain_words_run_directly() {
        let l = command_argv("npx vitest run");
        assert_eq!(
            (l.argv, l.shell),
            (vec!["npx".into(), "vitest".into(), "run".into()], false)
        );
        let l = command_argv("npm ci && npm test");
        assert!(l.shell);
        assert_eq!(l.argv, vec!["npm ci && npm test".to_string()]);
        assert!(!command_argv("cargo test --features=x").shell);
    }

    #[test]
    fn leading_assignments_become_environment() {
        let l = command_argv("CI=1 NODE_ENV=test npm test");
        assert!(!l.shell);
        assert_eq!(l.argv, vec!["npm".to_string(), "test".to_string()]);
        assert_eq!(
            l.env,
            vec![("CI".into(), "1".into()), ("NODE_ENV".into(), "test".into())]
        );
    }

    #[test]
    fn summary_lines_skip_what_says_nothing() {
        let ok = queue::Outcome {
            code: 0,
            status: "0".into(),
            label: "npx vitest".into(),
            kind: "test".into(),
            wait_ms: 2_000,
            run_ms: 500,
            ..Default::default()
        };
        assert_eq!(
            summary_line(&ok),
            "exit code 0, npx vitest, kind test, waited 2.0s, ran 0.5s"
        );
        let refused = queue::Outcome {
            code: 2,
            status: "refused".into(),
            ..Default::default()
        };
        assert_eq!(summary_line(&refused), "exit code 2 (refused)");
    }

    #[test]
    fn every_tool_has_a_matching_schema() {
        let names: Vec<&str> = TOOLS.iter().map(|t| t.name).collect();
        assert_eq!(
            names,
            vec!["run_task", "task_status", "cancel_task", "queue_status", "bump"]
        );
        for t in TOOLS {
            let s = (t.schema)();
            assert_eq!(s["name"], t.name);
            assert_eq!(s["inputSchema"]["type"], "object");
        }
    }
}
