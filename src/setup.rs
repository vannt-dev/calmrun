//! `calmrun setup`: add (or remove) calmrun instructions in coding agents' global instruction
//! files, between marker comments, and optionally the Claude Code hook.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Marker before the calmrun block.
pub const START: &str =
    "<!-- calmrun:start (managed by `calmrun setup`; remove with `calmrun setup --remove --write`) -->";
/// Marker after the calmrun block.
pub const END: &str = "<!-- calmrun:end -->";
/// The hook command `--hook` installs.
pub const HOOK_COMMAND: &str = "calmrun hook claude";

/// The instructions inside the block.
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

/// The whole block with its markers.
pub fn block() -> String {
    format!("{START}\n{BLOCK_BODY}\n{END}\n")
}

/// A supported agent: where its global instruction file lives. Adding an agent = one entry.
pub struct AgentSpec {
    pub name: &'static str,
    /// Environment variable that moves the agent's folder, if any.
    pub env_dir: Option<&'static str>,
    /// The folder under the home directory when the variable is not set.
    pub home_dir: &'static [&'static str],
    /// The instructions file inside that folder.
    pub file: &'static str,
    /// Another file in the folder that the agent reads instead, when it exists.
    pub shadowed_by: Option<&'static str>,
}

/// Agents `setup` knows, with paths taken from each agent's documentation.
pub const AGENTS: &[AgentSpec] = &[
    AgentSpec {
        name: "claude",
        env_dir: Some("CLAUDE_CONFIG_DIR"),
        home_dir: &[".claude"],
        file: "CLAUDE.md",
        shadowed_by: None,
    },
    AgentSpec {
        name: "codex",
        env_dir: Some("CODEX_HOME"),
        home_dir: &[".codex"],
        file: "AGENTS.md",
        shadowed_by: Some("AGENTS.override.md"),
    },
    AgentSpec {
        name: "gemini",
        env_dir: None,
        home_dir: &[".gemini"],
        file: "GEMINI.md",
        shadowed_by: None,
    },
    AgentSpec {
        name: "opencode",
        env_dir: None,
        home_dir: &[".config", "opencode"],
        file: "AGENTS.md",
        shadowed_by: None,
    },
];

/// The agent whose settings get the hook.
const HOOK_AGENT: &str = "claude";

/// One agent and where its global instructions live on this machine.
#[derive(Debug, Clone, PartialEq)]
pub struct Agent {
    pub name: &'static str,
    pub file: PathBuf,
    /// The folder that shows the agent is installed.
    pub dir: PathBuf,
    pub shadowed_by: Option<&'static str>,
}

/// Known agents resolved for `home`; `env` looks up variables such as CODEX_HOME.
pub fn agents(home: &Path, env: &dyn Fn(&str) -> Option<String>) -> Vec<Agent> {
    AGENTS
        .iter()
        .map(|spec| {
            let dir = spec
                .env_dir
                .and_then(env)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    spec.home_dir
                        .iter()
                        .fold(home.to_path_buf(), |p, part| p.join(part))
                });
            Agent {
                name: spec.name,
                file: dir.join(spec.file),
                dir,
                shadowed_by: spec.shadowed_by,
            }
        })
        .collect()
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

/// What `setup` was asked to do.
pub struct Options {
    pub write: bool,
    pub remove: bool,
    pub hook: bool,
    pub agents: Option<Vec<String>>,
    pub home: PathBuf,
}

/// Text to print and the exit code.
#[derive(Default)]
struct Report {
    text: String,
    code: i32,
}

impl Report {
    fn line(&mut self, s: String) {
        self.text.push_str(&s);
        self.text.push('\n');
    }

    fn fail(&mut self, s: String) {
        self.line(s);
        self.code = 1;
    }
}

/// Run setup; returns the report to print and the exit code.
pub fn run(opts: &Options, env: &dyn Fn(&str) -> Option<String>) -> (String, i32) {
    let all = agents(&opts.home, env);
    let chosen = match choose(&all, opts.agents.as_deref()) {
        Ok(c) => c,
        Err(msg) => return (msg, 2),
    };
    let mut r = Report::default();
    if chosen.is_empty() {
        r.fail(
            "calmrun: no agent folders found; name them with --agents claude,codex,gemini,opencode".into(),
        );
        return (r.text, r.code);
    }
    for a in &chosen {
        apply_block(&mut r, a, opts);
    }
    if opts.hook {
        if let Some(a) = all.iter().find(|a| a.name == HOOK_AGENT) {
            apply_hook(&mut r, &a.dir.join("settings.json"), opts);
        }
    }
    if !opts.write {
        if !opts.remove {
            r.text += "\nThe block:\n\n";
            r.text += &block();
        }
        r.text += "\nNothing was changed. Run again with --write to apply.\n";
    }
    (r.text, r.code)
}

/// The agents named with `--agents`, or by default those whose folder exists.
fn choose(all: &[Agent], names: Option<&[String]>) -> Result<Vec<Agent>, String> {
    let Some(names) = names else {
        return Ok(all.iter().filter(|a| a.dir.is_dir()).cloned().collect());
    };
    names
        .iter()
        .map(|n| {
            all.iter().find(|a| a.name == n.as_str()).cloned().ok_or_else(|| {
                let known: Vec<&str> = AGENTS.iter().map(|s| s.name).collect();
                format!("calmrun: unknown agent \"{n}\"; known: {}\n", known.join(", "))
            })
        })
        .collect()
}

fn verb(opts: &Options) -> &'static str {
    if opts.write {
        ""
    } else {
        "would "
    }
}

fn write_file(path: &Path, text: &str) -> std::io::Result<()> {
    path.parent().map_or(Ok(()), std::fs::create_dir_all)?;
    std::fs::write(path, text)
}

fn apply_block(r: &mut Report, a: &Agent, opts: &Options) {
    let old = std::fs::read_to_string(&a.file).unwrap_or_default();
    let new = if opts.remove {
        without_block(&old)
    } else {
        with_block(&old)
    };
    let v = verb(opts);
    let action = match () {
        _ if new == old => "unchanged".to_string(),
        _ if opts.remove => format!("{v}remove the calmrun block"),
        _ if find_block(&old).is_some() => format!("{v}update the calmrun block"),
        _ if a.file.exists() => format!("{v}append the calmrun block"),
        _ => format!("{v}create the file with the calmrun block"),
    };
    r.line(format!("{:<9} {}: {action}", a.name, a.file.display()));
    if let Some(shadow) = a
        .shadowed_by
        .map(|f| a.dir.join(f))
        .filter(|p| p.is_file() && !opts.remove)
    {
        r.line(format!(
            "          note: {} exists and {} reads it instead; copy the block there too",
            shadow.display(),
            a.name
        ));
    }
    if opts.write && new != old {
        if let Err(e) = write_file(&a.file, &new) {
            r.fail(format!("calmrun: cannot write {}: {e}", a.file.display()));
        }
    }
}

fn apply_hook(r: &mut Report, path: &Path, opts: &Options) {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let parsed = if text.trim().is_empty() {
        Ok(json!({}))
    } else {
        serde_json::from_str::<Value>(&text)
    };
    let settings = match parsed {
        Ok(v) => v,
        Err(e) => {
            return r.fail(format!(
                "calmrun: {} is not valid JSON ({e}); left it alone",
                path.display()
            ))
        }
    };
    let changed = if opts.remove {
        unmerge_hook(settings)
    } else {
        merge_hook(settings)
    };
    let Some(new) = changed else {
        return r.line(format!("hook      {}: unchanged", path.display()));
    };
    let what = if opts.remove { "remove" } else { "add" };
    r.line(format!(
        "hook      {}: {}{what} the PreToolUse hook (`{HOOK_COMMAND}`, matcher Bash|PowerShell)",
        path.display(),
        verb(opts)
    ));
    if !opts.write {
        return;
    }
    let backup = path.with_extension("json.calmrun.bak");
    let res = (|| -> std::io::Result<bool> {
        let had = path.exists();
        if had {
            std::fs::copy(path, &backup)?;
        }
        let mut s = serde_json::to_string_pretty(&new).map_err(std::io::Error::other)?;
        s.push('\n');
        write_file(path, &s)?;
        Ok(had)
    })();
    match res {
        Ok(true) => r.line(format!("          backup: {}", backup.display())),
        Ok(false) => {}
        Err(e) => r.fail(format!("calmrun: cannot write {}: {e}", path.display())),
    }
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
