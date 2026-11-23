//! Resource leases without a command (`calmrun hold` / `calmrun release`): ids, the CLI
//! arguments, renew and release. Waiting for the slots is [`crate::queue::hold`].

use crate::error::Result;
use crate::queue::{locked, now_ms, Ctx};
use crate::state::Lease;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// A lease lapses after this unless `--ttl` says otherwise.
pub const DEFAULT_TTL: Duration = Duration::from_secs(4 * 3600);
/// Hex digits in a lease id.
pub const ID_LEN: usize = 6;

/// `calmrun hold`.
#[derive(Debug, clap::Args)]
pub struct HoldArgs {
    /// Named resources to hold one slot of each (comma-separate: emulator,db).
    #[arg(
        value_name = "RESOURCE",
        value_delimiter = ',',
        required_unless_present = "renew",
        conflicts_with = "renew"
    )]
    pub resources: Vec<String>,
    /// Seconds until the lease lapses unless renewed (default 4 h; with --renew: the
    /// lease's own time to live).
    #[arg(long, value_name = "SECS", value_parser = clap::value_parser!(u64).range(1..))]
    pub ttl: Option<u64>,
    /// Label shown in status.
    #[arg(long, value_name = "TEXT", conflicts_with = "renew")]
    pub label: Option<String>,
    /// Budget points to hold as well (default 0: a lease runs nothing).
    #[arg(long, value_name = "N", conflicts_with = "renew")]
    pub cost: Option<u32>,
    /// Give up waiting after this many seconds: exit 75 (the place is not kept); 0 tries
    /// once.
    #[arg(long, value_name = "SECS", conflicts_with = "renew")]
    pub wait_timeout: Option<f64>,
    /// The folder that owns the lease: jobs whose folder is inside it use the leased
    /// resources instead of waiting for them (so do jobs started with CALMRUN_LEASE=<id>).
    #[arg(long = "for", value_name = "DIR", conflicts_with = "renew")]
    pub for_dir: Option<std::path::PathBuf>,
    /// Extend this lease by --ttl from now instead of taking a new one.
    #[arg(long, value_name = "LEASE")]
    pub renew: Option<String>,
}

/// `calmrun release`.
#[derive(Debug, clap::Args)]
pub struct ReleaseArgs {
    /// The id `calmrun hold` printed.
    #[arg(
        value_name = "LEASE",
        required_unless_present = "ticket",
        conflicts_with = "ticket"
    )]
    pub lease: Option<String>,
    /// Instead: give up the place in line kept for this ticket (by a --wait-timeout run).
    #[arg(long, value_name = "ID")]
    pub ticket: Option<u64>,
}

/// Whether `id` looks like a lease id (six hex digits). Returns it in lower case.
pub fn parse_id(id: &str) -> Option<String> {
    let id = id.trim().to_ascii_lowercase();
    (id.len() == ID_LEN && id.chars().all(|c| c.is_ascii_hexdigit())).then_some(id)
}

/// A fresh random id that `taken` does not know.
pub fn new_id(taken: impl Fn(&str) -> bool) -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    loop {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(now_ms());
        h.write_u32(std::process::id());
        h.write_u64(SEQ.fetch_add(1, Ordering::Relaxed));
        let id = format!("{:06x}", h.finish() & 0xff_ffff);
        if !taken(&id) {
            return id;
        }
    }
}

/// Take the lease out of the queue. Returns it, or `None` when it is already gone.
pub fn release(ctx: &mut Ctx, id: &str) -> Result<Option<Lease>> {
    let store = ctx.store()?;
    locked(&store, &mut ctx.sys, |s| {
        let at = s.leases.iter().position(|l| l.id == id)?;
        Some(s.leases.remove(at))
    })
}

/// Drop the place kept for ticket `id`. Returns the entry, or `None` when no place is kept
/// under that id (expired, already resumed, or unknown).
pub fn drop_ticket(ctx: &mut Ctx, id: u64) -> Result<Option<crate::state::Job>> {
    let store = ctx.store()?;
    locked(&store, &mut ctx.sys, |s| {
        let at = s.jobs.iter().position(|j| j.id == id && j.parked())?;
        Some(s.jobs.remove(at))
    })
}

/// Extend the lease to `ttl` (default: its own time to live) from now. `None` when it is gone.
pub fn renew(ctx: &mut Ctx, id: &str, ttl: Option<Duration>) -> Result<Option<Lease>> {
    let store = ctx.store()?;
    let now = now_ms();
    locked(&store, &mut ctx.sys, |s| {
        let l = s.leases.iter_mut().find(|l| l.id == id)?;
        if let Some(t) = ttl {
            l.ttl_ms = t.as_millis() as u64;
        }
        l.until_ms = now.saturating_add(l.ttl_ms);
        Some(l.clone())
    })
}

/// `emulator,db "label" (lease abc123, held 5m00s, expires in 3h55m, for /w/app-2)`.
pub fn line(l: &Lease, now: u64) -> String {
    format!(
        "{}  \"{}\"  (lease {}, held {}, expires in {}{})",
        l.resources.join(","),
        l.label,
        l.id,
        crate::history::human_ms(now.saturating_sub(l.granted_ms)),
        crate::history::human_ms(l.until_ms.saturating_sub(now)),
        l.for_dir
            .as_ref()
            .map(|d| format!(", for {d}"))
            .unwrap_or_default()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_six_hex_digits() {
        assert_eq!(parse_id("A1b2C3").as_deref(), Some("a1b2c3"));
        for bad in ["", "abc", "abcdefa", "xyz123", "12 345", "../../x"] {
            assert_eq!(parse_id(bad), None, "{bad}");
        }
        let id = new_id(|_| false);
        assert_eq!(parse_id(&id).as_deref(), Some(id.as_str()));
    }

    #[test]
    fn new_ids_avoid_taken_ones() {
        let first = new_id(|_| false);
        let other = new_id(|id| id == first);
        assert_ne!(first, other);
    }

    #[test]
    fn line_shows_resources_label_and_times() {
        let l = Lease {
            id: "abc123".into(),
            resources: vec!["emulator".into()],
            label: "lane calmrun-4".into(),
            granted_ms: 0,
            until_ms: 3_600_000,
            ..Lease::default()
        };
        let text = line(&l, 60_000);
        assert!(
            text.starts_with("emulator  \"lane calmrun-4\"  (lease abc123, held "),
            "{text}"
        );
        assert!(text.contains("expires in"), "{text}");
    }
}
