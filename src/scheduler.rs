//! The scheduling rules as a pure function: state snapshot in, waiters allowed to start out.
//!
//! Every waiting calmrun process runs the same function on the same snapshot and starts only
//! if its own id is in the result, so the queue behaves as if one scheduler decided.

use crate::state::Job;
use std::collections::HashSet;

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub budget: u32,
    /// Free RAM needed to start a job while something already runs (0 = no check).
    pub min_free_bytes: u64,
    /// A waiter older than this may no longer be passed (0 = strict order).
    pub pass_limit_ms: u64,
}

/// Cost as the scheduler sees it: at least 1, at most the budget.
pub fn effective_cost(cost: u32, budget: u32) -> u32 {
    cost.clamp(1, budget.max(1))
}

/// Waiting jobs in queue order: prio high first, then `order` (first come). Parked entries
/// (owner gone, place kept for a ticket) are not waiting.
pub fn waiting_in_order(jobs: &[Job]) -> Vec<&Job> {
    let mut w: Vec<&Job> = jobs.iter().filter(|j| !j.running() && !j.parked()).collect();
    w.sort_by_key(|j| (!j.high, j.order, j.id));
    w
}

pub fn used_points(jobs: &[Job], budget: u32) -> u32 {
    jobs.iter()
        .filter(|j| j.running())
        .map(|j| effective_cost(j.cost, budget))
        .sum()
}

/// Why the first blocked waiter cannot start (for the waiting message).
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Points { used: u32, needs: u32 },
    Ram { free: u64, needs: u64 },
    Key,
    Order,
}

/// Ids of the waiters that may start now, in queue order.
#[cfg(test)]
pub fn admissible(jobs: &[Job], now_ms: u64, free_ram: u64, lim: Limits) -> Vec<u64> {
    plan(jobs, now_ms, free_ram, lim).0
}

/// Admitted ids plus, for every waiter that is not admitted, the reason.
pub fn plan(jobs: &[Job], now_ms: u64, free_ram: u64, lim: Limits) -> (Vec<u64>, Vec<(u64, Block)>) {
    let mut used = used_points(jobs, lim.budget);
    let mut keys: HashSet<&str> = jobs
        .iter()
        .filter(|j| j.running())
        .flat_map(|j| j.keys.iter().map(String::as_str))
        .collect();
    let mut active = jobs.iter().any(Job::running);
    let mut admitted = Vec::new();
    let mut blocked = Vec::new();
    // Once a skipped waiter has waited past the pass limit, nobody behind it may pass.
    let mut barrier = false;
    for w in waiting_in_order(jobs) {
        let cost = effective_cost(w.cost, lim.budget);
        let reason = if barrier {
            Some(Block::Order)
        } else if used + cost > lim.budget {
            Some(Block::Points { used, needs: cost })
        } else if w.keys.iter().any(|k| keys.contains(k.as_str())) {
            Some(Block::Key)
        } else if active && lim.min_free_bytes > 0 && free_ram < lim.min_free_bytes {
            Some(Block::Ram {
                free: free_ram,
                needs: lim.min_free_bytes,
            })
        } else {
            None
        };
        match reason {
            None => {
                used += cost;
                keys.extend(w.keys.iter().map(String::as_str));
                active = true;
                admitted.push(w.id);
            }
            Some(r) => {
                let waited = now_ms.saturating_sub(w.queued_ms);
                if lim.pass_limit_ms == 0 || waited > lim.pass_limit_ms {
                    barrier = true;
                }
                blocked.push((w.id, r));
            }
        }
    }
    (admitted, blocked)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1 << 30;
    const LIM: Limits = Limits {
        budget: 4,
        min_free_bytes: 3 * GB,
        pass_limit_ms: 600_000,
    };

    fn job(id: u64, cost: u32, running: bool) -> Job {
        Job {
            id,
            pid: id as u32,
            pid_start: 0,
            label: format!("j{id}"),
            kind: "test".into(),
            cost,
            keys: vec![],
            cwd: ".".into(),
            high: false,
            order: id as i64,
            queued_ms: 1_000,
            started_ms: running.then_some(1_000),
            child_pid: None,
            parked_until: None,
            mem_bytes: None,
        }
    }
    fn keyed(mut j: Job, k: &str) -> Job {
        j.keys = vec![k.into()];
        j
    }

    #[test]
    fn fills_budget_in_order() {
        let jobs = vec![job(1, 2, false), job(2, 2, false), job(3, 2, false)];
        assert_eq!(admissible(&jobs, 2_000, 16 * GB, LIM), vec![1, 2]);
    }

    #[test]
    fn respects_running_points() {
        let jobs = vec![job(1, 3, true), job(2, 2, false), job(3, 1, false)];
        // 2 does not fit; 3 fits and may pass because 2 is young.
        assert_eq!(admissible(&jobs, 2_000, 16 * GB, LIM), vec![3]);
    }

    #[test]
    fn strict_order_when_pass_limit_zero() {
        let jobs = vec![job(1, 3, true), job(2, 2, false), job(3, 1, false)];
        let lim = Limits {
            pass_limit_ms: 0,
            ..LIM
        };
        assert!(admissible(&jobs, 2_000, 16 * GB, lim).is_empty());
    }

    #[test]
    fn old_waiter_blocks_passing() {
        let jobs = vec![job(1, 3, true), job(2, 2, false), job(3, 1, false)];
        // job 2 queued at 1s; at 1s + 11 min it is past the 10 min pass limit.
        let now = 1_000 + 11 * 60_000;
        assert!(admissible(&jobs, now, 16 * GB, LIM).is_empty());
        // Exactly at the limit, passing is still allowed.
        assert_eq!(admissible(&jobs, 1_000 + 600_000, 16 * GB, LIM), vec![3]);
    }

    #[test]
    fn ram_gate_skipped_when_idle() {
        let jobs = vec![job(1, 1, false), job(2, 1, false)];
        // Only 1 GB free: the first may start because nothing runs, the second may not.
        assert_eq!(admissible(&jobs, 2_000, GB, LIM), vec![1]);
        let (_, blocked) = plan(&jobs, 2_000, GB, LIM);
        assert_eq!(
            blocked,
            vec![(
                2,
                Block::Ram {
                    free: GB,
                    needs: 3 * GB
                }
            )]
        );
    }

    #[test]
    fn ram_gate_when_busy_and_disabled() {
        let jobs = vec![job(1, 1, true), job(2, 1, false)];
        assert!(admissible(&jobs, 2_000, GB, LIM).is_empty());
        let lim = Limits {
            min_free_bytes: 0,
            ..LIM
        };
        assert_eq!(admissible(&jobs, 2_000, GB, lim), vec![2]);
    }

    #[test]
    fn prio_high_goes_first() {
        let mut hi = job(3, 2, false);
        hi.high = true;
        let jobs = vec![job(1, 1, true), job(2, 2, false), hi];
        // budget 4, used 1: only one cost-2 job fits; the high one wins.
        assert_eq!(admissible(&jobs, 2_000, 16 * GB, LIM), vec![3]);
    }

    #[test]
    fn exclusive_keys_never_overlap() {
        let jobs = vec![
            keyed(job(1, 1, true), "nm:a"),
            keyed(job(2, 1, false), "nm:a"),
            keyed(job(3, 1, false), "nm:b"),
        ];
        assert_eq!(admissible(&jobs, 2_000, 16 * GB, LIM), vec![3]);
        // Two waiters on the same key: only the first is admitted.
        let jobs = vec![keyed(job(1, 1, false), "nm:a"), keyed(job(2, 1, false), "nm:a")];
        assert_eq!(admissible(&jobs, 2_000, 16 * GB, LIM), vec![1]);
    }

    #[test]
    fn cost_clamped_to_budget() {
        let jobs = vec![job(1, 9, false)];
        assert_eq!(admissible(&jobs, 2_000, 16 * GB, LIM), vec![1]);
        assert_eq!(effective_cost(9, 4), 4);
        assert_eq!(effective_cost(0, 4), 1);
        let jobs = vec![job(1, 9, true), job(2, 1, false)];
        assert_eq!(used_points(&jobs, 4), 4);
        assert!(admissible(&jobs, 2_000, 16 * GB, LIM).is_empty());
    }

    #[test]
    fn bumped_order_goes_first() {
        let mut b = job(3, 2, false);
        b.order = -1;
        let jobs = vec![job(1, 2, true), job(2, 2, false), b];
        assert_eq!(admissible(&jobs, 2_000, 16 * GB, LIM), vec![3]);
    }

    #[test]
    fn parked_entries_are_skipped() {
        let mut parked = job(1, 2, false);
        parked.parked_until = Some(99_999_999);
        let jobs = vec![parked, job(2, 2, false)];
        assert_eq!(admissible(&jobs, 2_000, 16 * GB, LIM), vec![2]);
        assert_eq!(waiting_in_order(&jobs).len(), 1);
    }

    #[test]
    fn running_jobs_never_listed() {
        let jobs = vec![job(1, 2, true), job(2, 2, true)];
        assert!(admissible(&jobs, 2_000, 16 * GB, LIM).is_empty());
    }
}
