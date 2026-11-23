//! `calmrun doctor`: what this machine has and what calmrun suggests.

use crate::config::{self, bytes_to_gb, Machine};
use std::path::Path;

/// The `calmrun doctor` text. `free_now` is the free RAM at this moment.
pub fn report(
    m: Machine,
    cfg: &config::Config,
    home: &Path,
    config_file: &Path,
    physical: Option<usize>,
    free_now: u64,
) -> String {
    let mut s = String::new();
    let cwd = std::env::current_dir().unwrap_or_else(|_| home.to_path_buf());
    let battery = match crate::sys::on_battery() {
        Some(true) => "yes (on battery: consider a smaller budget)",
        Some(false) => "no (on mains power)",
        None => "unknown / no battery",
    };
    s += &format!(
        "cores:        {} logical{}\n",
        m.logical_cores,
        physical.map(|p| format!(", {p} physical")).unwrap_or_default()
    );
    s += &format!(
        "RAM:          {:.1} GB total, {:.1} GB free now\n",
        bytes_to_gb(m.total_ram),
        bytes_to_gb(free_now)
    );
    s += &format!("disk (cwd):   {}\n", crate::sys::disk_kind(&cwd));
    s += &format!("battery:      {battery}\n");
    s += &format!("state dir:    {}\n", home.display());
    s += &format!(
        "config file:  {}{}\n",
        config_file.display(),
        if config_file.exists() {
            ""
        } else {
            " (not created; run `calmrun init`)"
        }
    );
    s += "\nIn effect now:\n";
    s += &format!("  budget          = {}\n", cfg.budget);
    s += &format!("  min_free_gb     = {:.1}\n", bytes_to_gb(cfg.min_free_bytes));
    s += &format!("  pass_limit_min  = {}\n", cfg.pass_limit_ms / 60_000);
    s += &format!("  vitest_workers  = {}\n", cfg.vitest_workers);
    s += &format!("  ceiling_gb      = {:.1}\n", bytes_to_gb(cfg.ceiling_bytes));
    s += &format!(
        "  critical_free_gb = {:.2}\n",
        bytes_to_gb(cfg.critical_free_bytes)
    );
    s += &format!("  low cpu         = {}\n", if cfg.low_cpu { "on" } else { "off" });
    s += &format!(
        "\nWorkers calmrun passes to each tool (unless you set them; at the tool's default cost, budget {}):\n",
        cfg.budget
    );
    let workers = |w: crate::kinds::Workers, cost: u32| match w {
        crate::kinds::Workers::Vitest => cfg.vitest_workers,
        crate::kinds::Workers::CoreShare => crate::queue::tool_workers(
            m.logical_cores,
            crate::scheduler::effective_cost(cost, cfg.budget),
            cfg.budget,
        ),
    };
    for line in crate::kinds::hint_summary(&workers, &cfg.kinds) {
        s += &format!("  {line}\n");
    }
    if let Some(w) = low_free_warning(cfg.min_free_bytes, free_now) {
        s += &format!("\n{w}\n");
    }
    let budget = config::default_budget(m);
    s += "\nSuggested for this machine:\n";
    s += &format!("  budget = {budget}\n");
    s += &format!("  min_free_gb = {:.1}\n", config::default_min_free_gb(m));
    s += &format!("  vitest_workers = {}\n", config::default_vitest_workers(m));
    s += "\nSet it in the config file, or with env vars:\n";
    s += &format!("  Windows (PowerShell, persistent):  setx CALMRUN_BUDGET {budget}\n");
    s += &format!("  macOS (zsh):  echo 'export CALMRUN_BUDGET={budget}' >> ~/.zshrc\n");
    s += &format!("  Linux (bash): echo 'export CALMRUN_BUDGET={budget}' >> ~/.bashrc\n");
    s
}

/// A warning when less RAM is free now than a job needs to start beside another one.
fn low_free_warning(min_free: u64, free_now: u64) -> Option<String> {
    (min_free > 0 && free_now < min_free).then(|| {
        format!(
            "warning: only {:.1} GB RAM is free now, below min_free_gb ({:.1}). While anything \
             runs, every other job waits for RAM. If the machine is at rest, close programs or \
             lower min_free_gb (e.g. {:.1}).",
            bytes_to_gb(free_now),
            bytes_to_gb(min_free),
            (bytes_to_gb(free_now) * 0.6).max(1.0)
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1 << 30;

    fn report_for(min_free_gb: &str, free_now: u64) -> String {
        let m = Machine {
            logical_cores: 20,
            total_ram: 16 * GB,
        };
        let env = |k: &str| (k == "CALMRUN_MIN_FREE_GB").then(|| min_free_gb.to_string());
        let cfg = config::resolve(m, config::FileConfig::default(), &env);
        report(
            m,
            &cfg,
            Path::new("/h"),
            Path::new("/h/config.toml"),
            None,
            free_now,
        )
    }

    #[test]
    fn suggested_budget_and_env_lines_agree() {
        let r = report_for("1.5", 8 * GB);
        assert!(r.contains("  budget = 6\n"), "{r}");
        assert!(r.contains("setx CALMRUN_BUDGET 6\n"), "{r}");
        assert!(r.contains("export CALMRUN_BUDGET=6'"), "{r}");
        assert!(!r.contains("warning:"), "{r}");
        assert!(r.is_ascii(), "{r}");
    }

    #[test]
    fn warns_when_free_ram_is_below_the_minimum() {
        let r = report_for("3", 2 * GB + GB / 2);
        assert!(
            r.contains("warning: only 2.5 GB RAM is free now, below min_free_gb (3.0)"),
            "{r}"
        );
        assert!(r.contains("(e.g. 1.5)"), "{r}");
        assert!(!report_for("0", GB).contains("warning:"));
    }
}
