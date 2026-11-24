//! `calmrun hook pi`: the hook behind calmrun's Pi extension.
//!
//! Pi extensions are TypeScript or JavaScript modules. `calmrun setup --hook pi` writes a small
//! one ([`extension`]) whose `tool_call` handler sends `{"toolName": "bash", "input": {...}}`
//! (or `"powershell"`, Pi's optional shell tool on Windows) to this hook. The answer is
//! `{"decision":"deny","reason":"..."}` (the extension blocks the call, and Pi shows the reason
//! to the model) or `{"decision":"rewrite","input":{"command":"..."}}` (the extension changes
//! the command in place). No output, or any failure, lets the call run unchanged.

use super::{deny_reason, plan, Background, Call, Mode, Opts, Shell};
use serde_json::{json, Value};

/// First line of the extension file: `setup` replaces or removes only a file that has it.
pub const MARKER: &str = "// calmrun:extension (managed by `calmrun setup --hook pi`)";

/// Pi's shell tool time limit in ms: its `timeout` is in seconds and has no default (no limit).
fn limit_ms(input: &Value) -> Option<u64> {
    let secs = input.get("timeout")?.as_f64()?;
    (secs.is_finite() && secs > 0.0).then_some((secs * 1000.0) as u64)
}

/// Process one hook input: the JSON to print, or `None` to allow the call as it is.
pub fn handle(input: &str, o: &Opts) -> Option<String> {
    let v: Value = serde_json::from_str(input).ok()?;
    let shell = match v.get("toolName")?.as_str()? {
        "bash" => Shell::Posix,
        "powershell" => Shell::PowerShell,
        _ => return None,
    };
    let args = v.get("input")?;
    let call = Call {
        command: args.get("command")?.as_str()?,
        shell,
        limit_ms: limit_ms(args),
        background: false,
    };
    let p = plan(&call, o.kinds, o.program)?;
    let out = match o.mode {
        Mode::Deny => json!({"decision": "deny", "reason": deny_reason(&p, Background::Hint, o)}),
        Mode::Rewrite => json!({"decision": "rewrite", "input": {"command": p.command()}}),
    };
    Some(out.to_string())
}

/// The extension file. `program` is how a shell starts calmrun (`calmrun`, or its path).
pub fn extension(program: &str) -> String {
    let command = serde_json::to_string(&format!("{program} hook pi")).unwrap_or_default();
    format!(
        r#"{MARKER}
// Before Pi runs a shell command, `calmrun hook pi` is asked whether it is heavy: heavy
// commands are blocked with the calmrun command to run instead (or rewritten, with
// CALMRUN_HOOK_MODE=rewrite). If calmrun cannot be asked, the command runs unchanged.
import {{ spawn }} from "node:child_process";

const COMMAND = process.env.CALMRUN_HOOK_COMMAND || {command};
const SHELL_TOOLS = new Set(["bash", "powershell"]);

// calmrun's answer, or null when it cannot be had in time.
function ask(call) {{
  return new Promise((resolve) => {{
    let out = "";
    let child;
    try {{
      child = spawn(COMMAND, {{ shell: true, windowsHide: true, stdio: ["pipe", "pipe", "ignore"] }});
    }} catch {{
      return resolve(null);
    }}
    const timer = setTimeout(() => {{
      child.kill();
      resolve(null);
    }}, 10000);
    child.on("error", () => {{
      clearTimeout(timer);
      resolve(null);
    }});
    child.stdout.on("data", (chunk) => (out += chunk));
    child.on("close", () => {{
      clearTimeout(timer);
      try {{
        resolve(JSON.parse(out.trim() || "null"));
      }} catch {{
        resolve(null);
      }}
    }});
    child.stdin.on("error", () => {{}});
    child.stdin.end(JSON.stringify(call));
  }});
}}

export default function (pi) {{
  pi.on("tool_call", async (event) => {{
    // Pi blocks a call whose handler throws, so every failure here lets the call run.
    try {{
      if (!SHELL_TOOLS.has(event?.toolName) || typeof event.input?.command !== "string") return;
      const answer = await ask({{ toolName: event.toolName, input: event.input }});
      if (answer?.decision === "deny") return {{ block: true, reason: String(answer.reason) }};
      if (answer?.decision === "rewrite" && typeof answer.input?.command === "string") {{
        event.input.command = answer.input.command;
      }}
    }} catch {{
      // run unchanged
    }}
  }});
}}
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(input: Value, mode: Mode) -> Option<Value> {
        handle(&input.to_string(), &super::super::opts(mode, "calmrun"))
            .map(|s| serde_json::from_str(&s).unwrap())
    }

    #[test]
    fn denies_without_a_cap_when_the_call_has_no_timeout() {
        let v = run(
            json!({"toolName": "bash", "input": {"command": "npx tsc --noEmit"}}),
            Mode::Deny,
        )
        .unwrap();
        assert_eq!(v["decision"], "deny");
        let reason = v["reason"].as_str().unwrap();
        assert!(reason.contains("\ncalmrun -- npx tsc --noEmit"), "{reason}");
        assert!(!reason.contains("--wait-timeout"), "{reason}");
        assert!(reason.contains("in the background"), "{reason}");
    }

    #[test]
    fn the_timeout_is_in_seconds() {
        let input = json!({"toolName": "bash", "input": {"command": "cargo test", "timeout": 600}});
        let v = run(input, Mode::Rewrite).unwrap();
        assert_eq!(v["decision"], "rewrite");
        assert_eq!(
            v["input"],
            json!({"command": "calmrun --wait-timeout 570 -- cargo test"})
        );
        let fraction = json!({"toolName": "bash", "input": {"command": "cargo test", "timeout": 120.5}});
        assert_eq!(
            run(fraction, Mode::Rewrite).unwrap()["input"]["command"],
            "calmrun --wait-timeout 108 -- cargo test"
        );
        let zero = json!({"toolName": "bash", "input": {"command": "cargo test", "timeout": 0}});
        assert_eq!(
            run(zero, Mode::Rewrite).unwrap()["input"]["command"],
            "calmrun -- cargo test"
        );
    }

    #[test]
    fn the_powershell_tool_gets_powershell_quoting() {
        let o = super::super::opts(Mode::Rewrite, r#""C:\Program Files\calmrun\calmrun.exe""#);
        let input = json!({"toolName": "powershell", "input": {"command": "npm test"}});
        let v: Value = serde_json::from_str(&handle(&input.to_string(), &o).unwrap()).unwrap();
        assert_eq!(
            v["input"]["command"],
            r#"& "C:\Program Files\calmrun\calmrun.exe" -- npm test"#
        );
    }

    #[test]
    fn other_tools_and_light_commands_pass() {
        assert!(run(
            json!({"toolName": "bash", "input": {"command": "ls"}}),
            Mode::Deny
        )
        .is_none());
        assert!(run(json!({"toolName": "read", "input": {"path": "a"}}), Mode::Deny).is_none());
        assert!(run(json!({"toolName": "bash", "input": {}}), Mode::Deny).is_none());
        assert!(run(json!("not a call"), Mode::Deny).is_none());
    }

    #[test]
    fn extension_names_the_program_and_carries_the_marker() {
        let e = extension("C:/Program Files/calmrun/calmrun.exe");
        assert!(e.starts_with(MARKER));
        assert!(
            e.contains(r#""C:/Program Files/calmrun/calmrun.exe hook pi""#),
            "{e}"
        );
        assert!(e.contains(r#"pi.on("tool_call""#));
    }
}
