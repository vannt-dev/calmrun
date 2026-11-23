//! Reading and nudging the queue: `status`, `queue_status`, `bump`.

use crate::error::Result;
use crate::queue::{locked, now_ms, Ctx};
use crate::scheduler;
use crate::state::{Job, Lease, State};
use serde_json::{json, Value};

/// A waiter older than this gets a "stuck?" warning.
pub const STUCK_MS: u64 = 30 * 60_000;

/// A pruned snapshot of the queue.
pub fn snapshot(ctx: &mut Ctx) -> Result<State> {
    let store = ctx.store()?;
    locked(&store, &mut ctx.sys, |s| s.clone())
}

fn job_json(j: &Job, now: u64, place: Option<usize>) -> Value {
    let since = j.started_ms.unwrap_or(j.queued_ms);
    json!({
        "id": j.id,
        "label": j.label,
        "kind": j.kind,
        "cost": j.cost,
        "pid": j.pid,
        "child_pid": j.child_pid,
        "mem_bytes": j.mem_bytes,
        "needs": j.needs,
        "cwd": j.cwd,
        "high": j.high,
        "since_ms": since,
        "for_ms": now.saturating_sub(since),
        "place": place,
    })
}

fn parked_json(j: &Job, now: u64) -> Value {
    json!({
        "ticket": j.id,
        "label": j.label,
        "kind": j.kind,
        "cost": j.cost,
        "cwd": j.cwd,
        "expires_in_ms": j.parked_until.unwrap_or(0).saturating_sub(now),
        "text": crate::messages::kept_line(j, now),
    })
}

fn lease_json(l: &Lease, now: u64) -> Value {
    json!({
        "lease": l.id,
        "resources": l.resources,
        "label": l.label,
        "cost": l.cost,
        "pid": l.pid,
        "cwd": l.cwd,
        "since_ms": l.granted_ms,
        "age_ms": now.saturating_sub(l.granted_ms),
        "expires_in_ms": l.until_ms.saturating_sub(now),
        "text": crate::lease::line(l, now),
    })
}

/// The data behind `status --json` and the MCP `queue_status` tool.
pub fn status_json(ctx: &mut Ctx, state: &State) -> Value {
    let now = now_ms();
    let budget = ctx.cfg.budget;
    let running: Vec<Value> = state
        .jobs
        .iter()
        .filter(|j| j.running())
        .map(|j| job_json(j, now, None))
        .collect();
    let waiting: Vec<Value> = scheduler::waiting_in_order(&state.jobs)
        .iter()
        .enumerate()
        .map(|(i, j)| job_json(j, now, Some(i + 1)))
        .collect();
    let parked: Vec<Value> = state
        .jobs
        .iter()
        .filter(|j| j.parked())
        .map(|j| parked_json(j, now))
        .collect();
    let leases: Vec<Value> = state.leases.iter().map(|l| lease_json(l, now)).collect();
    json!({
        "queue": ctx.queue,
        "budget": budget,
        "used": scheduler::used_points(&state.jobs, budget) + scheduler::leased_points(&state.leases, budget),
        "free_ram_bytes": ctx.sys.free_ram(),
        "min_free_bytes": ctx.cfg.min_free_bytes,
        "running": running,
        "waiting": waiting,
        "parked": parked,
        "leases": leases,
        "warnings": crate::messages::stuck_warnings(state, now, STUCK_MS),
    })
}

/// Move waiting jobs whose label contains `needle` (any case) to the front, keeping their
/// relative order. Returns how many moved.
pub fn bump(ctx: &mut Ctx, needle: &str) -> Result<usize> {
    let needle = needle.to_lowercase();
    let store = ctx.store()?;
    locked(&store, &mut ctx.sys, |s| {
        let min = s.jobs.iter().map(|j| j.order).min().unwrap_or(0);
        let ids: Vec<u64> = scheduler::waiting_in_order(&s.jobs)
            .into_iter()
            .filter(|j| j.label.to_lowercase().contains(&needle))
            .map(|j| j.id)
            .collect();
        let n = ids.len() as i64;
        for (i, id) in ids.iter().enumerate() {
            if let Some(j) = s.jobs.iter_mut().find(|j| j.id == *id) {
                j.high = true;
                j.order = min - n + i as i64;
            }
        }
        ids.len()
    })
}
