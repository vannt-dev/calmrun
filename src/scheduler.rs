//! The scheduling rules as a pure function: state snapshot in, waiters allowed to start out.
//!
//! Every waiting calmrun process runs the same function on the same snapshot and starts only
//! if its own id is in the result, so the queue behaves as if one scheduler decided. The start
//! conditions themselves are [`crate::gates`]; this module owns order and passing.

use crate::state::{Job, Lease};
use std::collections::{BTreeMap, HashMap};

/// Queue-wide limits from the config.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub budget: u32,
    /// Free RAM needed to start a job while something already runs (0 = no check).
    pub min_free_bytes: u64,
    /// A waiter older than this may no longer be passed (0 = strict order).
    pub pass_limit_ms: u64,
    /// RAM assumed taken per point by a job that has just started, before it shows up in
    /// the free-RAM reading (0 = no reservation).
    pub ram_per_point: u64,
    /// How long that assumption fades out over.
    pub reserve_window_ms: u64,
}

/// RAM assumed taken by jobs started in the last `reserve_window_ms`, fading linearly.
pub fn recent_reservation(jobs: &[Job], now_ms: u64, limits: &Limits) -> u64 {
    if limits.reserve_window_ms == 0 {
        return 0;
    }
    jobs.iter()
        .filter_map(|j| j.started_ms.map(|s| (j, now_ms.saturating_sub(s))))
        .filter(|(_, age)| *age < limits.reserve_window_ms)
        .map(|(j, age)| {
            let full = limits.ram_per_point * job_points(j, limits.budget) as u64;
            (full as u128 * (limits.reserve_window_ms - age) as u128 / limits.reserve_window_ms as u128)
                as u64
        })
        .sum()
}

/// Everything besides the jobs that a scheduling pass looks at.
#[derive(Debug, Clone, Copy)]
pub struct Inputs<'a> {
    pub now_ms: u64,
    pub free_ram: u64,
    pub limits: Limits,
    /// Slots per named resource.
    pub caps: &'a BTreeMap<String, u32>,
    /// Resource leases (`calmrun hold`): slots and points held without a running job.
    pub leases: &'a [Lease],
    /// Jobs running in the other queues (`--id`): they hold their exclusive keys and make the
    /// machine busy for the RAM and disk checks, but take none of this queue's points.
    pub foreign: &'a [Job],
}

/// Cost as the scheduler sees it: at least 1, at most the budget.
pub fn effective_cost(cost: u32, budget: u32) -> u32 {
    cost.clamp(1, budget.max(1))
}

/// Points a job takes: [`effective_cost`], except that a `calmrun hold` waiter may take 0
/// (it runs nothing unless it asked for points with `--cost`).
pub fn job_points(j: &Job, budget: u32) -> u32 {
    if j.is_hold() {
        j.cost.min(budget.max(1))
    } else {
        effective_cost(j.cost, budget)
    }
}

/// Points held by leases.
pub fn leased_points(leases: &[Lease], budget: u32) -> u32 {
    leases.iter().map(|l| l.cost.min(budget.max(1))).sum()
}

/// Whether the path key `key` is the folder `dir` or inside it.
pub fn inside(key: &str, dir: &str) -> bool {
    if key.is_empty() || dir.is_empty() {
        return false;
    }
    match key.strip_prefix(dir) {
        Some("") => true,
        Some(rest) => dir.ends_with(['/', '\\']) || rest.starts_with(['/', '\\']),
        None => false,
    }
}

/// Whether job `w` may use a lease's slot of resource `n` instead of waiting for one: it runs
/// in the lease's folder (`hold --for`) or names the lease in `CALMRUN_LEASE`. A hold never
/// does: it always takes slots of its own.
pub fn uses_lease(w: &Job, n: &str, leases: &[Lease]) -> bool {
    !w.is_hold()
        && leases.iter().any(|l| {
            l.resources.iter().any(|r| r == n)
                && (w.use_leases.contains(&l.id)
                    || l.for_key.as_deref().is_some_and(|d| inside(&w.cwd_key, d)))
        })
}

/// Slots of each named resource held by leases.
pub fn leased_slots(leases: &[Lease]) -> HashMap<&str, u32> {
    let mut held = HashMap::new();
    for n in leases.iter().flat_map(|l| &l.resources) {
        *held.entry(n.as_str()).or_default() += 1;
    }
    held
}

/// Priority tier: high (0), normal (1), low (2). `high` wins if both flags are set.
pub fn tier(j: &Job) -> u8 {
    match (j.high, j.low) {
        (true, _) => 0,
        (false, false) => 1,
        (false, true) => 2,
    }
}

/// Waiting jobs in queue order: prio high first, then normal, then low; within a tier by
/// `order` (first come, or moved by `bump`). Parked entries (owner gone, place kept for a
/// ticket) are not waiting.
pub fn waiting_in_order(jobs: &[Job]) -> Vec<&Job> {
    let mut w: Vec<&Job> = jobs.iter().filter(|j| !j.running() && !j.parked()).collect();
    w.sort_by_key(|j| (tier(j), j.order, j.id));
    w
}

/// Points taken by running jobs.
pub fn used_points(jobs: &[Job], budget: u32) -> u32 {
    jobs.iter()
        .filter(|j| j.running())
        .map(|j| job_points(j, budget))
        .sum()
}

/// Slots of a named resource (a resource that is not configured has 1 slot).
pub fn capacity(caps: &BTreeMap<String, u32>, name: &str) -> u32 {
    caps.get(name).copied().unwrap_or(1).max(1)
}

/// Why a waiter cannot start (for the waiting message).
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Points {
        used: u32,
        needs: u32,
    },
    /// Free RAM is below the minimum plus what jobs that just started are assumed to take.
    Ram {
        /// Free RAM as read.
        free: u64,
        /// Held back for jobs that started in the last seconds (or in this pass).
        reserved: u64,
        /// The minimum free RAM to start.
        needs: u64,
    },
    Disk,
    /// An exclusive key (a `node_modules`, a cargo `target`) is held by this running job.
    Key {
        holder: String,
    },
    /// All slots of this named resource are taken.
    Resource(String),
    /// Kept in line behind a waiter that may no longer be passed.
    Order,
    /// Kept in line behind this waiter moved to the front by `calmrun bump`, which waits for
    /// points or RAM: passing it would delay it.
    Bumped {
        id: u64,
    },
}

/// The outcome of one scheduling pass.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plan {
    /// Waiters that may start now, in queue order.
    pub admitted: Vec<u64>,
    /// Every other waiter with its reason.
    pub blocked: Vec<(u64, Block)>,
}

impl Plan {
    /// Whether this waiter may start.
    pub fn admits(&self, id: u64) -> bool {
        self.admitted.contains(&id)
    }

    /// Why this waiter may not start (`Order` if it was not looked at).
    pub fn reason(&self, id: u64) -> Block {
        self.blocked
            .iter()
            .find(|(b, _)| *b == id)
            .map_or(Block::Order, |(_, r)| r.clone())
    }
}

/// Walk the waiters in order; admit each that passes every gate, taking its share of each.
/// A later waiter may pass a blocked one only while no blocked waiter ahead of it has waited
/// longer than the pass limit. A waiter that needs a resource whose every slot is leased
/// never stops others from passing: a lease may last hours. A bumped waiter is never passed
/// while it waits only for points or RAM (what a passing job would take); one that waits for
/// an exclusive key, a resource slot or disk space may be passed, since that can last long and
/// passing does not delay it.
pub fn schedule(jobs: &[Job], inputs: &Inputs) -> Plan {
    schedule_with(jobs, inputs, crate::gates::standard(jobs, inputs))
}

/// [`schedule`] with an explicit list of gates.
pub fn schedule_with<'a>(
    jobs: &'a [Job],
    inputs: &Inputs,
    mut gates: Vec<Box<dyn crate::gates::Gate<'a> + 'a>>,
) -> Plan {
    let mut active = jobs.iter().any(Job::running) || !inputs.foreign.is_empty();
    let mut plan = Plan::default();
    // Set by the first blocked waiter that may not be passed; every later waiter waits too.
    let mut barrier: Option<Block> = None;
    let leased = leased_slots(inputs.leases);
    // A waiter that needs a resource whose every slot is leased may wait hours, whatever
    // gate reports first; it never holds the others back.
    let leased_out = |w: &Job| {
        w.needs.iter().any(|n| {
            !uses_lease(w, n, inputs.leases)
                && leased.get(n.as_str()).copied().unwrap_or(0) >= capacity(inputs.caps, n)
        })
    };
    for w in waiting_in_order(jobs) {
        let reason = match &barrier {
            Some(b) => Some(b.clone()),
            None => gates.iter().find_map(|g| g.blocks(w, active)),
        };
        match reason {
            None => {
                gates.iter_mut().for_each(|g| g.admit(w));
                active = true;
                plan.admitted.push(w.id);
            }
            Some(r) => {
                if barrier.is_none() {
                    let waited = inputs.now_ms.saturating_sub(w.queued_ms);
                    let limit = inputs.limits.pass_limit_ms;
                    if !leased_out(w) && (limit == 0 || waited > limit) {
                        barrier = Some(Block::Order);
                    } else if w.bumped && waits_only_for_room(&gates, w, active) {
                        barrier = Some(Block::Bumped { id: w.id });
                    }
                }
                plan.blocked.push((w.id, r));
            }
        }
    }
    plan
}

/// Whether every gate that blocks `w` is about room (points or RAM) that a job passing it
/// would take. The first gate to block is not enough: points are checked first, and a job
/// short of points may also wait for a key or a resource slot.
fn waits_only_for_room<'a>(gates: &[Box<dyn crate::gates::Gate<'a> + 'a>], w: &'a Job, active: bool) -> bool {
    gates
        .iter()
        .filter_map(|g| g.blocks(w, active))
        .all(|b| matches!(b, Block::Points { .. } | Block::Ram { .. }))
}

/// Test shorthand: no named resources.
#[cfg(test)]
pub fn plan(jobs: &[Job], now_ms: u64, free_ram: u64, lim: Limits) -> (Vec<u64>, Vec<(u64, Block)>) {
    plan_with(jobs, now_ms, free_ram, lim, &BTreeMap::new())
}

/// Test shorthand with named resources.
#[cfg(test)]
pub fn plan_with(
    jobs: &[Job],
    now_ms: u64,
    free_ram: u64,
    limits: Limits,
    caps: &BTreeMap<String, u32>,
) -> (Vec<u64>, Vec<(u64, Block)>) {
    plan_leased(jobs, now_ms, free_ram, limits, caps, &[])
}

/// Test shorthand with named resources and leases.
#[cfg(test)]
pub fn plan_leased(
    jobs: &[Job],
    now_ms: u64,
    free_ram: u64,
    limits: Limits,
    caps: &BTreeMap<String, u32>,
    leases: &[Lease],
) -> (Vec<u64>, Vec<(u64, Block)>) {
    let p = schedule(
        jobs,
        &Inputs {
            now_ms,
            free_ram,
            limits,
            caps,
            leases,
            foreign: &[],
        },
    );
    (p.admitted, p.blocked)
}

/// Test shorthand: only the admitted ids.
#[cfg(test)]
pub fn admissible(jobs: &[Job], now_ms: u64, free_ram: u64, lim: Limits) -> Vec<u64> {
    plan(jobs, now_ms, free_ram, lim).0
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1 << 30;
    const LIM: Limits = Limits {
        budget: 4,
        min_free_bytes: 3 * GB,
        pass_limit_ms: 600_000,
        ram_per_point: 0,
        reserve_window_ms: 0,
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
            disk_short: false,
            needs: Vec::new(),
            hold_ttl_ms: None,
            cwd_key: String::new(),
            use_leases: Vec::new(),
            ..Default::default()
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
                    reserved: 0,
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
    fn jobs_in_other_queues_hold_keys_and_make_the_machine_busy() {
        let mut other = keyed(job(9, 2, true), "nm:a");
        other.label = "npx vitest (queue kx)".into();
        let foreign = vec![other];
        let caps = BTreeMap::new();
        let run = |jobs: &[Job], free: u64| {
            schedule(
                jobs,
                &Inputs {
                    now_ms: 2_000,
                    free_ram: free,
                    limits: LIM,
                    caps: &caps,
                    leases: &[],
                    foreign: &foreign,
                },
            )
        };
        // Same key as the job in the other queue: waits, naming it; another key starts.
        let jobs = vec![keyed(job(1, 1, false), "nm:a"), keyed(job(2, 1, false), "nm:b")];
        let p = run(&jobs, 16 * GB);
        assert_eq!(p.admitted, vec![2]);
        assert_eq!(
            p.reason(1),
            Block::Key {
                holder: "npx vitest (queue kx)".into()
            }
        );
        // Nothing runs in this queue, but the machine is busy: the RAM check applies.
        let jobs = vec![job(3, 1, false)];
        let p = run(&jobs, GB);
        assert!(p.admitted.is_empty());
        assert!(matches!(p.reason(3), Block::Ram { .. }), "{p:?}");
        // Points stay per queue: the other queue's cost-2 job takes none of ours.
        let jobs = vec![job(4, 4, false)];
        assert_eq!(run(&jobs, 16 * GB).admitted, vec![4]);
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

    fn bumped(mut j: Job) -> Job {
        j.bumped = true;
        j.high = true;
        j.order = -1;
        j
    }

    #[test]
    fn a_bumped_waiter_short_of_points_or_ram_is_not_passed() {
        let lim = Limits { budget: 3, ..LIM };
        // Budget 3, a cost-2 job runs, the bumped cost-2 waiter does not fit: the cost-1 job
        // behind it would fit, but starting it would delay the bumped one. It waits, naming it.
        let jobs = vec![job(1, 2, true), bumped(job(2, 2, false)), job(3, 1, false)];
        let (adm, blocked) = plan(&jobs, 2_000, 16 * GB, lim);
        assert!(adm.is_empty(), "{blocked:?}");
        assert_eq!(blocked[1], (3, Block::Bumped { id: 2 }));
        // Only moved forward (an older binary, or high prio): passing is allowed.
        let mut plain = jobs.clone();
        plain[1].bumped = false;
        assert_eq!(admissible(&plain, 2_000, 16 * GB, lim), vec![3]);
        // Short of RAM: a hold that takes no points (and no RAM) still waits behind it.
        let mut hold = needing(job(3, 0, false), "emulator");
        hold.hold_ttl_ms = Some(60_000);
        let jobs = vec![job(1, 1, true), bumped(job(2, 1, false)), hold];
        let (adm, blocked) = plan(&jobs, 2_000, GB, LIM);
        assert!(adm.is_empty());
        assert!(matches!(blocked[0], (2, Block::Ram { .. })), "{blocked:?}");
        assert_eq!(blocked[1], (3, Block::Bumped { id: 2 }));
        // Once it fits, it starts first and the others follow as room allows.
        let jobs = vec![bumped(job(2, 2, false)), job(3, 1, false), job(4, 1, false)];
        assert_eq!(admissible(&jobs, 2_000, 16 * GB, lim), vec![2, 3]);
    }

    #[test]
    fn a_bumped_waiter_held_by_a_key_resource_or_disk_may_be_passed() {
        let lim = Limits { budget: 3, ..LIM };
        // Its dependency folder is in use: a strict line would stall behind one long job.
        let jobs = vec![
            keyed(job(1, 1, true), "nm:a"),
            bumped(keyed(job(2, 1, false), "nm:a")),
            job(3, 1, false),
        ];
        assert_eq!(admissible(&jobs, 2_000, 16 * GB, lim), vec![3]);
        // Short of points as well (reported first): the key still decides, no barrier.
        let jobs = vec![
            keyed(job(1, 2, true), "nm:a"),
            bumped(keyed(job(2, 2, false), "nm:a")),
            job(3, 1, false),
        ];
        let (adm, blocked) = plan(&jobs, 2_000, 16 * GB, lim);
        assert_eq!(adm, vec![3]);
        assert!(matches!(blocked[0], (2, Block::Points { .. })), "{blocked:?}");
        // A resource slot held by a running job, or a lease.
        let caps = BTreeMap::new();
        let jobs = vec![
            needing(job(1, 1, true), "db"),
            bumped(needing(job(2, 1, false), "db")),
            job(3, 1, false),
        ];
        assert_eq!(plan_with(&jobs, 2_000, 16 * GB, lim, &caps).0, vec![3]);
        let leases = [lease("abc123", &["emulator"], 0)];
        let jobs = vec![bumped(needing(job(2, 1, false), "emulator")), job(3, 1, false)];
        assert_eq!(plan_leased(&jobs, 2_000, 16 * GB, lim, &caps, &leases).0, vec![3]);
        // Its drive is short of space.
        let mut short = bumped(job(2, 1, false));
        short.disk_short = true;
        let jobs = vec![job(1, 1, true), short, job(3, 1, false)];
        assert_eq!(admissible(&jobs, 2_000, 16 * GB, lim), vec![3]);
    }

    #[test]
    fn low_prio_goes_after_normal_and_high() {
        let mut lo = job(1, 1, false);
        lo.low = true;
        let mut hi = job(3, 1, false);
        hi.high = true;
        let mut both = job(4, 1, false);
        (both.high, both.low) = (true, true);
        let jobs = vec![lo, job(2, 1, false), hi, both, job(5, 1, true)];
        let ids: Vec<u64> = waiting_in_order(&jobs).iter().map(|j| j.id).collect();
        assert_eq!(ids, vec![3, 4, 2, 1]);
    }

    #[test]
    fn named_resources_have_slots() {
        let need = |mut j: Job, n: &str| {
            j.needs = vec![n.to_string()];
            j
        };
        let mut caps = BTreeMap::new();
        caps.insert("db".to_string(), 1);
        caps.insert("gpu".to_string(), 2);
        // One db slot taken by a running job: the db waiter is blocked, the other passes.
        let jobs = vec![
            need(job(1, 1, true), "db"),
            need(job(2, 1, false), "db"),
            job(3, 1, false),
        ];
        let (adm, blocked) = plan_with(&jobs, 2_000, 16 * GB, LIM, &caps);
        assert_eq!(adm, vec![3]);
        assert_eq!(blocked, vec![(2, Block::Resource("db".into()))]);
        // Two gpu slots: two of three waiters start.
        let jobs = vec![
            need(job(1, 1, false), "gpu"),
            need(job(2, 1, false), "gpu"),
            need(job(3, 1, false), "gpu"),
        ];
        assert_eq!(plan_with(&jobs, 2_000, 16 * GB, LIM, &caps).0, vec![1, 2]);
        // Unconfigured resources have one slot.
        let jobs = vec![need(job(1, 1, false), "x"), need(job(2, 1, false), "x")];
        assert_eq!(plan_with(&jobs, 2_000, 16 * GB, LIM, &caps).0, vec![1]);
    }

    #[test]
    fn extra_gates_plug_in_without_touching_the_loop() {
        /// Lets only even job ids start.
        struct EvenOnly;
        impl<'a> crate::gates::Gate<'a> for EvenOnly {
            fn blocks(&self, w: &'a Job, _: bool) -> Option<Block> {
                (w.id % 2 == 1).then(|| Block::Key { holder: "odd".into() })
            }
            fn admit(&mut self, _: &'a Job) {}
        }
        let jobs = vec![job(1, 1, false), job(2, 1, false), job(4, 1, false)];
        let caps = BTreeMap::new();
        let inputs = Inputs {
            now_ms: 2_000,
            free_ram: 16 * GB,
            limits: LIM,
            caps: &caps,
            leases: &[],
            foreign: &[],
        };
        let mut gates = crate::gates::standard(&jobs, &inputs);
        gates.push(Box::new(EvenOnly));
        let plan = schedule_with(&jobs, &inputs, gates);
        assert_eq!(plan.admitted, vec![2, 4]);
        assert_eq!(plan.reason(1), Block::Key { holder: "odd".into() });
    }

    #[test]
    fn just_started_jobs_reserve_ram_for_the_whole_pass() {
        let lim = Limits {
            ram_per_point: GB,
            reserve_window_ms: 20_000,
            ..LIM
        };
        // 4.5 GB free, gate needs 3 GB. One job started 0 s ago with cost 1 reserves 1 GB:
        // 3.5 GB left, so one more cost-1 job may start (taking 1 GB), the next may not.
        let mut started = job(1, 1, true);
        started.started_ms = Some(2_000);
        let jobs = vec![started, job(2, 1, false), job(3, 1, false)];
        let (adm, blocked) = plan(&jobs, 2_000, 4 * GB + GB / 2, lim);
        assert_eq!(adm, vec![2]);
        assert!(matches!(blocked[0], (3, Block::Ram { .. })), "{blocked:?}");
        // The reservation fades: half way through the window only half is reserved.
        assert_eq!(recent_reservation(&jobs, 12_000, &lim), GB / 2);
        assert_eq!(recent_reservation(&jobs, 30_000, &lim), 0);
    }

    #[test]
    fn short_disk_waits_only_while_busy() {
        let mut short = job(2, 1, false);
        short.disk_short = true;
        let jobs = vec![job(1, 1, true), short.clone(), job(3, 1, false)];
        assert_eq!(admissible(&jobs, 2_000, 16 * GB, LIM), vec![3]);
        let (_, blocked) = plan(&jobs, 2_000, 16 * GB, LIM);
        assert_eq!(blocked, vec![(2, Block::Disk)]);
        // Nothing running: it starts (the owner warns).
        assert_eq!(admissible(&[short], 2_000, 16 * GB, LIM), vec![2]);
    }

    #[test]
    fn parked_entries_are_skipped() {
        let mut parked = job(1, 2, false);
        parked.parked_until = Some(99_999_999);
        let jobs = vec![parked, job(2, 2, false)];
        assert_eq!(admissible(&jobs, 2_000, 16 * GB, LIM), vec![2]);
        assert_eq!(waiting_in_order(&jobs).len(), 1);
    }

    fn lease(id: &str, resources: &[&str], cost: u32) -> Lease {
        Lease {
            id: id.into(),
            resources: resources.iter().map(|r| r.to_string()).collect(),
            cost,
            until_ms: u64::MAX,
            ..Lease::default()
        }
    }

    fn needing(mut j: Job, n: &str) -> Job {
        j.needs = vec![n.to_string()];
        j
    }

    #[test]
    fn a_lease_holds_a_resource_slot() {
        let caps = BTreeMap::new();
        let jobs = vec![needing(job(1, 1, false), "emulator"), job(2, 1, false)];
        let leases = [lease("abc123", &["emulator"], 0)];
        let (adm, blocked) = plan_leased(&jobs, 2_000, 16 * GB, LIM, &caps, &leases);
        assert_eq!(adm, vec![2]);
        assert_eq!(blocked, vec![(1, Block::Resource("emulator".into()))]);
        // With two slots the lease takes one and the job the other.
        let caps = BTreeMap::from([("emulator".to_string(), 2)]);
        assert_eq!(
            plan_leased(&jobs, 2_000, 16 * GB, LIM, &caps, &leases).0,
            vec![1, 2]
        );
    }

    #[test]
    fn a_lease_never_freezes_the_queue_behind_it() {
        let caps = BTreeMap::new();
        let leases = [lease("abc123", &["emulator"], 0)];
        // The emulator waiter is far past the pass limit, and even strict order is on: the
        // next waiter still starts, because only a lease blocks the first.
        let jobs = vec![needing(job(1, 1, false), "emulator"), job(2, 1, false)];
        let now = 1_000 + 5 * 3_600_000;
        for limit in [LIM.pass_limit_ms, 0] {
            let lim = Limits {
                pass_limit_ms: limit,
                ..LIM
            };
            assert_eq!(plan_leased(&jobs, now, 16 * GB, lim, &caps, &leases).0, vec![2]);
        }
        // Short of points as well (reported first): still no barrier.
        let mut a = needing(job(1, 2, false), "emulator");
        a.queued_ms = 0;
        let jobs = vec![job(9, 3, true), a, job(2, 1, false)];
        assert_eq!(plan_leased(&jobs, now, 16 * GB, LIM, &caps, &leases).0, vec![2]);
        // A running job holding the slot still keeps strict order.
        let jobs = vec![
            needing(job(1, 1, true), "db"),
            needing(job(2, 1, false), "db"),
            job(3, 1, false),
        ];
        assert!(plan_leased(&jobs, now, 16 * GB, LIM, &caps, &leases).0.is_empty());
    }

    #[test]
    fn a_lease_lets_its_own_jobs_use_its_resources() {
        let caps = BTreeMap::new();
        let mut owned = lease("abc123", &["emulator"], 0);
        owned.for_key = Some("/w/app-2".into());
        let leases = [owned];
        let at = |id: u64, key: &str| {
            let mut j = needing(job(id, 1, false), "emulator");
            j.cwd_key = key.into();
            j
        };
        // In the folder (or below it): no wait, and no second slot taken, so two such jobs
        // start together. Elsewhere, or a sibling folder with the same prefix: it waits.
        let jobs = vec![
            at(1, "/w/app-2"),
            at(2, "/w/app-2/sub"),
            at(3, "/w/app-20"),
            at(4, "/w"),
        ];
        assert_eq!(
            plan_leased(&jobs, 2_000, 16 * GB, LIM, &caps, &leases).0,
            vec![1, 2]
        );
        // Named in CALMRUN_LEASE: from anywhere.
        let mut named = at(5, "/elsewhere");
        named.use_leases = vec!["abc123".into()];
        assert_eq!(
            plan_leased(&[named], 2_000, 16 * GB, LIM, &caps, &leases).0,
            vec![5]
        );
        // A hold in the folder takes a slot of its own.
        let mut h = at(6, "/w/app-2");
        h.hold_ttl_ms = Some(1);
        assert!(plan_leased(&[h], 2_000, 16 * GB, LIM, &caps, &leases)
            .0
            .is_empty());
        assert!(inside("c:\\w\\a", "c:\\w") && inside("/x", "/") && !inside("", "/"));
    }

    #[test]
    fn holds_take_no_points_unless_asked() {
        let mut hold = needing(job(2, 0, false), "emulator");
        hold.hold_ttl_ms = Some(60_000);
        // The budget is full, the hold still gets the free emulator.
        let jobs = vec![job(1, 4, true), hold.clone()];
        let caps = BTreeMap::new();
        assert_eq!(plan_leased(&jobs, 2_000, 16 * GB, LIM, &caps, &[]).0, vec![2]);
        // Not even when free RAM is short.
        assert_eq!(plan_leased(&jobs, 2_000, GB, LIM, &caps, &[]).0, vec![2]);
        // With --cost 1 it waits for a point like any job.
        hold.cost = 1;
        let jobs = vec![job(1, 4, true), hold];
        assert!(plan_leased(&jobs, 2_000, 16 * GB, LIM, &caps, &[]).0.is_empty());
        // A lease with points takes them from the budget.
        let leases = [lease("abc123", &[], 3)];
        let jobs = vec![job(1, 2, false), job(2, 1, false)];
        assert_eq!(plan_leased(&jobs, 2_000, 16 * GB, LIM, &caps, &leases).0, vec![2]);
    }

    #[test]
    fn running_jobs_never_listed() {
        let jobs = vec![job(1, 2, true), job(2, 2, true)];
        assert!(admissible(&jobs, 2_000, 16 * GB, LIM).is_empty());
    }
}
