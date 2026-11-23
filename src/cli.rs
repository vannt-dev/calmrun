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

    /// The command to run. Separate words run directly; one quoted string runs in the shell.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, value_name = "COMMAND")]
    pub command: Vec<String>,
}

/// Queue priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Prio {
    Normal,
    High,
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
    /// Print the value of this environment variable (empty if unset).
    #[arg(long)]
    pub print_env: Option<String>,
    /// Allocate and touch this many MB before sleeping.
    #[arg(long, default_value_t = 0)]
    pub alloc_mb: usize,
    /// Start a detached helper that outlives this process and write its pid here.
    #[arg(long)]
    pub spawn_daemon: Option<std::path::PathBuf>,
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub rest: Vec<String>,
}
