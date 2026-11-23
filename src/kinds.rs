//! The one place that knows about tools: which commands are heavy, what they cost, which
//! folder they must not share, and how to size their workers.
//!
//! Adding a tool is one entry in [`RULES`] (and, for a new kind, one in [`KINDS`]). A command
//! that never ends goes in [`ENDLESS`] (refused, with its one-shot form) or [`LIGHT`] (a
//! server: runs at once, outside the queue). Nothing else in the crate matches on tool names.

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
    /// The project folder: the nearest ancestor holding the first marker found, trying the
    /// markers in order (`*.ext` matches any file with that extension). Two runs in one
    /// project share its build output (`build/`, `target/`, `obj/`, DerivedData, …).
    Project {
        prefix: &'static str,
        markers: &'static [&'static str],
    },
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
    /// No subcommand, or one that is not in the list (`parcel index.html` serves).
    NotSub(&'static [&'static str]),
    /// Any word is one of these (Maven goals, Gradle tasks, also as `:app:bootRun`).
    AnyWord(&'static [&'static str]),
    /// Any word starts with one of these (Gradle `assemble*`, `connected*`).
    AnyWordPrefix(&'static [&'static str]),
    /// Any of these flags is present (`cmake --build`, `pnpm -r`).
    Flag(&'static [&'static str]),
    /// The subcommand is `.0` and the next plain word is one of `.1` (`deno task dev`).
    SubThen(&'static str, &'static [&'static str]),
}

/// How many workers a hint asks for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Workers {
    /// `vitest_workers` from the config.
    Vitest,
    /// The job's share of the cores: logical cores × cost ÷ budget, at least 1.
    CoreShare,
}

/// How a flag and its number are written.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Style {
    /// One word: `-j8`, `--jobs=8`, `-m:8` (the flag includes any `=` or `:`).
    Joined,
    /// Two words: `--parallel 8`, `-T 8`.
    Split,
}

/// Where an argument hint goes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Position {
    /// At the end (before a `--`).
    End,
    /// Right after the subcommand (`go test -p 4 ./...`: Go stops reading flags at packages).
    AfterSub,
    /// Right after the tool (`pnpm --workspace-concurrency=4 -r run build`).
    AfterTool,
}

/// An argument hint: give the tool a worker count unless the user already chose one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ArgHint {
    pub flag: &'static str,
    pub style: Style,
    /// Skip when any argument starts with one of these (compared lowercase).
    pub opt_out: &'static [&'static str],
    /// Skip when one of these variables is set; with `Some(s)`, only when its value contains `s`.
    pub opt_out_env: &'static [(&'static str, Option<&'static str>)],
    /// Skip when there are fewer arguments than this.
    pub min_args: usize,
    pub at: Position,
    pub workers: Workers,
    /// Never more than this: the tool's own default when it is a small fixed number
    /// (`nx --parallel` is 3), so the hint caps and never raises it.
    pub max: Option<u32>,
}

impl ArgHint {
    /// The count to pass for a share of `n` workers.
    pub fn count(&self, n: u32) -> u32 {
        self.max.map_or(n, |m| n.min(m))
    }
}

/// A change calmrun makes so a tool does not take the whole machine. An explicit choice by
/// the user (a flag, an environment variable, a config file) always wins.
#[derive(Debug, Clone, Copy)]
pub enum Hint {
    Arg(ArgHint),
    /// Set this environment variable unless it is already set. `user_limit` reads a limit
    /// the user configured elsewhere (from the working directory); the lower one wins.
    Env {
        var: &'static str,
        workers: Workers,
        user_limit: Option<fn(&Path) -> Option<u32>>,
    },
    /// Replace an "all cores" value the user asked for (`pytest -n auto`, `tox -p auto`)
    /// with the job's share. `flags` are the spellings of the option.
    Replace {
        flags: &'static [&'static str],
        from: &'static str,
        workers: Workers,
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
    pub hints: &'static [Hint],
    /// Overrides the kind's default cost (a single compiler call is cheaper than a build).
    pub cost: Option<u32>,
    /// Named resources the job also takes (`connectedAndroidTest` needs a device).
    pub needs: &'static [&'static str],
}

const fn rule(tools: &'static [&'static str], when: When, kind: &'static str) -> Rule {
    Rule {
        tools,
        when,
        kind,
        key: None,
        hints: &[],
        cost: None,
        needs: &[],
    }
}

/// A rule with a key and hints, the common shape for build tools.
const fn tool(
    tools: &'static [&'static str],
    when: When,
    kind: &'static str,
    key: KeyRule,
    hints: &'static [Hint],
) -> Rule {
    Rule {
        tools,
        when,
        kind,
        key: Some(key),
        hints,
        cost: None,
        needs: &[],
    }
}

const fn arg(flag: &'static str, style: Style, opt_out: &'static [&'static str]) -> Hint {
    Hint::Arg(ArgHint {
        flag,
        style,
        opt_out,
        opt_out_env: &[],
        min_args: 0,
        at: Position::End,
        workers: Workers::CoreShare,
        max: None,
    })
}

/// A `--flag=N` hint capped at the tool's own default.
const fn capped(flag: &'static str, opt_out: &'static [&'static str], max: u32) -> Hint {
    Hint::Arg(ArgHint {
        flag,
        style: Style::Joined,
        opt_out,
        opt_out_env: &[],
        min_args: 0,
        at: Position::End,
        workers: Workers::CoreShare,
        max: Some(max),
    })
}

const fn kind(name: &'static str, cost: u32, key: KeyRule) -> KindSpec {
    KindSpec { name, cost, key }
}

/// Built-in kinds.
pub const KINDS: &[KindSpec] = &[
    kind("vitest", 2, KeyRule::NodeModules),
    kind("jest", 2, KeyRule::NodeModules),
    kind("tsc", 2, KeyRule::None),
    kind("capture", 2, KeyRule::None),
    kind("docker", 2, KeyRule::None),
    kind("eslint", 1, KeyRule::None),
    kind("pytest", 1, KeyRule::None),
    kind("build", 2, KeyRule::NodeModules),
    kind("test", 1, KeyRule::None),
    kind("lint", 1, KeyRule::None),
    kind("typecheck", 2, KeyRule::None),
    kind("monorepo", 3, KeyRule::NodeModules),
];

/// Cost of a kind nobody knows (and of `other`).
pub const UNKNOWN_COST: u32 = 1;

const COMPOSE_HEAVY: &[&str] = &["up", "build", "run", "create"];
/// Stand-alone compose programs, and container CLIs with a `compose` subcommand.
pub const COMPOSE: &[&str] = &["docker-compose", "podman-compose"];
pub const CONTAINERS: &[&str] = &["docker", "podman"];
const PACKAGE_MANAGERS: &[&str] = &["npm", "pnpm", "yarn", "bun"];
/// Task names that start a dev server in monorepo runners (`turbo run dev`).
const DEV_TASKS: &[&str] = &["dev", "start", "serve"];

// ---- Exclusive keys --------------------------------------------------------------------

const fn project(prefix: &'static str, markers: &'static [&'static str]) -> KeyRule {
    KeyRule::Project { prefix, markers }
}

const GRADLE_KEY: KeyRule = project(
    "gradle",
    &[
        "settings.gradle",
        "settings.gradle.kts",
        "build.gradle",
        "build.gradle.kts",
    ],
);
const MAVEN_KEY: KeyRule = project("maven", &["pom.xml"]);
const DOTNET_KEY: KeyRule = project(
    "dotnet",
    &["*.sln", "*.slnx", "*.csproj", "*.fsproj", "*.vbproj", "*.vcxproj"],
);
const MAKE_KEY: KeyRule = project("make", &["GNUmakefile", "Makefile", "makefile"]);
const CMAKE_KEY: KeyRule = project("cmake", &["CMakeCache.txt", "CMakeLists.txt"]);
const NINJA_KEY: KeyRule = project("ninja", &["build.ninja"]);
const MESON_KEY: KeyRule = project("meson", &["meson.build"]);
const BAZEL_KEY: KeyRule = project("bazel", &["MODULE.bazel", "WORKSPACE", "WORKSPACE.bazel"]);
const SCONS_KEY: KeyRule = project("scons", &["SConstruct"]);
const XMAKE_KEY: KeyRule = project("xmake", &["xmake.lua"]);
const SBT_KEY: KeyRule = project("sbt", &["build.sbt"]);
const ANT_KEY: KeyRule = project("ant", &["build.xml"]);
const SWIFT_KEY: KeyRule = project("swift", &["Package.swift"]);
const XCODE_KEY: KeyRule = project("xcode", &["*.xcworkspace", "*.xcodeproj"]);
const FLUTTER_KEY: KeyRule = project("flutter", &["pubspec.yaml"]);
const MIX_KEY: KeyRule = project("mix", &["mix.exs"]);
const REBAR_KEY: KeyRule = project("rebar", &["rebar.config"]);
const STACK_KEY: KeyRule = project("stack", &["stack.yaml"]);
const CABAL_KEY: KeyRule = project("cabal", &["cabal.project", "*.cabal"]);
const DUNE_KEY: KeyRule = project("dune", &["dune-project"]);
const ZIG_KEY: KeyRule = project("zig", &["build.zig"]);
const BUCK_KEY: KeyRule = project("buck", &[".buckconfig"]);
const TOX_KEY: KeyRule = project("tox", &["tox.ini", "pyproject.toml"]);
const NOX_KEY: KeyRule = project("nox", &["noxfile.py"]);

// ---- Worker hints ----------------------------------------------------------------------
//
// A hint caps a tool that would otherwise use every core. Tools that run one job at a time
// unless asked (make, scons, plain Maven, ctest, msbuild without -m) get none: turning on
// parallelism can break a build that was never written for it.

/// Vitest gets a small worker count when started directly.
const VITEST_HINT: Hint = Hint::Arg(ArgHint {
    flag: "--maxWorkers=",
    style: Style::Joined,
    opt_out: &["--maxworkers", "--max-workers", "--pool", "--no-file-parallelism"],
    opt_out_env: &[],
    min_args: 0,
    at: Position::End,
    workers: Workers::Vitest,
    max: None,
});
const GRADLE_HINT: Hint = Hint::Arg(ArgHint {
    flag: "--max-workers=",
    style: Style::Joined,
    opt_out: &["--max-workers", "-dorg.gradle.workers.max"],
    opt_out_env: &[],
    min_args: 1,
    at: Position::End,
    workers: Workers::CoreShare,
    max: None,
});
const CARGO_HINT: Hint = Hint::Env {
    var: "CARGO_BUILD_JOBS",
    workers: Workers::CoreShare,
    user_limit: Some(cargo_config_jobs),
};
const NEXTEST_HINT: Hint = Hint::Env {
    var: "NEXTEST_TEST_THREADS",
    workers: Workers::CoreShare,
    user_limit: None,
};
const NINJA_HINT: Hint = arg("-j", Style::Joined, &["-j"]);
const CMAKE_HINT: Hint = Hint::Arg(ArgHint {
    flag: "--parallel",
    style: Style::Split,
    opt_out: &["--parallel", "-j"],
    opt_out_env: &[("CMAKE_BUILD_PARALLEL_LEVEL", None)],
    min_args: 0,
    at: Position::End,
    workers: Workers::CoreShare,
    max: None,
});
const MESON_HINT: Hint = arg("-j", Style::Split, &["-j", "--jobs"]);
const MESON_TEST_HINT: Hint = Hint::Arg(ArgHint {
    flag: "--num-processes",
    style: Style::Split,
    opt_out: &["--num-processes"],
    opt_out_env: &[("MESON_TESTTHREADS", None)],
    min_args: 0,
    at: Position::End,
    workers: Workers::CoreShare,
    max: None,
});
const MSBUILD_HINT: Hint = arg(
    "-m:",
    Style::Joined,
    &["-m", "/m", "-maxcpucount", "/maxcpucount"],
);
const BAZEL_HINT: Hint = arg("--jobs=", Style::Joined, &["--jobs", "-j"]);
const BUCK_HINT: Hint = arg("-j", Style::Joined, &["-j", "--num-threads"]);
const XMAKE_HINT: Hint = arg("-j", Style::Split, &["-j", "--jobs"]);
/// Only for mvnd, which builds in parallel by default; plain Maven is serial unless asked.
const MVND_HINT: Hint = arg("-T", Style::Split, &["-t", "--threads"]);
const GO_HINT: Hint = Hint::Arg(ArgHint {
    flag: "-p",
    style: Style::Split,
    opt_out: &["-p"],
    opt_out_env: &[("GOFLAGS", Some("-p"))],
    min_args: 0,
    at: Position::AfterSub,
    workers: Workers::CoreShare,
    max: None,
});
const GOLANGCI_HINT: Hint = arg("--concurrency", Style::Split, &["--concurrency", "-j"]);
const SWIFT_HINT: Hint = arg("-j", Style::Split, &["-j", "--jobs"]);
const XCODE_HINT: Hint = arg("-jobs", Style::Split, &["-jobs"]);
const FLUTTER_TEST_HINT: Hint = arg("--concurrency=", Style::Joined, &["--concurrency", "-j"]);
const DART_TEST_HINT: Hint = arg("--concurrency=", Style::Joined, &["--concurrency", "-j"]);
const RAILS_HINT: Hint = Hint::Env {
    var: "PARALLEL_WORKERS",
    workers: Workers::CoreShare,
    user_limit: None,
};
const PARALLEL_TESTS_HINT: Hint = Hint::Env {
    var: "PARALLEL_TEST_PROCESSORS",
    workers: Workers::CoreShare,
    user_limit: None,
};
const PARATEST_HINT: Hint = arg("--processes=", Style::Joined, &["-p", "--processes"]);
const MIX_TEST_HINT: Hint = arg("--max-cases", Style::Split, &["--max-cases"]);
const STACK_HINT: Hint = arg("-j", Style::Split, &["-j", "--jobs"]);
const CABAL_HINT: Hint = arg("-j", Style::Joined, &["-j", "--jobs"]);
const DUNE_HINT: Hint = arg("-j", Style::Split, &["-j"]);
const ZIG_HINT: Hint = arg("-j", Style::Joined, &["-j"]);
const DENO_HINT: Hint = Hint::Env {
    var: "DENO_JOBS",
    workers: Workers::CoreShare,
    user_limit: None,
};
const TURBO_HINT: Hint = capped("--concurrency=", &["--concurrency"], 10);
const NX_HINT: Hint = capped("--parallel=", &["--parallel"], 3);
const MOON_HINT: Hint = Hint::Env {
    var: "MOON_CONCURRENCY",
    workers: Workers::CoreShare,
    user_limit: None,
};
const LERNA_HINT: Hint = arg("--concurrency", Style::Split, &["--concurrency"]);
const PNPM_RECURSIVE_HINT: Hint = Hint::Arg(ArgHint {
    flag: "--workspace-concurrency=",
    style: Style::Joined,
    opt_out: &["--workspace-concurrency"],
    opt_out_env: &[],
    min_args: 0,
    at: Position::AfterTool,
    workers: Workers::CoreShare,
    // pnpm's own default.
    max: Some(4),
});
const RUSH_HINT: Hint = arg("--parallelism", Style::Split, &["-p", "--parallelism"]);
const PYTEST_XDIST_HINT: Hint = Hint::Replace {
    flags: &["-n", "--numprocesses"],
    from: "auto",
    workers: Workers::CoreShare,
};
const TOX_HINT: Hint = Hint::Replace {
    flags: &["-p", "--parallel"],
    from: "auto",
    workers: Workers::CoreShare,
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

// ---- Detection rules -------------------------------------------------------------------

const GRADLE: &[&str] = &["gradle", "gradlew"];
const MAVEN: &[&str] = &["mvn", "mvnw", "mvnd"];
const PLAIN_MAVEN: &[&str] = &["mvn", "mvnw"];
const MAKE: &[&str] = &["make", "gmake", "mingw32-make", "nmake"];
const CC: &[&str] = &["gcc", "g++", "cc", "c++", "clang", "clang++", "clang-cl", "cl"];
const BAZEL: &[&str] = &["bazel", "bazelisk"];
const BUCK: &[&str] = &["buck2", "buck"];
/// Build rules outside JavaScript take no `node_modules` key.
const NO_KEY: Option<KeyRule> = Some(KeyRule::None);

/// Detection rules, tried in order after the user's own `[kinds]` patterns. The first match
/// wins, so specific rules (a subcommand, a task) come before general ones for the same tool.
pub const RULES: &[Rule] = &[
    // JavaScript / TypeScript
    Rule {
        hints: &[VITEST_HINT],
        ..rule(&["vitest"], When::Always, "vitest")
    },
    rule(&["jest"], When::Always, "jest"),
    rule(&["mocha", "ava"], When::Always, "test"),
    rule(&["tsc", "vue-tsc", "tsgo"], When::Always, "tsc"),
    rule(&["playwright", "puppeteer", "cypress"], When::Always, "capture"),
    rule(&["eslint", "oxlint", "biome"], When::Always, "eslint"),
    rule(
        &["webpack", "rspack", "rollup", "esbuild", "rolldown"],
        When::Always,
        "build",
    ),
    rule(&["parcel"], When::Sub(&["build"]), "build"),
    rule(
        &[
            "vite",
            "next",
            "ng",
            "nuxt",
            "nuxi",
            "astro",
            "remix",
            "svelte-kit",
        ],
        When::Sub(&["build"]),
        "build",
    ),
    rule(&["storybook"], When::Sub(&["build"]), "build"),
    rule(&["expo"], When::Sub(&["export", "prebuild"]), "build"),
    rule(&["elm"], When::Sub(&["make"]), "build"),
    // Monorepo runners: many jobs at once, so they cost more and get a concurrency cap.
    Rule {
        hints: &[TURBO_HINT],
        ..rule(
            &["turbo"],
            When::Sub(&["run", "build", "test", "lint"]),
            "monorepo",
        )
    },
    Rule {
        hints: &[NX_HINT],
        ..rule(&["nx"], When::Sub(&["run-many", "affected"]), "monorepo")
    },
    rule(&["nx"], When::Sub(&["run", "build", "test"]), "build"),
    Rule {
        hints: &[LERNA_HINT],
        ..rule(&["lerna"], When::Sub(&["run", "exec"]), "monorepo")
    },
    Rule {
        hints: &[PNPM_RECURSIVE_HINT],
        ..rule(&["pnpm"], When::Flag(&["-r", "--recursive"]), "monorepo")
    },
    Rule {
        hints: &[RUSH_HINT],
        ..rule(&["rush"], When::Sub(&["build", "rebuild", "test"]), "monorepo")
    },
    // `rushx` runs one project's script and passes extra arguments on to it: no hint.
    rule(&["rushx"], When::Sub(&["build", "test"]), "build"),
    Rule {
        hints: &[MOON_HINT],
        ..rule(&["moon"], When::Sub(&["run", "ci", "check"]), "monorepo")
    },
    // Package-manager scripts.
    rule(PACKAGE_MANAGERS, When::Sub(&["test", "t"]), "test"),
    rule(PACKAGE_MANAGERS, When::Script(&["test", "t"]), "test"),
    rule(PACKAGE_MANAGERS, When::Script(&["build"]), "build"),
    rule(PACKAGE_MANAGERS, When::Script(&["lint"]), "eslint"),
    rule(
        PACKAGE_MANAGERS,
        When::Script(&["typecheck", "type-check", "tsc"]),
        "tsc",
    ),
    rule(&["bun"], When::Sub(&["build"]), "build"),
    Rule {
        hints: &[DENO_HINT],
        ..rule(&["deno"], When::Sub(&["test", "bench"]), "test")
    },
    rule(&["deno"], When::Sub(&["check"]), "typecheck"),
    Rule {
        key: NO_KEY,
        ..rule(&["deno"], When::Sub(&["compile"]), "build")
    },
    // Python
    Rule {
        hints: &[PYTEST_XDIST_HINT],
        ..rule(&["pytest", "py.test"], When::Always, "pytest")
    },
    Rule {
        key: Some(TOX_KEY),
        hints: &[TOX_HINT],
        ..rule(&["tox"], When::Always, "test")
    },
    Rule {
        key: Some(NOX_KEY),
        ..rule(&["nox"], When::Always, "test")
    },
    rule(&["mypy", "pyright", "dmypy"], When::Always, "typecheck"),
    rule(&["ruff", "pylint", "flake8"], When::Always, "lint"),
    Rule {
        key: Some(KeyRule::CargoTarget),
        hints: &[CARGO_HINT],
        ..rule(&["maturin"], When::Sub(&["build", "develop"]), "build")
    },
    Rule {
        key: NO_KEY,
        ..rule(&["pip", "pip3"], When::Sub(&["wheel"]), "build")
    },
    Rule {
        key: NO_KEY,
        ..rule(&["build"], When::Always, "build")
    },
    // Rust
    Rule {
        key: Some(KeyRule::CargoTarget),
        hints: &[CARGO_HINT],
        ..rule(
            &["cargo"],
            When::Sub(&["build", "b", "check", "c", "clippy", "doc"]),
            "build",
        )
    },
    Rule {
        key: Some(KeyRule::CargoTarget),
        hints: &[CARGO_HINT],
        ..rule(&["cargo"], When::Sub(&["test", "t", "bench"]), "test")
    },
    // After the build rule: other cargo subcommands take the build rule's hints.
    Rule {
        key: Some(KeyRule::CargoTarget),
        hints: &[CARGO_HINT, NEXTEST_HINT],
        ..rule(&["cargo"], When::Sub(&["nextest"]), "test")
    },
    // Go
    Rule {
        key: NO_KEY,
        hints: &[GO_HINT],
        ..rule(&["go"], When::Sub(&["build", "install"]), "build")
    },
    Rule {
        hints: &[GO_HINT],
        ..rule(&["go"], When::Sub(&["test"]), "test")
    },
    Rule {
        hints: &[GO_HINT],
        ..rule(&["go"], When::Sub(&["vet"]), "lint")
    },
    Rule {
        hints: &[GOLANGCI_HINT],
        ..rule(&["golangci-lint"], When::Sub(&["run"]), "lint")
    },
    // JVM, Android
    Rule {
        needs: &["emulator"],
        ..tool(
            GRADLE,
            When::AnyWordPrefix(&["connected"]),
            "test",
            GRADLE_KEY,
            &[GRADLE_HINT],
        )
    },
    tool(
        GRADLE,
        When::AnyWordPrefix(&["test", "check"]),
        "test",
        GRADLE_KEY,
        &[GRADLE_HINT],
    ),
    tool(GRADLE, When::Always, "build", GRADLE_KEY, &[GRADLE_HINT]),
    tool(
        &["mvnd"],
        When::AnyWord(&["test", "verify"]),
        "test",
        MAVEN_KEY,
        &[MVND_HINT],
    ),
    tool(&["mvnd"], When::Always, "build", MAVEN_KEY, &[MVND_HINT]),
    tool(
        PLAIN_MAVEN,
        When::AnyWord(&["test", "verify"]),
        "test",
        MAVEN_KEY,
        &[],
    ),
    tool(PLAIN_MAVEN, When::Always, "build", MAVEN_KEY, &[]),
    tool(
        &["sbt"],
        When::AnyWord(&["test", "testOnly"]),
        "test",
        SBT_KEY,
        &[],
    ),
    tool(&["sbt"], When::Always, "build", SBT_KEY, &[]),
    tool(&["ant"], When::Always, "build", ANT_KEY, &[]),
    Rule {
        key: NO_KEY,
        ..rule(
            &["kotlinc", "kotlinc-jvm", "javac", "scalac"],
            When::Always,
            "build",
        )
    },
    // .NET
    tool(
        &["dotnet"],
        When::Sub(&["test"]),
        "test",
        DOTNET_KEY,
        &[MSBUILD_HINT],
    ),
    tool(
        &["dotnet"],
        When::Sub(&["build", "publish", "pack", "msbuild", "restore"]),
        "build",
        DOTNET_KEY,
        &[MSBUILD_HINT],
    ),
    tool(&["msbuild"], When::Always, "build", DOTNET_KEY, &[]),
    // C / C++
    tool(MAKE, When::Always, "build", MAKE_KEY, &[]),
    tool(
        &["cmake"],
        When::Flag(&["--build"]),
        "build",
        CMAKE_KEY,
        &[CMAKE_HINT],
    ),
    tool(&["ninja"], When::Always, "build", NINJA_KEY, &[NINJA_HINT]),
    tool(
        &["meson"],
        When::Sub(&["compile"]),
        "build",
        MESON_KEY,
        &[MESON_HINT],
    ),
    tool(
        &["meson"],
        When::Sub(&["test"]),
        "test",
        MESON_KEY,
        &[MESON_TEST_HINT],
    ),
    tool(&["ctest"], When::Always, "test", CMAKE_KEY, &[]),
    tool(
        BAZEL,
        When::Sub(&["test", "coverage"]),
        "test",
        BAZEL_KEY,
        &[BAZEL_HINT],
    ),
    tool(BAZEL, When::Sub(&["build"]), "build", BAZEL_KEY, &[BAZEL_HINT]),
    tool(BUCK, When::Sub(&["test"]), "test", BUCK_KEY, &[BUCK_HINT]),
    tool(BUCK, When::Sub(&["build"]), "build", BUCK_KEY, &[BUCK_HINT]),
    tool(&["scons"], When::Always, "build", SCONS_KEY, &[]),
    tool(
        &["xmake"],
        When::NoSubOr(&["build", "b"]),
        "build",
        XMAKE_KEY,
        &[XMAKE_HINT],
    ),
    Rule {
        cost: Some(1),
        key: NO_KEY,
        ..rule(CC, When::Always, "build")
    },
    // Swift / Apple
    tool(
        &["swift"],
        When::Sub(&["build"]),
        "build",
        SWIFT_KEY,
        &[SWIFT_HINT],
    ),
    tool(&["swift"], When::Sub(&["test"]), "test", SWIFT_KEY, &[SWIFT_HINT]),
    tool(
        &["xcodebuild"],
        When::AnyWord(&["test", "test-without-building"]),
        "test",
        XCODE_KEY,
        &[XCODE_HINT],
    ),
    tool(&["xcodebuild"], When::Always, "build", XCODE_KEY, &[XCODE_HINT]),
    // Dart / Flutter
    tool(&["flutter"], When::Sub(&["build"]), "build", FLUTTER_KEY, &[]),
    tool(
        &["flutter"],
        When::Sub(&["test"]),
        "test",
        FLUTTER_KEY,
        &[FLUTTER_TEST_HINT],
    ),
    tool(
        &["dart"],
        When::Sub(&["test"]),
        "test",
        FLUTTER_KEY,
        &[DART_TEST_HINT],
    ),
    tool(&["dart"], When::Sub(&["compile"]), "build", FLUTTER_KEY, &[]),
    // Code generation (`dart run build_runner build`).
    tool(
        &["dart"],
        When::SubThen("run", &["build_runner"]),
        "build",
        FLUTTER_KEY,
        &[],
    ),
    // Ruby, PHP
    rule(&["rspec", "cucumber"], When::Always, "test"),
    Rule {
        hints: &[PARALLEL_TESTS_HINT],
        ..rule(
            &["parallel_rspec", "parallel_test", "parallel_cucumber"],
            When::Always,
            "test",
        )
    },
    rule(&["rake"], When::Sub(&["test", "spec"]), "test"),
    Rule {
        hints: &[RAILS_HINT],
        ..rule(&["rails"], When::Sub(&["test"]), "test")
    },
    rule(&["rubocop"], When::Always, "lint"),
    rule(&["phpunit", "pest"], When::Always, "test"),
    Rule {
        hints: &[PARATEST_HINT],
        ..rule(&["paratest"], When::Always, "test")
    },
    rule(&["phpstan", "psalm"], When::Always, "typecheck"),
    rule(&["php"], When::SubThen("artisan", &["test"]), "test"),
    rule(&["composer"], When::Sub(&["test"]), "test"),
    rule(&["composer"], When::Script(&["test"]), "test"),
    // Elixir, Erlang
    tool(&["mix"], When::Sub(&["test"]), "test", MIX_KEY, &[MIX_TEST_HINT]),
    tool(
        &["mix"],
        When::Sub(&["compile", "release"]),
        "build",
        MIX_KEY,
        &[],
    ),
    tool(&["rebar3"], When::Sub(&["eunit", "ct"]), "test", REBAR_KEY, &[]),
    tool(
        &["rebar3"],
        When::Sub(&["compile", "release"]),
        "build",
        REBAR_KEY,
        &[],
    ),
    // Haskell, OCaml, Zig
    tool(&["stack"], When::Sub(&["test"]), "test", STACK_KEY, &[STACK_HINT]),
    tool(
        &["stack"],
        When::Sub(&["build", "install"]),
        "build",
        STACK_KEY,
        &[STACK_HINT],
    ),
    tool(&["cabal"], When::Sub(&["test"]), "test", CABAL_KEY, &[CABAL_HINT]),
    tool(
        &["cabal"],
        When::Sub(&["build", "install"]),
        "build",
        CABAL_KEY,
        &[CABAL_HINT],
    ),
    tool(
        &["dune"],
        When::Sub(&["test", "runtest"]),
        "test",
        DUNE_KEY,
        &[DUNE_HINT],
    ),
    tool(&["dune"], When::Sub(&["build"]), "build", DUNE_KEY, &[DUNE_HINT]),
    tool(&["zig"], When::Sub(&["build"]), "build", ZIG_KEY, &[ZIG_HINT]),
    // Containers
    rule(COMPOSE, When::Compose(COMPOSE_HEAVY), "docker"),
    rule(CONTAINERS, When::DockerCompose(COMPOSE_HEAVY), "docker"),
    rule(&["docker"], When::Sub(&["build", "run", "buildx"]), "docker"),
    rule(&["podman"], When::Sub(&["build", "run"]), "docker"),
    rule(&["buildah"], When::Sub(&["build", "bud"]), "docker"),
    rule(&["packer"], When::Sub(&["build"]), "docker"),
];

// ---- Commands that never end -----------------------------------------------------------

/// How to turn a never-ending command into its one-shot form.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Fix {
    /// Drop these flags (also `flag=value`).
    RemoveFlags(&'static [&'static str]),
    /// Replace the subcommand with this one.
    SetSub(&'static str),
    /// Put this subcommand right after the tool.
    InsertSub(&'static str),
    /// Drop the subcommand word (`dotnet watch test` -> `dotnet test`).
    DropSub,
    /// No automatic fix.
    None,
}

/// When a command never ends on its own.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Endless {
    /// Any of these flags (exact or `flag=…`).
    Flags(&'static [&'static str]),
    /// Like `Flags`, and also a short flag inside a cluster such as `-it`.
    FlagsOrCluster(&'static [&'static str]),
    /// The subcommand is one of these.
    Sub(&'static [&'static str]),
    /// Any word starts with this (`sbt ~compile`).
    WordPrefix(&'static str),
    /// Any plain word is one of these (`dart run build_runner watch`).
    Words(&'static [&'static str]),
    /// No subcommand at all (bare `sbt` opens its shell).
    NoSub,
    /// Always (watchers such as `ibazel`, `nodemon`).
    Always,
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

const fn endless(tools: &'static [&'static str], when: Endless, fix: Fix) -> EndlessRule {
    EndlessRule {
        tools,
        sub: None,
        when,
        fix,
        what: "watch mode",
    }
}

const WATCH: &[&str] = &["--watch", "-w"];

/// Watch modes and interactive sessions. They are refused: a job in the queue must end.
pub const ENDLESS: &[EndlessRule] = &[
    endless(
        &["tsc", "vue-tsc", "tsgo"],
        Endless::Flags(WATCH),
        Fix::RemoveFlags(WATCH),
    ),
    endless(
        &["jest"],
        Endless::Flags(&["--watch", "--watchall"]),
        Fix::RemoveFlags(&["--watch", "--watchall"]),
    ),
    endless(&["vitest"], Endless::Sub(&["watch", "dev"]), Fix::SetSub("run")),
    endless(&["vitest"], Endless::Flags(WATCH), Fix::RemoveFlags(WATCH)),
    EndlessRule {
        what: "watch mode (vitest watches by default in a terminal)",
        ..endless(
            &["vitest"],
            Endless::InteractiveDefault {
                one_shot: &["run", "bench", "list", "related"],
            },
            Fix::InsertSub("run"),
        )
    },
    endless(
        &["mocha", "ava", "webpack", "rspack", "rollup", "esbuild"],
        Endless::Flags(WATCH),
        Fix::RemoveFlags(WATCH),
    ),
    EndlessRule {
        sub: Some(&["build"]),
        ..endless(&["vite"], Endless::Flags(WATCH), Fix::RemoveFlags(WATCH))
    },
    endless(
        &["deno", "bun"],
        Endless::Flags(&["--watch"]),
        Fix::RemoveFlags(&["--watch"]),
    ),
    endless(
        GRADLE,
        Endless::Flags(&["--continuous", "-t"]),
        Fix::RemoveFlags(&["--continuous", "-t"]),
    ),
    endless(&["parcel"], Endless::Sub(&["watch"]), Fix::SetSub("build")),
    endless(&["turbo"], Endless::Sub(&["watch"]), Fix::SetSub("run")),
    endless(&["nx", "lerna"], Endless::Sub(&["watch"]), Fix::None),
    endless(&["dotnet"], Endless::Sub(&["watch"]), Fix::DropSub),
    endless(&["cargo"], Endless::Sub(&["watch"]), Fix::None),
    endless(
        &[
            "ibazel",
            "ptw",
            "pytest-watch",
            "watchexec",
            "nodemon",
            "bacon",
            "air",
            "guard",
        ],
        Endless::Always,
        Fix::None,
    ),
    endless(&["mix"], Endless::Sub(&["test.watch"]), Fix::SetSub("test")),
    endless(
        &["stack"],
        Endless::Flags(&["--file-watch"]),
        Fix::RemoveFlags(&["--file-watch"]),
    ),
    endless(&["dune"], Endless::Flags(WATCH), Fix::RemoveFlags(WATCH)),
    endless(
        &["zig"],
        Endless::Flags(&["--watch"]),
        Fix::RemoveFlags(&["--watch"]),
    ),
    endless(&["sbt"], Endless::WordPrefix("~"), Fix::None),
    EndlessRule {
        what: "an interactive shell",
        ..endless(&["sbt"], Endless::NoSub, Fix::None)
    },
    EndlessRule {
        what: "an interactive shell",
        ..endless(&["sbt"], Endless::Sub(&["shell", "console"]), Fix::None)
    },
    EndlessRule {
        sub: Some(&["run"]),
        what: "build_runner's watch mode",
        ..endless(&["dart"], Endless::Words(&["watch", "serve"]), Fix::None)
    },
    EndlessRule {
        sub: Some(&["run", "exec"]),
        what: "an interactive container",
        ..endless(
            &["docker", "podman"],
            Endless::FlagsOrCluster(&["-i", "-t", "--interactive", "--tty"]),
            Fix::None,
        )
    },
];

/// Commands that never end by design but are not heavy (dev servers, app runs) or only touch
/// the network (installs, plans): they run at once, outside the queue.
pub const LIGHT: &[Rule] = &[
    rule(
        &["vite"],
        When::NoSubOr(&["dev", "serve", "preview"]),
        "dev server",
    ),
    rule(
        &["next", "nuxt", "nuxi", "astro", "remix"],
        When::Sub(&["dev", "start", "preview"]),
        "dev server",
    ),
    rule(&["ng"], When::Sub(&["serve"]), "dev server"),
    rule(&["webpack", "rspack"], When::Sub(&["serve"]), "dev server"),
    rule(&["storybook"], When::Sub(&["dev"]), "dev server"),
    rule(&["parcel"], When::NotSub(&["build", "watch"]), "dev server"),
    rule(&["turbo"], When::Sub(DEV_TASKS), "dev server"),
    rule(&["turbo", "lerna"], When::SubThen("run", DEV_TASKS), "dev server"),
    rule(&["nx"], When::Sub(&["serve", "dev"]), "dev server"),
    rule(
        MAKE,
        When::Sub(&["dev", "run", "serve", "start", "watch"]),
        "dev server",
    ),
    rule(
        PACKAGE_MANAGERS,
        When::Script(&["dev", "start", "serve", "preview"]),
        "dev server",
    ),
    rule(
        &["deno"],
        When::SubThen("task", &["dev", "start", "serve"]),
        "dev server",
    ),
    rule(&["deno"], When::Sub(&["serve"]), "dev server"),
    rule(&["cargo"], When::Sub(&["run", "r"]), "app run"),
    rule(&["go"], When::Sub(&["run"]), "app run"),
    rule(&["dotnet"], When::Sub(&["run"]), "app run"),
    rule(&["flutter"], When::Sub(&["run", "attach"]), "app run"),
    rule(&["swift"], When::Sub(&["run"]), "app run"),
    rule(BAZEL, When::Sub(&["run"]), "app run"),
    rule(BUCK, When::Sub(&["run"]), "app run"),
    rule(&["zig"], When::SubThen("build", &["run"]), "app run"),
    rule(&["sbt"], When::AnyWord(&["run", "reStart"]), "app run"),
    rule(
        &["xcodebuild"],
        When::Flag(&[
            "-list",
            "-showbuildsettings",
            "-showsdks",
            "-showdestinations",
            "-version",
        ]),
        "only prints project information",
    ),
    rule(
        MAVEN,
        When::AnyWord(&["spring-boot:run", "quarkus:dev", "jetty:run", "exec:java"]),
        "dev server",
    ),
    rule(
        GRADLE,
        When::AnyWord(&["bootRun", "run", "quarkusDev"]),
        "dev server",
    ),
    rule(
        &["rails"],
        When::Sub(&["s", "server", "c", "console"]),
        "dev server",
    ),
    rule(&["mix"], When::Sub(&["phx.server", "run"]), "dev server"),
    rule(&["php"], When::SubThen("artisan", &["serve"]), "dev server"),
    rule(&["symfony"], When::Sub(&["serve", "server:start"]), "dev server"),
    rule(
        &["python", "python3", "py"],
        When::SubThen("manage.py", &["runserver"]),
        "dev server",
    ),
    rule(&["uvicorn", "gunicorn", "hypercorn"], When::Always, "dev server"),
    rule(&["flask"], When::Sub(&["run"]), "dev server"),
    rule(&["pod"], When::Sub(&["install", "update"]), "install"),
    // `pnpm -r install` and friends: package management, not a build.
    rule(
        &["pnpm"],
        When::Sub(&[
            "install",
            "i",
            "add",
            "update",
            "up",
            "remove",
            "rm",
            "uninstall",
            "list",
            "ls",
            "outdated",
            "publish",
            "why",
            "link",
            "unlink",
            "prune",
            "dedupe",
            "audit",
        ]),
        "install",
    ),
    rule(
        &["composer"],
        When::Sub(&["install", "update", "require"]),
        "install",
    ),
    rule(
        &["terraform", "tofu"],
        When::Sub(&["plan", "apply", "init", "destroy"]),
        "infrastructure",
    ),
    rule(
        &["pulumi"],
        When::Sub(&["preview", "up", "destroy"]),
        "infrastructure",
    ),
];

// ---- Lookups ---------------------------------------------------------------------------

fn sub_is(rest: &[String], list: &[&str]) -> bool {
    first_non_flag(rest).is_some_and(|s| list.contains(&s.to_ascii_lowercase().as_str()))
}

/// Whether `rest` (the words after the tool) matches an endless rule.
pub fn endless_rule(tool: &str, rest: &[String], stdin_tty: bool) -> Option<&'static EndlessRule> {
    ENDLESS.iter().find(|r| {
        r.tools.contains(&tool)
            && r.sub.is_none_or(|subs| sub_is(rest, subs))
            && match r.when {
                Endless::Flags(flags) => rest.iter().any(|w| exact_flag(w, flags)),
                Endless::FlagsOrCluster(flags) => rest.iter().any(|w| has_flag(w, flags)),
                Endless::Sub(list) => sub_is(rest, list),
                Endless::WordPrefix(p) => rest.iter().any(|w| w.starts_with(p)),
                Endless::Words(list) => rest.iter().any(|w| list.contains(&w.as_str())),
                Endless::NoSub => first_non_flag(rest).is_none(),
                Endless::Always => true,
                Endless::InteractiveDefault { one_shot } => {
                    stdin_tty && !first_non_flag(rest).is_some_and(|s| one_shot.contains(&s))
                }
            }
    })
}

/// `w` is one of `flags` (compared lowercase) or `flag=…`.
pub fn exact_flag(w: &str, flags: &[&str]) -> bool {
    let lw = w.to_ascii_lowercase();
    flags
        .iter()
        .any(|f| lw == *f || lw.strip_prefix(f).is_some_and(|r| r.starts_with('=')))
}

/// Like [`exact_flag`], and also a short flag inside a cluster such as `-it`.
pub fn has_flag(w: &str, flags: &[&str]) -> bool {
    exact_flag(w, flags)
        || flags.iter().any(|f| {
            f.len() == 2
                && !f.starts_with("--")
                && w.starts_with('-')
                && !w.starts_with("--")
                && w[1..].contains(&f[1..])
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

/// Cost of a job: `[kinds.<kind>]` in the config, then the matched rule's own cost (when the
/// job is of that rule's kind), then the kind's default.
pub fn job_cost(kind: &str, rule: Option<&Rule>, kinds: &BTreeMap<String, KindConfig>) -> u32 {
    kinds
        .get(kind)
        .and_then(|k| k.cost)
        .or_else(|| rule.filter(|r| r.kind == kind).and_then(|r| r.cost))
        .unwrap_or_else(|| cost(kind, kinds))
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

/// The task name of a Gradle task path (`:app:connectedDebugAndroidTest`).
fn task(w: &str) -> &str {
    w.rsplit(':').next().unwrap_or(w)
}

impl When {
    /// Whether the condition holds for this tool and its arguments.
    pub fn matches(&self, tool: &str, rest: &[String]) -> bool {
        let sub = || first_non_flag(rest).map(str::to_ascii_lowercase);
        let plain = || rest.iter().filter(|w| !w.starts_with('-'));
        match *self {
            When::Always => true,
            When::Sub(list) => sub().is_some_and(|s| list.contains(&s.as_str())),
            When::Compose(list) => compose_sub(rest).is_some_and(|s| list.contains(&s.as_str())),
            When::DockerCompose(list) => after_docker_compose(rest)
                .and_then(compose_sub)
                .is_some_and(|s| list.contains(&s.as_str())),
            When::Script(names) => script_matches(tool, rest, names),
            When::NoSubOr(list) => sub().is_none_or(|s| list.contains(&s.as_str())),
            When::NotSub(list) => sub().is_none_or(|s| !list.contains(&s.as_str())),
            When::AnyWord(list) => plain().any(|w| list.contains(&w.as_str()) || list.contains(&task(w))),
            When::AnyWordPrefix(list) => plain().any(|w| list.iter().any(|p| task(w).starts_with(p))),
            When::Flag(list) => rest.iter().any(|w| exact_flag(w, list)),
            When::SubThen(first, list) => {
                let mut words = plain();
                words.next().is_some_and(|w| w == first)
                    && words.next().is_some_and(|w| list.contains(&w.as_str()))
            }
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

/// The exclusive-key rule for a job of `kind` run by `tool` with `rest`: the matched rule's
/// key, else any rule of that kind for this tool (`--kind` given), else the kind's.
pub fn key_rule(kind: &str, tool: &str, rest: &[String]) -> KeyRule {
    let matched = find_rule(tool, rest)
        .filter(|r| r.kind == kind)
        .and_then(|r| r.key);
    matched
        .or_else(|| {
            RULES
                .iter()
                .find(|r| r.kind == kind && r.tools.contains(&tool) && r.key.is_some())
                .and_then(|r| r.key)
        })
        .or_else(|| spec(kind).map(|s| s.key))
        .unwrap_or(KeyRule::None)
}

/// The hints for a tool and its arguments: the matched rule's. A command no rule matches
/// keeps the tool's environment hints only, so `cargo xtask dist` still gets
/// `CARGO_BUILD_JOBS` while `go mod tidy` gets no `-p`.
pub fn hints_for(tool: &str, rest: &[String]) -> Vec<Hint> {
    if let Some(r) = find_rule(tool, rest) {
        return r.hints.to_vec();
    }
    RULES
        .iter()
        .find(|r| r.tools.contains(&tool) && !r.hints.is_empty())
        .map(|r| {
            r.hints
                .iter()
                .filter(|h| matches!(h, Hint::Env { .. }))
                .copied()
                .collect()
        })
        .unwrap_or_default()
}

/// One line per tool with a worker hint: what calmrun would pass with these settings
/// (for `calmrun doctor`).
pub fn hint_summary(
    workers: &dyn Fn(Workers, u32) -> u32,
    kinds: &BTreeMap<String, KindConfig>,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for r in RULES.iter().filter(|r| !r.hints.is_empty()) {
        let n_cost = job_cost(r.kind, Some(r), kinds);
        let parts: Vec<String> = r
            .hints
            .iter()
            .map(|h| match h {
                Hint::Arg(a) => match a.style {
                    Style::Joined => format!("{}{}", a.flag, a.count(workers(a.workers, n_cost))),
                    Style::Split => format!("{} {}", a.flag, a.count(workers(a.workers, n_cost))),
                },
                Hint::Env { var, workers: w, .. } => format!("{var}={}", workers(*w, n_cost)),
                Hint::Replace {
                    flags,
                    from,
                    workers: w,
                } => {
                    format!("{} {from} -> {} {}", flags[0], flags[0], workers(*w, n_cost))
                }
            })
            .collect();
        let line = format!("{:<28} {:<9} {}", r.tools.join("/"), r.kind, parts.join("  "));
        if !out.contains(&line) {
            out.push(line);
        }
    }
    out
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
        assert_eq!(cost("monorepo", &kinds), 3);
        assert_eq!(cost("whatever", &kinds), UNKNOWN_COST);
        assert_eq!(key_rule("build", "cargo", &w("build")), KeyRule::CargoTarget);
        assert_eq!(key_rule("build", "vite", &w("build")), KeyRule::NodeModules);
        assert_eq!(key_rule("vitest", "cargo", &w("build")), KeyRule::NodeModules);
        assert_eq!(key_rule("eslint", "eslint", &w(".")), KeyRule::None);
        assert_eq!(key_rule("build", "gradlew", &w("assembleDebug")), GRADLE_KEY);
        assert_eq!(key_rule("build", "make", &w("all")), MAKE_KEY);
    }

    #[test]
    fn rule_costs_and_config_overrides() {
        let none = BTreeMap::new();
        let gcc = find_rule("gcc", &w("-O2 -c big.c"));
        assert_eq!(job_cost("build", gcc, &none), 1);
        assert_eq!(job_cost("build", find_rule("make", &w("")), &none), 2);
        let mut kinds = BTreeMap::new();
        kinds.insert(
            "build".to_string(),
            KindConfig {
                cost: Some(5),
                patterns: vec![],
            },
        );
        assert_eq!(
            job_cost("build", gcc, &kinds),
            5,
            "config wins over a rule's cost"
        );
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

    /// (tool, arguments, kind) for every ecosystem; `None` means not heavy.
    const DETECTION: &[(&str, &str, Option<&str>)] = &[
        // C / C++
        ("make", "", Some("build")),
        ("gmake", "-C src all", Some("build")),
        ("cmake", "--build build --config Release", Some("build")),
        ("cmake", "-S . -B build", None),
        ("ninja", "-C out", Some("build")),
        ("meson", "compile -C builddir", Some("build")),
        ("meson", "setup builddir", None),
        ("ctest", "--test-dir build", Some("test")),
        ("msbuild", "App.sln /p:Configuration=Release", Some("build")),
        ("clang++", "-O2 -c huge.cpp", Some("build")),
        ("cl", "/c big.cpp", Some("build")),
        ("bazel", "build //...", Some("build")),
        ("bazelisk", "test //...", Some("test")),
        ("bazel", "query //...", None),
        ("scons", "-Q", Some("build")),
        ("nmake", "/f Makefile.win", Some("build")),
        ("buck2", "build //app:app", Some("build")),
        ("buck2", "test //...", Some("test")),
        ("buck2", "targets //...", None),
        ("xmake", "", Some("build")),
        // JVM, Android
        ("gradlew", "assembleDebug", Some("build")),
        ("gradlew", "bundleRelease", Some("build")),
        ("gradlew", ":app:connectedDebugAndroidTest", Some("test")),
        ("gradle", "test", Some("test")),
        ("mvnw", "-q package", Some("build")),
        ("mvn", "verify", Some("test")),
        ("mvn", "surefire:test", Some("test")),
        ("mvnd", "install", Some("build")),
        ("sbt", "compile", Some("build")),
        ("ant", "dist", Some("build")),
        ("kotlinc", "Main.kt", Some("build")),
        // .NET
        ("dotnet", "build", Some("build")),
        ("dotnet", "test --no-build", Some("test")),
        ("dotnet", "publish -c Release", Some("build")),
        ("dotnet", "pack", Some("build")),
        ("dotnet", "--info", None),
        // Go, Rust
        ("go", "build ./...", Some("build")),
        ("go", "test ./...", Some("test")),
        ("go", "vet ./...", Some("lint")),
        ("go", "install ./cmd/x", Some("build")),
        ("go", "mod tidy", None),
        ("golangci-lint", "run", Some("lint")),
        ("cargo", "build --release", Some("build")),
        ("cargo", "clippy", Some("build")),
        ("cargo", "nextest run", Some("test")),
        ("cargo", "fmt", None),
        // Swift, Apple, Dart
        ("swift", "build", Some("build")),
        ("swift", "test", Some("test")),
        ("xcodebuild", "-scheme App build", Some("build")),
        ("xcodebuild", "-scheme App test", Some("test")),
        ("flutter", "build apk", Some("build")),
        ("flutter", "test", Some("test")),
        ("dart", "test", Some("test")),
        ("dart", "run build_runner build -d", Some("build")),
        ("dart", "run bin/main.dart", None),
        // Ruby, PHP, Elixir, Erlang
        ("rspec", "spec/models", Some("test")),
        ("rake", "test", Some("test")),
        ("rails", "test", Some("test")),
        ("phpunit", "", Some("test")),
        ("paratest", "-p 4", Some("test")),
        ("mix", "test", Some("test")),
        ("mix", "compile", Some("build")),
        ("rebar3", "eunit", Some("test")),
        ("cucumber", "", Some("test")),
        ("parallel_rspec", "spec", Some("test")),
        ("composer", "test", Some("test")),
        ("composer", "run-script test", Some("test")),
        ("composer", "install", None),
        ("php", "artisan test --parallel", Some("test")),
        ("php", "artisan migrate", None),
        // Haskell, OCaml, Zig
        ("stack", "build", Some("build")),
        ("cabal", "test", Some("test")),
        ("dune", "build", Some("build")),
        ("zig", "build", Some("build")),
        // JS extra and monorepos
        ("rspack", "build", Some("build")),
        ("parcel", "build index.html", Some("build")),
        ("parcel", "index.html", None),
        ("nuxt", "build", Some("build")),
        ("astro", "build", Some("build")),
        ("storybook", "build", Some("build")),
        ("expo", "export", Some("build")),
        ("mocha", "", Some("test")),
        ("deno", "test", Some("test")),
        ("deno", "check main.ts", Some("typecheck")),
        ("bun", "build ./index.ts", Some("build")),
        ("turbo", "run build", Some("monorepo")),
        ("nx", "run-many -t test", Some("monorepo")),
        ("nx", "affected -t build", Some("monorepo")),
        ("lerna", "run test", Some("monorepo")),
        ("pnpm", "-r run build", Some("monorepo")),
        ("pnpm", "--recursive test", Some("monorepo")),
        ("rush", "build", Some("monorepo")),
        ("rushx", "build", Some("build")),
        ("moon", "run :build", Some("monorepo")),
        ("moon", "ci", Some("monorepo")),
        // Python
        ("tox", "-e py312", Some("test")),
        ("nox", "", Some("test")),
        ("mypy", "src", Some("typecheck")),
        ("pyright", "", Some("typecheck")),
        ("ruff", "check .", Some("lint")),
        ("maturin", "build --release", Some("build")),
        ("pip", "wheel .", Some("build")),
        ("pip", "install -r req.txt", None),
        // Containers, infrastructure
        ("docker", "buildx build .", Some("docker")),
        ("docker", "compose build", Some("docker")),
        ("podman", "build .", Some("docker")),
        ("packer", "build x.pkr.hcl", Some("docker")),
        ("podman", "compose up -d", Some("docker")),
        ("podman-compose", "build", Some("docker")),
        ("buildah", "bud -t x .", Some("docker")),
        ("cargo", "install ripgrep", None),
        ("terraform", "plan", None),
    ];

    #[test]
    fn detection_table() {
        for (tool, args, want) in DETECTION {
            let got = find_rule(tool, &w(args)).map(|r| r.kind);
            assert_eq!(got, *want, "{tool} {args}");
        }
    }

    #[test]
    fn android_device_tests_need_the_emulator() {
        let r = find_rule("gradlew", &w(":app:connectedDebugAndroidTest")).unwrap();
        assert_eq!(r.needs, ["emulator"]);
        assert!(find_rule("gradlew", &w("assembleDebug"))
            .unwrap()
            .needs
            .is_empty());
    }

    /// (tool, arguments, what) for servers and installs that run outside the queue.
    const LIGHT_TABLE: &[(&str, &str)] = &[
        ("vite", ""),
        ("next", "dev"),
        ("nuxt", "dev"),
        ("storybook", "dev"),
        ("npm", "run dev"),
        ("deno", "task dev"),
        ("cargo", "run --release"),
        ("go", "run ."),
        ("dotnet", "run"),
        ("flutter", "run"),
        ("swift", "run"),
        ("bazel", "run //app"),
        ("mvn", "spring-boot:run"),
        ("mvnw", "quarkus:dev"),
        ("gradlew", "bootRun"),
        ("rails", "server"),
        ("rails", "s"),
        ("mix", "phx.server"),
        ("pod", "install"),
        ("composer", "install"),
        ("pnpm", "-r install"),
        ("pnpm", "install -r"),
        ("pnpm", "--recursive update"),
        ("terraform", "apply"),
        ("pulumi", "up"),
        ("python", "manage.py runserver"),
        // Monorepo runners, Makefile task runners, other ecosystems
        ("turbo", "dev"),
        ("turbo", "run dev --filter web"),
        ("lerna", "run start"),
        ("nx", "serve web"),
        ("make", "dev"),
        ("make", "run"),
        ("parcel", "index.html"),
        ("parcel", "serve src/index.html"),
        ("gradlew", ":app:bootRun"),
        ("sbt", "run"),
        ("zig", "build run"),
        ("buck2", "run //app:app"),
        ("symfony", "serve"),
        ("xcodebuild", "-list"),
        ("xcodebuild", "-showBuildSettings -scheme App"),
    ];

    #[test]
    fn servers_and_installs_are_light() {
        for (tool, args) in LIGHT_TABLE {
            assert!(light_rule(tool, &w(args)).is_some(), "{tool} {args}");
        }
        assert!(light_rule("cargo", &w("build")).is_none());
        assert!(light_rule("deno", &w("task build")).is_none());
        for (tool, args) in [
            ("turbo", "run build"),
            ("make", "all"),
            ("make", ""),
            ("parcel", "build index.html"),
            ("zig", "build"),
            ("sbt", "compile"),
            ("gradlew", "runTests"),
            ("xcodebuild", "-scheme App build"),
        ] {
            assert!(light_rule(tool, &w(args)).is_none(), "{tool} {args}");
        }
    }

    /// (tool, arguments) for watch modes that are refused.
    const ENDLESS_TABLE: &[(&str, &str)] = &[
        ("gradlew", "build --continuous"),
        ("gradle", "-t test"),
        ("dotnet", "watch test"),
        ("cargo", "watch -x test"),
        ("webpack", "--watch"),
        ("rollup", "-c -w"),
        ("vite", "build --watch"),
        ("deno", "test --watch"),
        ("bun", "test --watch"),
        ("mocha", "--watch"),
        ("stack", "build --file-watch"),
        ("dune", "build -w"),
        ("zig", "build --watch"),
        ("sbt", "~compile"),
        ("mix", "test.watch"),
        ("ibazel", "build //..."),
        ("docker", "run -it ubuntu"),
        ("podman", "run -i alpine"),
        ("turbo", "watch build"),
        ("nx", "watch --all -- echo"),
        ("lerna", "watch -- tsc"),
        ("parcel", "watch index.html"),
        ("sbt", ""),
        ("sbt", "console"),
        ("dart", "run build_runner watch"),
        ("bacon", "clippy"),
        ("air", ""),
        ("guard", ""),
    ];

    #[test]
    fn watch_modes_are_endless() {
        for (tool, args) in ENDLESS_TABLE {
            assert!(endless_rule(tool, &w(args), false).is_some(), "{tool} {args}");
        }
        for (tool, args) in [
            ("gradle", "-Pfoo=1 build"),
            ("docker", "build -t x ."),
            ("vite", "build"),
            ("sbt", "compile test"),
            ("dart", "run build_runner build"),
            ("turbo", "run build"),
        ] {
            assert!(endless_rule(tool, &w(args), false).is_none(), "{tool} {args}");
        }
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
        let flag = |t: &str, a: &str| match hints_for(t, &w(a)).first() {
            Some(Hint::Arg(h)) => Some(h.flag),
            Some(Hint::Env { var, .. }) => Some(*var),
            Some(Hint::Replace { flags, .. }) => Some(flags[0]),
            None => None,
        };
        assert_eq!(flag("vitest", "run"), Some("--maxWorkers="));
        assert_eq!(flag("cargo", "build"), Some("CARGO_BUILD_JOBS"));
        assert_eq!(
            flag("cargo", "xtask dist"),
            Some("CARGO_BUILD_JOBS"),
            "any cargo subcommand"
        );
        assert_eq!(flag("make", ""), None, "make is serial unless asked");
        assert_eq!(flag("mvn", "package"), None, "plain Maven is serial unless asked");
        assert_eq!(flag("mvnd", "package"), Some("-T"));
        assert_eq!(flag("go", "mod tidy"), None, "no -p for other go subcommands");
        assert_eq!(flag("flutter", "build apk"), None);
        assert_eq!(flag("buck2", "build //..."), Some("-j"));
        assert_eq!(flag("moon", "run :build"), Some("MOON_CONCURRENCY"));
        assert_eq!(flag("cmake", "--build b"), Some("--parallel"));
        assert_eq!(flag("go", "test ./..."), Some("-p"));
        assert_eq!(flag("dotnet", "build"), Some("-m:"));
        assert_eq!(flag("bazel", "build //..."), Some("--jobs="));
        assert_eq!(flag("turbo", "run build"), Some("--concurrency="));
        assert_eq!(flag("pytest", "-n auto"), Some("-n"));
        assert_eq!(flag("jest", ""), None);
        let n = hints_for("cargo", &w("nextest run")).len();
        assert_eq!(n, 2, "nextest gets build jobs and test threads");
    }

    #[test]
    fn doctor_summary_lists_tools_with_numbers() {
        let lines = hint_summary(&|_, cost| cost * 3, &BTreeMap::new());
        assert!(
            lines.iter().any(|l| l.starts_with("ninja") && l.ends_with("-j6")),
            "{lines:?}"
        );
        assert!(lines.iter().any(|l| l.contains("--parallel 6")), "{lines:?}");
        assert!(
            lines.iter().any(|l| l.contains("--concurrency=9")),
            "monorepo cost 3: {lines:?}"
        );
        // Tools with a small default are capped at it, never raised.
        assert!(lines.iter().any(|l| l.contains("--parallel=3")), "nx: {lines:?}");
        assert!(
            lines.iter().any(|l| l.contains("--workspace-concurrency=4")),
            "pnpm: {lines:?}"
        );
    }
}
