//! Command analysis: where the program is in a command line, what to call the job, which
//! hints and keys apply, and which commands are refused.
//!
//! Detection looks only at the command position (after env assignments and launchers such as
//! `npx` or `pnpm exec`), so `git commit -m "fix vitest"` is not mistaken for a vitest run.
//! What counts as heavy lives in [`crate::kinds`].

use crate::config::KindConfig;
use crate::kinds::{self, first_non_flag, ArgHint, Hint, KeyRule, Position, Style, Workers};
use crate::shell::{is_shell_string, split_segments, tokenize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Result of analysing one command.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Detected {
    /// Heavy kind, or `None` for a command calmrun does not know.
    pub kind: Option<String>,
    /// Tool + subcommand only, never arguments.
    pub label: String,
    /// Normalised tool name (basename without extension).
    pub tool: String,
    /// The words after the tool (for rule lookups: cost, key, hints, needs).
    pub rest: Vec<String>,
}

impl Detected {
    /// The kind, or `other` for an unknown command.
    pub fn kind_name(&self) -> &str {
        self.kind.as_deref().unwrap_or("other")
    }
}

/// Basename, lowercased, without .cmd/.bat/.exe/.ps1/.js-style extensions and without a
/// package version (`tsc@5`, `@scope/pkg@1` -> `tsc`, `pkg`).
pub fn tool_name(word: &str) -> String {
    const EXTENSIONS: &[&str] = &[".cmd", ".bat", ".exe", ".ps1", ".js", ".mjs", ".cjs"];
    let base = word
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(word)
        .to_ascii_lowercase();
    // A package spec such as `tsc@5` or `@scope/pkg@1` (the scope is already cut off above).
    let base = match base.rfind('@') {
        Some(at) if at > 0 => base[..at].to_string(),
        _ => base,
    };
    EXTENSIONS
        .iter()
        .find_map(|ext| base.strip_suffix(ext).filter(|s| !s.is_empty()))
        .map(str::to_string)
        .unwrap_or(base)
}

fn is_assignment(w: &str) -> bool {
    w.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !name.starts_with(|c: char| c.is_ascii_digit())
    })
}

/// Words that run the next word as the command (`time npx vitest`).
const PREFIX_WRAPPERS: &[&str] = &[
    "env",
    "time",
    "command",
    "nohup",
    "cross-env",
    "call",
    "&",
    "exec",
];
/// Package bins that `pnpm <bin>` / `yarn <bin>` run directly.
const JS_BINS: &[&str] = &[
    "vitest",
    "jest",
    "tsc",
    "vue-tsc",
    "tsgo",
    "eslint",
    "oxlint",
    "biome",
    "playwright",
    "cypress",
    "puppeteer",
    "vite",
    "next",
    "ng",
    "webpack",
    "rollup",
    "esbuild",
];
/// Launchers whose flags take a value (`npx -p pkg`).
const PACKAGE_FLAGS: &[&str] = &["-p", "--package"];

/// A command split at its command position.
#[derive(Debug, Clone, PartialEq)]
pub struct Parsed {
    pub launcher: Vec<String>,
    pub tool: String,
    pub rest: Vec<String>,
    /// Index of the first word after env assignments and wrappers such as `time` or `nohup`
    /// (where `calmrun --` can be inserted).
    pub start: usize,
}

fn skip_flags(words: &[String], mut i: usize, value_flags: &[&str]) -> usize {
    while i < words.len() && words[i].starts_with('-') {
        if value_flags.contains(&words[i].as_str()) {
            i += 1;
        }
        i += 1;
    }
    i
}

/// Index of the first word after env assignments and wrappers.
fn command_start(words: &[String]) -> Option<usize> {
    let mut i = 0;
    loop {
        let w = words.get(i)?;
        if is_assignment(w) {
            i += 1;
            continue;
        }
        let t = tool_name(w);
        if PREFIX_WRAPPERS.contains(&t.as_str()) {
            i += 1;
            while i < words.len() && (words[i].starts_with('-') || is_assignment(&words[i])) {
                i += 1;
            }
        } else if t == "nice" {
            i = skip_flags(words, i + 1, &["-n"]);
        } else {
            return Some(i);
        }
    }
}

fn looks_like_script(w: &str) -> bool {
    [".js", ".mjs", ".cjs", ".ts"].iter().any(|e| w.ends_with(e)) && (w.contains('/') || w.contains('\\'))
}

/// Where the tool is after a launcher at `i`: (launcher words, tool index). `None` means the
/// word at `i` is itself the tool.
fn launcher_at(words: &[String], i: usize) -> Option<(Vec<String>, usize)> {
    let first = tool_name(&words[i]);
    let next = words.get(i + 1).map(|s| s.to_ascii_lowercase());
    match (first.as_str(), next.as_deref()) {
        ("npx" | "pnpx" | "bunx", _) => Some((vec![first.clone()], skip_flags(words, i + 1, PACKAGE_FLAGS))),
        ("pnpm" | "yarn" | "npm", Some(sub @ ("exec" | "dlx" | "x"))) | ("bun", Some(sub @ "x")) => {
            let j = skip_flags(words, i + 2, PACKAGE_FLAGS);
            let j = if words.get(j).map(String::as_str) == Some("--") {
                j + 1
            } else {
                j
            };
            Some((vec![first.clone(), sub.to_string()], j))
        }
        ("pnpm" | "yarn", Some(n)) if JS_BINS.contains(&n) => Some((vec![first.clone()], i + 1)),
        ("python" | "python3" | "py", _) => {
            let m = words[i + 1..]
                .iter()
                .take_while(|w| w.starts_with('-'))
                .position(|w| w == "-m")?;
            Some((vec![first.clone(), "-m".to_string()], i + 1 + m + 1))
        }
        ("node" | "bun" | "deno", Some(n)) if looks_like_script(n) => Some((vec![first.clone()], i + 1)),
        ("bundle", Some("exec")) => Some((
            vec![first.clone(), "exec".to_string()],
            skip_flags(words, i + 2, &[]),
        )),
        ("dotnet", Some("tool")) if words.get(i + 2).is_some_and(|w| w == "run") => {
            Some((vec![first.clone(), "tool".to_string(), "run".to_string()], i + 3))
        }
        ("xcrun", _) => Some((
            vec![first.clone()],
            skip_flags(words, i + 1, &["-sdk", "--sdk", "--toolchain"]),
        )),
        _ => None,
    }
}

/// Find the program in a list of words.
pub fn parse(words: &[String]) -> Option<Parsed> {
    let start = command_start(words)?;
    let first = tool_name(&words[start]);
    let next = words.get(start + 1).map(|s| s.to_ascii_lowercase());
    if matches!(first.as_str(), "uv" | "poetry" | "pipenv" | "pdm" | "hatch")
        && next.as_deref() == Some("run")
    {
        let inner = parse(&words[start + 2..])?;
        let mut launcher = vec![first, "run".to_string()];
        launcher.extend(inner.launcher);
        return Some(Parsed {
            launcher,
            tool: inner.tool,
            rest: inner.rest,
            start,
        });
    }
    match launcher_at(words, start) {
        Some((launcher, idx)) if idx < words.len() => Some(Parsed {
            launcher,
            tool: tool_name(&words[idx]),
            rest: words[idx + 1..].to_vec(),
            start,
        }),
        // A bare launcher such as `npx --version`, or a plain tool.
        _ => Some(Parsed {
            launcher: Vec::new(),
            tool: first,
            rest: words[start + 1..].to_vec(),
            start,
        }),
    }
}

/// Tools whose first plain word is a subcommand worth showing in labels.
const SUB_TOOLS: &[&str] = &[
    "cargo",
    "go",
    "docker",
    "docker-compose",
    "podman",
    "podman-compose",
    "npm",
    "pnpm",
    "yarn",
    "bun",
    "flutter",
    "dotnet",
    "gradle",
    "gradlew",
    "mvn",
    "mvnw",
    "vite",
    "next",
    "ng",
    "git",
    "deno",
    "make",
    "playwright",
    "mvnd",
    "sbt",
    "swift",
    "dart",
    "mix",
    "rebar3",
    "stack",
    "cabal",
    "dune",
    "zig",
    "meson",
    "bazel",
    "bazelisk",
    "buck2",
    "rake",
    "rails",
    "composer",
    "turbo",
    "nx",
    "lerna",
    "moon",
    "rush",
    "buildah",
];
const VITEST_SUBS: &[&str] = &["run", "watch", "bench", "dev", "related", "list"];
/// Longest word shown in a label; longer ones are likely arguments, not subcommands.
const LABEL_WORD_MAX: usize = 24;

fn safe_word(w: &str) -> bool {
    let mut chars = w.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
        && w.len() <= LABEL_WORD_MAX
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | ':' | '_'))
}

/// Label: launcher + tool + subcommand (two words for `npm run x` / `docker compose up`).
fn label_of(p: &Parsed) -> String {
    let mut parts = p.launcher.clone();
    parts.push(p.tool.clone());
    let sub = first_non_flag(&p.rest).unwrap_or("");
    let allowed = (SUB_TOOLS.contains(&p.tool.as_str()) && safe_word(sub))
        || (p.tool == "vitest" && VITEST_SUBS.contains(&sub));
    if allowed {
        parts.push(sub.to_string());
        let second = p
            .rest
            .iter()
            .skip_while(|w| w.as_str() != sub)
            .nth(1)
            .map(String::as_str);
        let two_word = matches!(
            (p.tool.as_str(), sub),
            ("npm" | "pnpm" | "yarn" | "bun", "run" | "run-script") | ("docker", "compose" | "buildx")
        );
        if let Some(s) = second.filter(|s| two_word && safe_word(s)) {
            parts.push(s.to_string());
        }
    }
    parts.join(" ")
}

/// Analyse a list of argv words (one segment).
pub fn detect_words(words: &[String], kinds: &BTreeMap<String, KindConfig>) -> Detected {
    match parse(words) {
        Some(p) => Detected {
            kind: kinds::classify(&p.tool, &p.rest, kinds),
            label: label_of(&p),
            tool: p.tool,
            rest: p.rest,
        },
        None => Detected::default(),
    }
}

/// Analyse the full command. A shell string is split into segments; the costliest heavy
/// segment decides the kind.
pub fn detect(argv: &[String], kinds: &BTreeMap<String, KindConfig>) -> Detected {
    if !is_shell_string(argv) {
        return detect_words(argv, kinds);
    }
    let s = &argv[0];
    let mut first: Option<Detected> = None;
    let mut best: Option<(u32, Detected)> = None;
    for r in split_segments(s) {
        let d = detect_words(&tokenize(&s[r]), kinds);
        if d.label.is_empty() {
            continue;
        }
        if let Some(k) = &d.kind {
            let c = kinds::cost(k, kinds);
            if best.as_ref().is_none_or(|(bc, _)| c > *bc) {
                best = Some((c, d.clone()));
            }
        }
        first.get_or_insert(d);
    }
    best.map(|(_, d)| d).or(first).unwrap_or_default()
}

/// The changes the tool's hint asks for: new argv (if any) and extra environment.
#[derive(Debug, Default, PartialEq)]
pub struct HintChanges {
    pub argv: Option<Vec<String>>,
    pub env: Vec<(String, String)>,
}

/// Index of the tool's own word in `argv` (after env assignments, wrappers and launchers).
fn tool_index(argv: &[String], p: &Parsed) -> Option<usize> {
    (p.start..argv.len()).find(|&i| tool_name(&argv[i]) == p.tool)
}

/// Apply the [`Hint`]s of the detected tool. `workers` turns a [`Workers`] into a number;
/// `env_get` reads the environment. Anything the user set explicitly wins: a flag on the
/// command line, an environment variable, or a limit in the tool's own config.
pub fn apply_hints(
    argv: &[String],
    det: &Detected,
    cwd: &Path,
    workers: &dyn Fn(Workers) -> u32,
    env_get: &dyn Fn(&str) -> Option<String>,
) -> HintChanges {
    let mut out = HintChanges::default();
    let mut words = argv.to_vec();
    for hint in kinds::hints_for(&det.tool, &det.rest) {
        match hint {
            Hint::Arg(a) => {
                if let Some(changed) = arg_hint(&words, &det.tool, &a, a.count(workers(a.workers)), env_get) {
                    words = changed;
                    out.argv = Some(words.clone());
                }
            }
            Hint::Replace {
                flags,
                from,
                workers: w,
            } => {
                if let Some(changed) = replace_hint(&words, flags, from, workers(w)) {
                    words = changed;
                    out.argv = Some(words.clone());
                }
            }
            Hint::Env {
                var,
                workers: w,
                user_limit,
            } if env_get(var).is_none() => {
                let ours = workers(w);
                let n = user_limit
                    .and_then(|f| f(cwd))
                    .map_or(ours, |theirs| theirs.min(ours));
                out.env.push((var.to_string(), n.to_string()))
            }
            Hint::Env { .. } => {}
        }
    }
    out
}

/// The argv with an argument hint added, if the tool was started directly as words and the
/// user has not chosen a number already.
fn arg_hint(
    argv: &[String],
    tool: &str,
    a: &ArgHint,
    n: u32,
    env_get: &dyn Fn(&str) -> Option<String>,
) -> Option<Vec<String>> {
    if is_shell_string(argv) || argv.is_empty() {
        return None;
    }
    let p = parse(argv).filter(|p| p.tool == tool)?;
    let at = tool_index(argv, &p)?;
    let rest = &argv[at + 1..];
    let opted_out = rest.iter().take_while(|w| w.as_str() != "--").any(|w| {
        let w = w.to_ascii_lowercase();
        a.opt_out.iter().any(|o| w.starts_with(o))
    });
    let env_says = a.opt_out_env.iter().any(|(var, needle)| {
        env_get(var).is_some_and(|v| !v.trim().is_empty() && needle.is_none_or(|s| v.contains(s)))
    });
    if opted_out || env_says || rest.len() < a.min_args || kinds::only_info_flags(rest) {
        return None;
    }
    let insert = match a.at {
        Position::AfterTool => at + 1,
        Position::AfterSub => at + 1 + rest.iter().position(|w| !w.starts_with('-')).map_or(0, |i| i + 1),
        Position::End => at + 1 + rest.iter().position(|w| w == "--").unwrap_or(rest.len()),
    };
    let mut out = argv.to_vec();
    match a.style {
        Style::Joined => out.insert(insert, format!("{}{n}", a.flag)),
        Style::Split => {
            out.insert(insert, n.to_string());
            out.insert(insert, a.flag.to_string());
        }
    }
    Some(out)
}

/// `-n auto` / `-n=auto` / `-nauto` -> the job's share (argv words only).
fn replace_hint(argv: &[String], flags: &[&str], from: &str, n: u32) -> Option<Vec<String>> {
    if is_shell_string(argv) {
        return None;
    }
    let mut out = argv.to_vec();
    for i in 0..out.len() {
        let w = out[i].clone();
        for f in flags {
            if w == *f && out.get(i + 1).is_some_and(|v| v == from) {
                out[i + 1] = n.to_string();
                return Some(out);
            }
            if w == format!("{f}={from}") {
                out[i] = format!("{f}={n}");
                return Some(out);
            }
            if !f.starts_with("--") && w == format!("{f}{from}") {
                out[i] = format!("{f}{n}");
                return Some(out);
            }
        }
    }
    None
}

fn compose_up_attached(words: &[String]) -> bool {
    let Some(p) = parse(words) else {
        return false;
    };
    let tool = p.tool.as_str();
    let after = if kinds::COMPOSE.contains(&tool) {
        &p.rest[..]
    } else if kinds::CONTAINERS.contains(&tool) {
        match kinds::after_docker_compose(&p.rest) {
            Some(a) => a,
            None => return false,
        }
    } else {
        return false;
    };
    if kinds::compose_sub(after).as_deref() != Some("up") {
        return false;
    }
    let detached = after.iter().any(|w| {
        w == "--detach"
            || w.starts_with("--detach=") && !w.ends_with("=false")
            || w == "--wait"
            || (w.starts_with('-') && !w.starts_with("--") && w[1..].contains('d'))
    });
    !detached
}

/// What to do with a command before it is queued.
#[derive(Debug, Clone, PartialEq)]
pub enum Disposition {
    /// Wait in the queue as usual.
    Queue,
    /// Run at once without the queue (help/version output, dev servers).
    Light(&'static str),
    /// Refuse: it would hold its points forever. `fixed` is the one-shot form, if known.
    Refuse {
        reason: String,
        fixed: Option<Vec<String>>,
    },
}

/// Decide whether a command is queued, run at once, or refused. `stdin_tty` matters for
/// tools that watch by default in a terminal (vitest).
pub fn disposition(argv: &[String], stdin_tty: bool) -> Disposition {
    if is_shell_string(argv) {
        let s = &argv[0];
        return split_segments(s)
            .into_iter()
            .find_map(|r| match words_disposition(&tokenize(&s[r]), stdin_tty) {
                Disposition::Refuse { reason, .. } => Some(Disposition::Refuse { reason, fixed: None }),
                _ => None,
            })
            .unwrap_or(Disposition::Queue);
    }
    words_disposition(argv, stdin_tty)
}

fn words_disposition(words: &[String], stdin_tty: bool) -> Disposition {
    if compose_up_attached(words) {
        return Disposition::Refuse {
            reason: "`docker compose up` without -d would hold queue points forever; run it detached \
                     (`docker compose up -d`) or outside calmrun"
                .to_string(),
            fixed: detached_compose(words),
        };
    }
    let Some(p) = parse(words) else {
        return Disposition::Queue;
    };
    if kinds::asks_for_info(&p.rest) {
        return Disposition::Light("only prints help or a version");
    }
    if let Some(rule) = kinds::endless_rule(&p.tool, &p.rest, stdin_tty) {
        return Disposition::Refuse {
            reason: format!(
                "`{}` is {} and never ends, so it would hold queue points forever; run the one-shot \
                 form, or run it outside calmrun",
                label_of(&p),
                rule.what
            ),
            fixed: apply_fix(words, &p, rule.fix),
        };
    }
    match kinds::light_rule(&p.tool, &p.rest) {
        Some(r) => Disposition::Light(r.kind),
        None => Disposition::Queue,
    }
}

/// The one-shot form of an endless command (argv words only).
fn apply_fix(words: &[String], p: &Parsed, fix: kinds::Fix) -> Option<Vec<String>> {
    let tool_at = (p.start..words.len()).find(|&i| tool_name(&words[i]) == p.tool)?;
    let mut out = words.to_vec();
    match fix {
        kinds::Fix::None => return None,
        kinds::Fix::RemoveFlags(flags) => {
            let tail = out.split_off(tool_at + 1);
            out.extend(tail.into_iter().filter(|w| {
                let lw = w.to_ascii_lowercase();
                !flags.iter().any(|f| lw == *f || lw.starts_with(&format!("{f}=")))
            }));
        }
        kinds::Fix::SetSub(sub) => {
            let at = (tool_at + 1..out.len()).find(|&i| !out[i].starts_with('-'))?;
            out[at] = sub.to_string();
        }
        kinds::Fix::InsertSub(sub) => out.insert(tool_at + 1, sub.to_string()),
        kinds::Fix::DropSub => {
            let at = (tool_at + 1..out.len()).find(|&i| !out[i].starts_with('-'))?;
            // Only when a subcommand follows: bare `dotnet watch` has no one-shot form.
            out[at + 1..].iter().find(|w| !w.starts_with('-'))?;
            out.remove(at);
        }
    }
    Some(out)
}

/// The same compose command with `-d` added after `up` (argv words only).
pub fn detached_compose(argv: &[String]) -> Option<Vec<String>> {
    if is_shell_string(argv) || !compose_up_attached(argv) {
        return None;
    }
    let start = argv.iter().position(|w| {
        let t = tool_name(w);
        kinds::COMPOSE.contains(&t.as_str()) || kinds::CONTAINERS.contains(&t.as_str())
    })?;
    let up = start + argv[start..].iter().position(|w| w == "up")?;
    let mut out = argv.to_vec();
    out.insert(up + 1, "-d".to_string());
    Some(out)
}

/// FNV-1a 64-bit, stable across builds and platforms.
pub fn fnv1a(s: &str) -> String {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0100_0000_01b3;
    let h = s.bytes().fold(OFFSET, |h, b| (h ^ b as u64).wrapping_mul(PRIME));
    format!("{h:016x}")
}

fn key_for(prefix: &str, p: &Path) -> String {
    format!("{prefix}:{}", fnv1a(&crate::sys::path_key(p)))
}

fn nearest(cwd: &Path, test: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    cwd.ancestors().find(|d| test(d)).map(Path::to_path_buf)
}

/// Whether `dir` holds `marker` (`*.ext` matches any file with that extension).
fn has_marker(dir: &Path, marker: &str) -> bool {
    match marker.strip_prefix("*.") {
        Some(ext) => std::fs::read_dir(dir).is_ok_and(|rd| {
            rd.flatten()
                .any(|e| e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case(ext)))
        }),
        None => dir.join(marker).exists(),
    }
}

/// The project folder: for each marker in order, the nearest ancestor holding it.
fn project_dir(cwd: &Path, markers: &[&str]) -> Option<PathBuf> {
    markers.iter().find_map(|m| nearest(cwd, |d| has_marker(d, m)))
}

/// Exclusive keys of a job of `kind` run by `tool` in `cwd`: two jobs sharing a key never
/// run together.
pub fn exclusive_keys(kind: &str, tool: &str, rest: &[String], cwd: &Path) -> Vec<String> {
    match kinds::key_rule(kind, tool, rest) {
        KeyRule::None => Vec::new(),
        KeyRule::Project { prefix, markers } => project_dir(cwd, markers)
            .map(|d| vec![key_for(prefix, &d)])
            .unwrap_or_default(),
        KeyRule::NodeModules => nearest(cwd, |d| d.join("node_modules").is_dir())
            .map(|d| vec![key_for("nm", &d.join("node_modules"))])
            .unwrap_or_default(),
        KeyRule::CargoTarget => {
            if let Some(t) = std::env::var_os("CARGO_TARGET_DIR").filter(|v| !v.is_empty()) {
                return vec![key_for("cargo", Path::new(&t))];
            }
            nearest(cwd, |d| {
                d.join("Cargo.toml").is_file() && d.join("target").is_dir()
            })
            .or_else(|| nearest(cwd, |d| d.join("Cargo.toml").is_file()))
            .map(|d| vec![key_for("cargo", &d.join("target"))])
            .unwrap_or_default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<String> {
        tokenize(s)
    }
    fn kind(s: &str) -> Option<String> {
        detect_words(&w(s), &BTreeMap::new()).kind
    }
    fn label(s: &str) -> String {
        detect_words(&w(s), &BTreeMap::new()).label
    }
    fn argv(s: &str) -> Vec<String> {
        s.split(' ').map(String::from).collect()
    }
    fn hinted(s: &str, n: u32) -> Option<Vec<String>> {
        hinted_env(s, n, &[]).argv
    }
    fn hinted_env(s: &str, n: u32, env: &[(&str, &str)]) -> HintChanges {
        let a = argv(s);
        let det = detect(&a, &BTreeMap::new());
        let env: Vec<(String, String)> = env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let get = move |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        apply_hints(&a, &det, Path::new("/nonexistent-cwd"), &|_| n, &get)
    }

    #[test]
    fn tool_names() {
        assert_eq!(tool_name("C:\\Program Files\\nodejs\\npx.cmd"), "npx");
        assert_eq!(tool_name("./node_modules/.bin/vitest"), "vitest");
        assert_eq!(tool_name("ESLINT.EXE"), "eslint");
        assert_eq!(tool_name("run.ps1"), "run");
        assert_eq!(tool_name("tsc@5"), "tsc");
        assert_eq!(tool_name("@vitest/browser@3.1"), "browser");
        assert_eq!(tool_name("eslint@9.0.0"), "eslint");
    }

    #[test]
    fn kinds_detected() {
        for (cmd, k) in [
            ("vitest run", "vitest"),
            ("npx vitest", "vitest"),
            ("npx -y jest --ci", "jest"),
            ("pnpm exec vue-tsc --noEmit", "tsc"),
            ("pnpm tsgo", "tsc"),
            ("npx playwright test", "capture"),
            ("docker compose -f a.yml up -d", "docker"),
            ("docker-compose build", "docker"),
            ("docker build .", "docker"),
            ("npx biome check .", "eslint"),
            ("python -m pytest -q", "pytest"),
            ("uv run pytest", "pytest"),
            ("npx vite build", "build"),
            ("./gradlew assembleDebug", "build"),
            ("cargo build --release", "build"),
            ("flutter build apk", "build"),
            ("dotnet build", "build"),
            ("cargo test", "test"),
            ("go test ./...", "test"),
            ("npm test", "test"),
            ("npm run build", "build"),
            ("pnpm lint", "eslint"),
            ("FOO=1 CI=true npx vitest", "vitest"),
            ("node ./node_modules/vitest/vitest.mjs run", "vitest"),
            ("npx tsc@5 --noEmit", "tsc"),
            ("npx vitest@3 run", "vitest"),
            ("npx -p typescript@5 tsc", "tsc"),
            ("pnpm dlx eslint@9 .", "eslint"),
            ("bunx eslint@9 .", "eslint"),
        ] {
            assert_eq!(kind(cmd).as_deref(), Some(k), "{cmd}");
        }
    }

    #[test]
    fn light_commands_not_detected() {
        for c in [
            "git commit -m \"fix vitest\"",
            "grep jest .",
            "npm install eslint",
            "npm ci",
            "ls -la",
            "cat vitest.config.ts",
            "docker ps",
            "docker compose logs",
            "cargo install calmrun",
            "npx vitest --version",
            "vite",
            "npm run dev",
            "echo tsc",
        ] {
            assert_eq!(kind(c), None, "{c}");
        }
    }

    #[test]
    fn labels_have_no_arguments() {
        assert_eq!(label("npx vitest run src/secret.test.ts"), "npx vitest run");
        assert_eq!(label("vitest --token=abc"), "vitest");
        assert_eq!(label("cargo build --release"), "cargo build");
        assert_eq!(label("npm run build -- --token abc"), "npm run build");
        assert_eq!(label("docker compose up -d"), "docker compose up");
        assert_eq!(label("jest mySecretFilter"), "jest");
        assert_eq!(label("curl https://x?token=1"), "curl");
        assert_eq!(label("pnpm exec tsc -p ."), "pnpm exec tsc");
        assert_eq!(label("npx --version"), "npx");
    }

    #[test]
    fn custom_patterns() {
        let mut kinds = BTreeMap::new();
        kinds.insert(
            "heavy".to_string(),
            KindConfig {
                cost: Some(3),
                patterns: vec!["make".into(), "bazel build".into()],
            },
        );
        assert_eq!(
            detect_words(&w("make -j8"), &kinds).kind.as_deref(),
            Some("heavy")
        );
        assert_eq!(
            detect_words(&w("bazel build //..."), &kinds).kind.as_deref(),
            Some("heavy")
        );
        assert_eq!(detect_words(&w("bazel query x"), &kinds).kind, None);
        assert_eq!(kinds::cost("heavy", &kinds), 3);
    }

    #[test]
    fn shell_string_takes_costliest_segment() {
        let d = detect(
            &["npm ci && npx eslint . && npx vitest run".to_string()],
            &BTreeMap::new(),
        );
        assert_eq!(d.kind.as_deref(), Some("vitest"));
        assert_eq!(d.label, "npx vitest run");
    }

    #[test]
    fn vitest_hint() {
        assert_eq!(hinted("vitest run", 2), Some(argv("vitest run --maxWorkers=2")));
        assert_eq!(hinted("npx vitest", 1), Some(argv("npx vitest --maxWorkers=1")));
        assert_eq!(
            hinted("pnpm exec vitest run", 3),
            Some(argv("pnpm exec vitest run --maxWorkers=3"))
        );
        assert_eq!(
            hinted("yarn exec vitest -- a", 1),
            Some(argv("yarn exec vitest --maxWorkers=1 -- a"))
        );
        assert_eq!(hinted("vitest --maxWorkers=4", 1), None);
        assert_eq!(hinted("vitest --pool=forks", 1), None);
        assert_eq!(hinted("vitest --no-file-parallelism", 1), None);
        assert_eq!(hinted("npm run test", 1), None);
        assert_eq!(hinted("npx jest", 1), None);
        // Env assignments and wrappers in front do not hide the tool.
        assert_eq!(
            hinted("FOO=1 vitest", 1),
            Some(argv("FOO=1 vitest --maxWorkers=1"))
        );
        let shell = vec!["npx vitest run".to_string()];
        let det = detect(&shell, &BTreeMap::new());
        assert_eq!(
            apply_hints(&shell, &det, Path::new("."), &|_| 1, &|_| None).argv,
            None
        );
    }

    #[test]
    fn gradle_and_cargo_hints() {
        assert_eq!(
            hinted("./gradlew build", 4),
            Some(argv("./gradlew build --max-workers=4"))
        );
        assert_eq!(hinted("gradle test --max-workers=2", 4), None);
        assert_eq!(hinted("gradlew build -Dorg.gradle.workers.max=2", 4), None);
        assert_eq!(hinted("gradlew --version", 4), None);
        assert_eq!(hinted("gradlew", 4), None);
        let c = hinted_env("cargo build", 5, &[]);
        // At most ours; lower if this machine's ~/.cargo config sets build.jobs.
        assert_eq!(c.env.len(), 1);
        assert_eq!(c.env[0].0, "CARGO_BUILD_JOBS");
        assert!((1..=5).contains(&c.env[0].1.parse::<u32>().unwrap()));
        assert!(hinted_env("cargo build", 5, &[("CARGO_BUILD_JOBS", "2")])
            .env
            .is_empty());
    }

    /// (command, injected form) for every tool with an argument hint, N = 4.
    const INJECTED: &[(&str, &str)] = &[
        ("cmake --build build", "cmake --build build --parallel 4"),
        (
            "cmake --build build -- VERBOSE=1",
            "cmake --build build --parallel 4 -- VERBOSE=1",
        ),
        ("ninja -C out", "ninja -C out -j4"),
        ("meson compile -C b", "meson compile -C b -j 4"),
        ("meson test -C b", "meson test -C b --num-processes 4"),
        ("bazel build //...", "bazel build //... --jobs=4"),
        ("bazelisk test //...", "bazelisk test //... --jobs=4"),
        ("buck2 build //app:app", "buck2 build //app:app -j4"),
        ("buck2 test //...", "buck2 test //... -j4"),
        ("xmake", "xmake -j 4"),
        (
            "./gradlew assembleDebug",
            "./gradlew assembleDebug --max-workers=4",
        ),
        ("mvnd package", "mvnd package -T 4"),
        ("dotnet build", "dotnet build -m:4"),
        ("dotnet test --no-build", "dotnet test --no-build -m:4"),
        ("go test ./...", "go test -p 4 ./..."),
        ("go build -o out ./cmd/x", "go build -p 4 -o out ./cmd/x"),
        ("go vet ./...", "go vet -p 4 ./..."),
        ("golangci-lint run", "golangci-lint run --concurrency 4"),
        ("swift build", "swift build -j 4"),
        (
            "xcodebuild -scheme App build",
            "xcodebuild -scheme App build -jobs 4",
        ),
        (
            "xcrun xcodebuild -scheme App",
            "xcrun xcodebuild -scheme App -jobs 4",
        ),
        ("flutter test", "flutter test --concurrency=4"),
        ("dart test", "dart test --concurrency=4"),
        ("paratest", "paratest --processes=4"),
        ("mix test", "mix test --max-cases 4"),
        ("stack build", "stack build -j 4"),
        ("cabal build", "cabal build -j4"),
        ("dune build", "dune build -j 4"),
        ("zig build", "zig build -j4"),
        ("turbo run build", "turbo run build --concurrency=4"),
        ("npx turbo run test", "npx turbo run test --concurrency=4"),
        ("nx run-many -t test", "nx run-many -t test --parallel=3"),
        ("lerna run test", "lerna run test --concurrency 4"),
        ("pnpm -r run build", "pnpm --workspace-concurrency=4 -r run build"),
        ("rush build", "rush build --parallelism 4"),
        ("pytest -n auto", "pytest -n 4"),
        ("python -m pytest -n auto -q", "python -m pytest -n 4 -q"),
        (
            "uv run pytest --numprocesses=auto",
            "uv run pytest --numprocesses=4",
        ),
        ("tox -p auto", "tox -p 4"),
        ("bundle exec rails test", "bundle exec rails test"),
    ];

    /// Commands that get no argument: serial tools (a hint would turn parallelism on, which
    /// can break a build never written for it), and subcommands without a worker flag.
    const NOT_HINTED: &[&str] = &[
        "make",
        "gmake -C src all",
        "nmake",
        "mingw32-make",
        "ctest --test-dir build",
        "msbuild App.sln",
        "C:\\tools\\MSBuild.exe App.sln",
        "scons",
        "mvn package",
        "./mvnw verify",
        "rushx build",
        "flutter build apk",
        "dart compile exe bin/a.dart",
        "dart run build_runner build",
        "go mod tidy",
        "cmake -S . -B build",
        "meson setup b",
        "mix compile",
        "pnpm install",
        "swift package resolve",
        "dotnet --info",
    ];

    #[test]
    fn serial_tools_and_other_subcommands_get_no_argument() {
        for cmd in NOT_HINTED {
            let a = argv(cmd);
            let got = hinted(cmd, 4);
            assert!(got.is_none() || got.as_ref() == Some(&a), "{cmd}: {got:?}");
        }
        // Environment hints still reach every cargo subcommand.
        assert_eq!(hinted_env("cargo xtask dist", 4, &[]).env.len(), 1);
    }

    #[test]
    fn hints_are_injected_for_every_tool() {
        for (cmd, want) in INJECTED {
            let got = hinted(cmd, 4)
                .map(|a| a.join(" "))
                .unwrap_or_else(|| cmd.to_string());
            assert_eq!(got, *want, "{cmd}");
        }
    }

    /// Commands where the user already chose: nothing is changed.
    const USER_CHOSE: &[(&str, &[(&str, &str)])] = &[
        ("cmake --build b -j 2", &[]),
        ("cmake --build b", &[("CMAKE_BUILD_PARALLEL_LEVEL", "2")]),
        ("ninja -j1", &[]),
        ("meson test --num-processes 2", &[]),
        ("meson test", &[("MESON_TESTTHREADS", "2")]),
        ("dotnet build -maxcpucount:2", &[]),
        ("dotnet test -m:1", &[]),
        ("bazel build --jobs=2 //...", &[]),
        ("buck2 build --num-threads 2 //...", &[]),
        ("mvnd -T 1C package", &[]),
        ("swift build -j 2", &[]),
        ("xcodebuild -jobs 2 build", &[]),
        ("flutter test --concurrency=2", &[]),
        ("mix test --max-cases 2", &[]),
        ("stack build -j2", &[]),
        ("go test -p 1 ./...", &[]),
        ("go test ./...", &[("GOFLAGS", "-p=2")]),
        ("turbo run build --concurrency=1", &[]),
        ("pnpm -r --workspace-concurrency=1 run build", &[]),
        ("pytest -n 2", &[]),
        ("gradle build --max-workers=2", &[]),
    ];

    #[test]
    fn explicit_user_choices_win() {
        for (cmd, env) in USER_CHOSE {
            assert_eq!(hinted_env(cmd, 4, env).argv, None, "{cmd} {env:?}");
        }
    }

    #[test]
    fn env_hints_for_other_tools() {
        let env = |cmd: &str| hinted_env(cmd, 3, &[]).env;
        assert_eq!(
            env("rails test"),
            vec![("PARALLEL_WORKERS".to_string(), "3".to_string())]
        );
        assert_eq!(env("deno test"), vec![("DENO_JOBS".to_string(), "3".to_string())]);
        assert_eq!(
            env("moon run :build"),
            vec![("MOON_CONCURRENCY".to_string(), "3".to_string())]
        );
        assert_eq!(
            env("bundle exec parallel_rspec spec"),
            vec![("PARALLEL_TEST_PROCESSORS".to_string(), "3".to_string())]
        );
        let nextest = env("cargo nextest run");
        assert!(
            nextest
                .iter()
                .any(|(k, v)| k == "NEXTEST_TEST_THREADS" && v == "3"),
            "{nextest:?}"
        );
        assert!(hinted_env("rails test", 3, &[("PARALLEL_WORKERS", "1")])
            .env
            .is_empty());
    }

    fn refused(a: &[String]) -> bool {
        matches!(disposition(a, false), Disposition::Refuse { .. })
    }

    #[test]
    fn compose_up_refused() {
        assert!(refused(&argv("docker compose up")));
        assert!(refused(&argv("docker-compose -f x.yml up --build")));
        assert!(refused(&["cd a && docker compose up".to_string()]));
        assert!(!refused(&argv("docker compose up -d")));
        assert!(!refused(&argv("docker compose up --detach")));
        assert!(!refused(&argv("docker-compose up -dV")));
        assert!(!refused(&argv("docker compose up --wait")));
        assert!(!refused(&argv("docker compose build")));
        assert_eq!(
            detached_compose(&argv("docker compose -f a.yml up --build")),
            Some(argv("docker compose -f a.yml up -d --build"))
        );
        assert_eq!(detached_compose(&argv("docker compose up -d")), None);
    }

    #[test]
    fn watch_modes_are_refused_with_the_one_shot_form() {
        let fixed = |s: &str, tty: bool| match disposition(&argv(s), tty) {
            Disposition::Refuse { fixed, .. } => fixed.map(|f| f.join(" ")),
            other => panic!("{s}: {other:?}"),
        };
        assert_eq!(fixed("npx tsc --watch", false).as_deref(), Some("npx tsc"));
        assert_eq!(fixed("npx tsc -w -p .", false).as_deref(), Some("npx tsc -p ."));
        assert_eq!(fixed("npx jest --watchAll", false).as_deref(), Some("npx jest"));
        assert_eq!(
            fixed("npx vitest watch", false).as_deref(),
            Some("npx vitest run")
        );
        assert_eq!(fixed("vitest", true).as_deref(), Some("vitest run"));
        assert_eq!(
            fixed("npx vitest --coverage", true).as_deref(),
            Some("npx vitest run --coverage")
        );
        assert_eq!(fixed("docker run -it ubuntu", false), None);
        // Not endless: one-shot forms, and vitest without a terminal (it runs once then).
        for ok in [
            "npx tsc -p .",
            "npx vitest run",
            "vitest",
            "docker build -t x .",
            "docker run --rm x",
        ] {
            assert_eq!(disposition(&argv(ok), false), Disposition::Queue, "{ok}");
        }
    }

    #[test]
    fn watch_modes_of_other_ecosystems_get_their_one_shot_form() {
        let fixed = |s: &str| match disposition(&argv(s), false) {
            Disposition::Refuse { fixed, .. } => fixed.map(|f| f.join(" ")),
            other => panic!("{s}: {other:?}"),
        };
        assert_eq!(
            fixed("./gradlew build --continuous").as_deref(),
            Some("./gradlew build")
        );
        assert_eq!(fixed("dotnet watch test").as_deref(), Some("dotnet test"));
        assert_eq!(fixed("npx webpack --watch").as_deref(), Some("npx webpack"));
        assert_eq!(fixed("mix test.watch").as_deref(), Some("mix test"));
        assert_eq!(fixed("turbo watch build").as_deref(), Some("turbo run build"));
        assert_eq!(
            fixed("parcel watch a.html").as_deref(),
            Some("parcel build a.html")
        );
        assert_eq!(
            fixed("podman compose up").as_deref(),
            Some("podman compose up -d")
        );
        assert_eq!(
            fixed("podman-compose up").as_deref(),
            Some("podman-compose up -d")
        );
        assert_eq!(fixed("cargo watch -x test"), None);
        assert_eq!(
            fixed("dotnet watch"),
            None,
            "no one-shot form without a subcommand"
        );
        assert_eq!(fixed("sbt"), None);
        for c in [
            "cargo run",
            "go run .",
            "mvn spring-boot:run",
            "rails server",
            "flutter run",
            "terraform apply",
            "turbo run dev",
            "nx serve web",
            "make dev",
            "sbt run",
            "zig build run",
            "parcel index.html",
            "xcodebuild -list",
        ] {
            assert!(
                matches!(disposition(&argv(c), false), Disposition::Light(_)),
                "{c}"
            );
        }
    }

    #[test]
    fn dev_servers_and_help_run_without_the_queue() {
        for c in [
            "npx vite",
            "npm run dev",
            "pnpm dev",
            "next dev",
            "ng serve",
            "cargo build --help",
            "npx tsc --version",
        ] {
            assert!(
                matches!(disposition(&argv(c), false), Disposition::Light(_)),
                "{c}"
            );
        }
        assert_eq!(disposition(&argv("npx vite build"), false), Disposition::Queue);
    }

    #[test]
    fn project_keys_for_build_tools() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("app");
        std::fs::create_dir_all(root.join("lib/src")).unwrap();
        std::fs::write(root.join("settings.gradle.kts"), "").unwrap();
        std::fs::write(root.join("lib/build.gradle.kts"), "").unwrap();
        std::fs::write(root.join("lib/App.csproj"), "").unwrap();
        std::fs::write(root.join("lib/pom.xml"), "").unwrap();
        let in_lib = root.join("lib/src");
        // Gradle keys on the settings root (multi-project builds share it), not the module.
        let g = exclusive_keys("build", "gradlew", &argv("assembleDebug"), &in_lib);
        assert_eq!(g, exclusive_keys("build", "gradlew", &argv("test"), &root));
        assert!(g[0].starts_with("gradle:"));
        let d = exclusive_keys("build", "dotnet", &argv("build"), &in_lib);
        assert!(d[0].starts_with("dotnet:"), "{d:?}");
        let m = exclusive_keys("build", "mvn", &argv("package"), &in_lib);
        assert!(m[0].starts_with("maven:"), "{m:?}");
        assert!(
            exclusive_keys("build", "make", &argv(""), &in_lib).is_empty(),
            "no Makefile"
        );
    }

    #[test]
    fn wrappers_and_windows_names_are_seen_through() {
        for (cmd, tool, kind) in [
            ("bundle exec rspec", "rspec", "test"),
            ("bundle exec rake test", "rake", "test"),
            ("dotnet tool run dotnet-format", "dotnet-format", ""),
            ("xcrun -sdk iphoneos xcodebuild build", "xcodebuild", "build"),
            ("poetry run pytest", "pytest", "pytest"),
            ("uv run mypy src", "mypy", "typecheck"),
            ("python3 -m mypy src", "mypy", "typecheck"),
            ("npx -y nx@19 run-many -t build", "nx", "monorepo"),
            ("C:\\tools\\cmake.exe --build out", "cmake", "build"),
            ("gradlew.bat assembleRelease", "gradlew", "build"),
            ("mvnw.cmd package", "mvnw", "build"),
            ("ninja.exe", "ninja", "build"),
        ] {
            let d = detect_words(&argv(cmd), &BTreeMap::new());
            assert_eq!(d.tool, tool, "{cmd}");
            assert_eq!(d.kind.as_deref().unwrap_or(""), kind, "{cmd}");
        }
    }

    #[test]
    fn keys_for_node_modules_and_cargo() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("proj");
        std::fs::create_dir_all(proj.join("node_modules")).unwrap();
        std::fs::create_dir_all(proj.join("src/deep")).unwrap();
        let none: Vec<String> = Vec::new();
        let k1 = exclusive_keys("vitest", "vitest", &none, &proj.join("src/deep"));
        assert_eq!(k1.len(), 1);
        assert_eq!(k1, exclusive_keys("vitest", "vitest", &none, &proj));
        assert!(exclusive_keys("eslint", "eslint", &none, &proj).is_empty());
        std::fs::write(proj.join("Cargo.toml"), "").unwrap();
        std::fs::create_dir_all(proj.join("target")).unwrap();
        let ck = exclusive_keys("build", "cargo", &argv("build"), &proj.join("src"));
        assert_eq!(ck.len(), 1);
        assert!(ck[0].starts_with("cargo:"));
    }
}
