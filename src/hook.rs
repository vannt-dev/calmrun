//! `calmrun hook <agent>`: PreToolUse hooks for coding agents.
//!
//! One decision for every agent ([`plan`]): which parts of a shell command are heavy, the
//! command with each of them wrapped in `calmrun --`, and a `--wait-timeout` below the tool's
//! own time limit. Each agent has a thin adapter that reads its tool call and writes its answer:
//! Claude Code here ([`handle`]), Codex, Antigravity and OpenCode in the child modules.
//!
//! Claude Code contract (https://code.claude.com/docs/en/hooks): the hook reads JSON with
//! `tool_name` and `tool_input` on stdin. To block a call it prints
//! `{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny",
//! "permissionDecisionReason":"..."}}` and exits 0; the reason is shown to Claude.
//! `updatedInput` replaces the whole tool input. Printing nothing leaves the call alone.

pub mod antigravity;
pub mod codex;
pub mod opencode;

use crate::config::KindConfig;
use crate::detect::{detect_words, disposition, parse, tool_name, Disposition};
use crate::shell::{split_segments, Word};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::ops::Range;

/// What the hook does with a heavy command.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    Deny,
    Rewrite,
}

/// The agents calmrun has a hook for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Agent {
    Claude,
    Codex,
    Antigravity,
    OpenCode,
}

/// What every adapter needs besides the agent's input.
pub struct Opts<'a> {
    pub mode: Mode,
    pub kinds: &'a BTreeMap<String, KindConfig>,
    /// What suggestions run: `calmrun`, or its absolute path when it is not on PATH.
    pub program: &'a str,
    /// A short line on the queue's load (`Queue now: 3 of 3 points in use, 1 waiting.`),
    /// asked for only when a command is denied.
    pub load: &'a dyn Fn() -> Option<String>,
}

/// [`Opts`] for tests: no config kinds, no load line.
#[cfg(test)]
pub fn opts(mode: Mode, program: &str) -> Opts<'_> {
    static NO_KINDS: BTreeMap<String, KindConfig> = BTreeMap::new();
    fn no_load() -> Option<String> {
        None
    }
    Opts {
        mode,
        kinds: &NO_KINDS,
        program,
        load: &no_load,
    }
}

/// Process one hook input of `agent`: the text to print, or `None` to stay silent (allow).
pub fn run(agent: Agent, input: &str, o: &Opts) -> Option<String> {
    match agent {
        Agent::Claude => handle(input, o),
        Agent::Codex => codex::handle(input, o),
        Agent::Antigravity => antigravity::handle(input, o),
        Agent::OpenCode => opencode::handle(input, o),
    }
}

/// The shell that runs the command, as far as the agent tells.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Shell {
    /// sh/bash (Git Bash on Windows), or a shell the agent does not name: the program path
    /// gets forward slashes, which every shell accepts.
    Posix,
    /// PowerShell: backslashes stay, and a quoted path is called with `&`.
    PowerShell,
}

/// A shell tool call, as every adapter reduces its agent's call to.
#[derive(Debug, Clone, Copy)]
pub struct Call<'a> {
    pub command: &'a str,
    pub shell: Shell,
    /// The tool's time limit in ms for a foreground command, after which it kills it.
    /// `None`: the tool does not kill long commands (or nothing is known about it).
    pub limit_ms: Option<u64>,
    /// The call already runs in the background (no time limit applies).
    pub background: bool,
}

/// What to do with a heavy call.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// The kinds found, in order.
    pub kinds: Vec<String>,
    /// The command with each heavy part wrapped, no `--wait-timeout`.
    pub plain: String,
    /// The same with `--wait-timeout` below the tool's limit (foreground calls with a limit).
    pub capped: Option<String>,
    /// The `--wait-timeout` value in `capped`.
    pub wait_secs: Option<u64>,
    /// Some kind found usually takes minutes (tests, builds, containers, monorepo runs).
    pub long: bool,
    /// The call already runs in the background.
    pub background: bool,
}

impl Plan {
    /// The command to suggest for the call as it is: capped when there is a limit.
    pub fn command(&self) -> &str {
        self.capped.as_deref().unwrap_or(&self.plain)
    }
}

/// Kinds that usually run for minutes: worth the background where the agent has one.
const LONG_KINDS: &[&str] = &[
    "vitest", "jest", "capture", "docker", "build", "test", "pytest", "monorepo",
];

/// A `--wait-timeout` a little below a tool limit of `limit_ms`, so calmrun gives up waiting
/// (exit 75, place kept) before the tool kills it: 120 s -> 108 s, 600 s -> 570 s.
pub fn wait_secs(limit_ms: u64) -> u64 {
    let total = limit_ms / 1000;
    let margin = (total / 10).clamp(5, 30);
    total.saturating_sub(margin).max(1)
}

/// The program as the call's shell runs it.
fn program_for(program: &str, shell: Shell) -> String {
    match shell {
        // Bash (Git Bash on Windows) eats backslashes in an unquoted path; C:/x/calmrun.exe works.
        Shell::Posix => program.replace('\\', "/"),
        // A quoted path is a string in PowerShell, not a command: call it with `&`.
        Shell::PowerShell if program.starts_with(['"', '\'']) => format!("& {program}"),
        Shell::PowerShell => program.to_string(),
    }
}

/// The shared decision: `None` when nothing in the call needs the queue.
pub fn plan(call: &Call, kinds: &BTreeMap<String, KindConfig>, program: &str) -> Option<Plan> {
    let program = program_for(program, call.shell);
    let (plain, found) = wrap_with(call.command, kinds, &program)?;
    let wait = call.limit_ms.filter(|_| !call.background).map(wait_secs);
    let capped = wait
        .and_then(|s| wrap_with(call.command, kinds, &format!("{program} --wait-timeout {s}")).map(|w| w.0));
    let long = found.iter().any(|k| LONG_KINDS.contains(&k.as_str()));
    Some(Plan {
        kinds: found,
        plain,
        capped,
        wait_secs: wait,
        long,
        background: call.background,
    })
}

/// How an agent can run a command in the background, for the deny text.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Background {
    /// Name the tool parameter (`run_in_background: true` for Claude Code).
    Param(&'static str),
    /// Only a general hint.
    Hint,
}

/// The deny reason every agent shows its model: the command to run instead, the queue's
/// load, and what to do about long waits and long runs.
pub fn deny_reason(p: &Plan, bg: Background, o: &Opts) -> String {
    let mut s = format!(
        "Heavy command ({}) must go through the machine-wide calmrun queue so parallel agents \
         do not freeze this computer. Run this instead:\n{}",
        p.kinds.join(", "),
        p.command()
    );
    if let Some(load) = (o.load)() {
        s.push('\n');
        s.push_str(&load);
    }
    if p.capped.is_some() {
        s.push_str(
            "\nExit 75 means it is still waiting in line: run the `calmrun: next:` command it \
             prints to keep its place.",
        );
        if let (true, Background::Param(param)) = (p.long, bg) {
            s.push_str(&format!(
                "\nIt may take minutes: better run it with {param}, without --wait-timeout:\n{}",
                p.plain
            ));
        }
    } else if !p.background {
        s.push_str("\nIf it may take more than a minute or two, run it in the background.");
    }
    s
}

impl Mode {
    /// `CALMRUN_HOOK_MODE=rewrite` rewrites; anything else denies with a suggestion.
    pub fn from_env() -> Mode {
        match std::env::var("CALMRUN_HOOK_MODE").ok().as_deref().map(str::trim) {
            Some("rewrite") => Mode::Rewrite,
            _ => Mode::Deny,
        }
    }
}

fn already_wrapped(words: &[String]) -> bool {
    let Some(p) = parse(words) else { return false };
    let first = words.get(p.start).map(|w| tool_name(w)).unwrap_or_default();
    let second = words.get(p.start + 1).map(|w| w.to_ascii_lowercase());
    first == "calmrun"
        || p.tool == "calmrun"
        || (first == "lane" && second.as_deref() == Some("with"))
        || (p.tool == "lane" && p.rest.first().map(String::as_str) == Some("with"))
}

/// The command with every heavy, unwrapped segment wrapped in place, plus the kinds found.
/// `None` when nothing needs wrapping.
#[cfg(test)]
pub fn wrap_command(cmd: &str, kinds: &BTreeMap<String, KindConfig>) -> Option<(String, Vec<String>)> {
    wrap_with(cmd, kinds, "calmrun")
}

/// [`wrap_command`] with the program to insert (`calmrun`, or its absolute path).
pub fn wrap_with(
    cmd: &str,
    kinds: &BTreeMap<String, KindConfig>,
    program: &str,
) -> Option<(String, Vec<String>)> {
    let mut out = String::with_capacity(cmd.len() + 16);
    let mut last = 0;
    let mut found = Vec::new();
    for r in split_segments(cmd)
        .into_iter()
        .flat_map(|r| commands(cmd, r, kinds))
    {
        let Some((range, text, kinds_here)) = wrap_piece(cmd, r, kinds, program) else {
            continue;
        };
        out.push_str(&cmd[last..range.start]);
        out.push_str(&text);
        last = range.end;
        for k in kinds_here {
            if !found.contains(&k) {
                found.push(k);
            }
        }
    }
    if found.is_empty() {
        return None;
    }
    out.push_str(&cmd[last..]);
    Some((out, found))
}

/// One command of `cmd` at `r`: the range to replace, its new text and the kinds found.
fn wrap_piece(
    cmd: &str,
    r: Range<usize>,
    kinds: &BTreeMap<String, KindConfig>,
    program: &str,
) -> Option<(Range<usize>, String, Vec<String>)> {
    let seg = &cmd[r.clone()];
    let spans = crate::shell::words(seg);
    let words: Vec<String> = spans.iter().map(|w| w.text.clone()).collect();
    if already_wrapped(&words) {
        return None;
    }
    let p = parse(&words)?;
    if let Some(inner) = shell_script(seg, &spans, p.start, program) {
        // `bash -c "npm test"`: wrap inside the script, where the command runs.
        let (text, found) = wrap_with(&seg[inner.clone()], kinds, program)?;
        return Some((r.start + inner.start..r.start + inner.end, text, found));
    }
    // Keep leading `VAR=value` assignments in front of calmrun (they reach the child).
    let insert_at = spans.get(p.start).map_or(0, |w| w.start);
    // The runner's own verdict, as if stdin were a terminal (the strictest case: plain
    // vitest watches then), so the suggestion is always a command calmrun accepts.
    let (kind, text) = match disposition(&words, true) {
        // Dev servers, app runs and help output run outside the queue anyway.
        Disposition::Light(_) => return None,
        Disposition::Queue => (detect_words(&words, kinds).kind?, seg[insert_at..].to_string()),
        // A watch mode with a one-shot form: suggest that form, if it is heavy.
        Disposition::Refuse { fixed: Some(f), .. } => {
            let f = crate::detect::accepted_form(f)?;
            let k = detect_words(&f, kinds).kind?;
            (k, render(seg, &spans[p.start..], &f[p.start..]))
        }
        // No one-shot form: calmrun would refuse it too, so leave it alone.
        Disposition::Refuse { fixed: None, .. } => return None,
    };
    Some((
        r.start + insert_at..r.end,
        format!("{program} -- {text}"),
        vec![kind],
    ))
}

/// Shells whose `-c` / `-Command` script is a command line of its own.
const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh", "pwsh", "powershell"];

/// In `bash -c "npm test"` (words from `start`), the range of the script inside its quotes,
/// when calmrun can be written into it as it is (the program has no quote of that kind).
fn shell_script(seg: &str, spans: &[Word], start: usize, program: &str) -> Option<Range<usize>> {
    if !SHELLS.contains(&tool_name(&spans.get(start)?.text).as_str()) {
        return None;
    }
    let flag = spans.get(start + 1)?.text.to_ascii_lowercase();
    if !["-c", "-lc", "-ic", "-command"].contains(&flag.as_str()) {
        return None;
    }
    let w = spans.get(start + 2)?;
    let end = spans.get(start + 3).map_or(seg.len(), |n| n.start);
    let raw = seg[w.start..end].trim_end();
    let q = raw.chars().next().filter(|c| matches!(c, '"' | '\''))?;
    (raw.len() >= 2 && raw.ends_with(q) && !program.contains(q)).then(|| w.start + 1..w.start + raw.len() - 1)
}

/// The commands of one segment: the whole segment when it is a command calmrun judges
/// (`npx jest src/{a,b} --watch`, `docker compose -f $(pwd)/c.yml up`), else the commands
/// inside its blocks (`if ($?) { npm test }`, `echo $(npm test)`), see [`pieces`].
fn commands(cmd: &str, r: Range<usize>, kinds: &BTreeMap<String, KindConfig>) -> Vec<Range<usize>> {
    let words = crate::shell::tokenize(&cmd[r.clone()]);
    let whole = already_wrapped(&words)
        || !matches!(disposition(&words, true), Disposition::Queue)
        || parse(&words)
            .is_some_and(|p| detect_words(&words, kinds).kind.is_some() || SHELLS.contains(&p.tool.as_str()));
    if whole {
        vec![r]
    } else {
        pieces(cmd, r)
    }
}

/// The commands inside one segment: split again at unquoted braces and parentheses, so a
/// command in a block (`if ($?) { npm test }`, `try { npm test } catch { }`,
/// `$(npm test)`) is found where it starts. `${VAR}` and `@{...}` are not blocks.
fn pieces(cmd: &str, r: Range<usize>) -> Vec<Range<usize>> {
    let b = cmd.as_bytes();
    let mut out = Vec::new();
    let mut start = r.start;
    let mut quote: Option<u8> = None;
    // For each open brace: whether it opened a block (true) or `${`/`@{` (false).
    let mut braces: Vec<bool> = Vec::new();
    let push = |a: usize, z: usize, out: &mut Vec<Range<usize>>| {
        let t = &cmd[a..z];
        let lead = t.len() - t.trim_start().len();
        let t = t.trim();
        if !t.is_empty() {
            out.push(a + lead..a + lead + t.len());
        }
    };
    let mut i = r.start;
    while i < r.end {
        let c = b[i];
        match quote {
            Some(q) if c == q => quote = None,
            Some(b'"') if c == b'\\' => i += 1,
            Some(_) => {}
            None => match c {
                b'\'' | b'"' => quote = Some(c),
                b'{' => {
                    let block = !(i > r.start && matches!(b[i - 1], b'$' | b'@'));
                    braces.push(block);
                    if block {
                        push(start, i, &mut out);
                        start = i + 1;
                    }
                }
                b'}' if braces.pop().unwrap_or(true) => {
                    push(start, i, &mut out);
                    start = i + 1;
                }
                b'(' | b')' => {
                    push(start, i, &mut out);
                    start = i + 1;
                }
                _ => {}
            },
        }
        i += 1;
    }
    push(start, r.end, &mut out);
    out
}

/// `fixed` written as shell text: words it shares with the original `spans` of `seg` keep
/// their exact text (quotes, escapes); new words are quoted when needed.
fn render(seg: &str, spans: &[Word], fixed: &[String]) -> String {
    // The raw text of each original word: up to the next word, without the spaces.
    let raw: Vec<&str> = spans
        .iter()
        .enumerate()
        .map(|(i, w)| {
            let end = spans.get(i + 1).map_or(seg.len(), |n| n.start);
            seg[w.start..end].trim_end()
        })
        .collect();
    // Longest common subsequence of the two word lists (both are short).
    let (n, m) = (spans.len(), fixed.len());
    let mut lcs = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if spans[i].text == fixed[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut out: Vec<String> = Vec::new();
    while j < m {
        if i < n && spans[i].text == fixed[j] {
            out.push(raw[i].to_string());
            i += 1;
            j += 1;
        } else if i < n && lcs[i + 1][j] >= lcs[i][j + 1] {
            i += 1;
        } else {
            out.push(crate::runner::quote_for_display(&fixed[j]));
            j += 1;
        }
    }
    out.join(" ")
}

/// `calmrun` when the agent's shell can find it on PATH, else this binary's absolute path.
pub fn program_name() -> String {
    crate::runner::program_name()
}

/// Claude Code's Bash tool limit when the call sets no `timeout` (2 minutes).
const CLAUDE_DEFAULT_LIMIT_MS: u64 = 120_000;

/// Claude Code: process one hook input. Returns the JSON to print, or `None` to stay silent
/// (allow). `program` is what the suggestion runs: `calmrun`, or its absolute path when
/// `calmrun` is not on PATH (see [`program_name`]).
pub fn handle(input: &str, o: &Opts) -> Option<String> {
    let v: Value = serde_json::from_str(input).ok()?;
    let tool = v.get("tool_name")?.as_str()?;
    let shell = match tool {
        "Bash" => Shell::Posix,
        "PowerShell" => Shell::PowerShell,
        _ => return None,
    };
    let tool_input = v.get("tool_input")?.as_object()?;
    let background = tool_input
        .get("run_in_background")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let call = Call {
        command: tool_input.get("command")?.as_str()?,
        shell,
        limit_ms: Some(
            tool_input
                .get("timeout")
                .and_then(Value::as_u64)
                .unwrap_or(CLAUDE_DEFAULT_LIMIT_MS),
        ),
        background,
    };
    let p = plan(&call, o.kinds, o.program)?;
    let bg = Background::Param("run_in_background: true");
    let out = match o.mode {
        Mode::Deny => json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": deny_reason(&p, bg, o),
            }
        }),
        Mode::Rewrite => {
            let mut new_input = tool_input.clone();
            // A long run goes to the background, where no time limit applies; anything else
            // gives up waiting before the limit, with its place kept.
            let command = if p.long && !background {
                new_input.insert("run_in_background".to_string(), Value::Bool(true));
                p.plain.clone()
            } else {
                p.command().to_string()
            };
            new_input.insert("command".to_string(), Value::String(command));
            json!({
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecisionReason": "calmrun: wrapped heavy command in the machine-wide queue",
                    "updatedInput": Value::Object(new_input),
                }
            })
        }
    };
    Some(out.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(tool: &str, cmd: &str) -> String {
        json!({
            "session_id": "abc",
            "hook_event_name": "PreToolUse",
            "tool_name": tool,
            "tool_input": {"command": cmd, "description": "d", "timeout": 120000},
            "tool_use_id": "toolu_1"
        })
        .to_string()
    }

    fn run(tool: &str, cmd: &str, mode: Mode) -> Option<Value> {
        handle(&input(tool, cmd), &opts(mode, "calmrun")).map(|s| serde_json::from_str(&s).unwrap())
    }

    fn reason(v: &Value) -> &str {
        v["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .unwrap()
    }

    #[test]
    fn rtk_commands_are_wrapped_in_their_rtk_form() {
        for (cmd, wrapped) in [
            ("rtk cargo test", "calmrun -- rtk cargo test"),
            ("rtk vitest", "calmrun -- rtk vitest"),
            ("rtk tsc", "calmrun -- rtk tsc"),
            ("cd web && rtk npm test", "cd web && calmrun -- rtk npm test"),
        ] {
            let v = run("Bash", cmd, Mode::Deny).unwrap_or_else(|| panic!("{cmd} was allowed"));
            let capped = wrapped.replace("calmrun --", "calmrun --wait-timeout 108 --");
            assert!(reason(&v).contains(&capped), "{cmd}: {}", reason(&v));
        }
        assert!(run("Bash", "rtk git status", Mode::Deny).is_none());
        assert!(run("Bash", "calmrun -- rtk cargo test", Mode::Deny).is_none());
    }

    #[test]
    fn denies_heavy_with_suggestion() {
        let v = run("Bash", "npx vitest run", Mode::Deny).unwrap();
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
        let r = reason(&v);
        assert!(
            r.contains("\ncalmrun --wait-timeout 108 -- npx vitest run\nExit 75 means"),
            "{r}"
        );
        // A long kind also gets the background form, without the cap.
        assert!(
            r.ends_with("run_in_background: true, without --wait-timeout:\ncalmrun -- npx vitest run"),
            "{r}"
        );
    }

    fn claude(tool_input: Value, mode: Mode) -> Value {
        let input = json!({"tool_name": "Bash", "tool_input": tool_input}).to_string();
        serde_json::from_str(&handle(&input, &opts(mode, "calmrun")).unwrap()).unwrap()
    }

    #[test]
    fn the_wait_is_capped_below_the_tool_timeout() {
        // No timeout: Claude Code's default of 2 minutes.
        let v = claude(json!({"command": "npx tsc --noEmit"}), Mode::Deny);
        let r = reason(&v);
        assert!(
            r.contains("\ncalmrun --wait-timeout 108 -- npx tsc --noEmit\n"),
            "{r}"
        );
        assert!(!r.contains("run_in_background"), "a short kind: {r}");
        let v = claude(json!({"command": "cargo build", "timeout": 600000}), Mode::Deny);
        assert!(reason(&v).contains("calmrun --wait-timeout 570 -- cargo build"));
        // Already in the background: no limit, no cap, no background advice.
        let v = claude(
            json!({"command": "npx vitest run", "run_in_background": true}),
            Mode::Deny,
        );
        let r = reason(&v);
        assert!(r.ends_with("Run this instead:\ncalmrun -- npx vitest run"), "{r}");
        assert_eq!(wait_secs(30_000), 25);
        assert_eq!(wait_secs(3_000), 1);
    }

    #[test]
    fn rewrite_sends_long_runs_to_the_background_and_caps_short_ones() {
        let v = claude(
            json!({"command": "npx vitest run", "timeout": 60000}),
            Mode::Rewrite,
        );
        let u = &v["hookSpecificOutput"]["updatedInput"];
        assert_eq!(u["command"], "calmrun -- npx vitest run");
        assert_eq!(u["run_in_background"], true);
        assert_eq!(u["timeout"], 60000);
        let v = claude(json!({"command": "npx eslint ."}), Mode::Rewrite);
        let u = &v["hookSpecificOutput"]["updatedInput"];
        assert_eq!(u["command"], "calmrun --wait-timeout 108 -- npx eslint .");
        assert!(u.get("run_in_background").is_none());
    }

    #[test]
    fn capped_suggestions_parse_as_calmrun_commands() {
        use clap::Parser;
        for c in MIXED {
            let call = Call {
                command: c,
                shell: Shell::Posix,
                limit_ms: Some(120_000),
                background: false,
            };
            let Some(p) = plan(&call, &BTreeMap::new(), "calmrun") else {
                continue;
            };
            let words = crate::shell::tokenize(p.command());
            let cli = crate::cli::Cli::try_parse_from(&words).unwrap_or_else(|e| panic!("{c}: {e}"));
            assert_eq!(cli.wait_timeout, Some(108.0), "{c}");
            let n = words.iter().position(|w| w == "--").unwrap();
            assert_eq!(disposition(&words[n + 1..], true), Disposition::Queue, "{c}");
        }
    }

    #[test]
    fn powershell_calls_a_quoted_path_with_the_call_operator() {
        let call = Call {
            command: "npm test",
            shell: Shell::PowerShell,
            limit_ms: None,
            background: false,
        };
        let p = plan(&call, &BTreeMap::new(), r#""C:\My Tools\calmrun.exe""#).unwrap();
        assert_eq!(p.plain, r#"& "C:\My Tools\calmrun.exe" -- npm test"#);
    }

    #[test]
    fn wraps_each_heavy_segment_in_place() {
        let (w, kinds) = wrap_command(
            "cd web && npm ci && npx tsc --noEmit && npx vitest run | tail -5",
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(
            w,
            "cd web && npm ci && calmrun -- npx tsc --noEmit && calmrun -- npx vitest run | tail -5"
        );
        assert_eq!(kinds, vec!["tsc", "vitest"]);
        let (w, _) = wrap_command("CI=1 FOO=\"a b\" npx jest", &BTreeMap::new()).unwrap();
        assert_eq!(w, "CI=1 FOO=\"a b\" calmrun -- npx jest");
        // Words with escapes or escaped quotes keep their exact text.
        let (w, _) = wrap_command(r"FOO=a\ b npx jest", &BTreeMap::new()).unwrap();
        assert_eq!(w, r"FOO=a\ b calmrun -- npx jest");
        let (w, _) = wrap_command(r#"FOO="a\"b c" npx jest"#, &BTreeMap::new()).unwrap();
        assert_eq!(w, r#"FOO="a\"b c" calmrun -- npx jest"#);
        let (w, _) = wrap_command("time npx vitest run", &BTreeMap::new()).unwrap();
        assert_eq!(w, "time calmrun -- npx vitest run");
        let (w, _) = wrap_command("nice -n 5 cargo build", &BTreeMap::new()).unwrap();
        assert_eq!(w, "nice -n 5 calmrun -- cargo build");
        assert!(wrap_command("time calmrun -- npx vitest", &BTreeMap::new()).is_none());
    }

    #[test]
    fn every_ecosystem_uses_the_same_table() {
        for c in [
            "./gradlew assembleDebug",
            "cmake --build build",
            "go test ./...",
            "make -j8",
            "dotnet test",
            "bundle exec rspec",
            "mvn -q package",
            "xcodebuild -scheme App test",
            "turbo run build",
            "buck2 build //...",
            "bazel test //...",
            "meson test -C build",
            "swift test",
            "flutter test",
            "dart run build_runner build",
            "mix test",
            "stack build",
            "cabal test",
            "composer test",
            "php artisan test",
            "nx run-many -t build",
            "moon run :build",
            "podman build .",
        ] {
            assert!(run("Bash", c, Mode::Deny).is_some(), "{c}");
        }
        for c in [
            "terraform plan",
            "pod install",
            "cargo run",
            "go mod tidy",
            "composer install",
            // Dev servers and app runs go outside the queue, so the hook leaves them alone.
            "./gradlew bootRun",
            "pnpm -r install",
            "pnpm install --recursive",
            "mvn spring-boot:run",
            "turbo run dev",
            "pnpm -r dev",
            "make dev",
            "sbt run",
            "xcodebuild -list",
            "cargo build --help",
        ] {
            assert!(run("Bash", c, Mode::Deny).is_none(), "{c}");
        }
    }

    /// Heavy commands, watch modes and servers from every ecosystem.
    const MIXED: &[&str] = &[
        "npx vitest",
        "npx vitest --watch",
        "vitest watch",
        "npx vitest run",
        "docker compose up",
        "docker-compose -f a.yml up --build",
        "podman compose up",
        "docker compose up -d",
        "docker run -it ubuntu",
        "tsc -w",
        "npx jest --watchAll",
        "sbt",
        "sbt shell",
        "sbt console",
        "sbt ~compile",
        "sbt compile",
        "dart run build_runner watch",
        "dart run build_runner build",
        "gradle build --continuous",
        "./gradlew -t test",
        "stack build --file-watch",
        "stack build",
        "nx watch --all -- echo hi",
        "lerna watch -- tsc",
        "parcel watch index.html",
        "mix test.watch",
        "cargo watch -x test",
        "dotnet watch test",
        "dotnet watch",
        "turbo watch build",
        "dune build -w",
        "zig build --watch",
        "ibazel build //...",
        "webpack --watch",
        "vite build --watch",
        "./gradlew bootRun",
        "turbo run dev",
        "cargo build",
        "go test ./...",
        "make",
    ];

    #[test]
    fn every_suggestion_is_a_command_calmrun_accepts() {
        for c in MIXED {
            let Some((w, _)) = wrap_command(c, &BTreeMap::new()) else {
                continue;
            };
            let rest = w
                .strip_prefix("calmrun -- ")
                .unwrap_or_else(|| panic!("{c}: {w}"));
            let words = crate::shell::tokenize(rest);
            for tty in [true, false] {
                assert_eq!(
                    disposition(&words, tty),
                    Disposition::Queue,
                    "{c} -> {w} (stdin tty: {tty})"
                );
            }
        }
    }

    #[test]
    fn commands_inside_blocks_and_wrappers_are_wrapped_in_place() {
        let w = |c: &str| wrap_command(c, &BTreeMap::new()).map(|(w, _)| w);
        for (c, want) in [
            // PowerShell: Claude Code's PowerShell tool chains with `A; if ($?) { B }`.
            (
                r"cd F:\x; if ($?) { npx tsc --noEmit }",
                r"cd F:\x; if ($?) { calmrun -- npx tsc --noEmit }",
            ),
            (
                "try { npm test } catch { }",
                "try { calmrun -- npm test } catch { }",
            ),
            (
                "if ($?) { npm test } else { cargo build }",
                "if ($?) { calmrun -- npm test } else { calmrun -- cargo build }",
            ),
            // sh/bash compound commands.
            (
                "if [ -f package.json ]; then npm test; fi",
                "if [ -f package.json ]; then calmrun -- npm test; fi",
            ),
            (
                "for d in a b; do npm test; done",
                "for d in a b; do calmrun -- npm test; done",
            ),
            (
                "if npm test; then echo ok; fi",
                "if calmrun -- npm test; then echo ok; fi",
            ),
            ("! npm test", "! calmrun -- npm test"),
            ("echo $(npm test)", "echo $(calmrun -- npm test)"),
            ("(cd web && npm test)", "(cd web && calmrun -- npm test)"),
            ("f() { cargo test; }", "f() { calmrun -- cargo test; }"),
            // Wrappers and nested shells.
            ("timeout 600 npm test", "timeout 600 calmrun -- npm test"),
            (
                "timeout -k 5 10m cargo build",
                "timeout -k 5 10m calmrun -- cargo build",
            ),
            (r#"bash -c "npm test""#, r#"bash -c "calmrun -- npm test""#),
            (
                "sh -c 'cd web && npx vitest run'",
                "sh -c 'cd web && calmrun -- npx vitest run'",
            ),
            ("pwsh -Command 'npm test'", "pwsh -Command 'calmrun -- npm test'"),
            // Tools found anew.
            ("npm --prefix web test", "calmrun -- npm --prefix web test"),
            ("npm -w pkg test", "calmrun -- npm -w pkg test"),
            ("pnpm -C web build", "calmrun -- pnpm -C web build"),
            ("node --test test/", "calmrun -- node --test test/"),
            ("node --test --watch", "calmrun -- node --test"),
        ] {
            assert_eq!(w(c).as_deref(), Some(want), "{c}");
        }
        // Every `calmrun -- …` written into a block runs a command calmrun accepts.
        for c in [
            r"cd F:\x; if ($?) { npx vitest }",
            "try { npx jest --watch } catch { }",
            "for d in a b; do npx vitest --watch; done",
            "echo $(docker compose up)",
            "bash -c 'npx vitest'",
        ] {
            let w = w(c).unwrap_or_else(|| panic!("{c}"));
            for part in w.split("calmrun -- ").skip(1) {
                let end = part
                    .find([';', '}', ')', '&', '|', '"', '\''])
                    .unwrap_or(part.len());
                let words = crate::shell::tokenize(&part[..end]);
                assert_eq!(disposition(&words, true), Disposition::Queue, "{c} -> {w}");
            }
        }
        // A whole command calmrun judges is not split at its braces or `$(...)`.
        for (c, want) in [
            ("npx jest src/{a,b} --watch", "calmrun -- npx jest src/{a,b}"),
            ("npx tsc -p $(pwd) --watch", "calmrun -- npx tsc -p $(pwd)"),
            (
                "docker compose -f $(pwd)/c.yml up --build",
                "calmrun -- docker compose -f $(pwd)/c.yml up -d --build",
            ),
            (
                "npm --prefix $(git rev-parse --show-toplevel) test",
                "calmrun -- npm --prefix $(git rev-parse --show-toplevel) test",
            ),
        ] {
            assert_eq!(w(c).as_deref(), Some(want), "{c}");
        }
        for c in [
            "echo ${HOME} && git status",
            "timeout /t 5",
            "timeout 5",
            "bash -c 'git status'",
            "bash -c 'calmrun -- npm test'",
            "if ($?) { calmrun -- npm test }",
            "$x = @{ a = 1 }; $x.a",
            "for d in a b; do echo $d; done",
            "git commit -m \"fix (npm test) flake\"",
            "pnpm -w add left-pad",
        ] {
            assert_eq!(w(c), None, "{c}");
        }
    }

    #[test]
    fn watch_modes_get_their_one_shot_form_or_are_left_alone() {
        let w = |c: &str| wrap_command(c, &BTreeMap::new()).map(|(w, _)| w);
        for (c, want) in [
            ("npx vitest", "calmrun -- npx vitest run"),
            ("npx vitest --watch", "calmrun -- npx vitest run"),
            ("dotnet watch test", "calmrun -- dotnet test"),
            ("gradle build --continuous", "calmrun -- gradle build"),
            ("stack build --file-watch", "calmrun -- stack build"),
            ("turbo watch build", "calmrun -- turbo run build"),
            ("mix test.watch", "calmrun -- mix test"),
            ("parcel watch index.html", "calmrun -- parcel build index.html"),
            ("docker compose up", "calmrun -- docker compose up -d"),
            (
                "CI=1 npx jest --watch -t \"my test\" && echo ok",
                "CI=1 calmrun -- npx jest -t \"my test\" && echo ok",
            ),
        ] {
            assert_eq!(w(c).as_deref(), Some(want), "{c}");
        }
        // No one-shot form: calmrun refuses them too, so the hook stays out of the way.
        for c in [
            "sbt",
            "sbt shell",
            "sbt console",
            "sbt ~compile",
            "dart run build_runner watch",
            "nx watch --all -- echo hi",
            "cargo watch -x test",
            "dotnet watch",
            "docker run -it ubuntu",
            "ibazel build //...",
        ] {
            assert_eq!(w(c), None, "{c}");
        }
    }

    #[test]
    fn powershell_tool_too() {
        let v = run("PowerShell", "npm test; echo $LASTEXITCODE", Mode::Deny).unwrap();
        assert!(reason(&v).contains("calmrun -- npm test; echo $LASTEXITCODE"));
        let here = "git commit -m @'\nfix: don't skip the queue\n\neslint config cleanup\n'@";
        assert!(run("PowerShell", here, Mode::Deny).is_none());
    }

    #[test]
    fn already_wrapped_is_allowed() {
        for c in [
            "calmrun -- npx vitest run",
            "calmrun --cost 3 -- cargo build",
            "npx calmrun -- npx vitest",
            "lane with npx vitest",
            "cd x && calmrun -- npm test",
            // Queue commands whose arguments name heavy tools are left alone too.
            "calmrun bump vitest",
            "calmrun bump \"npx vitest\"",
            "calmrun bump --back \"cargo test\"",
            "calmrun --id q bump --pos 2",
            "calmrun bump --ticket 7 && calmrun status",
            "calmrun bump --dir ../web --back",
            "calmrun --prio low -- cargo test",
        ] {
            assert!(run("Bash", c, Mode::Deny).is_none(), "{c}");
        }
    }

    #[test]
    fn light_commands_allowed() {
        for c in [
            "git commit -m \"fix vitest flake\"",
            "grep -r jest .",
            "npm install eslint",
            "ls -la",
            "cat package.json | grep vitest",
            "git status && git diff",
            "echo 'npx vitest' > notes.txt",
            "cat > run.sh <<'EOF'\nnpm test\nnpx vitest run\nEOF",
            "git commit -m \"$(cat <<'EOF'\nci: run npm test\n\nnpx vitest\nEOF\n)\"",
        ] {
            assert!(run("Bash", c, Mode::Deny).is_none(), "{c}");
        }
    }

    #[test]
    fn fails_open() {
        assert!(handle("not json", &opts(Mode::Deny, "calmrun")).is_none());
        assert!(handle("{}", &opts(Mode::Deny, "calmrun")).is_none());
        assert!(handle(&input("Read", "npx vitest"), &opts(Mode::Deny, "calmrun")).is_none());
        let no_cmd = json!({"tool_name": "Bash", "tool_input": {}}).to_string();
        assert!(handle(&no_cmd, &opts(Mode::Deny, "calmrun")).is_none());
    }

    #[test]
    fn rewrite_keeps_other_fields() {
        let v = run("Bash", "cargo test", Mode::Rewrite).unwrap();
        let out = &v["hookSpecificOutput"];
        assert!(out.get("permissionDecision").is_none());
        assert_eq!(out["updatedInput"]["command"], "calmrun -- cargo test");
        assert_eq!(out["updatedInput"]["description"], "d");
        assert_eq!(out["updatedInput"]["timeout"], 120000);
    }

    #[test]
    fn bash_gets_forward_slashes_in_an_absolute_path() {
        let exe = r"C:\tools\calmrun.exe";
        let bash = handle(&input("Bash", "npx vitest run"), &opts(Mode::Rewrite, exe)).unwrap();
        assert!(bash.contains("C:/tools/calmrun.exe -- npx vitest run"), "{bash}");
        let ps = handle(&input("PowerShell", "npx vitest run"), &opts(Mode::Rewrite, exe)).unwrap();
        let v: Value = serde_json::from_str(&ps).unwrap();
        let cmd = v["hookSpecificOutput"]["updatedInput"]["command"]
            .as_str()
            .unwrap();
        assert!(cmd.starts_with(r"C:\tools\calmrun.exe"), "{cmd}");
    }
}
