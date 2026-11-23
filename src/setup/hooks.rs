//! `calmrun setup --hook <agents>`: calmrun's hook in each agent's user-level hook file.
//!
//! The files are shared with other tools, so only calmrun's own entry is added, replaced or
//! removed: a hook whose command runs `calmrun … hook <agent>`, Antigravity's entry named
//! `calmrun`, or OpenCode's plugin file that starts with calmrun's marker line.

use super::{read_existing, report_write, verb, write_with_backup, Agent, Options, Report};
use crate::hook::opencode;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

/// Agents with a hook, in the order `all` installs them.
pub const HOOK_AGENTS: &[&str] = &["claude", "codex", "antigravity", "opencode"];

/// The command line an agent runs for its hook.
pub fn command(program: &str, agent: &str) -> String {
    format!("{program} hook {agent}")
}

/// A hook command of calmrun's for `agent`: `calmrun hook codex`, `C:/x/calmrun.exe hook codex`.
fn is_ours(cmd: &str, agent: &str) -> bool {
    cmd.contains("calmrun") && cmd.trim_end().ends_with(&format!("hook {agent}"))
}

fn hook_is_ours(h: &Value, agent: &str) -> bool {
    h["command"].as_str().is_some_and(|c| is_ours(c, agent))
}

/// `doc` with calmrun's hook in `doc.hooks.PreToolUse` (the Claude Code and Codex layout): an
/// existing entry of calmrun's is replaced in place, else `group` is added. `None` when the
/// file already says exactly that.
pub fn merge_pre_tool_use(doc: &Value, agent: &str, group: Value) -> Option<Value> {
    let mut out = if doc.is_object() { doc.clone() } else { json!({}) };
    let hooks = out.as_object_mut()?.entry("hooks").or_insert_with(|| json!({}));
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
    let groups = pre.as_array_mut()?;
    let ours = group["hooks"][0].clone();
    let mut placed = false;
    for g in groups.iter_mut() {
        let Some(hs) = g["hooks"].as_array_mut() else {
            continue;
        };
        let alone = hs.len() == 1;
        let mut hit = false;
        hs.retain_mut(|h| {
            if !hook_is_ours(h, agent) {
                return true;
            }
            hit = true;
            if placed {
                return false;
            }
            placed = true;
            *h = ours.clone();
            true
        });
        // A group that held only calmrun's hook gets calmrun's matcher too.
        if hit && alone {
            g["matcher"] = group["matcher"].clone();
        }
    }
    groups.retain(|g| g["hooks"].as_array().is_none_or(|hs| !hs.is_empty()));
    if !placed {
        groups.push(group);
    }
    (out != *doc).then_some(out)
}

/// `doc` without calmrun's PreToolUse hook. `None` when there was none.
pub fn unmerge_pre_tool_use(doc: &Value, agent: &str) -> Option<Value> {
    let mut out = doc.clone();
    let pre = out["hooks"]["PreToolUse"].as_array_mut()?;
    for g in pre.iter_mut() {
        if let Some(hs) = g["hooks"].as_array_mut() {
            hs.retain(|h| !hook_is_ours(h, agent));
        }
    }
    pre.retain(|g| g["hooks"].as_array().is_none_or(|hs| !hs.is_empty()));
    if pre.is_empty() {
        if let Some(h) = out["hooks"].as_object_mut() {
            h.remove("PreToolUse");
        }
    }
    (out != *doc).then_some(out)
}

/// Antigravity's entry name in its hooks.json.
pub const ANTIGRAVITY_ENTRY: &str = "calmrun";

/// `doc` with calmrun's named Antigravity entry set; `None` when unchanged.
pub fn merge_antigravity(doc: &Value, program: &str) -> Option<Value> {
    let entry = json!({
        "PreToolUse": [{
            "matcher": "run_command",
            "hooks": [{"type": "command", "command": command(program, "antigravity"), "timeout": 30}]
        }]
    });
    let mut out = match doc {
        Value::Object(m) => m.clone(),
        _ => Map::new(),
    };
    out.insert(ANTIGRAVITY_ENTRY.to_string(), entry);
    let out = Value::Object(out);
    (out != *doc).then_some(out)
}

/// `doc` without calmrun's Antigravity entry; `None` when there was none.
pub fn unmerge_antigravity(doc: &Value) -> Option<Value> {
    let mut out = doc.as_object()?.clone();
    out.remove(ANTIGRAVITY_ENTRY)?;
    Some(Value::Object(out))
}

/// The group calmrun adds for Claude Code or Codex.
fn group(program: &str, agent: &str) -> Value {
    match agent {
        "claude" => json!({
            "matcher": "Bash|PowerShell",
            "hooks": [{"type": "command", "command": command(program, agent)}]
        }),
        _ => json!({
            "matcher": "Bash",
            "hooks": [{"type": "command", "command": command(program, agent), "timeout": 30}]
        }),
    }
}

/// Where one agent's hook lives, and the folder that shows the agent is installed.
struct Place {
    agent: &'static str,
    dir: PathBuf,
    file: PathBuf,
}

fn place(agent: &'static str, home: &Path, all: &[Agent]) -> Place {
    let dir_of = |name: &str| {
        all.iter()
            .find(|a| a.name == name)
            .map_or_else(|| home.to_path_buf(), |a| a.dir.clone())
    };
    let (dir, file): (PathBuf, &[&str]) = match agent {
        "claude" => (dir_of("claude"), &["settings.json"]),
        "codex" => (dir_of("codex"), &["hooks.json"]),
        "antigravity" => (home.join(".gemini"), &["config", "hooks.json"]),
        _ => (dir_of("opencode"), &["plugins", "calmrun.js"]),
    };
    let file = file.iter().fold(dir.clone(), |p, part| p.join(part));
    Place { agent, dir, file }
}

/// The agents `--hook` names (`all` = every one), or an error for an unknown name.
pub fn chosen(names: &[String]) -> Result<Vec<&'static str>, String> {
    let mut out = Vec::new();
    for n in names {
        let found: Vec<&'static str> = match n.trim() {
            "all" => HOOK_AGENTS.to_vec(),
            n => match HOOK_AGENTS.iter().find(|a| **a == n) {
                Some(a) => vec![*a],
                None => {
                    return Err(format!(
                        "calmrun: no hook for \"{n}\"; known: {}, all\n",
                        HOOK_AGENTS.join(", ")
                    ))
                }
            },
        };
        for a in found {
            if !out.contains(&a) {
                out.push(a);
            }
        }
    }
    Ok(out)
}

/// Install or remove the hooks of `agents`. An agent whose folder is missing gets no hook:
/// an error when it was named, a note under `--hook all`.
pub(super) fn apply(r: &mut Report, opts: &Options, all: &[Agent], agents: &[&'static str]) {
    let every = opts.hook.iter().any(|n| n.trim() == "all");
    for &agent in agents {
        let p = place(agent, &opts.home, all);
        if !p.dir.is_dir() && !opts.remove {
            let msg = format!(
                "hook      {agent}: {} not found; is {agent} installed? (no hook added)",
                p.dir.display()
            );
            if every {
                r.line(msg);
            } else {
                r.fail(msg);
            }
            continue;
        }
        match agent {
            "opencode" => apply_plugin(r, &p, opts),
            _ => apply_json(r, &p, opts),
        }
    }
}

/// The JSON in `path`: `{}` when the file is missing or empty, an error when it is not JSON.
fn read_json(path: &Path) -> Result<Value, String> {
    let text = read_existing(path)?;
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&text).map_err(|e| {
        format!(
            "calmrun: {} is not valid JSON ({e}); left it alone",
            path.display()
        )
    })
}

fn apply_json(r: &mut Report, p: &Place, opts: &Options) {
    let doc = match read_json(&p.file) {
        Ok(v) => v,
        Err(msg) => return r.fail(msg),
    };
    let changed = match (p.agent, opts.remove) {
        ("antigravity", false) => merge_antigravity(&doc, &opts.program),
        ("antigravity", true) => unmerge_antigravity(&doc),
        (a, false) => merge_pre_tool_use(&doc, a, group(&opts.program, a)),
        (a, true) => unmerge_pre_tool_use(&doc, a),
    };
    let Some(new) = changed else {
        return r.line(format!("hook      {}: unchanged", p.file.display()));
    };
    let what = if opts.remove { "remove" } else { "add" };
    r.line(format!(
        "hook      {}: {}{what} the {} PreToolUse hook (`{}`)",
        p.file.display(),
        verb(opts),
        p.agent,
        command(&opts.program, p.agent)
    ));
    if p.agent == "codex" && !opts.remove {
        r.line(
            "          note: Codex runs a new or changed hook only after you trust it: \
             review it with /hooks in Codex"
                .into(),
        );
        r.line(
            "          note: Codex's workspace-write sandbox must let calmrun write its state \
             directory: add it (`calmrun doctor` shows it) to \
             sandbox_workspace_write.writable_roots in config.toml"
                .into(),
        );
    }
    if !opts.write {
        return;
    }
    let mut s = match serde_json::to_string_pretty(&new) {
        Ok(s) => s,
        Err(e) => return r.fail(format!("calmrun: cannot write {}: {e}", p.file.display())),
    };
    s.push('\n');
    report_write(r, &p.file, write_with_backup(&p.file, &s));
}

fn apply_plugin(r: &mut Report, p: &Place, opts: &Options) {
    let old = match read_existing(&p.file) {
        Ok(t) => t,
        Err(msg) => return r.fail(msg),
    };
    if !old.is_empty() && !old.starts_with(opencode::MARKER) {
        return r.fail(format!(
            "calmrun: {} was not written by calmrun; left it alone",
            p.file.display()
        ));
    }
    let new = if opts.remove {
        String::new()
    } else {
        opencode::plugin(&opts.program)
    };
    if new == old {
        return r.line(format!("hook      {}: unchanged", p.file.display()));
    }
    let what = if opts.remove { "remove" } else { "write" };
    r.line(format!(
        "hook      {}: {}{what} the OpenCode plugin (`{}`)",
        p.file.display(),
        verb(opts),
        command(&opts.program, "opencode")
    ));
    if !opts.write {
        return;
    }
    let res = if opts.remove {
        std::fs::remove_file(&p.file).map(|_| None)
    } else {
        // calmrun's own file: an older copy needs no backup.
        p.file
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|_| std::fs::write(&p.file, &new))
            .map(|_| None)
    };
    report_write(r, &p.file, res);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(doc: &Value, i: usize) -> &str {
        doc["hooks"]["PreToolUse"][i]["hooks"][0]["command"]
            .as_str()
            .unwrap()
    }

    #[test]
    fn pre_tool_use_keeps_foreign_entries_and_is_idempotent() {
        // What aiws writes into .codex/hooks.json, plus other settings.
        let aiws = json!({"matcher": "", "hooks": [{"type": "command",
            "command": "node aiws.js guard --from codex", "timeout": 30}]});
        let doc = json!({"x": 1, "hooks": {"PreToolUse": [aiws.clone()], "Stop": []}});
        let merged = merge_pre_tool_use(&doc, "codex", group("calmrun", "codex")).unwrap();
        assert_eq!(merged["x"], 1);
        assert_eq!(merged["hooks"]["Stop"], json!([]));
        assert_eq!(merged["hooks"]["PreToolUse"][0], aiws);
        assert_eq!(cmd(&merged, 1), "calmrun hook codex");
        assert_eq!(merged["hooks"]["PreToolUse"][1]["hooks"][0]["timeout"], 30);
        assert!(merge_pre_tool_use(&merged, "codex", group("calmrun", "codex")).is_none());
        // A new path replaces calmrun's entry in place, without touching the others.
        let moved = merge_pre_tool_use(&merged, "codex", group("C:/x/calmrun.exe", "codex")).unwrap();
        assert_eq!(moved["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);
        assert_eq!(cmd(&moved, 1), "C:/x/calmrun.exe hook codex");
        // Removal takes out calmrun's entry only; the Claude hook is not the Codex hook.
        assert!(unmerge_pre_tool_use(&moved, "claude").is_none());
        assert_eq!(unmerge_pre_tool_use(&moved, "codex").unwrap(), doc);
        assert!(unmerge_pre_tool_use(&doc, "codex").is_none());
    }

    #[test]
    fn claude_settings_as_before() {
        let s = json!({"model": "x", "hooks": {"PreToolUse": [{"matcher": "Edit",
            "hooks": [{"type": "command", "command": "fmt"}]}]}});
        let merged = merge_pre_tool_use(&s, "claude", group("calmrun", "claude")).unwrap();
        assert_eq!(merged["model"], "x");
        assert_eq!(merged["hooks"]["PreToolUse"][1]["matcher"], "Bash|PowerShell");
        assert!(merged["hooks"]["PreToolUse"][1]["hooks"][0]
            .get("timeout")
            .is_none());
        assert_eq!(unmerge_pre_tool_use(&merged, "claude").unwrap(), s);
        // The entry calmrun 0.2 wrote is the same: nothing changes.
        let old = json!({"hooks": {"PreToolUse": [{"matcher": "Bash|PowerShell",
            "hooks": [{"type": "command", "command": "calmrun hook claude"}]}]}});
        assert!(merge_pre_tool_use(&old, "claude", group("calmrun", "claude")).is_none());
        let fresh = merge_pre_tool_use(&json!({}), "claude", group("calmrun", "claude")).unwrap();
        assert_eq!(
            unmerge_pre_tool_use(&fresh, "claude").unwrap(),
            json!({"hooks": {}})
        );
    }

    #[test]
    fn antigravity_owns_one_named_entry() {
        let aiws = json!({"PreToolUse": [{"matcher": ".*", "hooks": [{"type": "command",
            "command": "aiws guard --from antigravity", "timeout": 30}]}]});
        let doc = json!({"aiws-guard": aiws});
        let merged = merge_antigravity(&doc, "calmrun").unwrap();
        assert_eq!(merged["aiws-guard"], aiws);
        let ours = &merged["calmrun"]["PreToolUse"][0];
        assert_eq!(ours["matcher"], "run_command");
        assert_eq!(ours["hooks"][0]["command"], "calmrun hook antigravity");
        assert!(merge_antigravity(&merged, "calmrun").is_none());
        assert_eq!(unmerge_antigravity(&merged).unwrap(), doc);
        assert!(unmerge_antigravity(&doc).is_none());
    }

    #[test]
    fn hook_names_are_checked() {
        assert_eq!(
            chosen(&["codex".into(), "claude".into()]).unwrap(),
            ["codex", "claude"]
        );
        assert_eq!(chosen(&["all".into(), "codex".into()]).unwrap(), HOOK_AGENTS);
        assert!(chosen(&["gemini".into()]).unwrap_err().contains("known: claude"));
    }

    #[test]
    fn own_entries_are_told_apart() {
        assert!(is_ours("calmrun hook claude", "claude"));
        assert!(is_ours(
            "\"C:/Program Files/calmrun/calmrun.exe\" hook codex",
            "codex"
        ));
        assert!(!is_ours("calmrun hook claude", "codex"));
        assert!(!is_ours("node aiws.js guard --from codex", "codex"));
    }
}
