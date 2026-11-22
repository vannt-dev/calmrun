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
pub struct Cli {
    #[command(subcommand)]
    pub sub: Option<Sub>,

    /// Kind of job (vitest, jest, tsc, capture, docker, eslint, pytest, build, test, or one from
    /// the config). Detected from the command when omitted.
    #[arg(long, value_name = "K")]
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

    /// The command to run. Separate words run directly; one quoted string runs in the shell.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, value_name = "COMMAND")]
    pub command: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Prio {
    Normal,
    High,
}

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
    /// Move waiting jobs whose label contains the text to the front of the queue.
    Bump {
        /// Part of the label to match (case-insensitive).
        label: String,
    },
    /// Show cores, RAM, disk, battery and the suggested config.
    Doctor,
    /// Hooks for coding agents.
    Hook {
        #[command(subcommand)]
        target: HookTarget,
    },
    /// Write a commented default config file and print its path.
    Init {
        /// Overwrite an existing file.
        #[arg(long)]
        force: bool,
    },
    /// Test helper: sleep and log start/end times.
    #[command(name = "__probe", hide = true)]
    Probe(ProbeArgs),
    /// Test helper: send Ctrl+C (Windows console event) or SIGINT (Unix) to a process.
    #[command(name = "__ctrl_c", hide = true)]
    CtrlC { pid: u32 },
}

#[derive(Debug, Subcommand)]
pub enum HookTarget {
    /// Claude Code PreToolUse hook: reads the hook JSON on stdin.
    Claude,
}

#[derive(Debug, clap::Args)]
pub struct ProbeArgs {
    #[arg(long)]
    pub log: Option<std::path::PathBuf>,
    #[arg(long, default_value = "")]
    pub id: String,
    #[arg(long, default_value_t = 0)]
    pub sleep_ms: u64,
    #[arg(long, default_value_t = 0)]
    pub exit: i32,
    /// Print the received arguments as JSON.
    #[arg(long)]
    pub print_args: bool,
    /// Start a detached helper that outlives this process and write its pid here.
    #[arg(long)]
    pub spawn_daemon: Option<std::path::PathBuf>,
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub rest: Vec<String>,
}
