//! `history.jsonl`: one line per run, and the `stats` summary.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Record {
    /// UTC time the run ended, `YYYY-MM-DDTHH:MM:SSZ`.
    pub at: String,
    pub label: String,
    pub kind: String,
    pub cost: u32,
    pub wait_ms: u64,
    pub run_ms: u64,
    /// Exit code, or a word such as "interrupted" / "spawn-failed".
    pub status: String,
    /// The separate queue (`--id`) it ran in, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue: Option<String>,
}

pub fn append(path: &Path, r: &Record) -> std::io::Result<()> {
    let mut line = serde_json::to_string(r).map_err(std::io::Error::other)?;
    line.push('\n');
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(line.as_bytes())
}

// Civil-date conversion (Howard Hinnant's algorithm), so no date crate is needed.
fn days_to_civil(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn civil_to_days(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub fn format_utc(unix_ms: u64) -> String {
    let secs = (unix_ms / 1000) as i64;
    let (y, m, d) = days_to_civil(secs.div_euclid(86_400));
    let rem = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

pub fn parse_utc(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let days = civil_to_days(n(0..4)?, n(5..7)? as u32, n(8..10)? as u32);
    let secs = days * 86_400 + n(11..13)? * 3600 + n(14..16)? * 60 + n(17..19)?;
    u64::try_from(secs).ok().map(|s| s * 1000)
}

#[derive(Debug, Default, Clone, PartialEq, Serialize)]
pub struct Row {
    pub label: String,
    pub kind: String,
    pub runs: u64,
    pub failures: u64,
    pub avg_wait_ms: u64,
    pub max_wait_ms: u64,
    pub avg_run_ms: u64,
}

pub fn summarize(text: &str, since_ms: u64) -> Vec<Row> {
    let mut acc: BTreeMap<(String, String), (Row, u64, u64)> = BTreeMap::new();
    for line in text.lines() {
        let Ok(r) = serde_json::from_str::<Record>(line) else {
            continue;
        };
        if parse_utc(&r.at).is_none_or(|t| t < since_ms) {
            continue;
        }
        let e = acc.entry((r.label.clone(), r.kind.clone())).or_insert_with(|| {
            (
                Row {
                    label: r.label.clone(),
                    kind: r.kind.clone(),
                    ..Row::default()
                },
                0,
                0,
            )
        });
        e.0.runs += 1;
        if r.status != "0" {
            e.0.failures += 1;
        }
        e.0.max_wait_ms = e.0.max_wait_ms.max(r.wait_ms);
        e.1 += r.wait_ms;
        e.2 += r.run_ms;
    }
    let mut rows: Vec<Row> = acc
        .into_values()
        .map(|(mut row, wait, run)| {
            row.avg_wait_ms = wait / row.runs;
            row.avg_run_ms = run / row.runs;
            row
        })
        .collect();
    rows.sort_by(|a, b| b.runs.cmp(&a.runs).then(a.label.cmp(&b.label)));
    rows
}

pub fn human_ms(ms: u64) -> String {
    let s = ms / 1000;
    if s < 60 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else if s < 3600 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{}h{:02}m", s / 3600, s % 3600 / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_roundtrip() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_utc(1_795_000_000_000), "2026-11-18T11:06:40Z");
        for ms in [0u64, 951_782_400_000, 1_795_000_000_000, 4_102_444_800_000] {
            assert_eq!(parse_utc(&format_utc(ms)), Some(ms));
        }
    }

    #[test]
    fn summary_groups_and_filters() {
        let rec = |at: &str, label: &str, wait: u64, run: u64, st: &str| {
            serde_json::to_string(&Record {
                at: at.into(),
                label: label.into(),
                kind: "vitest".into(),
                cost: 2,
                wait_ms: wait,
                run_ms: run,
                status: st.into(),
                queue: None,
            })
            .unwrap()
        };
        let text = [
            rec("2026-11-20T10:00:00Z", "npx vitest", 1000, 5000, "0"),
            rec("2026-11-21T10:00:00Z", "npx vitest", 3000, 7000, "1"),
            rec("2020-01-01T00:00:00Z", "npx vitest", 9000, 9000, "0"),
            "garbage".to_string(),
        ]
        .join("\n");
        let rows = summarize(&text, parse_utc("2026-11-15T00:00:00Z").unwrap());
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(
            (r.runs, r.failures, r.avg_wait_ms, r.max_wait_ms, r.avg_run_ms),
            (2, 1, 2000, 3000, 6000)
        );
    }

    #[test]
    fn human_durations() {
        assert_eq!(human_ms(1500), "1.5s");
        assert_eq!(human_ms(125_000), "2m05s");
        assert_eq!(human_ms(7_380_000), "2h03m");
    }
}
