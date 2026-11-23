//! Reading and nudging the queue: `status`, `queue_status`, `bump`.

use crate::error::Result;
use crate::messages::{self, WaitContext};
use crate::queue::{self, locked, now_ms, Ctx, Liveness};
use crate::scheduler::{self, Inputs, Limits};
use crate::state::{Job, Lease, State};
use serde_json::{json, Value};

/// A waiter older than this gets a "stuck?" warning.
pub const STUCK_MS: u64 = 30 * 60_000;

/// A pruned snapshot of the queue.
pub fn snapshot(ctx: &mut Ctx) -> Result<State> {
    let store = ctx.store()?;
    locked(&store, &mut ctx.sys, |s| s.clone())
}

/// The jobs running in the other queues (for exclusive keys and the RAM check).
pub fn foreign(ctx: &mut Ctx) -> Vec<Job> {
    let Ok(store) = ctx.store() else {
        return Vec::new();
    };
    let others = store.others().unwrap_or_default();
    let live = Liveness::check_with(&store, &others, &mut ctx.sys);
    queue::foreign_running(&others, &live)
}

/// Why each waiting job waits right now: (job id, reason code, reason text). A job the
/// scheduler would admit now is "starting": its owner picks that up at its next poll.
pub fn reasons(
    ctx: &mut Ctx,
    state: &State,
    limits: Limits,
    free_ram: u64,
) -> Vec<(u64, &'static str, String)> {
    let foreign = foreign(ctx);
    let caps = &ctx.cfg.resources;
    let now = now_ms();
    let plan = scheduler::schedule(
        &state.jobs,
        &Inputs {
            now_ms: now,
            free_ram,
            limits,
            caps,
            leases: &state.leases,
            foreign: &foreign,
        },
    );
    scheduler::waiting_in_order(&state.jobs)
        .into_iter()
        .map(|j| {
            if plan.admits(j.id) {
                return (j.id, "starting", "about to start".to_string());
            }
            let why = plan.reason(j.id);
            let c = WaitContext {
                me: j,
                budget: limits.budget,
                disk: None,
                caps,
                now_ms: now,
            };
            (
                j.id,
                messages::reason_code(&why),
                messages::reason(state, &why, &c),
            )
        })
        .collect()
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
        "for": l.for_dir,
        "text": crate::lease::line(l, now),
    })
}

/// The data behind `status --json` and the MCP `queue_status` tool. The budget and RAM
/// minimum are the queue's own (recorded by the job that started it), which may differ from
/// this process's config; `warnings` says so.
pub fn status_json(ctx: &mut Ctx, state: &State) -> Value {
    let now = now_ms();
    let limits = queue::effective_limits(ctx.limits(), state);
    let budget = limits.budget;
    let free_ram = ctx.sys.free_ram();
    let why = reasons(ctx, state, limits, free_ram);
    let running: Vec<Value> = state
        .jobs
        .iter()
        .filter(|j| j.running())
        .map(|j| job_json(j, now, None))
        .collect();
    let waiting: Vec<Value> = scheduler::waiting_in_order(&state.jobs)
        .iter()
        .enumerate()
        .map(|(i, j)| {
            let mut v = job_json(j, now, Some(i + 1));
            if let Some((_, code, text)) = why.iter().find(|w| w.0 == j.id) {
                v["why"] = json!(text);
                v["why_code"] = json!(code);
            }
            v
        })
        .collect();
    let mut warnings = messages::stuck_warnings(state, now, STUCK_MS);
    let mine = ctx.settings();
    if let Some(rec) = state.settings.filter(|r| !r.same_limits(&mine)) {
        let text = messages::settings_mismatch(&rec, &mine);
        warnings.push(text.trim_start_matches("calmrun: note: ").to_string());
    }
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
        "free_ram_bytes": free_ram,
        "min_free_bytes": limits.min_free_bytes,
        "pass_limit_ms": limits.pass_limit_ms,
        "settings_by_pid": state.settings.map(|s| s.pid),
        "running": running,
        "waiting": waiting,
        "parked": parked,
        "leases": leases,
        "warnings": warnings,
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
