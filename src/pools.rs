//! Resources with instances: `[resources.android] instances = ["emulator-5554", …]` or a
//! `discover` command (`adb devices`), and an optional `health` command. A job that needs such
//! a resource gets one free, healthy instance, handed to it as `CALMRUN_<RESOURCE>` (and
//! `ANDROID_SERIAL` for `android` and `emulator`).
//!
//! The waiting owner runs the commands here, outside every lock, and publishes the ready
//! instances in its queue entry; the gate then picks from what the entry says, so every
//! waiter decides alike (see [`crate::gates`]).

use std::collections::BTreeMap;
use std::io::Read;
use std::process::Stdio;
use std::time::{Duration, Instant};

/// Instances of a named resource and how to find and check them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Pool {
    /// Fixed instances, handed out in this order.
    pub instances: Vec<String>,
    /// A command whose output lists instances, one per line (the first word of each).
    pub discover: Option<String>,
    /// A command that exits 0 when an instance is ready; `{instance}` is replaced by it.
    pub health: Option<String>,
    /// Another env var that gets the instance (default `ANDROID_SERIAL` for `android` and
    /// `emulator`).
    pub env: Option<String>,
}

/// A `[resources]` value: slots (`db = 1`), a list of instances, or a table.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(try_from = "toml::Value")]
pub enum Entry {
    Slots(u32),
    Pool(Pool),
}

impl TryFrom<toml::Value> for Entry {
    type Error = String;

    fn try_from(v: toml::Value) -> Result<Self, String> {
        let strings = |v: &toml::Value, what: &str| -> Result<Vec<String>, String> {
            let list = v.as_array().ok_or(format!("{what} must be a list of names"))?;
            list.iter()
                .map(|i| match i.as_str() {
                    Some(s) if valid_instance(s) => Ok(s.to_string()),
                    _ => Err(format!(
                        "{what}: {i} is not a plain instance name (letters, digits, - _ . : @ / +)"
                    )),
                })
                .collect()
        };
        let text = |v: &toml::Value, what: &str| -> Result<String, String> {
            match v.as_str().map(str::trim) {
                Some(s) if !s.is_empty() => Ok(s.to_string()),
                _ => Err(format!("{what} must be a non-empty string")),
            }
        };
        let pool = match v {
            toml::Value::Integer(n) => {
                return u32::try_from(n)
                    .map(Entry::Slots)
                    .map_err(|_| format!("{n} is not a number of slots"))
            }
            toml::Value::Array(_) => Pool {
                instances: strings(&v, "the instance list")?,
                ..Pool::default()
            },
            toml::Value::Table(t) => {
                let mut p = Pool::default();
                for (k, v) in &t {
                    match k.as_str() {
                        "instances" => p.instances = strings(v, "instances")?,
                        "discover" => p.discover = Some(text(v, "discover")?),
                        "health" => p.health = Some(text(v, "health")?),
                        "env" => {
                            let name = text(v, "env")?;
                            if !valid_env_name(&name) {
                                return Err(format!("env: {name:?} is not a variable name"));
                            }
                            p.env = Some(name);
                        }
                        other => {
                            return Err(format!(
                                "unknown key {other:?} (expected instances, discover, health, env)"
                            ))
                        }
                    }
                }
                p
            }
            other => {
                return Err(format!(
                    "expected a number of slots, a list of instances or a table, got {}",
                    other.type_str()
                ))
            }
        };
        if pool.instances.is_empty() && pool.discover.is_none() {
            return Err("list at least one instance, or a discover command".into());
        }
        Ok(Entry::Pool(pool))
    }
}

/// Whether `s` may be an instance: it goes into a shell line, so only plain characters.
pub fn valid_instance(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && !s.starts_with('-')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '@' | '/' | '+'))
}

fn valid_env_name(s: &str) -> bool {
    s.chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Device states in `adb devices` output that are not ready for a job.
const NOT_READY: &[&str] = &[
    "offline",
    "unauthorized",
    "authorizing",
    "connecting",
    "disconnected",
    "recovery",
    "rescue",
    "sideload",
    "bootloader",
    "host",
    "no",
    "unknown",
];

/// Instances in a discover command's output: the first word of each line. Blank lines, `#`
/// comments, `*` notes and the `List of devices attached` header are skipped, and so is a
/// line whose second word is a device state that is not ready (`offline`, `unauthorized`).
pub fn parse_list(out: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for line in out.lines().map(str::trim) {
        if line.is_empty() || line.starts_with(['#', '*']) || line.starts_with("List of devices") {
            continue;
        }
        let mut words = line.split_whitespace();
        let Some(first) = words.next() else { continue };
        if words.next().is_some_and(|state| NOT_READY.contains(&state)) {
            continue;
        }
        if valid_instance(first) && !found.iter().any(|f| f == first) {
            found.push(first.to_string());
        }
    }
    found
}

/// The generic variable for a resource: `CALMRUN_ANDROID`, `CALMRUN_SIM_IOS` for `sim.ios`.
/// `None` when that name is one calmrun reads itself (a resource called `home`).
pub fn generic_var(resource: &str) -> Option<String> {
    let name: String = resource
        .chars()
        .map(|c| match c.is_ascii_alphanumeric() {
            true => c.to_ascii_uppercase(),
            false => '_',
        })
        .collect();
    let var = format!("CALMRUN_{name}");
    (!RESERVED.contains(&var.as_str())).then_some(var)
}

/// Variables calmrun itself reads: a resource must not set them for its job.
const RESERVED: &[&str] = &[
    "CALMRUN_HOME",
    "CALMRUN_CONFIG",
    "CALMRUN_LEASE",
    "CALMRUN_HELD",
    "CALMRUN_BUDGET",
    "CALMRUN_RESOURCES",
    "CALMRUN_LOG",
    "CALMRUN_LOW_CPU",
    "CALMRUN_INSTANCE",
];

/// The variable a resource's instance goes to besides the generic one.
pub fn extra_var(resource: &str, pool: Option<&Pool>) -> Option<String> {
    pool.and_then(|p| p.env.clone()).or_else(|| match resource {
        "android" | "emulator" => Some("ANDROID_SERIAL".into()),
        _ => None,
    })
}

/// The environment a job gets for its instances.
pub fn env_for(
    instances: &BTreeMap<String, String>,
    pools: &BTreeMap<String, Pool>,
) -> Vec<(String, String)> {
    let mut env = Vec::new();
    for (res, inst) in instances {
        if let Some(v) = generic_var(res) {
            env.push((v, inst.clone()));
        }
        if let Some(v) = extra_var(res, pools.get(res)) {
            env.push((v, inst.clone()));
        }
    }
    env
}

/// `android=emulator-5554,db`: the resources with the instance each got.
pub fn describe(resources: &[String], instances: &BTreeMap<String, String>) -> String {
    resources
        .iter()
        .map(|r| match instances.get(r) {
            Some(i) => format!("{r}={i}"),
            None => r.clone(),
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// A probe command gets this long before it counts as failed.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// Ready instances are looked up again after this long while a job waits.
pub const REFRESH: Duration = Duration::from_secs(5);

/// Run a shell line (no stdin, no stderr) for at most `timeout`. Returns whether it exited
/// 0 and its output; a command that cannot start or runs too long fails.
pub fn run_probe(line: &str, instance: Option<&str>, timeout: Duration) -> (bool, String) {
    let line = match instance {
        Some(i) => line.replace("{instance}", i),
        None => line.to_string(),
    };
    let mut cmd = crate::runner::shell_command(&line);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(i) = instance {
        cmd.env("CALMRUN_INSTANCE", i);
    }
    let Ok(mut child) = cmd.spawn() else {
        return (false, String::new());
    };
    let mut stdout = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        if let Some(s) = stdout.as_mut() {
            let _ = s.read_to_end(&mut out);
        }
        String::from_utf8_lossy(&out).to_string()
    });
    let end = Instant::now() + timeout;
    let ok = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) if Instant::now() < end => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
        }
    };
    // A grandchild may keep the pipe open after a kill: do not wait on it then.
    let out = match ok {
        true => reader.join().unwrap_or_default(),
        false => String::new(),
    };
    (ok, out)
}

/// The instances of `pool` that are ready now: the fixed ones, then the discovered ones not
/// already listed, each kept only if its health command passes (checked in parallel).
pub fn ready_now(pool: &Pool) -> Vec<String> {
    let mut all = pool.instances.clone();
    if let Some(d) = &pool.discover {
        let (ok, out) = run_probe(d, None, PROBE_TIMEOUT);
        if ok {
            for i in parse_list(&out) {
                if !all.contains(&i) {
                    all.push(i);
                }
            }
        }
    }
    let Some(health) = &pool.health else {
        return all;
    };
    let passed: Vec<bool> = std::thread::scope(|scope| {
        let checks: Vec<_> = all
            .iter()
            .map(|i| scope.spawn(move || run_probe(health, Some(i), PROBE_TIMEOUT).0))
            .collect();
        checks.into_iter().map(|c| c.join().unwrap_or(false)).collect()
    });
    all.into_iter()
        .zip(passed)
        .filter(|(_, ok)| *ok)
        .map(|(i, _)| i)
        .collect()
}

/// A waiter's view of its instanced resources, refreshed every [`REFRESH`].
#[derive(Debug, Default)]
pub struct Probe {
    cache: BTreeMap<String, (Instant, Vec<String>)>,
}

impl Probe {
    /// The ready instances of each of `needs` that has a pool (other needs are left out).
    pub fn ready(
        &mut self,
        needs: &[String],
        pools: &BTreeMap<String, Pool>,
    ) -> BTreeMap<String, Vec<String>> {
        let mut ready = BTreeMap::new();
        for n in needs {
            let Some(pool) = pools.get(n) else { continue };
            let fresh = self.cache.get(n).is_some_and(|(at, _)| at.elapsed() < REFRESH);
            if !fresh {
                self.cache.insert(n.clone(), (Instant::now(), ready_now(pool)));
            }
            ready.insert(n.clone(), self.cache[n].1.clone());
        }
        ready
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(toml_value: &str) -> Result<Entry, String> {
        #[derive(serde::Deserialize)]
        struct W {
            #[allow(dead_code)]
            r: Entry,
        }
        toml::from_str::<W>(&format!("r = {toml_value}"))
            .map(|w| w.r)
            .map_err(|e| e.to_string())
    }

    #[test]
    fn entries_are_slots_lists_or_tables() {
        assert_eq!(entry("2"), Ok(Entry::Slots(2)));
        let Ok(Entry::Pool(p)) = entry(r#"["emulator-5554", "emulator-5556"]"#) else {
            panic!("a list is a pool")
        };
        assert_eq!(p.instances, ["emulator-5554", "emulator-5556"]);
        let Ok(Entry::Pool(p)) =
            entry(r#"{ discover = "adb devices", health = "adb -s {instance} get-state", env = "SERIAL" }"#)
        else {
            panic!("a table is a pool")
        };
        assert_eq!(p.discover.as_deref(), Some("adb devices"));
        assert_eq!(p.health.as_deref(), Some("adb -s {instance} get-state"));
        assert_eq!(p.env.as_deref(), Some("SERIAL"));
        for bad in [
            "-1",
            "[]",
            r#"["a b"]"#,
            r#"["x;rm"]"#,
            r#"{ health = "true" }"#,
            r#"{ instances = ["a"], colour = "red" }"#,
            r#"{ instances = ["a"], env = "1BAD" }"#,
            r#""text""#,
        ] {
            assert!(entry(bad).is_err(), "{bad} should be refused");
        }
    }

    #[test]
    fn adb_devices_output_is_parsed() {
        // Captured from `adb devices` (CRLF on Windows), plus the states that are not ready.
        let out = "* daemon not running; starting now at tcp:5037\r\n\
                   * daemon started successfully\r\n\
                   List of devices attached\r\n\
                   emulator-5554\tdevice\r\n\
                   emulator-5556\toffline\r\n\
                   R58M123ABC\tunauthorized\r\n\
                   192.168.1.20:5555\tdevice\r\n\
                   0123456789\tno permissions (user in plugdev group)\r\n\
                   \r\n";
        assert_eq!(parse_list(out), ["emulator-5554", "192.168.1.20:5555"]);
        // `adb devices -l` keeps the state second.
        assert_eq!(
            parse_list("emulator-5554 device product:sdk model:x transport_id:1\n"),
            ["emulator-5554"]
        );
        // A plain list, one per line, with duplicates and junk.
        assert_eq!(parse_list("a\n# c\nb\na\n$(x)\n"), ["a", "b"]);
    }

    #[test]
    fn env_names() {
        assert_eq!(generic_var("android").as_deref(), Some("CALMRUN_ANDROID"));
        assert_eq!(generic_var("sim.ios-2").as_deref(), Some("CALMRUN_SIM_IOS_2"));
        assert_eq!(generic_var("home"), None, "calmrun reads CALMRUN_HOME itself");
        assert_eq!(extra_var("android", None).as_deref(), Some("ANDROID_SERIAL"));
        assert_eq!(extra_var("emulator", None).as_deref(), Some("ANDROID_SERIAL"));
        assert_eq!(extra_var("gpu", None), None);
        let p = Pool {
            env: Some("UDID".into()),
            ..Pool::default()
        };
        assert_eq!(extra_var("android", Some(&p)).as_deref(), Some("UDID"));
        let inst = BTreeMap::from([("android".to_string(), "emulator-5556".to_string())]);
        assert_eq!(
            env_for(&inst, &BTreeMap::new()),
            [
                ("CALMRUN_ANDROID".to_string(), "emulator-5556".to_string()),
                ("ANDROID_SERIAL".to_string(), "emulator-5556".to_string())
            ]
        );
        assert_eq!(
            describe(&["android".into(), "db".into()], &inst),
            "android=emulator-5556,db"
        );
    }

    #[test]
    fn static_pools_need_no_commands() {
        let p = Pool {
            instances: vec!["a".into(), "b".into()],
            ..Pool::default()
        };
        assert_eq!(ready_now(&p), ["a", "b"]);
        let pools = BTreeMap::from([("dev".to_string(), p)]);
        let mut probe = Probe::default();
        let ready = probe.ready(&["db".into(), "dev".into()], &pools);
        assert_eq!(ready.len(), 1, "only needs with a pool: {ready:?}");
        assert_eq!(ready["dev"], ["a", "b"]);
    }

    #[test]
    fn probes_report_exit_and_output() {
        // Shell builtins that sh and cmd both have.
        let (ok, out) = run_probe("echo {instance}", Some("dev-1"), PROBE_TIMEOUT);
        assert!(ok);
        assert_eq!(out.trim(), "dev-1");
        assert!(!run_probe("exit 3", None, PROBE_TIMEOUT).0);
        assert!(!run_probe("no-such-command-calmrun-xyz", None, PROBE_TIMEOUT).0);
    }
}
