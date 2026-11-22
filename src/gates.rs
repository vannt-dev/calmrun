//! Gates: the conditions a waiting job must pass to start. The scheduler runs them in order
//! for each waiter and stops at the first that blocks, so the order is also the order of the
//! reasons users see. Adding a gate = one type implementing [`Gate`] plus one line in
//! [`standard`]; the scheduler loop does not change.

use crate::scheduler::{effective_cost, Block, Inputs};
use crate::state::Job;
use std::collections::{HashMap, HashSet};

/// One start condition. `'a` is the lifetime of the job snapshot the scheduler works on.
pub trait Gate<'a> {
    /// Why `w` cannot start now. `active` is true when something runs or was already
    /// admitted in this pass.
    fn blocks(&self, w: &'a Job, active: bool) -> Option<Block>;
    /// Record that `w` starts in this pass.
    fn admit(&mut self, w: &'a Job);
}

/// Budget points.
struct Points {
    used: u32,
    budget: u32,
}

impl<'a> Gate<'a> for Points {
    fn blocks(&self, w: &'a Job, _: bool) -> Option<Block> {
        let needs = effective_cost(w.cost, self.budget);
        (self.used + needs > self.budget).then_some(Block::Points {
            used: self.used,
            needs,
        })
    }
    fn admit(&mut self, w: &'a Job) {
        self.used += effective_cost(w.cost, self.budget);
    }
}

/// Exclusive keys (`node_modules`, cargo `target`): one holder at a time.
struct Keys<'a> {
    held: HashSet<&'a str>,
}

impl<'a> Gate<'a> for Keys<'a> {
    fn blocks(&self, w: &'a Job, _: bool) -> Option<Block> {
        w.keys
            .iter()
            .any(|k| self.held.contains(k.as_str()))
            .then_some(Block::Key)
    }
    fn admit(&mut self, w: &'a Job) {
        self.held.extend(w.keys.iter().map(String::as_str));
    }
}

/// Named resources (`--needs db`) with a number of slots each.
struct Resources<'a> {
    held: HashMap<&'a str, u32>,
    caps: &'a std::collections::BTreeMap<String, u32>,
}

impl<'a> Gate<'a> for Resources<'a> {
    fn blocks(&self, w: &'a Job, _: bool) -> Option<Block> {
        w.needs
            .iter()
            .find(|n| {
                self.held.get(n.as_str()).copied().unwrap_or(0) >= crate::scheduler::capacity(self.caps, n)
            })
            .map(|n| Block::Resource(n.clone()))
    }
    fn admit(&mut self, w: &'a Job) {
        for n in &w.needs {
            *self.held.entry(n.as_str()).or_default() += 1;
        }
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
/// take. Skipped when nothing runs, so an idle machine always starts a job.
struct Ram {
    free: u64,
    needs: u64,
    per_point: u64,
    budget: u32,
}

impl<'a> Gate<'a> for Ram {
    fn blocks(&self, _: &'a Job, active: bool) -> Option<Block> {
        (active && self.needs > 0 && self.free < self.needs).then_some(Block::Ram {
            free: self.free,
            needs: self.needs,
        })
    }
    fn admit(&mut self, w: &'a Job) {
        let take = self.per_point * effective_cost(w.cost, self.budget) as u64;
        self.free = self.free.saturating_sub(take);
    }
}

/// The standard gates, primed with what already runs, in reporting order.
pub fn standard<'a>(jobs: &'a [Job], inputs: &Inputs<'a>) -> Vec<Box<dyn Gate<'a> + 'a>> {
    let running = || jobs.iter().filter(|j| j.running());
    let mut keys = Keys { held: HashSet::new() };
    let mut resources = Resources {
        held: HashMap::new(),
        caps: inputs.caps,
    };
    for j in running() {
        keys.admit(j);
        resources.admit(j);
    }
    vec![
        Box::new(Points {
            used: crate::scheduler::used_points(jobs, inputs.limits.budget),
            budget: inputs.limits.budget,
        }),
        Box::new(keys),
        Box::new(resources),
        Box::new(Disk),
        Box::new(Ram {
            free: inputs
                .free_ram
                .saturating_sub(crate::scheduler::recent_reservation(
                    jobs,
                    inputs.now_ms,
                    &inputs.limits,
                )),
            needs: inputs.limits.min_free_bytes,
            per_point: inputs.limits.ram_per_point,
            budget: inputs.limits.budget,
        }),
    ]
}
