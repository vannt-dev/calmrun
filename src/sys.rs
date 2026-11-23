//! The machine: RAM, cores, process liveness, disks, power, paths. Everything here that
//! differs per OS stays inside this module (and `runner`).

use crate::config::Machine;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use sysinfo::{Disk, Disks, MemoryRefreshKind, Pid, ProcessRefreshKind, ProcessesToUpdate, System};

/// Start times of the same process read twice may differ by rounding.
const START_TIME_SLACK_S: u64 = 1;
/// How often a waiting job re-reads free disk space.
const DISK_REFRESH: Duration = Duration::from_secs(3);
/// Fallback when the core count cannot be read.
const FALLBACK_CORES: usize = 2;

/// System information that is refreshed on demand.
pub struct Sys {
    sys: System,
}

impl Sys {
    /// Fails only when the OS gives no process or memory information.
    pub fn new() -> crate::error::Result<Sys> {
        let sys = System::new().map_err(|e| crate::error::Error::SystemInfo(e.to_string()))?;
        Ok(Sys { sys })
    }

    /// Cores and total RAM.
    pub fn machine(&mut self) -> Machine {
        self.sys
            .refresh_memory_specifics(MemoryRefreshKind::nothing().with_ram());
        let logical = std::thread::available_parallelism().map_or(FALLBACK_CORES, |n| n.get());
        Machine {
            logical_cores: logical,
            total_ram: self.sys.total_memory(),
        }
    }

    /// RAM available to new processes, in bytes.
    pub fn free_ram(&mut self) -> u64 {
        self.sys
            .refresh_memory_specifics(MemoryRefreshKind::nothing().with_ram());
        self.sys.available_memory()
    }

    /// Start time of a process (seconds since the epoch), or `None` if it is not running.
    pub fn start_time(&mut self, pid: u32) -> Option<u64> {
        let p = Pid::from_u32(pid);
        self.sys.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[p]),
            true,
            ProcessRefreshKind::nothing(),
        );
        self.sys.process(p).map(|pr| pr.start_time())
    }

    /// Refresh many pids at once and report which (pid, start) pairs are alive.
    pub fn alive_set(&mut self, pairs: &[(u32, u64)]) -> Vec<bool> {
        let pids: Vec<Pid> = pairs.iter().map(|(p, _)| Pid::from_u32(*p)).collect();
        if !pids.is_empty() {
            self.sys.refresh_processes_specifics(
                ProcessesToUpdate::Some(&pids),
                true,
                ProcessRefreshKind::nothing(),
            );
        }
        pairs
            .iter()
            .map(|(pid, start)| {
                self.sys
                    .process(Pid::from_u32(*pid))
                    .is_some_and(|p| p.start_time().abs_diff(*start) <= START_TIME_SLACK_S)
            })
            .collect()
    }
}

/// Physical cores, when the OS says.
pub fn physical_cores() -> Option<usize> {
    System::physical_core_count().ok()
}

/// The per-user base directory for calmrun's state.
pub fn state_base_dir() -> Option<PathBuf> {
    let base = if cfg!(target_os = "linux") {
        dirs::state_dir()
    } else if cfg!(target_os = "macos") {
        dirs::data_dir()
    } else {
        dirs::data_local_dir()
    };
    base.or_else(|| dirs::home_dir().map(|h| h.join(".local").join("state")))
}

/// Whether this process runs inside a Windows AppContainer: a sandbox (Antigravity's terminal
/// sandbox is one) that hides the other processes and redirects the per-user folders, so a
/// calmrun there would take every other job's owner for dead and drop it from the queue.
#[cfg(windows)]
pub fn in_app_container() -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{GetTokenInformation, TokenIsAppContainer, TOKEN_QUERY};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: plain Win32 calls on this process's own token, closed before returning.
    unsafe {
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut flag: u32 = 0;
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenIsAppContainer,
            (&mut flag as *mut u32).cast(),
            std::mem::size_of::<u32>() as u32,
            &mut len,
        );
        CloseHandle(token);
        ok != 0 && flag != 0
    }
}

/// Whether this process runs inside a Windows AppContainer (never elsewhere).
#[cfg(not(windows))]
pub fn in_app_container() -> bool {
    false
}

/// A path as a comparable string: canonical, without the Windows `\\?\` prefix, and
/// lowercased on Windows (whose file names are case-insensitive).
pub fn path_key(p: &Path) -> String {
    let real = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let s = real.to_string_lossy();
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s.to_string()
    }
}

fn mount_key(d: &Disk) -> String {
    let m = d.mount_point().to_string_lossy();
    if cfg!(windows) {
        m.to_lowercase()
    } else {
        m.to_string()
    }
}

/// The disk holding `key` (a [`path_key`]): the longest mount point it starts with.
fn disk_for<'a>(disks: &'a Disks, key: &str) -> Option<&'a Disk> {
    disks
        .iter()
        .filter(|d| key.starts_with(&mount_key(d)))
        .max_by_key(|d| d.mount_point().as_os_str().len())
}

/// "SSD", "HDD" or "unknown" for the disk holding `path`.
pub fn disk_kind(path: &Path) -> &'static str {
    let Ok(disks) = Disks::new_with_refreshed_list() else {
        return "unknown";
    };
    match disk_for(&disks, &path_key(path)).map(Disk::kind) {
        Some(sysinfo::DiskKind::SSD) => "SSD",
        Some(sysinfo::DiskKind::HDD) => "HDD",
        _ => "unknown",
    }
}

/// Free space on the drive of a directory, re-read at most every few seconds.
pub struct DiskCheck {
    disks: Option<Disks>,
    key: String,
    /// Bytes required; 0 turns the check off.
    pub need: u64,
    /// Last known free bytes.
    pub free: u64,
    /// Mount point of the drive.
    pub mount: String,
    last: Option<Instant>,
}

impl DiskCheck {
    /// A check for the drive of `dir` (nothing is read until the first call).
    pub fn new(dir: &Path, need: u64) -> DiskCheck {
        DiskCheck {
            disks: None,
            key: path_key(dir),
            need,
            free: u64::MAX,
            mount: String::new(),
            last: None,
        }
    }

    /// True while the drive has less than the required free space. If disk information is
    /// unavailable the check passes.
    pub fn short(&mut self) -> bool {
        if self.need == 0 {
            return false;
        }
        if self.last.is_none_or(|t| t.elapsed() >= DISK_REFRESH) {
            self.last = Some(Instant::now());
            self.refresh();
        }
        self.free < self.need
    }

    fn refresh(&mut self) {
        if self.disks.is_none() {
            self.disks = Disks::new_with_refreshed_list().ok();
        } else if let Some(d) = self.disks.as_mut() {
            d.refresh(true);
        }
        if let Some(disk) = self.disks.as_ref().and_then(|d| disk_for(d, &self.key)) {
            self.free = disk.available_space();
            self.mount = disk.mount_point().to_string_lossy().to_string();
        }
    }
}

/// `Some(true)` on battery, `Some(false)` on mains, `None` if unknown or no battery.
pub fn on_battery() -> Option<bool> {
    power::on_battery()
}

#[cfg(windows)]
mod power {
    /// See [`super::on_battery`].
    pub fn on_battery() -> Option<bool> {
        use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
        /// `BatteryFlag` value meaning "no system battery".
        const NO_BATTERY: u8 = 128;
        // SAFETY: GetSystemPowerStatus fills a plain struct we own.
        let s = unsafe {
            let mut s: SYSTEM_POWER_STATUS = std::mem::zeroed();
            if GetSystemPowerStatus(&mut s) == 0 {
                return None;
            }
            s
        };
        if s.BatteryFlag == NO_BATTERY {
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
mod power {
    /// See [`super::on_battery`].
    pub fn on_battery() -> Option<bool> {
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
}

#[cfg(all(unix, not(target_os = "macos")))]
mod power {
    /// See [`super::on_battery`].
    pub fn on_battery() -> Option<bool> {
        let dir = std::fs::read_dir("/sys/class/power_supply").ok()?;
        let read = |p: &std::path::Path, f: &str| std::fs::read_to_string(p.join(f)).unwrap_or_default();
        let mut has_battery = false;
        let mut mains_online = false;
        for e in dir.flatten() {
            let p = e.path();
            match read(&p, "type").trim() {
                "Battery" => has_battery = true,
                "Mains" | "USB" if read(&p, "online").trim() == "1" => mains_online = true,
                _ => {}
            }
        }
        has_battery.then_some(!mains_online)
    }
}
