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

/// Process one hook input of `agent`: the text to print, or `None` to stay silent (allow).
pub fn run(
    agent: Agent,
    input: &str,
    mode: Mode,
    kinds: &BTreeMap<String, KindConfig>,
    program: &str,
) -> Option<String> {
    match agent {
        Agent::Claude => handle(input, mode, kinds, program),
        Agent::Codex => codex::handle(input, mode, kinds, program),
        Agent::Antigravity => antigravity::handle(input, mode, kinds, program),
        Agent::OpenCode => opencode::handle(input, mode, kinds, program),
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

/// The deny reason every agent shows its model: the command to run instead, and what to do
/// about long waits and long runs.
pub fn deny_reason(p: &Plan, bg: Background) -> String {
    let mut s = format!(
        "Heavy command ({}) must go through the machine-wide calmrun queue so parallel agents \
         do not freeze this computer. Run this instead:\n{}",
        p.kinds.join(", "),
        p.command()
    );
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
    for r in split_segments(cmd) {
        let seg = &cmd[r.clone()];
        let spans = crate::shell::words(seg);
        let words: Vec<String> = spans.iter().map(|w| w.text.clone()).collect();
        if already_wrapped(&words) {
            continue;
        }
        let Some(p) = parse(&words) else { continue };
        // Keep leading `VAR=value` assignments in front of calmrun (they reach the child).
        let insert_at = spans.get(p.start).map_or(0, |w| w.start);
        // The runner's own verdict, as if stdin were a terminal (the strictest case: plain
        // vitest watches then), so the suggestion is always a command calmrun accepts.
        let (kind, text) = match disposition(&words, true) {
            // Dev servers, app runs and help output run outside the queue anyway.
            Disposition::Light(_) => continue,
            Disposition::Queue => match detect_words(&words, kinds).kind {
                Some(k) => (k, seg[insert_at..].to_string()),
                None => continue,
            },
            // A watch mode with a one-shot form: suggest that form, if it is heavy.
            Disposition::Refuse { fixed: Some(f), .. } => {
                let Some(f) = accepted(f) else { continue };
                let Some(k) = detect_words(&f, kinds).kind else {
                    continue;
                };
                let body = seg.trim_end();
                let text = render(body, &spans[p.start..], &f[p.start..]);
                (k, format!("{text}{}", &seg[body.len()..]))
            }
            // No one-shot form: calmrun would refuse it too, so leave it alone.
            Disposition::Refuse { fixed: None, .. } => continue,
        };
        out.push_str(&cmd[last..r.start + insert_at]);
        out.push_str(program);
        out.push_str(" -- ");
        out.push_str(&text);
        last = r.end;
        if !found.contains(&kind) {
            found.push(kind);
        }
    }
    if found.is_empty() {
        return None;
    }
    out.push_str(&cmd[last..]);
    Some((out, found))
}

/// The one-shot form, fixed again while calmrun would still refuse it (`npx vitest --watch`
/// -> `npx vitest` -> `npx vitest run`); `None` when no accepted form is found.
fn accepted(mut words: Vec<String>) -> Option<Vec<String>> {
    for _ in 0..3 {
        match disposition(&words, true) {
            Disposition::Queue => return Some(words),
            Disposition::Refuse { fixed: Some(f), .. } => words = f,
            _ => return None,
        }
    }
    None
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

/// `calmrun` when the agent's shell can find it on PATH, else this binary's absolute path,
/// so a suggested command never fails with "command not found".
pub fn program_name() -> String {
    if crate::runner::on_path("calmrun") {
        return "calmrun".to_string();
    }
    std::env::current_exe()
        .map(|p| crate::runner::quote_for_display(&p.to_string_lossy()))
        .unwrap_or_else(|_| "calmrun".to_string())
}

/// Claude Code's Bash tool limit when the call sets no `timeout` (2 minutes).
const CLAUDE_DEFAULT_LIMIT_MS: u64 = 120_000;

/// Claude Code: process one hook input. Returns the JSON to print, or `None` to stay silent
/// (allow). `program` is what the suggestion runs: `calmrun`, or its absolute path when
/// `calmrun` is not on PATH (see [`program_name`]).
pub fn handle(
    input: &str,
    mode: Mode,
    kinds: &BTreeMap<String, KindConfig>,
    program: &str,
) -> Option<String> {
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
    let p = plan(&call, kinds, program)?;
    let out = match mode {
        Mode::Deny => json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": deny_reason(&p, Background::Param("run_in_background: true")),
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
        handle(&input(tool, cmd), mode, &BTreeMap::new(), "calmrun")
            .map(|s| serde_json::from_str(&s).unwrap())
    }

    fn reason(v: &Value) -> &str {
        v["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .unwrap()
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
        serde_json::from_str(&handle(&input, mode, &BTreeMap::new(), "calmrun").unwrap()).unwrap()
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
        let kinds = BTreeMap::new();
        assert!(handle("not json", Mode::Deny, &kinds, "calmrun").is_none());
        assert!(handle("{}", Mode::Deny, &kinds, "calmrun").is_none());
        assert!(handle(&input("Read", "npx vitest"), Mode::Deny, &kinds, "calmrun").is_none());
        let no_cmd = json!({"tool_name": "Bash", "tool_input": {}}).to_string();
        assert!(handle(&no_cmd, Mode::Deny, &kinds, "calmrun").is_none());
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
        let kinds = BTreeMap::new();
        let exe = r"C:\tools\calmrun.exe";
        let bash = handle(&input("Bash", "npx vitest run"), Mode::Rewrite, &kinds, exe).unwrap();
        assert!(bash.contains("C:/tools/calmrun.exe -- npx vitest run"), "{bash}");
        let ps = handle(&input("PowerShell", "npx vitest run"), Mode::Rewrite, &kinds, exe).unwrap();
        let v: Value = serde_json::from_str(&ps).unwrap();
        let cmd = v["hookSpecificOutput"]["updatedInput"]["command"]
            .as_str()
            .unwrap();
        assert!(cmd.starts_with(r"C:\tools\calmrun.exe"), "{cmd}");
    }
}
