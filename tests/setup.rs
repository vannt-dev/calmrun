//! `calmrun setup` against a temporary home directory.

use std::path::Path;
use std::process::Command;

const EXE: &str = env!("CARGO_BIN_EXE_calmrun");

fn setup(home: &Path, args: &[&str]) -> (i32, String) {
    let mut c = Command::new(EXE);
    c.env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CODEX_HOME")
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
    assert_eq!(
        v["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
        "calmrun hook claude"
    );
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
