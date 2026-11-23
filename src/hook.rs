//! `calmrun hook claude`: a Claude Code PreToolUse hook.
//!
//! Contract (https://code.claude.com/docs/en/hooks): the hook reads JSON with `tool_name` and
//! `tool_input` on stdin. To block a call it prints
//! `{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny",
//! "permissionDecisionReason":"..."}}` and exits 0; the reason is shown to Claude.
//! `updatedInput` replaces the whole tool input. Printing nothing leaves the call alone.

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
