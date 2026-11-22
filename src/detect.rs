//! Command analysis: kind, label, exclusive keys and argv rules.
//!
//! Detection looks only at the command position (after env assignments and launchers such as
//! `npx` or `pnpm exec`), so `git commit -m "fix vitest"` is not mistaken for a vitest run.

use crate::config::KindConfig;
use std::collections::BTreeMap;
use std::ops::Range;
use std::path::{Path, PathBuf};

pub const BUILTIN_KINDS: &[&str] = &[
    "vitest", "jest", "tsc", "capture", "docker", "eslint", "pytest", "build", "test",
];

/// Result of analysing one command.
#[derive(Debug, Clone, PartialEq)]
pub struct Detected {
    /// Heavy kind, or `None` for a command calmrun does not know.
    pub kind: Option<String>,
    /// Tool + subcommand only, never arguments.
    pub label: String,
    /// Normalised tool name (basename without extension).
    pub tool: String,
}

impl Detected {
    pub fn kind_name(&self) -> &str {
        self.kind.as_deref().unwrap_or("other")
    }
}

pub fn default_cost(kind: &str) -> u32 {
    match kind {
        "vitest" | "jest" | "tsc" | "build" | "capture" | "docker" => 2,
        _ => 1,
    }
}

pub fn kind_cost(kind: &str, kinds: &BTreeMap<String, KindConfig>) -> u32 {
    kinds
        .get(kind)
        .and_then(|k| k.cost)
        .unwrap_or_else(|| default_cost(kind))
}

/// Basename, lowercased, without .cmd/.bat/.exe/.ps1/.js-style extensions.
pub fn tool_name(word: &str) -> String {
    let base = word
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(word)
        .to_ascii_lowercase();
    for ext in [".cmd", ".bat", ".exe", ".ps1", ".js", ".mjs", ".cjs"] {
        if let Some(stripped) = base.strip_suffix(ext) {
            if !stripped.is_empty() {
                return stripped.to_string();
            }
        }
    }
    base
}

fn is_assignment(w: &str) -> bool {
    match w.split_once('=') {
        Some((name, _)) => {
            !name.is_empty()
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && !name.starts_with(|c: char| c.is_ascii_digit())
        }
        None => false,
    }
}

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

pub fn parse(words: &[String]) -> Option<Parsed> {
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
            continue;
        }
        if t == "nice" {
            i = skip_flags(words, i + 1, &["-n"]);
            continue;
        }
        break;
    }
    let start = i;
    let first = tool_name(&words[i]);
    let next = words.get(i + 1).map(|s| s.to_ascii_lowercase());
    let mut launcher = Vec::new();
    let tool_idx;
    match (first.as_str(), next.as_deref()) {
        ("npx" | "pnpx" | "bunx", _) => {
            launcher.push(first.clone());
            tool_idx = skip_flags(words, i + 1, &["-p", "--package"]);
        }
        ("pnpm" | "yarn" | "npm", Some(sub @ ("exec" | "dlx" | "x"))) | ("bun", Some(sub @ "x")) => {
            launcher.push(first.clone());
            launcher.push(sub.to_string());
            let j = skip_flags(words, i + 2, &["-p", "--package"]);
            tool_idx = if words.get(j).map(String::as_str) == Some("--") {
                j + 1
            } else {
                j
            };
        }
        ("pnpm" | "yarn", Some(n)) if JS_BINS.contains(&n) => {
            launcher.push(first.clone());
            tool_idx = i + 1;
        }
        ("uv" | "poetry" | "pipenv" | "pdm" | "hatch", Some("run")) => {
            let inner = parse(&words[i + 2..])?;
            let mut l = vec![first.clone(), "run".to_string()];
            l.extend(inner.launcher);
            return Some(Parsed {
                launcher: l,
                tool: inner.tool,
                rest: inner.rest,
                start,
            });
        }
        ("python" | "python3" | "py", _) => {
            let m = words[i + 1..]
                .iter()
                .take_while(|w| w.starts_with('-'))
                .position(|w| w == "-m");
            if let Some(p) = m {
                launcher.push(first.clone());
                launcher.push("-m".to_string());
                tool_idx = i + 1 + p + 1;
            } else {
                tool_idx = i;
            }
        }
        ("node" | "bun" | "deno", Some(n)) if looks_like_script(n) => {
            launcher.push(first.clone());
            tool_idx = i + 1;
        }
        _ => tool_idx = i,
    }
    if tool_idx >= words.len() {
        // A bare launcher such as `npx --version`: the launcher is the tool.
        return Some(Parsed {
            launcher: Vec::new(),
            tool: first,
            rest: words[i + 1..].to_vec(),
            start,
        });
    }
    let tool = tool_name(&words[tool_idx]);
    Some(Parsed {
        launcher,
        tool,
        rest: words[tool_idx + 1..].to_vec(),
        start,
    })
}

fn looks_like_script(w: &str) -> bool {
    [".js", ".mjs", ".cjs", ".ts"].iter().any(|e| w.ends_with(e)) && (w.contains('/') || w.contains('\\'))
}

fn first_non_flag(rest: &[String]) -> Option<&str> {
    rest.iter().map(String::as_str).find(|w| !w.starts_with('-'))
}

fn only_info_flags(rest: &[String]) -> bool {
    !rest.is_empty()
        && rest.iter().all(|w| {
            matches!(
                w.as_str(),
                "--version" | "-v" | "-V" | "--help" | "-h" | "version" | "help"
            )
        })
}

fn script_kind(name: &str) -> Option<&'static str> {
    let base = name.split(':').next().unwrap_or(name);
    match base {
        "test" | "t" => Some("test"),
        "build" => Some("build"),
        "lint" => Some("eslint"),
        "typecheck" | "type-check" | "tsc" => Some("tsc"),
        _ => None,
    }
}

/// Compose subcommand after compose's own flags (`-f file`, `-p name`, `--profile x`).
fn compose_sub(rest: &[String]) -> Option<String> {
    let mut i = 0;
    while i < rest.len() {
        let w = &rest[i];
        if w.starts_with('-') {
            if matches!(
                w.as_str(),
                "-f" | "--file"
                    | "-p"
                    | "--project-name"
                    | "--profile"
                    | "--env-file"
                    | "--project-directory"
            ) {
                i += 1;
            }
            i += 1;
            continue;
        }
        return Some(w.to_ascii_lowercase());
    }
    None
}

fn classify_builtin(tool: &str, rest: &[String]) -> Option<&'static str> {
    if only_info_flags(rest) {
        return None;
    }
    let sub = first_non_flag(rest).map(str::to_ascii_lowercase);
    let sub = sub.as_deref();
    match tool {
        "vitest" => Some("vitest"),
        "jest" => Some("jest"),
        "tsc" | "vue-tsc" | "tsgo" => Some("tsc"),
        "playwright" | "puppeteer" | "cypress" => Some("capture"),
        "eslint" | "oxlint" | "biome" => Some("eslint"),
        "pytest" | "py.test" => Some("pytest"),
        "webpack" | "rollup" | "esbuild" | "gradle" | "gradlew" | "mvn" | "mvnw" => Some("build"),
        "vite" | "next" | "ng" if sub == Some("build") => Some("build"),
        "docker-compose" => match compose_sub(rest).as_deref() {
            Some("up" | "build" | "run" | "create") => Some("docker"),
            _ => None,
        },
        "docker" => match sub {
            Some("compose") => {
                let pos = rest.iter().position(|w| w == "compose").unwrap_or(0);
                match compose_sub(&rest[pos + 1..]).as_deref() {
                    Some("up" | "build" | "run" | "create") => Some("docker"),
                    _ => None,
                }
            }
            Some("build" | "run" | "buildx") => Some("docker"),
            _ => None,
        },
        "cargo" => match sub {
            Some("build" | "b" | "check" | "c" | "clippy") => Some("build"),
            Some("test" | "t" | "nextest") => Some("test"),
            _ => None,
        },
        "flutter" | "dotnet" | "go" => match sub {
            Some("build") => Some("build"),
            Some("test") => Some("test"),
            Some("publish") if tool == "dotnet" => Some("build"),
            _ => None,
        },
        "npm" | "pnpm" | "yarn" | "bun" => match sub {
            Some("run" | "run-script") => {
                let pos = rest
                    .iter()
                    .position(|w| w == "run" || w == "run-script")
                    .unwrap_or(0);
                first_non_flag(&rest[pos + 1..]).and_then(script_kind)
            }
            Some("test" | "t") => Some("test"),
            Some(s) if tool != "npm" => script_kind(s),
            _ => None,
        },
        _ => None,
    }
}

fn classify(p: &Parsed, kinds: &BTreeMap<String, KindConfig>) -> Option<String> {
    for (name, kc) in kinds {
        for pat in &kc.patterns {
            let mut pw = pat.split_whitespace();
            let Some(head) = pw.next() else { continue };
            if tool_name(head) != p.tool {
                continue;
            }
            let tail: Vec<&str> = pw.collect();
            if tail.len() <= p.rest.len() && tail.iter().zip(&p.rest).all(|(a, b)| a.eq_ignore_ascii_case(b))
            {
                return Some(name.clone());
            }
        }
    }
    classify_builtin(&p.tool, &p.rest).map(str::to_string)
}

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

fn safe_word(w: &str) -> bool {
    let mut chars = w.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
        && w.len() <= 24
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | ':' | '_'))
}

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
            .map(String::as_str)
            .unwrap_or("");
        let two_word = matches!(
            (p.tool.as_str(), sub),
            ("npm" | "pnpm" | "yarn" | "bun", "run" | "run-script") | ("docker", "compose" | "buildx")
        );
        if two_word && safe_word(second) {
            parts.push(second.to_string());
        }
    }
    parts.join(" ")
}

/// Analyse a list of argv words (one segment).
pub fn detect_words(words: &[String], kinds: &BTreeMap<String, KindConfig>) -> Detected {
    match parse(words) {
        Some(p) => Detected {
            kind: classify(&p, kinds),
            label: label_of(&p),
            tool: p.tool,
        },
        None => Detected {
            kind: None,
            label: String::new(),
            tool: String::new(),
        },
    }
}

/// True when the argv is one shell string ("npm ci && npm test") rather than separate words.
pub fn is_shell_string(argv: &[String]) -> bool {
    argv.len() == 1 && argv[0].trim().contains(char::is_whitespace)
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
            let c = kind_cost(k, kinds);
            if best.as_ref().is_none_or(|(bc, _)| c > *bc) {
                best = Some((c, d.clone()));
            }
        }
        first.get_or_insert(d);
    }
    best.map(|(_, d)| d).or(first).unwrap_or(Detected {
        kind: None,
        label: String::new(),
        tool: String::new(),
    })
}

/// Split shell words. Quotes group words; backslash escapes only `\"` inside double quotes
/// and `\ ` outside quotes, so Windows paths keep their backslashes.
pub fn tokenize(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut has = false;
    let mut quote: Option<char> = None;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else if c == '\\' && q == '"' && chars.peek() == Some(&'"') {
                    cur.push(chars.next().unwrap_or('"'));
                } else {
                    cur.push(c);
                }
            }
            None => match c {
                '"' | '\'' => {
                    quote = Some(c);
                    has = true;
                }
                '\\' if chars.peek() == Some(&' ') => {
                    cur.push(' ');
                    chars.next();
                    has = true;
                }
                c if c.is_whitespace() => {
                    if has {
                        out.push(std::mem::take(&mut cur));
                        has = false;
                    }
                }
                _ => {
                    cur.push(c);
                    has = true;
                }
            },
        }
    }
    if has {
        out.push(cur);
    }
    out
}

/// Byte ranges of the command segments separated by `&&`, `||`, `;`, `|`, `&` and newlines,
/// outside quotes. Ranges are trimmed of whitespace and grouping parentheses/braces.
pub fn split_segments(s: &str) -> Vec<Range<usize>> {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut quote: Option<u8> = None;
    let mut seg_start = 0;
    let mut i = 0;
    let push = |out: &mut Vec<Range<usize>>, a: usize, b: usize| {
        let mut a = a;
        let mut b = b;
        while a < b && (bytes[a].is_ascii_whitespace() || matches!(bytes[a], b'(' | b'{')) {
            a += 1;
        }
        while b > a && (bytes[b - 1].is_ascii_whitespace() || matches!(bytes[b - 1], b')' | b'}')) {
            b -= 1;
        }
        if a < b {
            out.push(a..b);
        }
    };
    // Heredoc terminators waiting for the end of the current line.
    let mut pending: Vec<String> = Vec::new();
    while i < bytes.len() {
        let c = bytes[i];
        // Heredocs (`<<EOF`, `<<-'EOF'`) are found in any quote state, so a heredoc inside
        // "$(cat <<'EOF' ... )" is skipped too. Their bodies are data, never commands.
        if c == b'<' && bytes.get(i + 1) == Some(&b'<') && bytes.get(i + 2) != Some(&b'<') {
            if let Some((delim, used)) = heredoc_delimiter(&s[i + 2..]) {
                pending.push(delim);
                i += 2 + used;
                continue;
            }
        }
        // PowerShell here-strings: `@'` / `@"` at a line end, closed by `'@` / `"@` at the
        // start of a later line. The body is one string argument, never commands.
        if c == b'@' && matches!(bytes.get(i + 1), Some(b'\'' | b'"')) {
            let q = bytes[i + 1];
            let line_end = s[i + 2..].find('\n').map(|e| i + 2 + e);
            if let Some(le) = line_end.filter(|&le| s[i + 2..le].trim().is_empty()) {
                let close = [q, b'@'];
                let mut pos = le + 1;
                let mut found = None;
                while pos < bytes.len() {
                    if bytes[pos..].starts_with(&close) {
                        found = Some(pos + 2);
                        break;
                    }
                    pos = s[pos..].find('\n').map_or(bytes.len(), |e| pos + e + 1);
                }
                i = found.unwrap_or(bytes.len());
                continue;
            }
        }
        if c == b'\n' && !pending.is_empty() {
            if quote.is_none() {
                push(&mut out, seg_start, i);
            }
            let mut pos = i + 1;
            for d in pending.drain(..) {
                while pos < bytes.len() {
                    let end = s[pos..].find('\n').map_or(bytes.len(), |e| pos + e);
                    let line = s[pos..end].trim_start_matches('\t').trim_end();
                    pos = end + 1;
                    if line == d {
                        break;
                    }
                }
            }
            i = pos.min(bytes.len());
            if quote.is_none() {
                seg_start = i;
            }
            continue;
        }
        if let Some(q) = quote {
            if c == b'\\' && q == b'"' {
                i += 2;
                continue;
            }
            if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match c {
            b'"' | b'\'' => quote = Some(c),
            b';' | b'\n' | b'\r' => {
                push(&mut out, seg_start, i);
                seg_start = i + 1;
            }
            b'|' | b'&' => {
                let prev = if i > 0 { bytes[i - 1] } else { 0 };
                let next = bytes.get(i + 1).copied().unwrap_or(0);
                // Redirections such as 2>&1, &>file, >&2 are not separators.
                if c == b'&' && (prev == b'>' || prev == b'<' || next == b'>') {
                    i += 1;
                    continue;
                }
                push(&mut out, seg_start, i);
                let len = if next == c { 2 } else { 1 };
                i += len;
                seg_start = i;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    push(&mut out, seg_start, bytes.len());
    out
}

/// The delimiter word after `<<` (with optional `-` and quotes) and the bytes it used.
fn heredoc_delimiter(rest: &str) -> Option<(String, usize)> {
    let b = rest.as_bytes();
    let mut i = 0;
    if b.first() == Some(&b'-') {
        i += 1;
    }
    while i < b.len() && (b[i] == b' ' || b[i] == b'\t') {
        i += 1;
    }
    let mut word = String::new();
    while i < b.len() {
        let c = b[i];
        if c == b'\'' || c == b'"' {
            let close = rest[i + 1..].find(c as char)? + i + 1;
            word.push_str(&rest[i + 1..close]);
            i = close + 1;
        } else if c == b'\\' {
            i += 1;
        } else if c.is_ascii_whitespace() || b";|&<>()`".contains(&c) {
            break;
        } else {
            word.push(c as char);
            i += 1;
        }
    }
    (!word.is_empty()).then_some((word, i))
}

/// `--maxWorkers=N` rule for vitest invoked directly as separate argv words.
/// Returns the new argv, or `None` when the rule does not apply.
pub fn vitest_args(argv: &[String], workers: u32) -> Option<Vec<String>> {
    if is_shell_string(argv) || argv.is_empty() {
        return None;
    }
    let lower: Vec<String> = argv.iter().map(|w| w.to_ascii_lowercase()).collect();
    let t0 = tool_name(&argv[0]);
    let vitest_at = if t0 == "vitest" {
        0
    } else if matches!(t0.as_str(), "npx" | "pnpx" | "bunx")
        && lower.get(1).map(|w| tool_name(w)) == Some("vitest".into())
    {
        1
    } else if matches!(t0.as_str(), "pnpm" | "yarn")
        && lower.get(1).map(String::as_str) == Some("exec")
        && lower.get(2).map(|w| tool_name(w)) == Some("vitest".into())
    {
        2
    } else {
        return None;
    };
    let rest = &argv[vitest_at + 1..];
    let opted_out = rest.iter().any(|w| {
        let w = w.to_ascii_lowercase();
        w.starts_with("--maxworkers")
            || w.starts_with("--max-workers")
            || w.starts_with("--pool")
            || w == "--no-file-parallelism"
    });
    if opted_out || only_info_flags(rest) {
        return None;
    }
    let mut out = argv.to_vec();
    let flag = format!("--maxWorkers={workers}");
    match rest.iter().position(|w| w == "--") {
        Some(p) => out.insert(vitest_at + 1 + p, flag),
        None => out.push(flag),
    }
    Some(out)
}

fn compose_up_attached(words: &[String]) -> bool {
    let Some(p) = parse(words) else { return false };
    let after = match p.tool.as_str() {
        "docker-compose" => &p.rest[..],
        "docker" if first_non_flag(&p.rest) == Some("compose") => {
            let pos = p.rest.iter().position(|w| w == "compose").unwrap_or(0);
            &p.rest[pos + 1..]
        }
        _ => return false,
    };
    if compose_sub(after).as_deref() != Some("up") {
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
    let start = argv.iter().position(|w| {
        let t = tool_name(w);
        t == "docker" || t == "docker-compose"
    })?;
    let up = start + argv[start..].iter().position(|w| w == "up")?;
    let mut out = argv.to_vec();
    out.insert(up + 1, "-d".to_string());
    Some(out)
}

/// FNV-1a 64-bit, stable across builds and platforms.
pub fn fnv1a(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

fn norm_key(prefix: &str, p: &Path) -> String {
    let real = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let mut s = real.to_string_lossy().to_string();
    if cfg!(windows) {
        s = s.to_lowercase();
    }
    format!("{prefix}:{}", fnv1a(&s))
}

fn nearest(cwd: &Path, test: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    cwd.ancestors().find(|d| test(d)).map(Path::to_path_buf)
}

/// Exclusive keys: two jobs sharing a key never run together.
pub fn exclusive_keys(d: &Detected, cwd: &Path) -> Vec<String> {
    let kind = d.kind_name();
    if d.tool == "cargo" && matches!(kind, "build" | "test") {
        if let Some(t) = std::env::var_os("CARGO_TARGET_DIR").filter(|v| !v.is_empty()) {
            return vec![norm_key("cargo", Path::new(&t))];
        }
        let dir = nearest(cwd, |d| {
            d.join("Cargo.toml").is_file() && d.join("target").is_dir()
        })
        .or_else(|| nearest(cwd, |d| d.join("Cargo.toml").is_file()));
        return dir
            .map(|d| vec![norm_key("cargo", &d.join("target"))])
            .unwrap_or_default();
    }
    if matches!(kind, "vitest" | "jest" | "build") {
        if let Some(d) = nearest(cwd, |d| d.join("node_modules").is_dir()) {
            return vec![norm_key("nm", &d.join("node_modules"))];
        }
    }
    Vec::new()
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

    #[test]
    fn tool_names() {
        assert_eq!(tool_name("C:\\Program Files\\nodejs\\npx.cmd"), "npx");
        assert_eq!(tool_name("./node_modules/.bin/vitest"), "vitest");
        assert_eq!(tool_name("ESLINT.EXE"), "eslint");
        assert_eq!(tool_name("run.ps1"), "run");
    }

    #[test]
    fn kinds_detected() {
        assert_eq!(kind("vitest run").as_deref(), Some("vitest"));
        assert_eq!(kind("npx vitest").as_deref(), Some("vitest"));
        assert_eq!(kind("npx -y jest --ci").as_deref(), Some("jest"));
        assert_eq!(kind("pnpm exec vue-tsc --noEmit").as_deref(), Some("tsc"));
        assert_eq!(kind("pnpm tsgo").as_deref(), Some("tsc"));
        assert_eq!(kind("npx playwright test").as_deref(), Some("capture"));
        assert_eq!(kind("docker compose -f a.yml up -d").as_deref(), Some("docker"));
        assert_eq!(kind("docker-compose build").as_deref(), Some("docker"));
        assert_eq!(kind("docker build .").as_deref(), Some("docker"));
        assert_eq!(kind("npx biome check .").as_deref(), Some("eslint"));
        assert_eq!(kind("python -m pytest -q").as_deref(), Some("pytest"));
        assert_eq!(kind("uv run pytest").as_deref(), Some("pytest"));
        assert_eq!(kind("npx vite build").as_deref(), Some("build"));
        assert_eq!(kind("./gradlew assembleDebug").as_deref(), Some("build"));
        assert_eq!(kind("cargo build --release").as_deref(), Some("build"));
        assert_eq!(kind("flutter build apk").as_deref(), Some("build"));
        assert_eq!(kind("dotnet build").as_deref(), Some("build"));
        assert_eq!(kind("cargo test").as_deref(), Some("test"));
        assert_eq!(kind("go test ./...").as_deref(), Some("test"));
        assert_eq!(kind("npm test").as_deref(), Some("test"));
        assert_eq!(kind("npm run build").as_deref(), Some("build"));
        assert_eq!(kind("pnpm lint").as_deref(), Some("eslint"));
        assert_eq!(kind("FOO=1 CI=true npx vitest").as_deref(), Some("vitest"));
        assert_eq!(
            kind("node ./node_modules/vitest/vitest.mjs run").as_deref(),
            Some("vitest")
        );
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
    fn custom_patterns_and_costs() {
        let mut kinds = BTreeMap::new();
        kinds.insert(
            "heavy".to_string(),
            KindConfig {
                cost: Some(3),
                patterns: vec!["make".into(), "bazel build".into()],
            },
        );
        kinds.insert(
            "vitest".to_string(),
            KindConfig {
                cost: Some(4),
                patterns: vec![],
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
        assert_eq!(kind_cost("heavy", &kinds), 3);
        assert_eq!(kind_cost("vitest", &kinds), 4);
        assert_eq!(kind_cost("jest", &kinds), 2);
        assert_eq!(kind_cost("eslint", &kinds), 1);
        assert_eq!(kind_cost("other", &kinds), 1);
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
    fn segments() {
        let s = "cd a && npx vitest run 2>&1 | tee \"x|y\"; echo 'a;b' & ls";
        let parts: Vec<&str> = split_segments(s).into_iter().map(|r| &s[r]).collect();
        assert_eq!(
            parts,
            vec!["cd a", "npx vitest run 2>&1", "tee \"x|y\"", "echo 'a;b'", "ls"]
        );
        let s = "(cd x && vitest)";
        let parts: Vec<&str> = split_segments(s).into_iter().map(|r| &s[r]).collect();
        assert_eq!(parts, vec!["cd x", "vitest"]);
    }

    #[test]
    fn heredoc_bodies_are_not_commands() {
        let s = "cat > run.sh <<'EOF'\nnpm test\nnpx vitest run\nEOF\nls";
        let parts: Vec<&str> = split_segments(s).into_iter().map(|r| &s[r]).collect();
        assert_eq!(parts, vec!["cat > run.sh <<'EOF'", "ls"]);
        let s = "git commit -m \"$(cat <<'EOF'\nfix: run npm test in CI\n\nnpx vitest \"x\"\nEOF\n)\" && git push";
        let parts: Vec<&str> = split_segments(s).into_iter().map(|r| &s[r]).collect();
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert!(parts[0].starts_with("git commit"));
        assert_eq!(parts[1], "git push");
        let s = "git commit -m @'\nfix: don't skip\n\neslint cleanup\n'@; npm test";
        let parts: Vec<&str> = split_segments(s).into_iter().map(|r| &s[r]).collect();
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert!(parts[0].starts_with("git commit") && parts[0].ends_with("'@"));
        assert_eq!(parts[1], "npm test");
        let s = "cat <<-END >x\n\tcargo build\n\tEND\ncargo test";
        let parts: Vec<&str> = split_segments(s).into_iter().map(|r| &s[r]).collect();
        assert_eq!(parts, vec!["cat <<-END >x", "cargo test"]);
    }

    #[test]
    fn tokenize_quotes() {
        assert_eq!(
            tokenize(r#"a "b c" 'd e' C:\x\y "q\"t""#),
            vec!["a", "b c", "d e", r"C:\x\y", "q\"t"]
        );
    }

    fn argv(s: &str) -> Vec<String> {
        s.split(' ').map(String::from).collect()
    }

    #[test]
    fn vitest_rule() {
        assert_eq!(
            vitest_args(&argv("vitest run"), 2),
            Some(argv("vitest run --maxWorkers=2"))
        );
        assert_eq!(
            vitest_args(&argv("npx vitest"), 1),
            Some(argv("npx vitest --maxWorkers=1"))
        );
        assert_eq!(
            vitest_args(&argv("pnpm exec vitest run"), 3),
            Some(argv("pnpm exec vitest run --maxWorkers=3"))
        );
        assert_eq!(
            vitest_args(&argv("yarn exec vitest -- a"), 1),
            Some(argv("yarn exec vitest --maxWorkers=1 -- a"))
        );
        assert_eq!(vitest_args(&argv("vitest --maxWorkers=4"), 1), None);
        assert_eq!(vitest_args(&argv("vitest --pool=forks"), 1), None);
        assert_eq!(vitest_args(&argv("vitest --no-file-parallelism"), 1), None);
        assert_eq!(vitest_args(&argv("npm run test"), 1), None);
        assert_eq!(vitest_args(&argv("npx jest"), 1), None);
        assert_eq!(vitest_args(&["npx vitest run".to_string()], 1), None);
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
        let d = detect_words(&w("npx vitest"), &BTreeMap::new());
        let k1 = exclusive_keys(&d, &proj.join("src/deep"));
        let k2 = exclusive_keys(&d, &proj);
        assert_eq!(k1.len(), 1);
        assert_eq!(k1, k2);
        let lint = detect_words(&w("npx eslint ."), &BTreeMap::new());
        assert!(exclusive_keys(&lint, &proj).is_empty());

        std::fs::write(proj.join("Cargo.toml"), "").unwrap();
        std::fs::create_dir_all(proj.join("target")).unwrap();
        let c = detect_words(&w("cargo build"), &BTreeMap::new());
        let ck = exclusive_keys(&c, &proj.join("src"));
        assert_eq!(ck.len(), 1);
        assert!(ck[0].starts_with("cargo:"));
    }
}
