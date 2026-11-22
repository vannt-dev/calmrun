//! The one place that knows about tools: which commands are heavy, what they cost, which
//! folder they must not share, and how to size their workers.
//!
//! Adding a tool is one entry in [`RULES`] (and, for a new kind, one in [`KINDS`]). Nothing
//! else in the crate matches on tool names.

use crate::config::KindConfig;
use std::collections::BTreeMap;
use std::path::Path;

/// A kind of job and its defaults.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KindSpec {
    pub name: &'static str,
    /// Budget points when neither `--cost` nor the config says otherwise.
    pub cost: u32,
    /// Exclusive key every job of this kind takes, unless its rule says otherwise.
    pub key: KeyRule,
}

/// Which folder a job must not share with another job of the same key.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum KeyRule {
    None,
    /// The nearest `node_modules` folder above the working directory.
    NodeModules,
    /// `CARGO_TARGET_DIR`, or the `target` folder of the nearest Cargo project.
    CargoTarget,
}

/// When a rule matches, given the words after the tool.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum When {
    /// Any arguments.
    Always,
    /// The first non-flag word is one of these.
    Sub(&'static [&'static str]),
    /// `docker-compose [flags] <sub>` with `<sub>` in the list.
    Compose(&'static [&'static str]),
    /// `docker [flags] compose [flags] <sub>` with `<sub>` in the list.
    DockerCompose(&'static [&'static str]),
    /// A package-manager script: `run <name>` / `run-script <name>` (also `<name>:suffix`), or
    /// `<name>` directly for pnpm, yarn and bun (npm only runs its own commands directly).
    Script(&'static [&'static str]),
    /// No subcommand at all, or one of these.
    NoSubOr(&'static [&'static str]),
}

/// How many workers a hint asks for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Workers {
    /// `vitest_workers` from the config.
    Vitest,
    /// The job's share of the cores: logical cores × cost ÷ budget, at least 1.
    CoreShare,
}

/// A change calmrun makes so a tool does not take the whole machine.
#[derive(Debug, Clone, Copy)]
pub enum Hint {
    /// Append `<flag><N>` when the tool was started directly as separate words through one of
    /// `launchers` (an empty launcher means the tool itself is the program). Skipped when any
    /// argument starts with one of `opt_out` (compared lowercase) or there are fewer than
    /// `min_args` arguments. Inserted before a `--` if there is one.
    Arg {
        flag: &'static str,
        opt_out: &'static [&'static str],
        launchers: &'static [&'static [&'static str]],
        min_args: usize,
        workers: Workers,
    },
    /// Set this environment variable unless it is already set. `user_limit` reads a limit
    /// the user configured elsewhere (from the working directory); the lower one wins.
    Env {
        var: &'static str,
        workers: Workers,
        user_limit: Option<fn(&Path) -> Option<u32>>,
    },
}

/// A detection rule: these tools, under this condition, are this kind.
#[derive(Debug, Clone, Copy)]
pub struct Rule {
    pub tools: &'static [&'static str],
    pub when: When,
    pub kind: &'static str,
    /// Overrides the kind's key for these tools.
    pub key: Option<KeyRule>,
    pub hint: Option<Hint>,
}

const fn rule(tools: &'static [&'static str], when: When, kind: &'static str) -> Rule {
    Rule {
        tools,
        when,
        kind,
        key: None,
        hint: None,
    }
}

/// Built-in kinds.
pub const KINDS: &[KindSpec] = &[
    KindSpec {
        name: "vitest",
        cost: 2,
        key: KeyRule::NodeModules,
    },
    KindSpec {
        name: "jest",
        cost: 2,
        key: KeyRule::NodeModules,
    },
    KindSpec {
        name: "tsc",
        cost: 2,
        key: KeyRule::None,
    },
    KindSpec {
        name: "capture",
        cost: 2,
        key: KeyRule::None,
    },
    KindSpec {
        name: "docker",
        cost: 2,
        key: KeyRule::None,
    },
    KindSpec {
        name: "eslint",
        cost: 1,
        key: KeyRule::None,
    },
    KindSpec {
        name: "pytest",
        cost: 1,
        key: KeyRule::None,
    },
    KindSpec {
        name: "build",
        cost: 2,
        key: KeyRule::NodeModules,
    },
    KindSpec {
        name: "test",
        cost: 1,
        key: KeyRule::None,
    },
];

/// Cost of a kind nobody knows (and of `other`).
pub const UNKNOWN_COST: u32 = 1;

const COMPOSE_HEAVY: &[&str] = &["up", "build", "run", "create"];
const PACKAGE_MANAGERS: &[&str] = &["npm", "pnpm", "yarn", "bun"];

/// Vitest gets a small worker count when started directly.
const VITEST_HINT: Hint = Hint::Arg {
    flag: "--maxWorkers=",
    opt_out: &["--maxworkers", "--max-workers", "--pool", "--no-file-parallelism"],
    launchers: &[
        &[],
        &["npx"],
        &["pnpx"],
        &["bunx"],
        &["pnpm", "exec"],
        &["yarn", "exec"],
    ],
    min_args: 0,
    workers: Workers::Vitest,
};
const GRADLE_HINT: Hint = Hint::Arg {
    flag: "--max-workers=",
    opt_out: &["--max-workers", "-dorg.gradle.workers.max"],
    launchers: &[&[]],
    min_args: 1,
    workers: Workers::CoreShare,
};
const CARGO_HINT: Hint = Hint::Env {
    var: "CARGO_BUILD_JOBS",
    workers: Workers::CoreShare,
    user_limit: Some(cargo_config_jobs),
};

/// `build.jobs` from the cargo config that applies in `cwd`: the nearest `.cargo/config.toml`
/// (or `.cargo/config`) up the tree, then the one in `CARGO_HOME` (default `~/.cargo`).
pub fn cargo_config_jobs(cwd: &Path) -> Option<u32> {
    let home = std::env::var_os("CARGO_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".cargo")));
    let project = cwd.ancestors().map(|d| d.join(".cargo"));
    project.chain(home).find_map(|dir| {
        ["config.toml", "config"].iter().find_map(|f| {
            let text = std::fs::read_to_string(dir.join(f)).ok()?;
            let v: toml::Value = toml::from_str(&text).ok()?;
            v.get("build")?
                .get("jobs")?
                .as_integer()
                .and_then(|n| u32::try_from(n).ok())
                .filter(|n| *n > 0)
        })
    })
}

/// Detection rules, tried in order after the user's own `[kinds]` patterns.
pub const RULES: &[Rule] = &[
    Rule {
        hint: Some(VITEST_HINT),
        ..rule(&["vitest"], When::Always, "vitest")
    },
    rule(&["jest"], When::Always, "jest"),
    rule(&["tsc", "vue-tsc", "tsgo"], When::Always, "tsc"),
    rule(&["playwright", "puppeteer", "cypress"], When::Always, "capture"),
    rule(&["eslint", "oxlint", "biome"], When::Always, "eslint"),
    rule(&["pytest", "py.test"], When::Always, "pytest"),
    rule(
        &["webpack", "rollup", "esbuild", "mvn", "mvnw"],
        When::Always,
        "build",
    ),
    Rule {
        hint: Some(GRADLE_HINT),
        ..rule(&["gradle", "gradlew"], When::Always, "build")
    },
    rule(&["vite", "next", "ng"], When::Sub(&["build"]), "build"),
    rule(&["docker-compose"], When::Compose(COMPOSE_HEAVY), "docker"),
    rule(&["docker"], When::DockerCompose(COMPOSE_HEAVY), "docker"),
    rule(&["docker"], When::Sub(&["build", "run", "buildx"]), "docker"),
    Rule {
        key: Some(KeyRule::CargoTarget),
        hint: Some(CARGO_HINT),
        ..rule(
            &["cargo"],
            When::Sub(&["build", "b", "check", "c", "clippy"]),
            "build",
        )
    },
    Rule {
        key: Some(KeyRule::CargoTarget),
        hint: Some(CARGO_HINT),
        ..rule(&["cargo"], When::Sub(&["test", "t", "nextest"]), "test")
    },
    rule(&["flutter", "dotnet", "go"], When::Sub(&["build"]), "build"),
    rule(&["dotnet"], When::Sub(&["publish"]), "build"),
    rule(&["flutter", "dotnet", "go"], When::Sub(&["test"]), "test"),
    rule(PACKAGE_MANAGERS, When::Sub(&["test", "t"]), "test"),
    rule(PACKAGE_MANAGERS, When::Script(&["test", "t"]), "test"),
    rule(PACKAGE_MANAGERS, When::Script(&["build"]), "build"),
    rule(PACKAGE_MANAGERS, When::Script(&["lint"]), "eslint"),
    rule(
        PACKAGE_MANAGERS,
        When::Script(&["typecheck", "type-check", "tsc"]),
        "tsc",
    ),
];

/// How to turn a never-ending command into its one-shot form.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Fix {
    /// Drop these flags (also `flag=value`).
    RemoveFlags(&'static [&'static str]),
    /// Replace the subcommand with this one.
    SetSub(&'static str),
    /// Put this subcommand right after the tool.
    InsertSub(&'static str),
    /// No automatic fix.
    None,
}

/// When a command never ends on its own.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Endless {
    /// Any of these flags (exact, `flag=…`, or a short flag inside a cluster like `-it`).
    Flags(&'static [&'static str]),
    /// The subcommand is one of these.
    Sub(&'static [&'static str]),
    /// No subcommand from `one_shot` and stdin is a terminal (vitest watches then).
    InteractiveDefault { one_shot: &'static [&'static str] },
}

/// A command that would hold its points forever: refused with a fix.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EndlessRule {
    pub tools: &'static [&'static str],
    /// Only for these subcommands (e.g. `docker run`), if set.
    pub sub: Option<&'static [&'static str]>,
    pub when: Endless,
    pub fix: Fix,
    /// What to tell the user it is.
    pub what: &'static str,
}

/// Watch modes and interactive sessions. They are refused: a job in the queue must end.
pub const ENDLESS: &[EndlessRule] = &[
    EndlessRule {
        tools: &["tsc", "vue-tsc", "tsgo"],
        sub: None,
        when: Endless::Flags(&["--watch", "-w"]),
        fix: Fix::RemoveFlags(&["--watch", "-w"]),
        what: "watch mode",
    },
    EndlessRule {
        tools: &["jest"],
        sub: None,
        when: Endless::Flags(&["--watch", "--watchall"]),
        fix: Fix::RemoveFlags(&["--watch", "--watchall"]),
        what: "watch mode",
    },
    EndlessRule {
        tools: &["vitest"],
        sub: None,
        when: Endless::Sub(&["watch", "dev"]),
        fix: Fix::SetSub("run"),
        what: "watch mode",
    },
    EndlessRule {
        tools: &["vitest"],
        sub: None,
        when: Endless::Flags(&["--watch", "-w"]),
        fix: Fix::RemoveFlags(&["--watch", "-w"]),
        what: "watch mode",
    },
    EndlessRule {
        tools: &["vitest"],
        sub: None,
        when: Endless::InteractiveDefault {
            one_shot: &["run", "bench", "list", "related"],
        },
        fix: Fix::InsertSub("run"),
        what: "watch mode (vitest watches by default in a terminal)",
    },
    EndlessRule {
        tools: &["docker"],
        sub: Some(&["run", "exec"]),
        when: Endless::Flags(&["-i", "-t", "--interactive", "--tty"]),
        fix: Fix::None,
        what: "an interactive container",
    },
];

/// Commands that never end by design but are not heavy (dev servers): they run at once,
/// outside the queue.
pub const LIGHT: &[Rule] = &[
    rule(
        &["vite"],
        When::NoSubOr(&["dev", "serve", "preview"]),
        "dev server",
    ),
    rule(
        &["next", "nuxt", "astro"],
        When::Sub(&["dev", "start", "preview"]),
        "dev server",
    ),
    rule(&["ng"], When::Sub(&["serve"]), "dev server"),
    rule(&["webpack"], When::Sub(&["serve"]), "dev server"),
    rule(
        PACKAGE_MANAGERS,
        When::Script(&["dev", "start", "serve", "preview"]),
        "dev server",
    ),
];

/// Whether `rest` (the words after the tool) matches an endless rule.
pub fn endless_rule(tool: &str, rest: &[String], stdin_tty: bool) -> Option<&'static EndlessRule> {
    ENDLESS.iter().find(|r| {
        r.tools.contains(&tool)
            && r.sub
                .is_none_or(|subs| first_non_flag(rest).is_some_and(|s| subs.contains(&s)))
            && match r.when {
                Endless::Flags(flags) => rest.iter().any(|w| has_flag(w, flags)),
                Endless::Sub(list) => {
                    first_non_flag(rest).is_some_and(|s| list.contains(&s.to_ascii_lowercase().as_str()))
                }
                Endless::InteractiveDefault { one_shot } => {
                    stdin_tty && !first_non_flag(rest).is_some_and(|s| one_shot.contains(&s))
                }
            }
    })
}

/// `w` is one of `flags` (lowercase), `flag=…`, or a short-flag cluster containing one.
pub fn has_flag(w: &str, flags: &[&str]) -> bool {
    let lw = w.to_ascii_lowercase();
    flags.iter().any(|f| {
        lw == *f
            || lw.starts_with(&format!("{f}="))
            || (f.len() == 2
                && !f.starts_with("--")
                && w.starts_with('-')
                && !w.starts_with("--")
                && w[1..].contains(&f[1..]))
    })
}

/// The light (unqueued) rule for this tool, if any.
pub fn light_rule(tool: &str, rest: &[String]) -> Option<&'static Rule> {
    LIGHT
        .iter()
        .find(|r| r.tools.contains(&tool) && r.when.matches(tool, rest))
}

/// The `--kind` help text, listing the built-in kinds from [`KINDS`].
pub fn kind_help() -> String {
    let names: Vec<&str> = builtin_names().collect();
    format!(
        "Kind of job ({}, or one from the config). Detected from the command when omitted.",
        names.join(", ")
    )
}

/// Built-in kind names.
pub fn builtin_names() -> impl Iterator<Item = &'static str> {
    KINDS.iter().map(|k| k.name)
}

/// The built-in spec of a kind.
pub fn spec(kind: &str) -> Option<&'static KindSpec> {
    KINDS.iter().find(|k| k.name == kind)
}

/// Cost of a kind: config first, then the built-in default.
pub fn cost(kind: &str, kinds: &BTreeMap<String, KindConfig>) -> u32 {
    kinds
        .get(kind)
        .and_then(|k| k.cost)
        .or_else(|| spec(kind).map(|s| s.cost))
        .unwrap_or(UNKNOWN_COST)
}

/// Arguments that only ask for version or help: never heavy.
pub fn only_info_flags(rest: &[String]) -> bool {
    !rest.is_empty()
        && rest.iter().all(|w| {
            matches!(
                w.as_str(),
                "--version" | "-v" | "-V" | "--help" | "-h" | "version" | "help"
            )
        })
}

/// The first word that is not a flag.
/// `--help`, `-h`, `--version` or `-V` before any `--`: the tool only prints and exits.
pub fn asks_for_info(rest: &[String]) -> bool {
    only_info_flags(rest)
        || rest
            .iter()
            .take_while(|w| w.as_str() != "--")
            .any(|w| matches!(w.as_str(), "--help" | "-h" | "--version" | "-V"))
}

/// The first word that is not a flag.
pub fn first_non_flag(rest: &[String]) -> Option<&str> {
    rest.iter().map(String::as_str).find(|w| !w.starts_with('-'))
}

/// Compose subcommand after compose's own flags (`-f file`, `-p name`, `--profile x`).
pub fn compose_sub(rest: &[String]) -> Option<String> {
    const VALUE_FLAGS: &[&str] = &[
        "-f",
        "--file",
        "-p",
        "--project-name",
        "--profile",
        "--env-file",
        "--project-directory",
    ];
    let mut i = 0;
    while i < rest.len() {
        let w = &rest[i];
        if !w.starts_with('-') {
            return Some(w.to_ascii_lowercase());
        }
        i += if VALUE_FLAGS.contains(&w.as_str()) { 2 } else { 1 };
    }
    None
}

/// The words after `docker … compose`, if the first non-flag word is `compose`.
pub fn after_docker_compose(rest: &[String]) -> Option<&[String]> {
    if first_non_flag(rest) != Some("compose") {
        return None;
    }
    let pos = rest.iter().position(|w| w == "compose")?;
    Some(&rest[pos + 1..])
}

fn script_matches(tool: &str, rest: &[String], names: &[&str]) -> bool {
    let is = |w: &str| names.contains(&w.split(':').next().unwrap_or(w));
    match first_non_flag(rest) {
        Some("run" | "run-script") => {
            let pos = rest
                .iter()
                .position(|w| w == "run" || w == "run-script")
                .unwrap_or(0);
            first_non_flag(&rest[pos + 1..]).is_some_and(is)
        }
        Some(s) => tool != "npm" && is(s),
        None => false,
    }
}

impl When {
    /// Whether the condition holds for this tool and its arguments.
    pub fn matches(&self, tool: &str, rest: &[String]) -> bool {
        let sub = || first_non_flag(rest).map(str::to_ascii_lowercase);
        match *self {
            When::Always => true,
            When::Sub(list) => sub().is_some_and(|s| list.contains(&s.as_str())),
            When::Compose(list) => compose_sub(rest).is_some_and(|s| list.contains(&s.as_str())),
            When::DockerCompose(list) => after_docker_compose(rest)
                .and_then(compose_sub)
                .is_some_and(|s| list.contains(&s.as_str())),
            When::Script(names) => script_matches(tool, rest, names),
            When::NoSubOr(list) => sub().is_none_or(|s| list.contains(&s.as_str())),
        }
    }
}

/// The first built-in rule for this tool and its arguments.
pub fn find_rule(tool: &str, rest: &[String]) -> Option<&'static Rule> {
    if only_info_flags(rest) {
        return None;
    }
    RULES
        .iter()
        .find(|r| r.tools.contains(&tool) && r.when.matches(tool, rest))
}

/// The kind for a tool and its arguments: the user's `[kinds]` patterns first, then [`RULES`].
pub fn classify(tool: &str, rest: &[String], kinds: &BTreeMap<String, KindConfig>) -> Option<String> {
    for (name, kc) in kinds {
        if kc.patterns.iter().any(|p| pattern_matches(p, tool, rest)) {
            return Some(name.clone());
        }
    }
    find_rule(tool, rest).map(|r| r.kind.to_string())
}

/// A config pattern is the command's first words: `make`, `bazel build`.
fn pattern_matches(pattern: &str, tool: &str, rest: &[String]) -> bool {
    let mut words = pattern.split_whitespace();
    let Some(head) = words.next() else {
        return false;
    };
    if crate::detect::tool_name(head) != tool {
        return false;
    }
    let mut rest_iter = rest.iter();
    words.all(|w| rest_iter.next().is_some_and(|r| w.eq_ignore_ascii_case(r)))
}

/// The exclusive-key rule for a job of `kind` run by `tool`.
pub fn key_rule(kind: &str, tool: &str) -> KeyRule {
    RULES
        .iter()
        .find(|r| r.kind == kind && r.tools.contains(&tool) && r.key.is_some())
        .and_then(|r| r.key)
        .or_else(|| spec(kind).map(|s| s.key))
        .unwrap_or(KeyRule::None)
}

/// The hint for a tool, from the first rule that names it (whatever its arguments).
pub fn hint_for(tool: &str) -> Option<Hint> {
    RULES
        .iter()
        .find(|r| r.tools.contains(&tool))
        .and_then(|r| r.hint)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<String> {
        crate::shell::tokenize(s)
    }

    #[test]
    fn every_rule_names_a_known_kind() {
        for r in RULES {
            assert!(
                spec(r.kind).is_some(),
                "rule for {:?} names unknown kind {}",
                r.tools,
                r.kind
            );
        }
    }

    #[test]
    fn costs_and_keys() {
        let mut kinds = BTreeMap::new();
        kinds.insert(
            "vitest".to_string(),
            KindConfig {
                cost: Some(4),
                patterns: vec![],
            },
        );
        assert_eq!(cost("vitest", &kinds), 4);
        assert_eq!(cost("jest", &kinds), 2);
        assert_eq!(cost("eslint", &kinds), 1);
        assert_eq!(cost("whatever", &kinds), UNKNOWN_COST);
        assert_eq!(key_rule("build", "cargo"), KeyRule::CargoTarget);
        assert_eq!(key_rule("build", "vite"), KeyRule::NodeModules);
        assert_eq!(key_rule("vitest", "cargo"), KeyRule::NodeModules);
        assert_eq!(key_rule("eslint", "eslint"), KeyRule::None);
    }

    #[test]
    fn script_rules() {
        let k = |t: &str, s: &str| find_rule(t, &w(s)).map(|r| r.kind);
        assert_eq!(k("npm", "run build:prod"), Some("build"));
        assert_eq!(k("pnpm", "lint"), Some("eslint"));
        assert_eq!(k("npm", "lint"), None);
        assert_eq!(k("npm", "test"), Some("test"));
        assert_eq!(k("yarn", "typecheck"), Some("tsc"));
        assert_eq!(k("npm", "install"), None);
    }

    #[test]
    fn cargo_config_jobs_found_up_the_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let deep = tmp.path().join("ws/crates/a");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::create_dir_all(tmp.path().join("ws/.cargo")).unwrap();
        std::fs::write(tmp.path().join("ws/.cargo/config.toml"), "[build]\njobs = 3\n").unwrap();
        assert_eq!(cargo_config_jobs(&deep), Some(3));
    }

    #[test]
    fn hints_by_tool() {
        assert!(matches!(
            hint_for("vitest"),
            Some(Hint::Arg {
                flag: "--maxWorkers=",
                ..
            })
        ));
        assert!(matches!(
            hint_for("cargo"),
            Some(Hint::Env {
                var: "CARGO_BUILD_JOBS",
                ..
            })
        ));
        assert!(hint_for("jest").is_none());
    }
}
