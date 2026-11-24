//! Command-line interface.

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "calmrun",
    version,
    about = "A machine-wide queue for heavy commands, so parallel agents and terminals don't freeze the computer",
    long_about = None,
    args_conflicts_with_subcommands = true,
    after_help = "Examples:\n  calmrun -- npx vitest run\n  calmrun --cost 3 --label e2e -- npx playwright test\n  calmrun status"
)]
/// The command line.
pub struct Cli {
    #[command(subcommand)]
    pub sub: Option<Sub>,

    /// Kind of job; detected from the command when omitted.
    #[arg(long, value_name = "K", help = crate::kinds::kind_help())]
    pub kind: Option<String>,

    /// Points this job takes from the budget (clamped to the budget).
    #[arg(long, value_name = "N")]
    pub cost: Option<u32>,

    /// Queue priority.
    #[arg(long, value_enum, value_name = "PRIO")]
    pub prio: Option<Prio>,

    /// Label shown in status and stats (default: tool + subcommand).
    #[arg(long, value_name = "L")]
    pub label: Option<String>,

    /// Also take a slot of this named resource (repeat or comma-separate: --needs db,gpu).
    #[arg(long, value_name = "NAME", value_delimiter = ',')]
    pub needs: Vec<String>,

    /// Use a separate queue with this id instead of the machine-wide one (like GNU sem --id).
    /// Also selects the queue for status and bump.
    #[arg(long, value_name = "QUEUE", global = true)]
    pub id: Option<String>,

    /// Give up waiting after this many seconds: exit 75 and keep the place in line for
    /// 10 minutes. calmrun prints the command that resumes it.
    #[arg(long, value_name = "SECS")]
    pub wait_timeout: Option<f64>,

    /// Resume the place in line kept by an earlier --wait-timeout run.
    #[arg(long, value_name = "ID")]
    pub ticket: Option<u64>,

    /// Stop the command once it has run this many seconds (exit 124). 0 = no limit for this
    /// run. Default: max_run_min from the config (60 min for test-like kinds, else none).
    #[arg(long, value_name = "SECS")]
    pub timeout: Option<f64>,

    /// Also copy the command's output to a log file (see `calmrun logs`). The command then
    /// writes to a pipe instead of the terminal, so many tools drop colours and progress bars.
    #[arg(long, conflicts_with = "no_log")]
    pub log: bool,

    /// Keep the terminal and write no log, even with `log = true` in the config.
    #[arg(long)]
    pub no_log: bool,

    /// The command to run. Separate words run directly; one quoted string runs in the shell.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, value_name = "COMMAND")]
    pub command: Vec<String>,
}

/// Let `--id` come before a subcommand too: `calmrun --id q status` is `calmrun status --id q`.
/// (A run's flags conflict with subcommands, so clap alone would take `status` for a command
/// to run.) Anything else is left as it is.
pub fn id_flag_first(args: Vec<std::ffi::OsString>) -> Vec<std::ffi::OsString> {
    use clap::CommandFactory;
    let text = |i: usize| args.get(i).and_then(|a| a.to_str());
    let mut i = 1;
    loop {
        match text(i) {
            Some("--id") if args.len() > i + 1 => i += 2,
            Some(a) if a.starts_with("--id=") => i += 1,
            _ => break,
        }
    }
    let is_sub = text(i).is_some_and(|w| Cli::command().get_subcommands().any(|s| s.get_name() == w));
    if i == 1 || !is_sub {
        return args;
    }
    let mut out = vec![args[0].clone(), args[i].clone()];
    out.extend(args[1..i].iter().cloned());
    out.extend(args[i + 1..].iter().cloned());
    out
}

/// Queue priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Prio {
    /// First come, first served (the default).
    Normal,
    /// Before every normal and low job.
    High,
    /// After every normal and high job, also ones that come later.
    Low,
}

/// `calmrun bump`: which waiting jobs to move, and where.
#[derive(Debug, clap::Args)]
#[command(
    group(clap::ArgGroup::new("select").required(true).multiple(false)
        .args(["label", "pos", "ticket", "dir"])),
    after_help = "Pick the jobs with exactly one of: a label substring, --pos, --ticket, --dir.\n\
        To the front, they go before every other waiter, and later jobs may not pass them while\n\
        they wait for points or RAM (they may while they wait for a dependency folder, a\n\
        resource or disk space). With --back they go to the end, with prio low.\n\n\
        Examples:\n  calmrun bump e2e\n  calmrun bump --pos 3\n  calmrun bump --dir ../web\n  \
        calmrun bump --back --ticket 42"
)]
pub struct BumpArgs {
    /// Part of the label to match (case-insensitive).
    pub label: Option<String>,
    /// The job at this place in the waiting line (the # column of `calmrun status`).
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u64).range(1..))]
    pub pos: Option<u64>,
    /// The job with this id (`status --json`, or the ticket a --wait-timeout run printed).
    #[arg(long, value_name = "ID")]
    pub ticket: Option<u64>,
    /// Jobs whose folder is this one or inside it.
    #[arg(long, value_name = "PATH")]
    pub dir: Option<std::path::PathBuf>,
    /// Move them to the end of the line instead, with prio low.
    #[arg(long)]
    pub back: bool,
}

impl BumpArgs {
    /// The one selector given (clap makes sure there is exactly one).
    pub fn selector(&self) -> crate::status::Selector {
        use crate::status::Selector;
        match (&self.label, self.pos, self.ticket, &self.dir) {
            (Some(l), ..) => Selector::Label(l.clone()),
            (_, Some(p), ..) => Selector::Pos(p as usize),
            (_, _, Some(t), _) => Selector::Ticket(t),
            (_, _, _, Some(d)) => Selector::dir(d),
            _ => Selector::Label(String::new()),
        }
    }
}

/// Subcommands.
#[derive(Debug, Subcommand)]
pub enum Sub {
    /// Show running and waiting jobs, the budget and free RAM.
    Status {
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Per label/kind: runs, failures, wait and run times.
    Stats {
        /// How many days back to look.
        #[arg(long, default_value_t = 7)]
        days: u32,
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Hold slots of named resources (a device, a database) without running a command:
    /// waits like --needs, then prints a lease id and exits.
    Hold(crate::lease::HoldArgs),
    /// Give back a lease taken with `calmrun hold`.
    Release(crate::lease::ReleaseArgs),
    /// Stop a running or waiting job (by its id from `calmrun status`, or --label): its
    /// calmrun stops the command tree and exits 130. An owner that does not react is killed.
    Stop(StopArgs),
    /// Print the output of a running (or recently finished) job that has a log: one run
    /// with --log or through MCP.
    Logs(LogsArgs),
    /// Move waiting jobs to the front of the queue (or with --back, to the end).
    Bump(BumpArgs),
    /// Show cores, RAM, disk, battery and the suggested config.
    Doctor,
    /// Hooks for coding agents.
    Hook {
        #[command(subcommand)]
        target: HookTarget,
    },
    /// Add calmrun instructions to coding agents' global instruction files (dry run unless
    /// --write).
    Setup {
        /// Apply the changes (default: only show them).
        #[arg(long)]
        write: bool,
        /// Take the calmrun block (and with --hook, the hook) out again.
        #[arg(long)]
        remove: bool,
        /// Also install the hook of these agents (claude, codex, antigravity, opencode, all;
        /// default claude), in their user-level files; other hooks there are kept.
        #[arg(long, value_name = "AGENTS", num_args = 0..=1, default_missing_value = "claude",
              value_delimiter = ',')]
        hook: Option<Vec<String>>,
        /// Which agents (claude, codex, gemini, opencode). Default: those whose folder exists.
        #[arg(long, value_delimiter = ',')]
        agents: Option<Vec<String>>,
        /// Home directory to use instead of yours (for tests and other users' profiles).
        #[arg(long, value_name = "DIR")]
        home: Option<std::path::PathBuf>,
    },
    /// Print short instructions for coding agents.
    Guide,
    /// Serve the queue to coding agents over MCP (JSON-RPC on stdin/stdout).
    Mcp,
    /// Write a commented default config file and print its path.
    Init {
        /// Overwrite an existing file.
        #[arg(long)]
        force: bool,
    },
    /// Test helper: sleep and log start/end times.
    #[command(name = "__probe", hide = true)]
    Probe(ProbeArgs),
    /// Internal (macOS): end a command's process group when its calmrun disappears.
    #[command(name = "__watchdog", hide = true)]
    Watchdog { parent: i32, pgid: i32 },
    /// Test helper: send Ctrl+C (Windows console event) or SIGINT (Unix) to a process.
    #[command(name = "__ctrl_c", hide = true)]
    CtrlC { pid: u32 },
}

/// Arguments of `calmrun logs`.
#[derive(Debug, Clone, clap::Args)]
pub struct LogsArgs {
    /// The job's id, as in `calmrun status` (`id` in --json).
    #[arg(
        value_name = "JOB",
        required_unless_present = "label",
        conflicts_with = "label"
    )]
    pub job: Option<u64>,
    /// Pick the job by part of its label (case-insensitive) instead; it must match one job.
    #[arg(long, value_name = "TEXT")]
    pub label: Option<String>,
    /// Keep printing new output until the job ends (or Ctrl+C).
    #[arg(long, short = 'f')]
    pub follow: bool,
    /// How many of the last lines to print first.
    #[arg(long, value_name = "N", default_value_t = 50)]
    pub tail: usize,
}

/// Agents calmrun has a hook for.
#[derive(Debug, Subcommand)]
pub enum HookTarget {
    /// Claude Code PreToolUse hook: reads the hook JSON on stdin.
    Claude,
    /// Codex CLI PreToolUse hook: reads the hook JSON on stdin.
    Codex,
    /// Antigravity PreToolUse hook: reads the hook JSON on stdin.
    Antigravity,
    /// Behind calmrun's OpenCode plugin: reads the bash call as JSON on stdin.
    Opencode,
}

/// Arguments of `calmrun stop`.
#[derive(Debug, clap::Args)]
pub struct StopArgs {
    /// The job's id (the `id` column of `calmrun status`).
    #[arg(
        value_name = "JOB",
        required_unless_present = "label",
        conflicts_with = "label"
    )]
    pub job: Option<u64>,
    /// The job's exact label instead; it must match exactly one job.
    #[arg(long, value_name = "TEXT")]
    pub label: Option<String>,
    /// Seconds to wait for the job's calmrun to stop it before killing that calmrun and its
    /// process tree directly.
    #[arg(long, value_name = "SECS", default_value_t = 10.0)]
    pub wait: f64,
}

/// Arguments of the hidden test helper `__probe`.
#[derive(Debug, clap::Args)]
pub struct ProbeArgs {
    #[arg(long)]
    pub log: Option<std::path::PathBuf>,
    #[arg(long, default_value = "")]
    pub name: String,
    #[arg(long, default_value_t = 0)]
    pub sleep_ms: u64,
    #[arg(long, default_value_t = 0)]
    pub exit: i32,
    /// Print the received arguments as JSON.
    #[arg(long)]
    pub print_args: bool,
    /// Print this text to stderr.
    #[arg(long)]
    pub stderr: Option<String>,
    /// Print `tick <i>` to stdout this many times, `--tick-ms` apart.
    #[arg(long, default_value_t = 0)]
    pub ticks: u32,
    #[arg(long, default_value_t = 0)]
    pub tick_ms: u64,
    /// Copy stdin to stdout.
    #[arg(long)]
    pub cat: bool,
    /// Print the value of this environment variable (empty if unset).
    #[arg(long)]
    pub print_env: Option<String>,
    /// Allocate and touch this many MB before sleeping.
    #[arg(long, default_value_t = 0)]
    pub alloc_mb: usize,
    /// Keep one core busy for this many ms before sleeping.
    #[arg(long, default_value_t = 0)]
    pub burn_ms: u64,
    /// Start a detached helper that outlives this process and write its pid here.
    #[arg(long)]
    pub spawn_daemon: Option<std::path::PathBuf>,
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub rest: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> Cli {
        let args = line.split(' ').map(std::ffi::OsString::from).collect();
        Cli::try_parse_from(id_flag_first(args)).unwrap()
    }

    #[test]
    fn id_works_before_and_after_a_subcommand() {
        for line in [
            "calmrun --id q status",
            "calmrun status --id q",
            "calmrun --id=q status",
        ] {
            let c = parse(line);
            assert!(matches!(c.sub, Some(Sub::Status { .. })), "{line}");
            assert_eq!(c.id.as_deref(), Some("q"), "{line}");
        }
        let c = parse("calmrun --id q release --ticket 4");
        assert!(matches!(c.sub, Some(Sub::Release(_))));
        // A run keeps its flags and command.
        let c = parse("calmrun --id q -- status");
        assert!(c.sub.is_none());
        assert_eq!(c.command, vec!["status"]);
        let c = parse("calmrun --id q --cost 2 -- npm test");
        assert_eq!((c.id.as_deref(), c.cost), (Some("q"), Some(2)));
        assert_eq!(c.command, vec!["npm", "test"]);
    }

    fn try_bump(line: &str) -> Result<(crate::status::Selector, bool), clap::Error> {
        let args = line.split(' ').map(std::ffi::OsString::from).collect();
        match Cli::try_parse_from(id_flag_first(args))?.sub {
            Some(Sub::Bump(b)) => Ok((b.selector(), b.back)),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn bump_takes_exactly_one_selector() {
        use crate::status::Selector;
        assert_eq!(
            try_bump("calmrun bump e2e").unwrap(),
            (Selector::Label("e2e".into()), false)
        );
        assert_eq!(
            try_bump("calmrun bump --pos 3").unwrap(),
            (Selector::Pos(3), false)
        );
        assert_eq!(
            try_bump("calmrun --id q bump --back --ticket 42").unwrap(),
            (Selector::Ticket(42), true)
        );
        assert!(matches!(
            try_bump("calmrun bump --dir .").unwrap().0,
            Selector::Dir(_)
        ));
        for bad in [
            "calmrun bump",
            "calmrun bump --back",
            "calmrun bump e2e --pos 1",
            "calmrun bump --pos 1 --ticket 2",
            "calmrun bump --pos 0",
        ] {
            assert!(try_bump(bad).is_err(), "{bad}");
        }
        assert_eq!(parse("calmrun --prio low -- npm test").prio, Some(Prio::Low));
    }

    #[test]
    fn stop_takes_an_id_or_a_label_and_timeout_a_number() {
        let c = parse("calmrun --id q stop 12");
        assert!(matches!(c.sub, Some(Sub::Stop(StopArgs { job: Some(12), .. }))));
        assert_eq!(c.id.as_deref(), Some("q"));
        let c = parse("calmrun stop --label e2e --wait 2");
        let Some(Sub::Stop(a)) = c.sub else {
            panic!("not a stop")
        };
        assert_eq!((a.job, a.label.as_deref(), a.wait), (None, Some("e2e"), 2.0));
        let bad = |line: &str| {
            let args = line.split(' ').map(std::ffi::OsString::from).collect();
            Cli::try_parse_from(id_flag_first(args)).is_err()
        };
        assert!(bad("calmrun stop"));
        assert!(bad("calmrun stop 3 --label x"));
        let c = parse("calmrun --timeout 600 -- npx vitest run");
        assert_eq!(c.timeout, Some(600.0));
        assert_eq!(c.command, vec!["npx", "vitest", "run"]);
    }

    #[test]
    fn logs_takes_a_job_or_a_label() {
        let c = parse("calmrun --id q logs 5 --follow --tail 10");
        let Some(Sub::Logs(a)) = c.sub else { panic!() };
        assert_eq!(
            (a.job, a.follow, a.tail, c.id.as_deref()),
            (Some(5), true, 10, Some("q"))
        );
        let Some(Sub::Logs(a)) = parse("calmrun logs --label e2e").sub else {
            panic!()
        };
        assert_eq!((a.job, a.label.as_deref(), a.tail), (None, Some("e2e"), 50));
        let args = |l: &str| l.split(' ').map(std::ffi::OsString::from).collect::<Vec<_>>();
        assert!(Cli::try_parse_from(args("calmrun logs")).is_err());
        assert!(Cli::try_parse_from(args("calmrun logs 5 --label x")).is_err());
        let c = parse("calmrun --log -- npm test");
        assert!(c.log && c.sub.is_none());
        assert!(Cli::try_parse_from(args("calmrun --log --no-log -- npm test")).is_err());
    }
}
