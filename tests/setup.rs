//! `calmrun setup` against a temporary home directory.

use std::path::Path;
use std::process::Command;

const EXE: &str = env!("CARGO_BIN_EXE_calmrun");

fn setup(home: &Path, args: &[&str]) -> (i32, String) {
    let mut c = Command::new(EXE);
    c.env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CODEX_HOME")
        .env("CALMRUN_HOME", home.join("calmrun state"))
        .arg("setup")
        .arg("--home")
        .arg(home)
        .args(args);
    let out = c.output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
    )
}

#[test]
fn dry_run_changes_nothing() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".claude")).unwrap();
    let (code, out) = setup(home.path(), &[]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("would create the file"), "{out}");
    assert!(out.contains("<!-- calmrun:start"), "{out}");
    assert!(!home.path().join(".claude/CLAUDE.md").exists());
}

#[test]
fn write_replace_and_remove() {
    let home = tempfile::tempdir().unwrap();
    let claude_md = home.path().join(".claude/CLAUDE.md");
    let codex_md = home.path().join(".codex/AGENTS.md");
    std::fs::create_dir_all(claude_md.parent().unwrap()).unwrap();
    std::fs::write(&claude_md, "# Mine\n\nKeep this.\n").unwrap();

    let (code, out) = setup(home.path(), &["--write", "--agents", "claude,codex"]);
    assert_eq!(code, 0, "{out}");
    let text = std::fs::read_to_string(&claude_md).unwrap();
    assert!(text.starts_with("# Mine\n\nKeep this.\n\n<!-- calmrun:start"));
    assert!(std::fs::read_to_string(&codex_md)
        .unwrap()
        .contains("calmrun -- npx vitest run"));

    // Running again changes nothing.
    let (_, out) = setup(home.path(), &["--write", "--agents", "claude,codex"]);
    assert_eq!(out.matches("unchanged").count(), 2, "{out}");
    assert_eq!(std::fs::read_to_string(&claude_md).unwrap(), text);

    let (code, _) = setup(home.path(), &["--write", "--remove", "--agents", "claude,codex"]);
    assert_eq!(code, 0);
    assert_eq!(
        std::fs::read_to_string(&claude_md).unwrap(),
        "# Mine\n\nKeep this.\n"
    );
    assert_eq!(std::fs::read_to_string(&codex_md).unwrap(), "");
}

#[test]
fn hook_is_merged_with_a_backup() {
    let home = tempfile::tempdir().unwrap();
    let settings = home.path().join(".claude/settings.json");
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    std::fs::write(&settings, "{\"model\": \"opus\"}\n").unwrap();
    let (code, out) = setup(home.path(), &["--write", "--hook", "--agents", "claude"]);
    assert_eq!(code, 0, "{out}");
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    assert_eq!(v["model"], "opus");
    let cmd = v["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    assert!(cmd.contains("calmrun") && cmd.ends_with(" hook claude"), "{cmd}");
    assert!(home.path().join(".claude/settings.json.calmrun.bak").exists());
    let (_, out) = setup(home.path(), &["--write", "--hook", "--agents", "claude"]);
    assert!(out.contains("hook") && out.contains("unchanged"), "{out}");
    let (_, _) = setup(
        home.path(),
        &["--write", "--remove", "--hook", "--agents", "claude"],
    );
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    assert!(v["hooks"]["PreToolUse"].is_null(), "{v}");
}

#[test]
fn broken_settings_are_left_alone() {
    let home = tempfile::tempdir().unwrap();
    let settings = home.path().join(".claude/settings.json");
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    std::fs::write(&settings, "{ not json").unwrap();
    let (code, out) = setup(home.path(), &["--write", "--hook", "--agents", "claude"]);
    assert_eq!(code, 1);
    assert!(out.contains("not valid JSON"), "{out}");
    assert_eq!(std::fs::read_to_string(&settings).unwrap(), "{ not json");
}

#[test]
fn unknown_agent_is_an_error() {
    let home = tempfile::tempdir().unwrap();
    let (code, out) = setup(home.path(), &["--agents", "nope"]);
    assert_eq!(code, 2);
    assert!(out.contains("unknown agent"));
}

fn read_json(path: &Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn codex_and_antigravity_hooks_keep_other_entries() {
    let home = tempfile::tempdir().unwrap();
    let codex = home.path().join(".codex/hooks.json");
    let agy = home.path().join(".gemini/config/hooks.json");
    std::fs::create_dir_all(codex.parent().unwrap()).unwrap();
    std::fs::create_dir_all(agy.parent().unwrap()).unwrap();
    let aiws_codex = serde_json::json!({"hooks": {"PreToolUse": [{"matcher": "", "hooks": [
        {"type": "command", "command": "node aiws.js guard --from codex", "timeout": 30}]}]}});
    let aiws_agy = serde_json::json!({"aiws-guard": {"PreToolUse": [{"matcher": ".*", "hooks": [
        {"type": "command", "command": "aiws guard --from antigravity", "timeout": 30}]}]}});
    std::fs::write(&codex, aiws_codex.to_string()).unwrap();
    std::fs::write(&agy, aiws_agy.to_string()).unwrap();

    // Dry run first: nothing changes.
    let (code, out) = setup(home.path(), &["--hook", "codex,antigravity", "--agents", "codex"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("would add the codex PreToolUse hook"), "{out}");
    assert!(out.contains("/hooks"), "trust note: {out}");
    assert_eq!(read_json(&codex), aiws_codex);

    let args = ["--write", "--hook", "codex,antigravity", "--agents", "codex"];
    let (code, out) = setup(home.path(), &args);
    assert_eq!(code, 0, "{out}");
    let c = read_json(&codex);
    assert_eq!(c["hooks"]["PreToolUse"][0], aiws_codex["hooks"]["PreToolUse"][0]);
    let ours = c["hooks"]["PreToolUse"][1]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    assert!(ours.ends_with(" hook codex"), "{ours}");
    let a = read_json(&agy);
    assert_eq!(a["aiws-guard"], aiws_agy["aiws-guard"]);
    assert_eq!(a["calmrun"]["PreToolUse"][0]["matcher"], "run_command");

    // Again: unchanged, byte for byte (Codex trusts a hook by its hash).
    let before = std::fs::read(&codex).unwrap();
    let (_, out) = setup(home.path(), &args);
    assert_eq!(out.matches("unchanged").count(), 3, "{out}");
    assert_eq!(std::fs::read(&codex).unwrap(), before);

    let (code, _) = setup(
        home.path(),
        &[
            "--write",
            "--remove",
            "--hook",
            "codex,antigravity",
            "--agents",
            "codex",
        ],
    );
    assert_eq!(code, 0);
    assert_eq!(read_json(&codex), aiws_codex);
    assert_eq!(read_json(&agy), aiws_agy);
}

#[test]
fn opencode_plugin_is_written_and_only_its_own_file_replaced() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join(".config/opencode");
    std::fs::create_dir_all(&dir).unwrap();
    let args = ["--write", "--hook", "opencode", "--agents", "opencode"];
    let (code, out) = setup(home.path(), &args);
    assert_eq!(code, 0, "{out}");
    let plugin = dir.join("plugins/calmrun.js");
    let text = std::fs::read_to_string(&plugin).unwrap();
    assert!(text.starts_with("// calmrun:plugin"), "{text}");
    assert!(text.contains(" hook opencode"), "{text}");
    let (_, out) = setup(home.path(), &args);
    assert!(out.contains("calmrun.js: unchanged"), "{out}");
    let (code, _) = setup(
        home.path(),
        &[
            "--write", "--remove", "--hook", "opencode", "--agents", "opencode",
        ],
    );
    assert_eq!(code, 0);
    assert!(!plugin.exists());
    // Someone else's calmrun.js is left alone.
    std::fs::write(
        &plugin,
        "export const Mine = 1;
",
    )
    .unwrap();
    let (code, out) = setup(home.path(), &args);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("not written by calmrun"), "{out}");
    assert_eq!(
        std::fs::read_to_string(&plugin).unwrap(),
        "export const Mine = 1;
"
    );
}

#[test]
fn a_hook_for_a_missing_agent_creates_nothing() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".claude")).unwrap();
    let (code, out) = setup(home.path(), &["--write", "--hook", "codex", "--agents", "claude"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("is codex installed?"), "{out}");
    assert!(!home.path().join(".codex").exists());
    // Under `all`, missing agents are only noted.
    let (code, out) = setup(home.path(), &["--hook", "all", "--agents", "claude"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("is opencode installed?"), "{out}");
    assert!(out.contains("leaves out antigravity (experimental"), "{out}");
    assert!(!out.contains("is antigravity installed?"), "{out}");
    let (code, out) = setup(home.path(), &["--hook", "gemini"]);
    assert_eq!(code, 2, "{out}");
}

#[test]
fn codex_sandbox_gets_the_state_directory_as_a_writable_root() {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join(".codex/config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    let mine = "# mine
model = \"gpt-5\"

[sandbox_workspace_write]
network_access = true
";
    std::fs::write(&config, mine).unwrap();
    let state = home.path().join("calmrun state").to_string_lossy().to_string();
    // The dry run shows the line and changes nothing.
    let (code, out) = setup(home.path(), &["--hook", "codex", "--agents", "codex"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains(&format!("+ writable_roots = ['{state}']")), "{out}");
    assert_eq!(std::fs::read_to_string(&config).unwrap(), mine);
    let (code, out) = setup(home.path(), &["--write", "--hook", "codex", "--agents", "codex"]);
    assert_eq!(code, 0, "{out}");
    let text = std::fs::read_to_string(&config).unwrap();
    assert!(
        text.starts_with(
            "# mine
model = \"gpt-5\"
"
        ),
        "{text}"
    );
    assert!(text.contains("network_access = true"), "{text}");
    let t: toml::Table = toml::from_str(&text).unwrap();
    assert_eq!(
        t["sandbox_workspace_write"]["writable_roots"][0].as_str(),
        Some(state.as_str())
    );
    // Again: already there.
    let (_, out) = setup(home.path(), &["--write", "--hook", "codex", "--agents", "codex"]);
    assert!(
        out.contains("already in sandbox_workspace_write.writable_roots"),
        "{out}"
    );
}
