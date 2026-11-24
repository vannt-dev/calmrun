//! calmrun: a machine-wide queue for heavy commands. See ARCHITECTURE.md for the map.

mod cli;
mod commands;
mod config;
mod detect;
mod doctor;
mod error;
mod gates;
mod history;
mod hook;
mod kinds;
mod lease;
mod mcp;
mod memwatch;
mod messages;
mod queue;
mod runner;
mod scheduler;
mod setup;
mod shell;
mod state;
mod status;
mod sys;
mod tasks;

use clap::Parser;
use cli::{Cli, Sub};
use queue::Ctx;

fn main() {
    let cli = Cli::parse_from(cli::id_flag_first(std::env::args_os().collect()));
    std::process::exit(dispatch(cli));
}

/// Load settings for the selected queue, or print why not.
fn load(queue: Option<String>) -> Result<Ctx, i32> {
    Ctx::load()
        .and_then(|c| c.with_queue(queue))
        .map_err(|e| commands::fail(&e))
}

fn dispatch(cli: Cli) -> i32 {
    let queue = cli.id.clone();
    let with_ctx = |f: &dyn Fn(Ctx) -> i32| load(queue.clone()).map_or_else(|code| code, f);
    match cli.sub {
        Some(Sub::Status { json }) => with_ctx(&|c| commands::status(c, json)),
        Some(Sub::Stats { days, json }) => commands::stats(days, json),
        Some(Sub::Bump(ref a)) => with_ctx(&|c| commands::bump(c, a)),
        Some(Sub::Hold(ref a)) => with_ctx(&|c| commands::hold(c, a)),
        Some(Sub::Release(ref a)) => with_ctx(&|c| commands::release(c, a)),
        Some(Sub::Doctor) => with_ctx(&commands::doctor),
        Some(Sub::Hook { ref target }) => commands::hook(target),
        Some(Sub::Init { force }) => commands::init(force),
        Some(Sub::Mcp) => mcp::serve(),
        Some(Sub::Setup {
            write,
            remove,
            ref hook,
            ref agents,
            ref home,
        }) => {
            let Some(home) = home.clone().or_else(dirs::home_dir) else {
                eprintln!("calmrun: cannot find your home directory; pass --home");
                return 1;
            };
            commands::setup(setup::Options {
                write,
                remove,
                hook: hook.clone().unwrap_or_default(),
                program: crate::hook::program_name(),
                agents: agents.clone(),
                home,
                state_dir: config::home_dir(),
            })
        }
        Some(Sub::Guide) => {
            print!("{}", commands::GUIDE);
            0
        }
        Some(Sub::Probe(ref a)) => commands::probe(a),
        Some(Sub::CtrlC { pid }) => commands::ctrl_c(pid),
        Some(Sub::Watchdog { parent, pgid }) => runner::watchdog(parent, pgid),
        None if cli.command.is_empty() => {
            use clap::CommandFactory;
            let _ = Cli::command().print_help();
            2
        }
        None => match load(queue) {
            Ok(ctx) => commands::run_queued(ctx, cli),
            Err(code) => code,
        },
    }
}
