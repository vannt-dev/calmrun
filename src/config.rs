//! Configuration: defaults derived from the machine, the optional `config.toml`, and env vars.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const GB: f64 = 1024.0 * 1024.0 * 1024.0;

/// Facts about the machine that drive the defaults.
#[derive(Debug, Clone, Copy)]
pub struct Machine {
    pub logical_cores: usize,
    pub total_ram: u64,
}

/// Per-kind settings from the `[kinds]` table.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct KindConfig {
    /// Cost in points for this kind.
    pub cost: Option<u32>,
    /// Extra command patterns ("make", "bazel build") that select this kind.
    #[serde(default)]
    pub patterns: Vec<String>,
    /// Run time limit for this kind in minutes (0 = none); wins over the global one.
    pub max_run_min: Option<f64>,
}

/// The raw config file.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    pub budget: Option<u32>,
    pub min_free_gb: Option<f64>,
    pub pass_limit_min: Option<f64>,
    pub vitest_workers: Option<u32>,
    pub ceiling_gb: Option<f64>,
    pub min_free_disk_gb: Option<f64>,
    pub critical_free_gb: Option<f64>,
    pub max_run_min: Option<f64>,
    /// Copy every CLI run's output to a log file (`--log` by default).
    pub log: Option<bool>,
    #[serde(default)]
    pub kinds: BTreeMap<String, KindConfig>,
    /// Named resources: slots (`[resources] db = 1`) or instances (`android = ["emulator-5554"]`,
    /// or a table with `instances`, `discover`, `health`, `env`).
    #[serde(default)]
    pub resources: BTreeMap<String, crate::pools::Entry>,
}

/// Effective settings after defaults, file and env are merged.
#[derive(Debug, Clone)]
pub struct Config {
    pub budget: u32,
    pub min_free_bytes: u64,
    pub pass_limit_ms: u64,
    pub vitest_workers: u32,
    pub low_cpu: bool,
    /// Stop a job whose process tree uses more than this (0 = off).
    pub ceiling_bytes: u64,
    /// Stop the biggest job when free RAM stays below this for a few seconds (0 = off).
    pub critical_free_bytes: u64,
    /// A job waits while its cwd's drive has less free space than this (0 = off).
    pub min_free_disk_bytes: u64,
    /// Run time limit in minutes for every kind without its own (`None`: the built-in
    /// per-kind defaults; 0 = no limit).
    pub max_run_min: Option<f64>,
    /// CLI runs copy their output to a log file unless `--no-log` (`log = true`).
    pub log: bool,
    pub kinds: BTreeMap<String, KindConfig>,
    /// Slots per named resource (a fixed instance list counts its instances).
    pub resources: BTreeMap<String, u32>,
    /// Resources with instances, handed out one per job.
    pub pools: BTreeMap<String, crate::pools::Pool>,
}

/// Where a run's time limit comes from, named when it stops the run.
#[derive(Debug, Clone, PartialEq)]
pub enum LimitFrom {
    /// `--timeout` (CLI) or `timeout_s` (MCP).
    Asked,
    /// `[kinds.<kind>] max_run_min`.
    Kind(String),
    /// `max_run_min` or `CALMRUN_MAX_RUN_MIN`.
    Global,
    /// The built-in default of the kind.
    Builtin(String),
}

impl LimitFrom {
    /// How to change the limit, for the message after a stop.
    pub fn how_to_change(&self) -> String {
        let one_run = "--timeout SECS for one run (--timeout 0 = no limit)";
        match self {
            LimitFrom::Asked => format!("pass a larger {one_run}, or a larger timeout_s (MCP run_task)"),
            LimitFrom::Kind(k) => format!("raise [kinds.{k}] max_run_min in the config, or {one_run}"),
            LimitFrom::Global => {
                format!("raise max_run_min in the config (or CALMRUN_MAX_RUN_MIN), or {one_run}")
            }
            LimitFrom::Builtin(k) => format!(
                "set [kinds.{k}] max_run_min (or max_run_min for every kind, or CALMRUN_MAX_RUN_MIN) \
                 in the config; 0 = no limit; or {one_run}"
            ),
        }
    }
}

/// The run time limit the config gives a job of `kind`: `[kinds.<kind>] max_run_min`, then the
/// global `max_run_min` (env over file), then the kind's built-in default. `None`: no limit;
/// 0 at any level means no limit.
pub fn run_limit(cfg: &Config, kind: &str) -> Option<(std::time::Duration, LimitFrom)> {
    let (min, from) = if let Some(m) = cfg.kinds.get(kind).and_then(|k| k.max_run_min) {
        (m, LimitFrom::Kind(kind.to_string()))
    } else if let Some(m) = cfg.max_run_min {
        (m, LimitFrom::Global)
    } else {
        (
            crate::kinds::default_max_run_min(kind)?,
            LimitFrom::Builtin(kind.to_string()),
        )
    };
    (min.is_finite() && min > 0.0).then(|| (std::time::Duration::from_secs_f64(min * 60.0), from))
}

/// `db=1,gpu=2` -> map.
pub fn parse_resources(s: &str) -> BTreeMap<String, u32> {
    s.split(',')
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            Some((k.trim().to_string(), v.trim().parse().ok()?))
        })
        .filter(|(k, _): &(String, u32)| !k.is_empty())
        .collect()
}

/// Cores kept free for the editor, the agents and the OS.
const RESERVED_CORES: usize = 2;
/// RAM per budget point. The free-RAM gate is what keeps a RAM-poor machine responsive, so
/// the RAM cap only has to stop a budget far beyond what the RAM could ever hold.
const GB_PER_POINT: f64 = 2.5;
/// The budget never goes below this.
const MIN_BUDGET: u64 = 2;
/// Free-RAM gate: the RAM divided by this, at least this many GB. A fixed 3 GB was above the
/// free RAM of a 16 GB laptop at rest, so every second job waited for the first to end.
const MIN_FREE_RAM_DIVISOR: f64 = 10.0;
const MIN_FREE_FLOOR_GB: f64 = 1.5;
/// Memory ceiling for one job's tree, as a fraction of the RAM.
const CEILING_FRACTION: f64 = 0.5;
/// Critical free-RAM floor: a fraction of the RAM, at most this many GB.
const CRITICAL_FRACTION: f64 = 0.05;
const CRITICAL_MAX_GB: f64 = 1.0;
/// Free space needed on the cwd's drive.
pub const DEFAULT_MIN_FREE_DISK_GB: f64 = 5.0;
/// Minutes a waiter may be passed.
pub const DEFAULT_PASS_LIMIT_MIN: f64 = 10.0;

fn ram_gb(m: Machine) -> f64 {
    m.total_ram as f64 / GB
}

/// Logical cores - 2, capped at floor(RAM GB / 2.5), at least 2.
pub fn default_budget(m: Machine) -> u32 {
    let by_cores = m.logical_cores.saturating_sub(RESERVED_CORES) as u64;
    let by_ram = (ram_gb(m) / GB_PER_POINT).floor() as u64;
    by_cores.min(by_ram).max(MIN_BUDGET) as u32
}

/// max(1.5, RAM / 10) GB.
pub fn default_min_free_gb(m: Machine) -> f64 {
    (ram_gb(m) / MIN_FREE_RAM_DIVISOR).max(MIN_FREE_FLOOR_GB)
}

/// Half of the RAM.
pub fn default_ceiling_gb(m: Machine) -> f64 {
    ram_gb(m) * CEILING_FRACTION
}

/// 5% of the RAM, at most 1 GB.
pub fn default_critical_free_gb(m: Machine) -> f64 {
    (ram_gb(m) * CRITICAL_FRACTION).min(CRITICAL_MAX_GB)
}

/// 1 worker up to 4 cores, 2 below 12, else 3.
pub fn default_vitest_workers(m: Machine) -> u32 {
    match m.logical_cores {
        0..=4 => 1,
        5..=11 => 2,
        _ => 3,
    }
}

/// The per-user state directory (`CALMRUN_HOME` wins).
pub fn home_dir() -> PathBuf {
    if let Some(h) = std::env::var_os("CALMRUN_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(h);
    }
    crate::sys::state_base_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("calmrun")
}

/// The config file: `CALMRUN_CONFIG`, else `config.toml` in the state directory.
pub fn config_path(home: &Path) -> PathBuf {
    match std::env::var_os("CALMRUN_CONFIG").filter(|v| !v.is_empty()) {
        Some(p) => PathBuf::from(p),
        None => home.join("config.toml"),
    }
}

/// Parse the config file; a missing file is an empty config.
pub fn read_file_config(path: &Path) -> Result<FileConfig, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(FileConfig::default()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Merge defaults, file and env. `env` is a lookup so tests can inject values.
pub fn resolve(m: Machine, file: FileConfig, env: &dyn Fn(&str) -> Option<String>) -> Config {
    let num = |k: &str| env(k).and_then(|v| v.trim().parse::<f64>().ok());
    let budget = num("CALMRUN_BUDGET")
        .map(|v| v as u32)
        .or(file.budget)
        .unwrap_or_else(|| default_budget(m))
        .max(1);
    let min_free_gb = num("CALMRUN_MIN_FREE_GB")
        .or(file.min_free_gb)
        .unwrap_or_else(|| default_min_free_gb(m))
        .max(0.0);
    let pass_min = num("CALMRUN_PASS_MIN")
        .or(file.pass_limit_min)
        .unwrap_or(DEFAULT_PASS_LIMIT_MIN)
        .max(0.0);
    let vitest_workers = num("CALMRUN_VITEST_WORKERS")
        .map(|v| v as u32)
        .or(file.vitest_workers)
        .unwrap_or_else(|| default_vitest_workers(m))
        .max(1);
    let ceiling_gb = num("CALMRUN_CEILING_GB")
        .or(file.ceiling_gb)
        .unwrap_or_else(|| default_ceiling_gb(m))
        .max(0.0);
    let critical_gb = num("CALMRUN_CRITICAL_FREE_GB")
        .or(file.critical_free_gb)
        .unwrap_or_else(|| default_critical_free_gb(m))
        .max(0.0);
    let disk_gb = num("CALMRUN_MIN_FREE_DISK_GB")
        .or(file.min_free_disk_gb)
        .unwrap_or(DEFAULT_MIN_FREE_DISK_GB)
        .max(0.0);
    let max_run_min = num("CALMRUN_MAX_RUN_MIN")
        .or(file.max_run_min)
        .map(|v| v.max(0.0));
    let low_cpu = !matches!(
        env("CALMRUN_LOW_CPU").as_deref().map(str::trim),
        Some("0" | "false" | "no" | "off")
    );
    let log = match env("CALMRUN_LOG").as_deref().map(str::trim) {
        Some("1" | "true" | "yes" | "on") => true,
        Some("0" | "false" | "no" | "off") => false,
        _ => file.log.unwrap_or(false),
    };
    let (mut resources, mut pools) = (BTreeMap::new(), BTreeMap::new());
    for (name, entry) in file.resources {
        match entry {
            crate::pools::Entry::Slots(n) => {
                resources.insert(name, n);
            }
            crate::pools::Entry::Pool(p) => {
                if p.discover.is_none() {
                    resources.insert(name.clone(), p.instances.len() as u32);
                }
                pools.insert(name, p);
            }
        }
    }
    // The env sets slots, and a resource it names is count-only again.
    if let Some(v) = env("CALMRUN_RESOURCES") {
        for (name, n) in parse_resources(&v) {
            pools.remove(&name);
            resources.insert(name, n);
        }
    }
    Config {
        budget,
        min_free_bytes: (min_free_gb * GB) as u64,
        pass_limit_ms: (pass_min * 60_000.0) as u64,
        vitest_workers,
        low_cpu,
        ceiling_bytes: (ceiling_gb * GB) as u64,
        critical_free_bytes: (critical_gb * GB) as u64,
        min_free_disk_bytes: (disk_gb * GB) as u64,
        max_run_min,
        log,
        kinds: file.kinds,
        resources,
        pools,
    }
}

/// Bytes to (binary) gigabytes.
pub fn bytes_to_gb(b: u64) -> f64 {
    b as f64 / GB
}

/// The commented default config written by `calmrun init`.
pub fn default_config_text(m: Machine) -> String {
    format!(
        r#"# calmrun configuration. Every value is optional; env vars override these.
# Defaults shown are computed for this machine.

# Total points that may run at once (env CALMRUN_BUDGET).
# Default: logical cores - 2, capped at floor(RAM GB / 2.5), at least 2.
# budget = {budget}

# A job starts only when at least this much RAM is free (env CALMRUN_MIN_FREE_GB).
# Skipped when nothing is running. 0 disables the check. Default: max(1.5, RAM / 10).
# min_free_gb = {min_free:.1}

# Minutes a waiter may be passed by later, smaller jobs before strict order applies
# (env CALMRUN_PASS_MIN). 0 = always strict first-come order.
# pass_limit_min = 10

# Stop a job whose whole process tree uses more memory than this (env CALMRUN_CEILING_GB).
# 0 turns it off. Default: half the RAM.
# ceiling_gb = {ceiling:.1}

# When free RAM stays below this for a few seconds, stop the running job that uses the most
# memory (env CALMRUN_CRITICAL_FREE_GB). 0 turns it off. Default: 5% of RAM, at most 1 GB.
# critical_free_gb = {critical:.2}

# A job waits while the drive of its working directory has less free space than this
# (env CALMRUN_MIN_FREE_DISK_GB). 0 turns it off. If nothing else runs it starts anyway.
# min_free_disk_gb = 5

# --maxWorkers value appended to direct vitest calls (env CALMRUN_VITEST_WORKERS).
# vitest_workers = {workers}

# Run time limit in minutes: a job still running after it is stopped (exit 124). This sets
# it for every kind (env CALMRUN_MAX_RUN_MIN); [kinds.<name>] max_run_min sets one kind.
# 0 = no limit. Default: 60 for test, vitest, jest, pytest, typecheck, tsc, lint and eslint;
# none for build, monorepo, docker, capture and other kinds. `--timeout SECS` sets one run.
# max_run_min = 60

# Copy the output of every `calmrun -- <cmd>` to a log file under logs/ in the state
# directory, as --log does (env CALMRUN_LOG=1). `calmrun logs <id>` shows it. The command
# then writes to a pipe, so many tools drop colours and progress bars; --no-log skips it.
# log = false

# Named resources and how many jobs may hold each at once (env CALMRUN_RESOURCES="db=1").
# A job takes a slot with --needs db. A resource that is not listed has 1 slot.
# A resource can list its instances instead (devices, simulators): each job gets a free
# one in CALMRUN_<NAME> (and ANDROID_SERIAL for android and emulator). `discover` lists
# them with a command (first word of each output line); `health` skips an instance whose
# check fails ({{instance}} is replaced by it); `env` names another variable to set.
# [resources]
# db = 1
# android = ["emulator-5554", "emulator-5556"]
#
# [resources.emulator]
# discover = "adb devices"
# health = "adb -s {{instance}} get-state"

# Override the cost of a built-in kind, or add your own kind with command patterns.
# A pattern is the command's first words, e.g. "make" or "bazel build".
# [kinds.vitest]
# cost = 3
# max_run_min = 90
#
# [kinds.heavy]
# cost = 2
# patterns = ["make", "bazel build"]
"#,
        budget = default_budget(m),
        min_free = default_min_free_gb(m),
        workers = default_vitest_workers(m),
        ceiling = default_ceiling_gb(m),
        critical = default_critical_free_gb(m),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(cores: usize, ram_gb: u64) -> Machine {
        Machine {
            logical_cores: cores,
            total_ram: ram_gb * 1024 * 1024 * 1024,
        }
    }

    #[test]
    fn budget_defaults() {
        assert_eq!(default_budget(m(16, 32)), 12); // ram cap
        assert_eq!(default_budget(m(8, 64)), 6); // cores - 2
        assert_eq!(default_budget(m(2, 4)), 2); // min 2
        assert_eq!(default_budget(m(20, 16)), 6);
        assert_eq!(default_budget(m(4, 8)), 2); // cores - 2
                                                // A "16 GB" laptop reports 15.6 GB: 6, not 3 as floor(15.6 / 4) gave.
        let laptop = Machine {
            logical_cores: 20,
            total_ram: (15.6 * GB) as u64,
        };
        assert_eq!(default_budget(laptop), 6);
        assert!((default_min_free_gb(laptop) - 1.56).abs() < 1e-9);
    }

    #[test]
    fn min_free_and_workers() {
        assert_eq!(default_min_free_gb(m(8, 8)), 1.5);
        assert_eq!(default_min_free_gb(m(8, 16)), 1.6);
        assert_eq!(default_min_free_gb(m(8, 64)), 6.4);
        assert_eq!(default_vitest_workers(m(4, 8)), 1);
        assert_eq!(default_vitest_workers(m(8, 8)), 2);
        assert_eq!(default_vitest_workers(m(12, 8)), 3);
        assert_eq!(default_ceiling_gb(m(8, 16)), 8.0);
        assert_eq!(default_critical_free_gb(m(8, 16)), 0.8);
        assert_eq!(default_critical_free_gb(m(8, 64)), 1.0);
    }

    #[test]
    fn env_overrides_file() {
        let file: FileConfig =
            toml::from_str("budget = 5\npass_limit_min = 2\n[kinds.vitest]\ncost = 3\n").unwrap();
        let env = |k: &str| match k {
            "CALMRUN_BUDGET" => Some("3".to_string()),
            "CALMRUN_MIN_FREE_GB" => Some("0".to_string()),
            "CALMRUN_LOW_CPU" => Some("0".to_string()),
            _ => None,
        };
        let c = resolve(m(8, 16), file, &env);
        assert_eq!(c.budget, 3);
        assert_eq!(c.min_free_bytes, 0);
        assert_eq!(c.pass_limit_ms, 120_000);
        assert!(!c.low_cpu);
        assert_eq!(c.kinds["vitest"].cost, Some(3));
        assert!(!c.log, "off by default");
    }

    #[test]
    fn log_from_file_and_env() {
        let file: FileConfig = toml::from_str(
            "log = true
",
        )
        .unwrap();
        assert!(resolve(m(8, 16), file.clone(), &|_| None).log);
        let off = |k: &str| (k == "CALMRUN_LOG").then(|| "0".to_string());
        assert!(!resolve(m(8, 16), file, &off).log, "env 0 beats the file");
        let on = |k: &str| (k == "CALMRUN_LOG").then(|| "1".to_string());
        assert!(resolve(m(8, 16), FileConfig::default(), &on).log);
    }

    #[test]
    fn resources_from_file_and_env() {
        let file: FileConfig = toml::from_str("[resources]\ndb = 1\ngpu = 2\n").unwrap();
        let env = |k: &str| (k == "CALMRUN_RESOURCES").then(|| "gpu=4, cache = 3,bad".to_string());
        let c = resolve(m(8, 16), file, &env);
        assert_eq!(c.resources["db"], 1);
        assert_eq!(c.resources["gpu"], 4);
        assert_eq!(c.resources["cache"], 3);
        assert_eq!(c.resources.len(), 3);
    }

    #[test]
    fn resources_with_instances() {
        let file: FileConfig = toml::from_str(
            "[resources]
db = 1
android = [\"emulator-5554\", \"emulator-5556\"]
             [resources.ios]
discover = \"xcrun simctl list\"
env = \"UDID\"
",
        )
        .unwrap();
        let c = resolve(m(8, 16), file.clone(), &|_| None);
        assert_eq!(c.resources["db"], 1);
        assert_eq!(c.resources["android"], 2, "a fixed list counts its instances");
        assert!(!c.resources.contains_key("ios"), "discovered: no fixed count");
        assert_eq!(c.pools["android"].instances, ["emulator-5554", "emulator-5556"]);
        assert_eq!(c.pools["ios"].env.as_deref(), Some("UDID"));
        assert!(!c.pools.contains_key("db"));
        let env = |k: &str| (k == "CALMRUN_RESOURCES").then(|| "android=1".to_string());
        let c = resolve(m(8, 16), file, &env);
        assert!(!c.pools.contains_key("android"), "the env makes it count-only");
        assert_eq!(c.resources["android"], 1);
        let bad = toml::from_str::<FileConfig>(
            "[resources.android]
colour = \"red\"
",
        );
        let err = bad.unwrap_err().to_string();
        assert!(err.contains("colour") && err.contains("android"), "{err}");
    }

    #[test]
    fn run_limits_by_kind_global_and_default() {
        let limit = |file: &str, env: Option<&str>, kind: &str| {
            let env = |k: &str| {
                (k == "CALMRUN_MAX_RUN_MIN")
                    .then(|| env.map(String::from))
                    .flatten()
            };
            let c = resolve(m(8, 16), toml::from_str(file).unwrap(), &env);
            run_limit(&c, kind).map(|(d, from)| (d.as_secs(), from))
        };
        // Built-in: an hour for test-like kinds, nothing for builds and unknown kinds.
        assert_eq!(
            limit("", None, "vitest"),
            Some((3600, LimitFrom::Builtin("vitest".into())))
        );
        assert_eq!(limit("", None, "build"), None);
        assert_eq!(limit("", None, "other"), None);
        // The global value covers every kind; the env wins over the file; 0 turns it off.
        assert_eq!(
            limit("max_run_min = 5", None, "build"),
            Some((300, LimitFrom::Global))
        );
        assert_eq!(
            limit("max_run_min = 5", Some("2"), "test"),
            Some((120, LimitFrom::Global))
        );
        assert_eq!(limit("max_run_min = 0", None, "test"), None);
        // A kind's own value wins over both.
        let file = "max_run_min = 5\n[kinds.test]\nmax_run_min = 90\n[kinds.lint]\nmax_run_min = 0\n";
        assert_eq!(
            limit(file, Some("2"), "test"),
            Some((5400, LimitFrom::Kind("test".into())))
        );
        assert_eq!(limit(file, None, "lint"), None);
        for from in [
            LimitFrom::Asked,
            LimitFrom::Kind("test".into()),
            LimitFrom::Global,
            LimitFrom::Builtin("vitest".into()),
        ] {
            let text = from.how_to_change();
            assert!(text.contains("--timeout") && text.is_ascii(), "{text}");
        }
        assert!(LimitFrom::Builtin("vitest".into())
            .how_to_change()
            .contains("[kinds.vitest] max_run_min"));
    }

    #[test]
    fn default_config_parses() {
        let text = default_config_text(m(8, 16));
        let parsed: FileConfig = toml::from_str(&text).unwrap();
        assert!(parsed.budget.is_none());
    }
}
