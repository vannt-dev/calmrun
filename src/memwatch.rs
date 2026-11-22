//! Memory guard for a running job: the whole process tree's memory against a ceiling, and the
//! system's free RAM against a critical floor.

use crate::config::bytes_to_gb;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use sysinfo::{MemoryRefreshKind, Pid, ProcessRefreshKind, ProcessesToUpdate, System};

/// How long free RAM must stay below the floor before the biggest job is stopped.
pub const CRITICAL_FOR: Duration = Duration::from_secs(3);
const SAMPLE_EVERY: Duration = Duration::from_secs(1);

/// Why a running job was stopped.
#[derive(Debug, Clone, PartialEq)]
pub enum Stop {
    /// The tree used more than the ceiling.
    Ceiling { used: u64, ceiling: u64 },
    /// Free RAM stayed below the floor and this job used the most memory.
    LowMemory { free: u64, floor: u64, used: u64 },
}

impl Stop {
    /// The history status word.
    pub fn status(&self) -> &'static str {
        match self {
            Stop::Ceiling { .. } => "ceiling",
            Stop::LowMemory { .. } => "low-memory",
        }
    }

    /// The message for the user, with the numbers and what to do next.
    pub fn message(&self, label: &str) -> String {
        match self {
            Stop::Ceiling { used, ceiling } => format!(
                "calmrun: stopped {label}: its processes used {:.1} GB, above the {:.1} GB ceiling\n\
                 calmrun: next: give it fewer workers, or raise the ceiling (CALMRUN_CEILING_GB / \
                 ceiling_gb; 0 turns it off)",
                bytes_to_gb(*used),
                bytes_to_gb(*ceiling)
            ),
            Stop::LowMemory { free, floor, used } => format!(
                "calmrun: stopped {label}: free RAM fell to {:.1} GB (critical floor {:.1} GB) and it \
                 used the most memory ({:.1} GB)\ncalmrun: next: run it again when the machine is \
                 quieter (`calmrun status`), or with fewer workers",
                bytes_to_gb(*free),
                bytes_to_gb(*floor),
                bytes_to_gb(*used)
            ),
        }
    }
}

/// The decision, kept pure for tests. `low_for` is how long free RAM has been below the floor;
/// `biggest` says whether this job's tree is the largest of the running jobs.
pub fn decide(
    used: u64,
    free: u64,
    ceiling: u64,
    floor: u64,
    low_for: Option<Duration>,
    biggest: bool,
) -> Option<Stop> {
    if ceiling > 0 && used > ceiling {
        return Some(Stop::Ceiling { used, ceiling });
    }
    if floor > 0 && free < floor && biggest && low_for.is_some_and(|d| d >= CRITICAL_FOR) {
        return Some(Stop::LowMemory { free, floor, used });
    }
    None
}

/// Sum of memory over `root` and all its descendants, from (pid, parent, bytes) rows.
pub fn tree_bytes(rows: &[(u32, Option<u32>, u64)], root: u32) -> u64 {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut mem: HashMap<u32, u64> = HashMap::new();
    for &(pid, parent, bytes) in rows {
        mem.insert(pid, bytes);
        if let Some(pp) = parent {
            if pp != pid {
                children.entry(pp).or_default().push(pid);
            }
        }
    }
    let mut seen = HashSet::new();
    let mut stack = vec![root];
    let mut total = 0;
    while let Some(p) = stack.pop() {
        if !seen.insert(p) {
            continue;
        }
        total += mem.get(&p).copied().unwrap_or(0);
        if let Some(c) = children.get(&p) {
            stack.extend(c.iter().copied());
        }
    }
    total
}

/// Samples one job's process tree and the free RAM.
pub struct MemWatch {
    sys: System,
    pub ceiling: u64,
    pub floor: u64,
    last: Option<Instant>,
    low_since: Option<Instant>,
    /// Latest tree size.
    pub used: u64,
}

impl MemWatch {
    /// A watcher, or `None` when both limits are off.
    pub fn new(ceiling: u64, floor: u64) -> Option<MemWatch> {
        if ceiling == 0 && floor == 0 {
            return None;
        }
        Some(MemWatch {
            sys: System::new().ok()?,
            ceiling,
            floor,
            last: None,
            low_since: None,
            used: 0,
        })
    }

    /// Sample at most once a second. `biggest` is asked only when free RAM is critical.
    /// Returns `Some(true)` when a fresh sample was taken (so callers can publish `used`).
    pub fn check(&mut self, root: u32, biggest: &mut dyn FnMut(u64) -> bool) -> (bool, Option<Stop>) {
        if self.last.is_some_and(|t| t.elapsed() < SAMPLE_EVERY) {
            return (false, None);
        }
        self.last = Some(Instant::now());
        self.sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_memory(),
        );
        let rows: Vec<(u32, Option<u32>, u64)> = self
            .sys
            .processes()
            .iter()
            .map(|(pid, p)| (pid.as_u32(), p.parent().map(Pid::as_u32), p.memory()))
            .collect();
        self.used = tree_bytes(&rows, root);
        self.sys
            .refresh_memory_specifics(MemoryRefreshKind::nothing().with_ram());
        let free = self.sys.available_memory();
        let low = self.floor > 0 && free < self.floor;
        if low {
            self.low_since.get_or_insert_with(Instant::now);
        } else {
            self.low_since = None;
        }
        let low_for = self.low_since.map(|t| t.elapsed());
        let is_biggest = low && low_for.is_some_and(|d| d >= CRITICAL_FOR) && biggest(self.used);
        (
            true,
            decide(self.used, free, self.ceiling, self.floor, low_for, is_biggest),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1 << 30;

    #[test]
    fn ceiling_wins_first() {
        assert_eq!(
            decide(9 * GB, 10 * GB, 8 * GB, GB, None, false),
            Some(Stop::Ceiling {
                used: 9 * GB,
                ceiling: 8 * GB
            })
        );
        assert_eq!(decide(7 * GB, 10 * GB, 8 * GB, GB, None, false), None);
        assert_eq!(decide(90 * GB, 10 * GB, 0, 0, None, false), None);
    }

    #[test]
    fn low_memory_needs_time_and_biggest() {
        let short = Some(Duration::from_secs(1));
        let long = Some(Duration::from_secs(4));
        assert_eq!(decide(GB, GB / 2, 0, GB, short, true), None);
        assert_eq!(decide(GB, GB / 2, 0, GB, long, false), None);
        assert_eq!(
            decide(GB, GB / 2, 0, GB, long, true),
            Some(Stop::LowMemory {
                free: GB / 2,
                floor: GB,
                used: GB
            })
        );
        assert_eq!(decide(GB, 2 * GB, 0, GB, long, true), None);
    }

    #[test]
    fn tree_sums_descendants_only() {
        let rows = vec![
            (1, None, 100),
            (10, Some(1), 5),
            (11, Some(10), 7),
            (12, Some(11), 3),
            (20, Some(1), 50),
        ];
        assert_eq!(tree_bytes(&rows, 10), 15);
        assert_eq!(tree_bytes(&rows, 12), 3);
        assert_eq!(tree_bytes(&rows, 99), 0);
    }

    #[test]
    fn messages_name_the_numbers() {
        let m = Stop::Ceiling {
            used: 9 * GB + GB / 10,
            ceiling: 8 * GB,
        }
        .message("npx vitest");
        assert!(
            m.starts_with("calmrun: stopped npx vitest: its processes used 9.1 GB, above the 8.0 GB ceiling")
        );
    }
}
