//! `calmrun doctor`: what this machine has and what calmrun suggests.

use crate::config::{self, bytes_to_gb, Machine};
use std::path::Path;

/// The `calmrun doctor` text.
pub fn report(
    m: Machine,
    cfg: &config::Config,
    home: &Path,
    config_file: &Path,
    physical: Option<usize>,
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
    s += &format!("RAM:          {:.1} GB total\n", bytes_to_gb(m.total_ram));
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
    s += "\nSuggested for this machine:\n";
    s += &format!("  budget = {}\n", config::default_budget(m));
    s += &format!("  min_free_gb = {:.1}\n", config::default_min_free_gb(m));
    s += &format!("  vitest_workers = {}\n", config::default_vitest_workers(m));
    s += "\nSet it in the config file, or with env vars:\n";
    s += "  Windows (PowerShell, persistent):  setx CALMRUN_BUDGET 4\n";
    s += "  macOS (zsh):  echo 'export CALMRUN_BUDGET=4' >> ~/.zshrc\n";
    s += "  Linux (bash): echo 'export CALMRUN_BUDGET=4' >> ~/.bashrc\n";
    s
}
