//! Gates: the conditions a waiting job must pass to start. The scheduler runs them in order
//! for each waiter and stops at the first that blocks, so the order is also the order of the
//! reasons users see. Adding a gate = one type implementing [`Gate`] plus one line in
//! [`standard`]; the scheduler loop does not change.

use crate::scheduler::{job_points, uses_lease, Block, Inputs};
use crate::state::{Job, Lease};
use std::collections::{BTreeMap, HashMap, HashSet};

/// One start condition. `'a` is the lifetime of the job snapshot the scheduler works on.
pub trait Gate<'a> {
    /// Why `w` cannot start now. `active` is true when something runs or was already
    /// admitted in this pass.
    fn blocks(&self, w: &'a Job, active: bool) -> Option<Block>;
    /// Record that `w` starts in this pass.
    fn admit(&mut self, w: &'a Job);
    /// The instances this gate gave `w` when it admitted it (resources with instances).
    fn picked(&self, _w: &'a Job) -> Option<BTreeMap<String, String>> {
        None
    }
}

/// Budget points.
struct Points {
    used: u32,
    budget: u32,
}

impl<'a> Gate<'a> for Points {
    fn blocks(&self, w: &'a Job, _: bool) -> Option<Block> {
        let needs = job_points(w, self.budget);
        (self.used + needs > self.budget).then_some(Block::Points {
            used: self.used,
            needs,
        })
    }
    fn admit(&mut self, w: &'a Job) {
        self.used += job_points(w, self.budget);
    }
}

/// Exclusive keys (`node_modules`, cargo `target`): one holder at a time, machine-wide.
struct Keys<'a> {
    /// Key -> the label of the job holding it.
    held: HashMap<&'a str, &'a str>,
}

impl<'a> Gate<'a> for Keys<'a> {
    fn blocks(&self, w: &'a Job, _: bool) -> Option<Block> {
        w.keys
            .iter()
            .find_map(|k| self.held.get(k.as_str()))
            .map(|holder| Block::Key {
                holder: holder.to_string(),
            })
    }
    fn admit(&mut self, w: &'a Job) {
        for k in &w.keys {
            self.held.entry(k.as_str()).or_insert(w.label.as_str());
        }
    }
}

/// Named resources (`--needs db`) with a number of slots each. A job may use a lease's slot
/// instead (its folder owns the lease, or it names it): such a need neither waits nor takes one.
///
/// A need the waiter published ready instances for (`Job::ready`) takes a free one of those
/// instead of a slot: one not held by a running job, a lease, or a job admitted in this pass.
/// A holder without an instance (count only) is assumed to hold one of the free ones.
struct Resources<'a> {
    held: HashMap<&'a str, u32>,
    /// Instances in use, per resource.
    busy: HashMap<&'a str, HashSet<&'a str>>,
    /// The instances given to the jobs admitted in this pass.
    picks: BTreeMap<u64, BTreeMap<String, String>>,
    caps: &'a BTreeMap<String, u32>,
    leases: &'a [Lease],
}

impl<'a> Resources<'a> {
    fn own_needs(&self, w: &'a Job) -> impl Iterator<Item = &'a String> + '_ {
        w.needs.iter().filter(move |n| !uses_lease(w, n, self.leases))
    }

    /// The ready instances of `n` a waiter may take, in order.
    fn free(&self, n: &str, ready: &'a [String]) -> Vec<&'a str> {
        let busy = self.busy.get(n);
        let named = busy.map_or(0, |b| b.len()) as u32;
        let count_only = self.held.get(n).copied().unwrap_or(0).saturating_sub(named) as usize;
        ready
            .iter()
            .map(String::as_str)
            .filter(|i| busy.is_none_or(|b| !b.contains(i)))
            .skip(count_only)
            .collect()
    }

    /// Count one holder of `n`, and its instance when it has one.
    fn take(&mut self, n: &'a str, instance: Option<&'a str>) {
        *self.held.entry(n).or_default() += 1;
        if let Some(i) = instance {
            self.busy.entry(n).or_default().insert(i);
        }
    }

    /// Count what a running job already holds (it picks nothing).
    fn prime(&mut self, j: &'a Job) {
        let own: Vec<&'a String> = self.own_needs(j).collect();
        for n in own {
            self.take(n, j.instances.get(n).map(String::as_str));
        }
    }
}

impl<'a> Gate<'a> for Resources<'a> {
    fn blocks(&self, w: &'a Job, _: bool) -> Option<Block> {
        self.own_needs(w).find_map(|n| match w.ready.get(n) {
            Some(ready) if ready.is_empty() => Some(Block::NoInstance(n.clone())),
            Some(ready) => self.free(n, ready).is_empty().then(|| Block::Resource(n.clone())),
            None => (self.held.get(n.as_str()).copied().unwrap_or(0)
                >= crate::scheduler::capacity(self.caps, n))
            .then(|| Block::Resource(n.clone())),
        })
    }
    fn admit(&mut self, w: &'a Job) {
        let own: Vec<&'a String> = self.own_needs(w).collect();
        for n in own {
            let pick = w.ready.get(n).and_then(|r| self.free(n, r).first().copied());
            if let Some(i) = pick {
                self.picks
                    .entry(w.id)
                    .or_default()
                    .insert(n.clone(), i.to_string());
            }
            self.take(n, pick);
        }
    }
    fn picked(&self, w: &'a Job) -> Option<BTreeMap<String, String>> {
        self.picks.get(&w.id).cloned()
    }
}

/// The waiter's own drive is short of space (published by its owner). Only while busy.
struct Disk;

impl<'a> Gate<'a> for Disk {
    fn blocks(&self, w: &'a Job, active: bool) -> Option<Block> {
        (active && w.disk_short).then_some(Block::Disk)
    }
    fn admit(&mut self, _: &'a Job) {}
}

/// Free RAM, minus what recently started jobs and jobs admitted in this pass are assumed to
/// take. Skipped when nothing runs on the machine (in any queue), so an idle machine always
/// starts a job, and for a hold that takes no points (it runs nothing).
struct Ram {
    free: u64,
    /// Assumed taken by jobs that just started and by jobs admitted in this pass.
    reserved: u64,
    needs: u64,
    per_point: u64,
    budget: u32,
}

impl<'a> Gate<'a> for Ram {
    fn blocks(&self, w: &'a Job, active: bool) -> Option<Block> {
        let runs_something = job_points(w, self.budget) > 0;
        let left = self.free.saturating_sub(self.reserved);
        (runs_something && active && self.needs > 0 && left < self.needs).then_some(Block::Ram {
            free: self.free,
            reserved: self.reserved,
            needs: self.needs,
        })
    }
    fn admit(&mut self, w: &'a Job) {
        self.reserved += self.per_point * job_points(w, self.budget) as u64;
    }
}

/// The standard gates, primed with what already runs and what leases hold, in reporting order.
pub fn standard<'a>(jobs: &'a [Job], inputs: &Inputs<'a>) -> Vec<Box<dyn Gate<'a> + 'a>> {
    let running = || jobs.iter().filter(|j| j.running());
    let mut keys = Keys { held: HashMap::new() };
    let mut resources = Resources {
        held: HashMap::new(),
        busy: HashMap::new(),
        picks: BTreeMap::new(),
        caps: inputs.caps,
        leases: inputs.leases,
    };
    for j in running() {
        keys.admit(j);
        resources.prime(j);
    }
    // Other queues' jobs hold their keys too (their resources and points are their own).
    for j in inputs.foreign {
        keys.admit(j);
    }
    for l in inputs.leases {
        for r in &l.resources {
            resources.take(r, l.instances.get(r).map(String::as_str));
        }
    }
    let budget = inputs.limits.budget;
    vec![
        Box::new(Points {
            used: crate::scheduler::used_points(jobs, budget)
                + crate::scheduler::leased_points(inputs.leases, budget),
            budget,
        }),
        Box::new(keys),
        Box::new(resources),
        Box::new(Disk),
        Box::new(Ram {
            free: inputs.free_ram,
            reserved: crate::scheduler::recent_reservation(jobs, inputs.now_ms, &inputs.limits)
                + crate::scheduler::recent_reservation(inputs.foreign, inputs.now_ms, &inputs.limits),
            needs: inputs.limits.min_free_bytes,
            per_point: inputs.limits.ram_per_point,
            budget: inputs.limits.budget,
        }),
    ]
}
