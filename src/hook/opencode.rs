//! `calmrun hook opencode`: the hook behind calmrun's OpenCode plugin.
//!
//! OpenCode plugins are JavaScript. `calmrun setup --hook opencode` writes a small plugin
//! ([`plugin`]) whose `tool.execute.before` sends `{"tool": "bash", "args": {...}}` to this
//! hook. The answer is `{"decision":"deny","reason":"..."}` (the plugin throws, and OpenCode
//! shows the reason to the model) or `{"decision":"rewrite","args":{"command":"..."}}` (the
//! plugin changes the arguments). No output, or any failure, lets the call run unchanged.

use super::{deny_reason, plan, Background, Call, Mode, Shell};
use crate::config::KindConfig;
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// OpenCode's bash tool limit when the call sets no `timeout` (2 minutes).
const DEFAULT_LIMIT_MS: u64 = 120_000;

/// First line of the plugin file: `setup` replaces or removes only a file that has it.
pub const MARKER: &str = "// calmrun:plugin (managed by `calmrun setup --hook opencode`)";

/// Process one hook input: the JSON to print, or `None` to allow the call as it is.
pub fn handle(
    input: &str,
    mode: Mode,
    kinds: &BTreeMap<String, KindConfig>,
    program: &str,
) -> Option<String> {
    let v: Value = serde_json::from_str(input).ok()?;
    if v.get("tool")?.as_str()? != "bash" {
        return None;
    }
    let args = v.get("args")?;
    let call = Call {
        command: args.get("command")?.as_str()?,
        shell: Shell::Posix,
        limit_ms: Some(
            args.get("timeout")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_LIMIT_MS),
        ),
        background: false,
    };
    let p = plan(&call, kinds, program)?;
    let out = match mode {
        Mode::Deny => json!({"decision": "deny", "reason": deny_reason(&p, Background::Hint)}),
        Mode::Rewrite => json!({"decision": "rewrite", "args": {"command": p.command()}}),
    };
    Some(out.to_string())
}

/// The plugin file. `program` is how a shell starts calmrun (`calmrun`, or its path).
pub fn plugin(program: &str) -> String {
    let command = serde_json::to_string(&format!("{program} hook opencode")).unwrap_or_default();
    format!(
        r#"{MARKER}
// Before OpenCode runs a shell command, `calmrun hook opencode` is asked whether it is heavy:
// heavy commands are refused with the calmrun command to run instead (or rewritten, with
// CALMRUN_HOOK_MODE=rewrite). If calmrun cannot be asked, the command runs unchanged.
import {{ spawnSync }} from "node:child_process";

const COMMAND = process.env.CALMRUN_HOOK_COMMAND || {command};

export const Calmrun = async () => ({{
  "tool.execute.before": async (input, output) => {{
    if (input?.tool !== "bash" || !output?.args) return;
    const res = spawnSync(COMMAND, {{
      input: JSON.stringify({{ tool: input.tool, args: output.args }}),
      encoding: "utf8",
      shell: true,
      timeout: 10000,
      windowsHide: true,
    }});
    let answer;
    try {{
      answer = JSON.parse(String(res.stdout ?? "").trim() || "null");
    }} catch {{
      return;
    }}
    if (answer?.decision === "deny") throw new Error(answer.reason);
    if (answer?.decision === "rewrite" && answer.args) Object.assign(output.args, answer.args);
  }},
}});
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(input: Value, mode: Mode) -> Option<Value> {
        handle(&input.to_string(), mode, &BTreeMap::new(), "calmrun")
            .map(|s| serde_json::from_str(&s).unwrap())
    }

    #[test]
    fn denies_with_a_capped_wait() {
        let v = run(
            json!({"tool": "bash", "args": {"command": "npx tsc --noEmit"}}),
            Mode::Deny,
        )
        .unwrap();
        assert_eq!(v["decision"], "deny");
        let reason = v["reason"].as_str().unwrap();
        assert!(
            reason.contains("\ncalmrun --wait-timeout 108 -- npx tsc --noEmit"),
            "{reason}"
        );
        assert!(reason.contains("Exit 75"), "{reason}");
    }

    #[test]
    fn rewrite_follows_the_call_timeout() {
        let input = json!({"tool": "bash", "args": {"command": "cargo test", "timeout": 600000}});
        let v = run(input, Mode::Rewrite).unwrap();
        assert_eq!(v["decision"], "rewrite");
        assert_eq!(
            v["args"],
            json!({"command": "calmrun --wait-timeout 570 -- cargo test"})
        );
    }

    #[test]
    fn other_tools_and_light_commands_pass() {
        assert!(run(json!({"tool": "bash", "args": {"command": "ls"}}), Mode::Deny).is_none());
        assert!(run(json!({"tool": "edit", "args": {"filePath": "a"}}), Mode::Deny).is_none());
    }

    #[test]
    fn plugin_names_the_program_and_carries_the_marker() {
        let p = plugin("C:/Program Files/calmrun/calmrun.exe");
        assert!(p.starts_with(MARKER));
        assert!(
            p.contains(r#""C:/Program Files/calmrun/calmrun.exe hook opencode""#),
            "{p}"
        );
        assert!(p.contains("\"tool.execute.before\""));
    }
}
