//! `history.jsonl`: one line per run, and the `stats` summary.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

/// One line of `history.jsonl`.
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
    /// The folder it ran in, as a path key (absent in lines written before 0.4), so the usual
    /// run time of `npx vitest` is not mixed across projects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// The job's id in its queue, so `calmrun logs <id>` finds a finished job's log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    /// Its log file (MCP task or `--log`), if it had one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log: Option<String>,
}

/// The newest successful runs kept per label (and per label and folder) for a median.
const USUAL_KEEP: usize = 20;
/// Successful runs of the same label in the same folder needed for a usual run time.
pub const USUAL_MIN_IN_FOLDER: usize = 3;
/// Successful runs of the same label anywhere needed when the folder has too few.
pub const USUAL_MIN_ANYWHERE: usize = 5;
/// How much of the newest history the medians are read from.
const USUAL_READ_BYTES: u64 = 1 << 20;

/// Usual run times from the history: the median of the newest successful runs (status "0")
/// of a label, in the same folder when there are enough, else anywhere.
#[derive(Debug, Default, Clone)]
pub struct Usual {
    in_folder: std::collections::HashMap<(String, String), Vec<u64>>,
    anywhere: std::collections::HashMap<String, Vec<u64>>,
}

impl Usual {
    /// The newest part of the history at `path` (both files), read once.
    pub fn load(path: &Path) -> Usual {
        Usual::from_text(&read_tail(path, USUAL_READ_BYTES))
    }

    /// From history lines, oldest first.
    pub fn from_text(text: &str) -> Usual {
        let mut u = Usual::default();
        let keep = |v: &mut Vec<u64>, ms: u64| {
            v.push(ms);
            if v.len() > USUAL_KEEP {
                v.remove(0);
            }
        };
        for r in text
            .lines()
            .filter_map(|l| serde_json::from_str::<Record>(l).ok())
        {
            if r.status != "0" {
                continue;
            }
            keep(u.anywhere.entry(r.label.clone()).or_default(), r.run_ms);
            if let Some(cwd) = r.cwd {
                keep(u.in_folder.entry((r.label, cwd)).or_default(), r.run_ms);
            }
        }
        u
    }

    /// The usual run time of `label` in the folder with path key `cwd_key`, if the history
    /// has enough successful runs.
    pub fn median(&self, label: &str, cwd_key: &str) -> Option<u64> {
        let here = self.in_folder.get(&(label.to_string(), cwd_key.to_string()));
        match (here, self.anywhere.get(label)) {
            (Some(v), _) if v.len() >= USUAL_MIN_IN_FOLDER => Some(median(v)),
            (_, Some(v)) if v.len() >= USUAL_MIN_ANYWHERE => Some(median(v)),
            _ => None,
        }
    }
}

fn median(v: &[u64]) -> u64 {
    let mut s = v.to_vec();
    s.sort_unstable();
    let n = s.len();
    match n {
        0 => 0,
        _ if n % 2 == 1 => s[n / 2],
        _ => (s[n / 2 - 1] + s[n / 2]) / 2,
    }
}

/// The last `max` bytes of the history (the rotated file first when the current one is
/// shorter), starting at a whole line.
pub fn read_tail(path: &Path, max: u64) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let tail = |p: &Path, want: u64| -> Vec<u8> {
        let Ok(mut f) = std::fs::File::open(p) else {
            return Vec::new();
        };
        let len = f.metadata().map_or(0, |m| m.len());
        let from = len.saturating_sub(want);
        let mut buf = Vec::new();
        if f.seek(SeekFrom::Start(from)).is_ok() && f.read_to_end(&mut buf).is_ok() && from > 0 {
            // Drop the partial first line.
            let cut = buf.iter().position(|b| *b == b'\n').map_or(buf.len(), |i| i + 1);
            buf.drain(..cut);
        }
        buf
    };
    let mut now = tail(path, max);
    let room = max.saturating_sub(std::fs::metadata(path).map_or(0, |m| m.len()));
    if room > 0 {
        let mut old = tail(&previous(path), room);
        old.append(&mut now);
        now = old;
    }
    String::from_utf8_lossy(&now).into_owned()
}

/// Past this size `history.jsonl` becomes `history.1.jsonl` (replacing the older one), so
/// the history keeps between 4 and 8 MB: some 20,000 to 40,000 runs.
pub const ROTATE_BYTES: u64 = 4 << 20;

/// The rotated file next to `path`.
pub fn previous(path: &Path) -> std::path::PathBuf {
    path.with_file_name("history.1.jsonl")
}

/// The whole history: the rotated file, then the current one.
pub fn read_all(path: &Path) -> String {
    let old = std::fs::read_to_string(previous(path)).unwrap_or_default();
    old + &std::fs::read_to_string(path).unwrap_or_default()
}

/// Append one record (one write, so concurrent runs do not interleave), rotating the file
/// once it passes `max_bytes`.
pub fn append_rotating(path: &Path, r: &Record, max_bytes: u64) -> std::io::Result<()> {
    if std::fs::metadata(path).is_ok_and(|m| m.len() > max_bytes) {
        // Best effort: another calmrun may have rotated it already, or still hold it open.
        let _ = std::fs::rename(path, previous(path));
    }
    append(path, r)
}

/// Append one record (one write, so concurrent runs do not interleave).
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

/// Unix milliseconds as `YYYY-MM-DDTHH:MM:SSZ`.
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

/// The inverse of [`format_utc`].
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

/// One row of `calmrun stats`.
#[derive(Debug, Default, Clone, PartialEq, Serialize)]
pub struct Row {
    pub label: String,
    pub kind: String,
    pub runs: u64,
    pub failures: u64,
    pub avg_wait_ms: u64,
    pub max_wait_ms: u64,
    pub avg_run_ms: u64,
    /// Waits that ended before the command started (killed or cancelled while waiting);
    /// not counted in `runs`.
    pub gave_up: u64,
}

/// Whether a record is a wait that never started its command.
fn never_started(r: &Record) -> bool {
    r.status == "abandoned" || (matches!(r.status.as_str(), "cancelled" | "stopped") && r.run_ms == 0)
}

/// Group the history since `since_ms` by label and kind.
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
        e.0.max_wait_ms = e.0.max_wait_ms.max(r.wait_ms);
        if never_started(&r) {
            e.0.gave_up += 1;
            continue;
        }
        e.0.runs += 1;
        if r.status != "0" {
            e.0.failures += 1;
        }
        e.1 += r.wait_ms;
        e.2 += r.run_ms;
    }
    let mut rows: Vec<Row> = acc
        .into_values()
        .map(|(mut row, wait, run)| {
            row.avg_wait_ms = wait / row.runs.max(1);
            row.avg_run_ms = run / row.runs.max(1);
            row
        })
        .collect();
    rows.sort_by(|a, b| b.runs.cmp(&a.runs).then(a.label.cmp(&b.label)));
    rows
}

/// A duration for people: `1.5s`, `2m05s`, `2h03m`.
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
                cwd: None,
                id: None,
                log: None,
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
        // A job killed while running is a failed run; one killed while waiting gave up.
        let more = [
            text,
            rec("2026-11-21T11:00:00Z", "npx vitest", 1000, 3000, "killed"),
            rec("2026-11-21T12:00:00Z", "npx vitest", 8000, 0, "abandoned"),
            rec("2026-11-21T13:00:00Z", "npx vitest", 500, 0, "cancelled"),
        ]
        .join("\n");
        let r = &summarize(&more, parse_utc("2026-11-15T00:00:00Z").unwrap())[0];
        assert_eq!(
            (r.runs, r.failures, r.gave_up, r.max_wait_ms, r.avg_run_ms),
            (3, 2, 2, 8000, 5000)
        );
    }

    #[test]
    fn history_rotates_and_reads_both_files() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("history.jsonl");
        let rec = |label: &str| Record {
            at: "2026-11-20T10:00:00Z".into(),
            label: label.into(),
            kind: "test".into(),
            cost: 1,
            wait_ms: 0,
            run_ms: 1,
            status: "0".into(),
            queue: None,
            cwd: None,
            id: None,
            log: None,
        };
        for i in 0..5 {
            append_rotating(&path, &rec(&format!("r{i}")), 200).unwrap();
        }
        assert!(previous(&path).exists());
        assert!(std::fs::metadata(&path).unwrap().len() <= 400);
        let labels: Vec<String> = read_all(&path)
            .lines()
            .map(|l| serde_json::from_str::<Record>(l).unwrap().label)
            .collect();
        // The oldest lines went with an earlier rotated file; the newest are all there.
        assert_eq!(labels.last().map(String::as_str), Some("r4"));
        assert!(labels.windows(2).all(|w| w[0] < w[1]), "{labels:?}");
    }

    #[test]
    fn usual_run_time_prefers_the_same_folder() {
        let rec = |label: &str, cwd: Option<&str>, run: u64, st: &str| {
            serde_json::to_string(&Record {
                at: "2026-11-20T10:00:00Z".into(),
                label: label.into(),
                kind: "vitest".into(),
                cost: 2,
                wait_ms: 0,
                run_ms: run,
                status: st.into(),
                queue: None,
                cwd: cwd.map(String::from),
                id: None,
                log: None,
            })
            .unwrap()
        };
        let mut lines = vec![
            rec("npx vitest", Some("/a"), 10_000, "0"),
            rec("npx vitest", Some("/a"), 30_000, "0"),
            rec("npx vitest", Some("/a"), 20_000, "0"),
            // Failures do not count.
            rec("npx vitest", Some("/a"), 900_000, "1"),
            rec("npx vitest", Some("/b"), 300_000, "0"),
            rec("npx vitest", None, 400_000, "0"),
        ];
        let u = Usual::from_text(&lines.join("\n"));
        // Three runs in /a: their median. /b has one, so the five successful runs anywhere.
        assert_eq!(u.median("npx vitest", "/a"), Some(20_000));
        assert_eq!(u.median("npx vitest", "/b"), Some(30_000));
        assert_eq!(u.median("cargo test", "/a"), None);
        // Too few anywhere: no hint.
        lines.truncate(2);
        assert_eq!(
            Usual::from_text(&lines.join("\n")).median("npx vitest", "/a"),
            None
        );
        assert_eq!(median(&[4, 1, 3, 2]), 2);
    }

    #[test]
    fn history_tail_starts_at_a_whole_line_and_reaches_the_rotated_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("history.jsonl");
        std::fs::write(previous(&path), "old1\nold2\n").unwrap();
        std::fs::write(&path, "new1\nnew2\n").unwrap();
        assert_eq!(read_tail(&path, 1 << 20), "old1\nold2\nnew1\nnew2\n");
        assert_eq!(read_tail(&path, 7), "new2\n");
        assert_eq!(read_tail(&tmp.path().join("none").join("history.jsonl"), 100), "");
    }

    #[test]
    fn human_durations() {
        assert_eq!(human_ms(1500), "1.5s");
        assert_eq!(human_ms(125_000), "2m05s");
        assert_eq!(human_ms(7_380_000), "2h03m");
    }
}
