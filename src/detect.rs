//! Command analysis: where the program is in a command line, what to call the job, which
//! hints and keys apply, and which commands are refused.
//!
//! Detection looks only at the command position (after env assignments and launchers such as
//! `npx` or `pnpm exec`), so `git commit -m "fix vitest"` is not mistaken for a vitest run.
//! What counts as heavy lives in [`crate::kinds`].

use crate::config::KindConfig;
use crate::kinds::{self, first_non_flag, Hint, KeyRule, Workers};
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
}

impl Detected {
    /// The kind, or `other` for an unknown command.
    pub fn kind_name(&self) -> &str {
        self.kind.as_deref().unwrap_or("other")
    }
}

/// Basename, lowercased, without .cmd/.bat/.exe/.ps1/.js-style extensions.
pub fn tool_name(word: &str) -> String {
    const EXTENSIONS: &[&str] = &[".cmd", ".bat", ".exe", ".ps1", ".js", ".mjs", ".cjs"];
    let base = word
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(word)
        .to_ascii_lowercase();
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

/// Apply the [`Hint`] of the detected tool. `workers` turns a [`Workers`] into a number;
/// `env_set` says whether an environment variable is already set.
pub fn apply_hints(
    argv: &[String],
    tool: &str,
    workers: &dyn Fn(Workers) -> u32,
    env_set: &dyn Fn(&str) -> bool,
) -> HintChanges {
    let mut out = HintChanges::default();
    match kinds::hint_for(tool) {
        Some(Hint::Arg {
            flag,
            opt_out,
            launchers,
            min_args,
            workers: w,
        }) => {
            out.argv = arg_hint(argv, tool, launchers, opt_out, min_args).map(|(mut a, at)| {
                a.insert(at, format!("{flag}{}", workers(w)));
                a
            });
        }
        Some(Hint::Env { var, workers: w }) if !env_set(var) => {
            out.env.push((var.to_string(), workers(w).to_string()))
        }
        _ => {}
    }
    out
}

/// Where to insert an argument hint, if the tool was started directly through one of the
/// launchers and has not opted out. Returns the argv copy and the insert position.
fn arg_hint(
    argv: &[String],
    tool: &str,
    launchers: &[&[&str]],
    opt_out: &[&str],
    min_args: usize,
) -> Option<(Vec<String>, usize)> {
    if is_shell_string(argv) || argv.is_empty() {
        return None;
    }
    let word = |i: usize| {
        if i == 0 {
            tool_name(&argv[0])
        } else {
            argv[i].to_ascii_lowercase()
        }
    };
    let at = launchers
        .iter()
        .find(|l| {
            argv.len() > l.len()
                && l.iter().enumerate().all(|(i, w)| word(i) == *w)
                && tool_name(&argv[l.len()]) == tool
        })?
        .len();
    let rest = &argv[at + 1..];
    let opted_out = rest.iter().any(|w| {
        let w = w.to_ascii_lowercase();
        opt_out.iter().any(|o| w.starts_with(o))
    });
    if opted_out || rest.len() < min_args || kinds::only_info_flags(rest) {
        return None;
    }
    let insert = at + 1 + rest.iter().position(|w| w == "--").unwrap_or(rest.len());
    Some((argv.to_vec(), insert))
}

fn compose_up_attached(words: &[String]) -> bool {
    let Some(p) = parse(words) else {
        return false;
    };
    let after = match p.tool.as_str() {
        "docker-compose" => &p.rest[..],
        "docker" => match kinds::after_docker_compose(&p.rest) {
            Some(a) => a,
            None => return false,
        },
        _ => return false,
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

/// A reason to refuse the command outright, if any.
pub fn refuse_reason(argv: &[String]) -> Option<String> {
    let attached = if is_shell_string(argv) {
        split_segments(&argv[0])
            .into_iter()
            .any(|r| compose_up_attached(&tokenize(&argv[0][r])))
    } else {
        compose_up_attached(argv)
    };
    attached.then(|| {
        "`docker compose up` without -d would hold queue points forever; \
         run it detached (`docker compose up -d`) or outside calmrun"
            .to_string()
    })
}

/// The same compose command with `-d` added after `up` (argv words only).
pub fn detached_compose(argv: &[String]) -> Option<Vec<String>> {
    if is_shell_string(argv) || !compose_up_attached(argv) {
        return None;
    }
    let start = argv
        .iter()
        .position(|w| matches!(tool_name(w).as_str(), "docker" | "docker-compose"))?;
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

/// Exclusive keys of a job of `kind` run by `tool` in `cwd`: two jobs sharing a key never
/// run together.
pub fn exclusive_keys(kind: &str, tool: &str, cwd: &Path) -> Vec<String> {
    match kinds::key_rule(kind, tool) {
        KeyRule::None => Vec::new(),
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
        let a = argv(s);
        let tool = detect_words(&a, &BTreeMap::new()).tool;
        apply_hints(&a, &tool, &|_| n, &|_| false).argv
    }

    #[test]
    fn tool_names() {
        assert_eq!(tool_name("C:\\Program Files\\nodejs\\npx.cmd"), "npx");
        assert_eq!(tool_name("./node_modules/.bin/vitest"), "vitest");
        assert_eq!(tool_name("ESLINT.EXE"), "eslint");
        assert_eq!(tool_name("run.ps1"), "run");
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
        assert_eq!(hinted("FOO=1 vitest", 1), None);
        let shell = vec!["npx vitest run".to_string()];
        assert_eq!(apply_hints(&shell, "vitest", &|_| 1, &|_| false).argv, None);
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
        assert_eq!(hinted("mvn package", 4), None);
        let c = apply_hints(&argv("cargo build"), "cargo", &|_| 5, &|_| false);
        assert_eq!(c.env, vec![("CARGO_BUILD_JOBS".to_string(), "5".to_string())]);
        assert!(apply_hints(&argv("cargo build"), "cargo", &|_| 5, &|_| true)
            .env
            .is_empty());
    }

    #[test]
    fn compose_up_refused() {
        assert!(refuse_reason(&argv("docker compose up")).is_some());
        assert!(refuse_reason(&argv("docker-compose -f x.yml up --build")).is_some());
        assert!(refuse_reason(&["cd a && docker compose up".to_string()]).is_some());
        assert!(refuse_reason(&argv("docker compose up -d")).is_none());
        assert!(refuse_reason(&argv("docker compose up --detach")).is_none());
        assert!(refuse_reason(&argv("docker-compose up -dV")).is_none());
        assert!(refuse_reason(&argv("docker compose up --wait")).is_none());
        assert!(refuse_reason(&argv("docker compose build")).is_none());
        assert_eq!(
            detached_compose(&argv("docker compose -f a.yml up --build")),
            Some(argv("docker compose -f a.yml up -d --build"))
        );
        assert_eq!(detached_compose(&argv("docker compose up -d")), None);
    }

    #[test]
    fn keys_for_node_modules_and_cargo() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("proj");
        std::fs::create_dir_all(proj.join("node_modules")).unwrap();
        std::fs::create_dir_all(proj.join("src/deep")).unwrap();
        let k1 = exclusive_keys("vitest", "vitest", &proj.join("src/deep"));
        assert_eq!(k1.len(), 1);
        assert_eq!(k1, exclusive_keys("vitest", "vitest", &proj));
        assert!(exclusive_keys("eslint", "eslint", &proj).is_empty());
        std::fs::write(proj.join("Cargo.toml"), "").unwrap();
        std::fs::create_dir_all(proj.join("target")).unwrap();
        let ck = exclusive_keys("build", "cargo", &proj.join("src"));
        assert_eq!(ck.len(), 1);
        assert!(ck[0].starts_with("cargo:"));
    }
}
