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
    pub critical_free_gb: Option<f64>,
    #[serde(default)]
    pub kinds: BTreeMap<String, KindConfig>,
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
    pub kinds: BTreeMap<String, KindConfig>,
}

pub fn default_budget(m: Machine) -> u32 {
    let by_cores = m.logical_cores.saturating_sub(2) as u64;
    let by_ram = (m.total_ram as f64 / GB / 4.0).floor() as u64;
    by_cores.min(by_ram).max(2) as u32
}

pub fn default_min_free_gb(m: Machine) -> f64 {
    (m.total_ram as f64 / GB / 8.0).max(3.0)
}

/// Half of the RAM.
pub fn default_ceiling_gb(m: Machine) -> f64 {
    m.total_ram as f64 / GB / 2.0
}

/// 5% of the RAM, at most 1 GB.
pub fn default_critical_free_gb(m: Machine) -> f64 {
    (m.total_ram as f64 / GB * 0.05).min(1.0)
}

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
    let base = if cfg!(target_os = "linux") {
        dirs::state_dir()
    } else if cfg!(target_os = "macos") {
        dirs::data_dir()
    } else {
        dirs::data_local_dir()
    };
    base.or_else(|| dirs::home_dir().map(|h| h.join(".local").join("state")))
        .unwrap_or_else(std::env::temp_dir)
        .join("calmrun")
}

pub fn config_path(home: &Path) -> PathBuf {
    match std::env::var_os("CALMRUN_CONFIG").filter(|v| !v.is_empty()) {
        Some(p) => PathBuf::from(p),
        None => home.join("config.toml"),
    }
}

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
        .unwrap_or(10.0)
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
    let low_cpu = !matches!(
        env("CALMRUN_LOW_CPU").as_deref().map(str::trim),
        Some("0" | "false" | "no" | "off")
    );
    Config {
        budget,
        min_free_bytes: (min_free_gb * GB) as u64,
        pass_limit_ms: (pass_min * 60_000.0) as u64,
        vitest_workers,
        low_cpu,
        ceiling_bytes: (ceiling_gb * GB) as u64,
        critical_free_bytes: (critical_gb * GB) as u64,
        kinds: file.kinds,
    }
}

pub fn bytes_to_gb(b: u64) -> f64 {
    b as f64 / GB
}

/// The commented default config written by `calmrun init`.
pub fn default_config_text(m: Machine) -> String {
    format!(
        r#"# calmrun configuration. Every value is optional; env vars override these.
# Defaults shown are computed for this machine.

# Total points that may run at once (env CALMRUN_BUDGET).
# Default: logical cores - 2, capped at floor(RAM GB / 4), at least 2.
# budget = {budget}

# A job starts only when at least this much RAM is free (env CALMRUN_MIN_FREE_GB).
# Skipped when nothing is running. 0 disables the check. Default: max(3, RAM / 8).
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

# --maxWorkers value appended to direct vitest calls (env CALMRUN_VITEST_WORKERS).
# vitest_workers = {workers}

# Override the cost of a built-in kind, or add your own kind with command patterns.
# A pattern is the command's first words, e.g. "make" or "bazel build".
# [kinds.vitest]
# cost = 3
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
        assert_eq!(default_budget(m(16, 32)), 8); // ram cap
        assert_eq!(default_budget(m(8, 64)), 6); // cores - 2
        assert_eq!(default_budget(m(2, 4)), 2); // min 2
        assert_eq!(default_budget(m(20, 16)), 4);
    }

    #[test]
    fn min_free_and_workers() {
        assert_eq!(default_min_free_gb(m(8, 16)), 3.0);
        assert_eq!(default_min_free_gb(m(8, 64)), 8.0);
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
    }

    #[test]
    fn default_config_parses() {
        let text = default_config_text(m(8, 16));
        let parsed: FileConfig = toml::from_str(&text).unwrap();
        assert!(parsed.budget.is_none());
    }
}
