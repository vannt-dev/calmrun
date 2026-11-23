//! `calmrun hook antigravity`: an Antigravity PreToolUse hook.
//!
//! Contract (https://antigravity.google/docs/hooks): Antigravity sends JSON with
//! `toolCall: {name, args}` on stdin. Shell commands are the `run_command` tool, the command in
//! `args.CommandLine` (with `Cwd` and `WaitMsBeforeAsync`). A refusal is
//! `{"decision":"deny","reason":"..."}` on stdout; `overwrite` is merged into the arguments
//! (shallow) before the call runs. Printing nothing leaves the call alone.
//!
//! A command still running after `WaitMsBeforeAsync` goes on in the background instead of
//! being stopped, so no `--wait-timeout` is added.

use super::{deny_reason, plan, Background, Call, Mode, Opts, Shell};
use serde_json::{json, Value};

/// Process one hook input: the JSON to print, or `None` to allow the call as it is.
pub fn handle(input: &str, o: &Opts) -> Option<String> {
    let v: Value = serde_json::from_str(input).ok()?;
    let call = v.get("toolCall")?;
    if call.get("name")?.as_str()? != "run_command" {
        return None;
    }
    let command = call.get("args")?.get("CommandLine")?.as_str()?;
    let c = Call {
        command,
        // run_command runs PowerShell on Windows.
        shell: if cfg!(windows) {
            Shell::PowerShell
        } else {
            Shell::Posix
        },
        limit_ms: None,
        background: false,
    };
    let p = plan(&c, o.kinds, o.program)?;
    let out = match o.mode {
        Mode::Deny => json!({"decision": "deny", "reason": deny_reason(&p, Background::Hint, o)}),
        Mode::Rewrite => json!({
            "decision": "allow",
            "reason": "calmrun: wrapped heavy command in the machine-wide queue",
            "overwrite": {"CommandLine": p.command()},
        }),
    };
    Some(out.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(name: &str, command: &str) -> String {
        json!({
            "conversationId": "c",
            "stepIdx": 3,
            "workspacePaths": ["/w"],
            "toolCall": {
                "name": name,
                "args": {"CommandLine": command, "Cwd": "/w", "WaitMsBeforeAsync": 5000},
            },
        })
        .to_string()
    }

    fn run(name: &str, command: &str, mode: Mode) -> Option<Value> {
        handle(
            &input(name, command),
            &super::super::opts(mode, r"C:\t\calmrun.exe"),
        )
        .map(|s| serde_json::from_str(&s).unwrap())
    }

    #[test]
    fn denies_heavy_commands() {
        let v = run("run_command", "npm run build", Mode::Deny).unwrap();
        assert_eq!(v["decision"], "deny");
        let reason = v["reason"].as_str().unwrap();
        // PowerShell on Windows keeps the backslashes; sh elsewhere gets forward slashes.
        let exe = if cfg!(windows) {
            r"C:\t\calmrun.exe"
        } else {
            "C:/t/calmrun.exe"
        };
        assert!(reason.contains(&format!("\n{exe} -- npm run build")), "{reason}");
        assert!(!reason.contains("--wait-timeout"), "{reason}");
    }

    #[test]
    fn rewrite_overwrites_only_the_command_line() {
        let v = run("run_command", "cd web && npx vitest run", Mode::Rewrite).unwrap();
        assert_eq!(v["decision"], "allow");
        let exe = if cfg!(windows) {
            r"C:\t\calmrun.exe"
        } else {
            "C:/t/calmrun.exe"
        };
        assert_eq!(
            v["overwrite"],
            json!({"CommandLine": format!("cd web && {exe} -- npx vitest run")})
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_quoted_path_is_called_with_the_call_operator_on_windows() {
        let out = handle(
            &input("run_command", "npm test"),
            &super::super::opts(Mode::Rewrite, r#""C:\My Tools\calmrun.exe""#),
        )
        .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["overwrite"]["CommandLine"],
            r#"& "C:\My Tools\calmrun.exe" -- npm test"#
        );
    }

    #[test]
    fn other_tools_and_light_commands_pass() {
        assert!(run("run_command", "git status", Mode::Deny).is_none());
        assert!(run("write_file", "npm test", Mode::Deny).is_none());
        assert!(handle("{}", &super::super::opts(Mode::Deny, "calmrun")).is_none());
    }
}
