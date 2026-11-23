//! `calmrun hook codex`: a Codex CLI PreToolUse hook.
//!
//! Contract (https://developers.openai.com/codex/hooks): Codex sends JSON with `tool_name`
//! (`Bash` for shell commands, `apply_patch` for edits, MCP tool names), `tool_input` and `cwd`
//! on stdin. Only `Bash` is looked at; `tool_input.command` is a string (an argv array is
//! accepted too). A refusal is `hookSpecificOutput.permissionDecision: "deny"` with a reason,
//! exit 0. A changed command is `permissionDecision: "allow"` with `updatedInput`, the whole
//! tool input with a string `command`. Printing nothing leaves the call alone.

use super::{deny_reason, plan, Background, Call, Mode, Shell};
use crate::config::KindConfig;
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// The shell command of a Codex `Bash` call: a string, or an argv array (`["bash", "-lc",
/// "npm test"]` gives the script, anything else its words joined).
fn command_of(v: &Value) -> Option<String> {
    if let Some(s) = v.as_str() {
        return Some(s.to_string());
    }
    let words: Vec<&str> = v.as_array()?.iter().map(Value::as_str).collect::<Option<_>>()?;
    if let [_, flag, script] = words.as_slice() {
        let flag = flag.to_ascii_lowercase();
        if ["-c", "-lc", "-command", "/c"].contains(&flag.as_str()) {
            return Some(script.to_string());
        }
    }
    Some(words.join(" "))
}

/// Process one hook input: the JSON to print, or `None` to allow the call as it is.
pub fn handle(
    input: &str,
    mode: Mode,
    kinds: &BTreeMap<String, KindConfig>,
    program: &str,
) -> Option<String> {
    let v: Value = serde_json::from_str(input).ok()?;
    if v.get("tool_name")?.as_str()? != "Bash" {
        return None;
    }
    let tool_input = v.get("tool_input")?.as_object()?;
    let command = command_of(tool_input.get("command")?)?;
    let call = Call {
        command: &command,
        shell: Shell::Posix,
        limit_ms: None,
        background: false,
    };
    let p = plan(&call, kinds, program)?;
    let out = match mode {
        Mode::Deny => json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": deny_reason(&p, Background::Hint),
            }
        }),
        Mode::Rewrite => {
            let mut new_input = tool_input.clone();
            new_input.insert("command".to_string(), Value::String(p.command().to_string()));
            json!({
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": "allow",
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

    fn run(input: Value, mode: Mode) -> Option<Value> {
        handle(&input.to_string(), mode, &BTreeMap::new(), "calmrun")
            .map(|s| serde_json::from_str(&s).unwrap())
    }

    fn bash(command: Value) -> Value {
        json!({
            "session_id": "s",
            "hook_event_name": "PreToolUse",
            "cwd": "/w",
            "tool_name": "Bash",
            "tool_input": {"command": command},
        })
    }

    #[test]
    fn denies_heavy_shell_commands() {
        let v = run(bash(json!("npm test && git status")), Mode::Deny).unwrap();
        let out = &v["hookSpecificOutput"];
        assert_eq!(out["hookEventName"], "PreToolUse");
        assert_eq!(out["permissionDecision"], "deny");
        let reason = out["permissionDecisionReason"].as_str().unwrap();
        assert!(reason.contains("\ncalmrun -- npm test && git status"), "{reason}");
        assert!(!reason.contains("--wait-timeout"), "{reason}");
    }

    #[test]
    fn argv_commands_are_read() {
        let v = run(bash(json!(["bash", "-lc", "cargo build"])), Mode::Deny).unwrap();
        let reason = v["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .unwrap();
        assert!(reason.contains("\ncalmrun -- cargo build"), "{reason}");
        assert!(run(bash(json!(["git", "status"])), Mode::Deny).is_none());
        let v = run(bash(json!(["npx", "vitest", "run"])), Mode::Deny).unwrap();
        assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
    }

    #[test]
    fn rewrite_allows_with_the_whole_input() {
        let mut input = bash(json!(["bash", "-lc", "npx vitest run"]));
        input["tool_input"]["workdir"] = json!("/w/web");
        let v = run(input, Mode::Rewrite).unwrap();
        let out = &v["hookSpecificOutput"];
        assert_eq!(out["permissionDecision"], "allow");
        assert_eq!(out["updatedInput"]["command"], "calmrun -- npx vitest run");
        assert_eq!(out["updatedInput"]["workdir"], "/w/web");
    }

    #[test]
    fn patches_light_commands_and_other_tools_pass() {
        let patch = json!({
            "tool_name": "apply_patch",
            "tool_input": {"command": "*** Begin Patch\n*** Add File: a.txt\n+npm test\n*** End Patch"},
        });
        assert!(run(patch, Mode::Deny).is_none());
        let mcp = json!({"tool_name": "mcp__fs__read", "tool_input": {"path": "x"}});
        assert!(run(mcp, Mode::Deny).is_none());
        assert!(run(bash(json!("ls -la")), Mode::Deny).is_none());
        assert!(run(bash(json!("calmrun -- npm test")), Mode::Deny).is_none());
        assert!(handle("nope", Mode::Deny, &BTreeMap::new(), "calmrun").is_none());
    }
}
