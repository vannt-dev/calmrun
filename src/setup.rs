//! `calmrun setup`: add (or remove) calmrun instructions in coding agents' global instruction
//! files, between marker comments, and optionally the Claude Code hook.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub const START: &str =
    "<!-- calmrun:start (managed by `calmrun setup`; remove with `calmrun setup --remove --write`) -->";
pub const END: &str = "<!-- calmrun:end -->";
pub const HOOK_COMMAND: &str = "calmrun hook claude";

pub const BLOCK_BODY: &str = "## Heavy commands (calmrun)

This computer is shared by several agents. Run heavy commands through the calmrun queue so
they take turns instead of freezing the machine: tests, type checks, linters, builds,
Playwright, docker compose, pytest, cargo/gradle builds.

- Wrap them: `calmrun -- npx vitest run`, `calmrun -- cargo test`.
- Run anything that may take more than a minute or two in the background and check back.
- To cap the wait, add `--wait-timeout 120`; on exit 75, run the `calmrun: next:` command it
  prints to keep your place in line.
- `calmrun status` shows what runs and waits. Do not wrap light commands (git, ls, cat,
  package installs).";

pub fn block() -> String {
    format!("{START}\n{BLOCK_BODY}\n{END}\n")
}

/// One agent and where its global instructions live.
#[derive(Debug, Clone, PartialEq)]
pub struct Agent {
    pub name: &'static str,
    pub file: PathBuf,
    /// The folder that shows the agent is installed.
    pub dir: PathBuf,
}

pub const AGENT_NAMES: &[&str] = &["claude", "codex", "gemini", "opencode"];

/// Known agents. `home` is the user's home; `env` looks up variables such as CODEX_HOME.
pub fn agents(home: &Path, env: &dyn Fn(&str) -> Option<String>) -> Vec<Agent> {
    let dir_or = |var: &str, default: PathBuf| {
        env(var)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or(default)
    };
    let claude = dir_or("CLAUDE_CONFIG_DIR", home.join(".claude"));
    let codex = dir_or("CODEX_HOME", home.join(".codex"));
    let gemini = home.join(".gemini");
    let opencode = home.join(".config").join("opencode");
    vec![
        Agent {
            name: "claude",
            file: claude.join("CLAUDE.md"),
            dir: claude,
        },
        Agent {
            name: "codex",
            file: codex.join("AGENTS.md"),
            dir: codex,
        },
        Agent {
            name: "gemini",
            file: gemini.join("GEMINI.md"),
            dir: gemini,
        },
        Agent {
            name: "opencode",
            file: opencode.join("AGENTS.md"),
            dir: opencode,
        },
    ]
}

fn find_block(text: &str) -> Option<(usize, usize)> {
    let a = text.find("<!-- calmrun:start")?;
    let rel = text[a..].find(END)?;
    let mut b = a + rel + END.len();
    if text[b..].starts_with("\r\n") {
        b += 2;
    } else if text[b..].starts_with('\n') {
        b += 1;
    }
    Some((a, b))
}

/// The text with the calmrun block added or replaced.
pub fn with_block(text: &str) -> String {
    let blk = block();
    match find_block(text) {
        Some((a, b)) => format!("{}{blk}{}", &text[..a], &text[b..]),
        None if text.trim().is_empty() => blk,
        None => {
            let mut out = text.trim_end_matches(['\n', '\r']).to_string();
            out.push_str("\n\n");
            out.push_str(&blk);
            out
        }
    }
}

/// The text with the calmrun block removed (and the blank line before it).
pub fn without_block(text: &str) -> String {
    match find_block(text) {
        Some((a, b)) => {
            let head = text[..a].trim_end_matches(['\n', '\r']);
            let tail = &text[b..];
            if head.is_empty() {
                tail.trim_start_matches(['\n', '\r']).to_string()
            } else if tail.trim().is_empty() {
                format!("{head}\n")
            } else {
                format!("{head}\n\n{}", tail.trim_start_matches(['\n', '\r']))
            }
        }
        None => text.to_string(),
    }
}

fn has_hook(settings: &Value) -> bool {
    settings["hooks"]["PreToolUse"].as_array().is_some_and(|groups| {
        groups.iter().any(|g| {
            g["hooks"].as_array().is_some_and(|hs| {
                hs.iter()
                    .any(|h| h["command"].as_str().is_some_and(|c| c.contains(HOOK_COMMAND)))
            })
        })
    })
}

/// Settings with the PreToolUse hook added. `None` when it is already there.
pub fn merge_hook(mut settings: Value) -> Option<Value> {
    if has_hook(&settings) {
        return None;
    }
    if !settings.is_object() {
        settings = json!({});
    }
    let hooks = settings
        .as_object_mut()?
        .entry("hooks")
        .or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }
    let pre = hooks
        .as_object_mut()?
        .entry("PreToolUse")
        .or_insert_with(|| json!([]));
    if !pre.is_array() {
        *pre = json!([]);
    }
    pre.as_array_mut()?.push(json!({
        "matcher": "Bash|PowerShell",
        "hooks": [{"type": "command", "command": HOOK_COMMAND}]
    }));
    Some(settings)
}

/// Settings with calmrun's hook removed. `None` when there was none.
pub fn unmerge_hook(mut settings: Value) -> Option<Value> {
    if !has_hook(&settings) {
        return None;
    }
    let pre = settings["hooks"]["PreToolUse"].as_array_mut()?;
    for g in pre.iter_mut() {
        if let Some(hs) = g["hooks"].as_array_mut() {
            hs.retain(|h| !h["command"].as_str().is_some_and(|c| c.contains(HOOK_COMMAND)));
        }
    }
    pre.retain(|g| g["hooks"].as_array().is_none_or(|hs| !hs.is_empty()));
    if pre.is_empty() {
        if let Some(h) = settings["hooks"].as_object_mut() {
            h.remove("PreToolUse");
        }
    }
    Some(settings)
}

pub struct Options {
    pub write: bool,
    pub remove: bool,
    pub hook: bool,
    pub agents: Option<Vec<String>>,
    pub home: PathBuf,
}

/// Run setup; returns the report to print and the exit code.
pub fn run(opts: &Options, env: &dyn Fn(&str) -> Option<String>) -> (String, i32) {
    let mut out = String::new();
    let all = agents(&opts.home, env);
    let chosen: Vec<Agent> = match &opts.agents {
        Some(names) => {
            let mut v = Vec::new();
            for n in names {
                match all.iter().find(|a| a.name == n.as_str()) {
                    Some(a) => v.push(a.clone()),
                    None => {
                        return (
                            format!(
                                "calmrun: unknown agent \"{n}\"; known: {}\n",
                                AGENT_NAMES.join(", ")
                            ),
                            2,
                        )
                    }
                }
            }
            v
        }
        // By default only agents that look installed (their folder exists).
        None => all.iter().filter(|a| a.dir.is_dir()).cloned().collect(),
    };
    if chosen.is_empty() {
        out += "calmrun: no agent folders found; name them with --agents claude,codex,gemini,opencode\n";
        return (out, 1);
    }
    let verb = if opts.write { "" } else { "would " };
    let mut code = 0;
    for a in &chosen {
        let old = std::fs::read_to_string(&a.file).unwrap_or_default();
        let new = if opts.remove {
            without_block(&old)
        } else {
            with_block(&old)
        };
        let action = if new == old {
            "unchanged".to_string()
        } else if opts.remove {
            format!("{verb}remove the calmrun block")
        } else if find_block(&old).is_some() {
            format!("{verb}update the calmrun block")
        } else if a.file.exists() {
            format!("{verb}append the calmrun block")
        } else {
            format!("{verb}create the file with the calmrun block")
        };
        out += &format!("{:<9} {}: {action}\n", a.name, a.file.display());
        if opts.write && new != old {
            let res = a
                .file
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|_| std::fs::write(&a.file, &new));
            if let Err(e) = res {
                out += &format!("calmrun: cannot write {}: {e}\n", a.file.display());
                code = 1;
            }
        }
    }
    if opts.hook {
        let claude = all.iter().find(|a| a.name == "claude").expect("claude is known");
        let path = claude.dir.join("settings.json");
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let parsed: Result<Value, _> = if text.trim().is_empty() {
            Ok(json!({}))
        } else {
            serde_json::from_str(&text)
        };
        match parsed {
            Err(e) => {
                out += &format!(
                    "calmrun: {} is not valid JSON ({e}); left it alone\n",
                    path.display()
                );
                code = 1;
            }
            Ok(v) => {
                let changed = if opts.remove {
                    unmerge_hook(v)
                } else {
                    merge_hook(v)
                };
                match changed {
                    None => out += &format!("hook      {}: unchanged\n", path.display()),
                    Some(nv) => {
                        let what = if opts.remove { "remove" } else { "add" };
                        out += &format!(
                            "hook      {}: {verb}{what} the PreToolUse hook (`{HOOK_COMMAND}`, matcher Bash|PowerShell)\n",
                            path.display()
                        );
                        if opts.write {
                            let backup = path.with_extension("json.calmrun.bak");
                            let res = (|| -> std::io::Result<()> {
                                if path.exists() {
                                    std::fs::copy(&path, &backup)?;
                                }
                                if let Some(p) = path.parent() {
                                    std::fs::create_dir_all(p)?;
                                }
                                let mut s =
                                    serde_json::to_string_pretty(&nv).map_err(std::io::Error::other)?;
                                s.push('\n');
                                std::fs::write(&path, s)
                            })();
                            match res {
                                Ok(()) if path.exists() && backup.exists() => {
                                    out += &format!("          backup: {}\n", backup.display())
                                }
                                Ok(()) => {}
                                Err(e) => {
                                    out += &format!("calmrun: cannot write {}: {e}\n", path.display());
                                    code = 1;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    if !opts.write {
        if !opts.remove {
            out += "\nThe block:\n\n";
            out += &block();
        }
        out += "\nNothing was changed. Run again with --write to apply.\n";
    }
    (out, code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_is_added_replaced_and_removed() {
        let once = with_block("# My rules\n\nBe nice.\n");
        assert!(once.starts_with("# My rules\n\nBe nice.\n\n<!-- calmrun:start"));
        assert!(once.ends_with("<!-- calmrun:end -->\n"));
        assert_eq!(with_block(&once), once, "re-running must not duplicate");
        let stale = once.replace("Wrap them", "OLD TEXT");
        assert_eq!(with_block(&stale), once, "an old block is replaced");
        assert_eq!(without_block(&once), "# My rules\n\nBe nice.\n");
        assert_eq!(with_block(""), block());
        assert_eq!(without_block(&block()), "");
        let middle = format!("a\n\n{}\nb\n", block());
        assert_eq!(without_block(&middle), "a\n\nb\n");
    }

    #[test]
    fn hook_merge_keeps_other_settings() {
        let s = json!({"model": "x", "hooks": {"PreToolUse": [{"matcher": "Edit", "hooks": [{"type": "command", "command": "fmt"}]}]}});
        let merged = merge_hook(s.clone()).unwrap();
        assert_eq!(merged["model"], "x");
        assert_eq!(merged["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);
        assert!(merge_hook(merged.clone()).is_none(), "already there");
        let back = unmerge_hook(merged).unwrap();
        assert_eq!(back, s);
        assert!(unmerge_hook(s).is_none());
        let fresh = merge_hook(json!({})).unwrap();
        assert_eq!(fresh["hooks"]["PreToolUse"][0]["matcher"], "Bash|PowerShell");
        assert_eq!(unmerge_hook(fresh).unwrap(), json!({"hooks": {}}));
    }

    #[test]
    fn agent_paths_follow_env() {
        let home = Path::new("/h");
        let env = |k: &str| (k == "CODEX_HOME").then(|| "/c".to_string());
        let a = agents(home, &env);
        assert_eq!(a[0].file, Path::new("/h/.claude/CLAUDE.md"));
        assert_eq!(a[1].file, Path::new("/c/AGENTS.md"));
        assert_eq!(a[2].file, Path::new("/h/.gemini/GEMINI.md"));
        assert_eq!(a[3].file, Path::new("/h/.config/opencode/AGENTS.md"));
    }
}
