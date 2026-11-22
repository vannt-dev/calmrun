//! Thin wrappers over `sysinfo`: RAM, cores, process liveness.

use crate::config::Machine;
use sysinfo::{MemoryRefreshKind, Pid, ProcessRefreshKind, ProcessesToUpdate, System};

pub struct Sys {
    sys: System,
}

impl Sys {
    pub fn new() -> Sys {
        let sys = System::new().unwrap_or_else(|_| System::new().expect("system info unavailable"));
        Sys { sys }
    }

    pub fn machine(&mut self) -> Machine {
        self.sys
            .refresh_memory_specifics(MemoryRefreshKind::nothing().with_ram());
        let logical = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2);
        Machine {
            logical_cores: logical,
            total_ram: self.sys.total_memory(),
        }
    }

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
                    // Allow 1 s of rounding between reads of the same start time.
                    .is_some_and(|p| p.start_time().abs_diff(*start) <= 1)
            })
            .collect()
    }
}

pub fn physical_cores() -> Option<usize> {
    System::physical_core_count().ok()
}
