//! `calmrun doctor`: what this machine has and what calmrun suggests.

use crate::config::{self, bytes_to_gb, Machine};
use std::path::Path;

fn disk_kind(path: &Path) -> String {
    let disks = match sysinfo::Disks::new_with_refreshed_list() {
        Ok(d) => d,
        Err(_) => return "unknown".into(),
    };
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let target = target.to_string_lossy().to_lowercase().replace("\\\\?\\", "");
    disks
        .iter()
        .filter(|d| {
            let m = d.mount_point().to_string_lossy().to_lowercase();
            target.starts_with(&m)
        })
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .map(|d| match d.kind() {
            sysinfo::DiskKind::SSD => "SSD".to_string(),
            sysinfo::DiskKind::HDD => "HDD".to_string(),
            _ => "unknown".to_string(),
        })
        .unwrap_or_else(|| "unknown".into())
}

/// `Some(true)` on battery, `Some(false)` on mains, `None` if unknown / no battery.
fn on_battery() -> Option<bool> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
        // SAFETY: GetSystemPowerStatus fills a plain struct we own.
        unsafe {
            let mut s: SYSTEM_POWER_STATUS = std::mem::zeroed();
            if GetSystemPowerStatus(&mut s) == 0 || s.BatteryFlag == 128 {
                return None;
            }
            match s.ACLineStatus {
                0 => Some(true),
                1 => Some(false),
                _ => None,
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("pmset")
            .args(["-g", "batt"])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        if text.contains("Battery Power") {
            Some(true)
        } else if text.contains("AC Power") {
            Some(false)
        } else {
            None
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let dir = std::fs::read_dir("/sys/class/power_supply").ok()?;
        let mut has_battery = false;
        let mut mains_online = false;
        for e in dir.flatten() {
            let p = e.path();
            let kind = std::fs::read_to_string(p.join("type")).unwrap_or_default();
            match kind.trim() {
                "Battery" => has_battery = true,
                "Mains" | "USB"
                    if std::fs::read_to_string(p.join("online"))
                        .unwrap_or_default()
                        .trim()
                        == "1" =>
                {
                    mains_online = true;
                }
                _ => {}
            }
        }
        has_battery.then_some(!mains_online)
    }
}

pub fn report(
    m: Machine,
    cfg: &config::Config,
    home: &Path,
    config_file: &Path,
    physical: Option<usize>,
) -> String {
    let mut s = String::new();
    let cwd = std::env::current_dir().unwrap_or_else(|_| home.to_path_buf());
    let battery = match on_battery() {
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
    s += &format!("disk (cwd):   {}\n", disk_kind(&cwd));
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
