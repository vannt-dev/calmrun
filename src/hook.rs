//! `calmrun hook claude`: a Claude Code PreToolUse hook.
//!
//! Contract (https://code.claude.com/docs/en/hooks): the hook reads JSON with `tool_name` and
//! `tool_input` on stdin. To block a call it prints
//! `{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny",
//! "permissionDecisionReason":"..."}}` and exits 0; the reason is shown to Claude.
//! `updatedInput` replaces the whole tool input. Printing nothing leaves the call alone.

use crate::config::KindConfig;
use crate::detect::{detect_words, disposition, parse, tool_name, Disposition};
use crate::shell::split_segments;
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// What the hook does with a heavy command.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    Deny,
    Rewrite,
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
        let d = detect_words(&words, kinds);
        let Some(kind) = d.kind else { continue };
        // Dev servers, app runs and help output run outside the queue anyway.
        if matches!(disposition(&words, false), Disposition::Light(_)) {
            continue;
        }
        let Some(p) = parse(&words) else { continue };
        // Keep leading `VAR=value` assignments in front of calmrun (they reach the child).
        let insert_at = spans.get(p.start).map_or(0, |w| w.start);
        out.push_str(&cmd[last..r.start + insert_at]);
        out.push_str(program);
        out.push_str(" -- ");
        out.push_str(&cmd[r.start + insert_at..r.end]);
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

/// Process one hook input. Returns the JSON to print, or `None` to stay silent (allow).
/// `program` is what the suggestion runs: `calmrun`, or its absolute path when `calmrun` is
/// not on PATH (see [`program_name`]).
pub fn handle(
    input: &str,
    mode: Mode,
    kinds: &BTreeMap<String, KindConfig>,
    program: &str,
) -> Option<String> {
    let v: Value = serde_json::from_str(input).ok()?;
    let tool = v.get("tool_name")?.as_str()?;
    if tool != "Bash" && tool != "PowerShell" {
        return None;
    }
    let tool_input = v.get("tool_input")?.as_object()?;
    let cmd = tool_input.get("command")?.as_str()?;
    // Bash (Git Bash on Windows) eats backslashes in an unquoted path; C:/x/calmrun.exe works.
    let program = if tool == "Bash" {
        program.replace('\\', "/")
    } else {
        program.to_string()
    };
    let (wrapped, found) = wrap_with(cmd, kinds, &program)?;
    let out = match mode {
        Mode::Deny => json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": format!(
                    "Heavy command ({}) must go through the machine-wide calmrun queue so parallel \
                     agents do not freeze this computer. Run this instead:\n{wrapped}\n\
                     If it may take more than a minute or two, run it in the background.",
                    found.join(", ")
                ),
            }
        }),
        Mode::Rewrite => {
            let mut new_input = tool_input.clone();
            new_input.insert("command".to_string(), Value::String(wrapped));
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
        assert!(reason(&v).contains("\ncalmrun -- npx vitest run\n"));
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
