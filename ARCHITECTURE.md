# Architecture

calmrun is one binary with no daemon. Every `calmrun` process that wants to run something
adds an entry to a shared state file. It then polls until the scheduler, which every
process evaluates the same way, admits it. Only then does it run the command.

## Modules

| Module | Role |
| --- | --- |
| `main.rs` | Declares modules and dispatches the parsed CLI to `commands`. |
| `cli.rs` | The clap definition of flags and subcommands. |
| `commands.rs` | One function per CLI subcommand. Each returns the exit code. |
| `queue.rs` | **The service.** `queue::run(ctx, Request, Notify) -> Outcome` submits, waits, runs and records a job. The CLI and the MCP server both call it. |
| `scheduler.rs` | **Pure.** `schedule(jobs, inputs) -> Plan` handles queue order, prio, passing and the pass limit. |
| `gates.rs` | Start conditions: points, exclusive keys, named resources, disk, RAM. These are the `Gate` trait and its implementations. |
| `kinds.rs` | **The tool table.** Kinds with default costs and key rules, detection rules, and worker hints. |
| `detect.rs` | Finds the program in a command line (wrappers, launchers), the label, hint application, exclusive keys and refusals. |
| `shell.rs` | Just enough shell syntax for that: words, `&&`/`|`/`;` segments, heredocs and here-strings. |
| `state.rs` | `state.json` under an OS file lock (`state.lock`), atomic writes, pruning of dead entries. |
| `status.rs` | Snapshot, `status --json` / MCP `queue_status`, and `bump`. |
| `lease.rs` | `hold` / `release` arguments, lease ids, renew and release. Waiting for the slots is `queue::hold`. |
| `messages.rs` | Every waiting, refusal and timeout line, each ending with a `next:` step where there is one. |
| `memwatch.rs` | Process-tree memory and the critical free-RAM floor while a job runs. |
| `runner.rs` | Spawns the command: name lookup, priority, Job Object or process group, signals, exit codes. |
| `sys.rs` | Machine facts: RAM, cores, process liveness, disks, power, path normalisation. |
| `mcp.rs` | The MCP server: newline-delimited JSON-RPC on stdio and the tool registry. |
| `tasks.rs` | The MCP server's background tasks: a worker thread per `run_task` job, waited on with a timeout, followed by `task_status` / `cancel_task`. |
| `hook.rs` | The Claude Code PreToolUse hook (deny with a suggestion, or rewrite). |
| `setup.rs` | Writes and removes the instruction block in agents' global files, and merges the hook. |
| `config.rs` | Defaults derived from the machine, `config.toml`, and env overrides. |
| `history.rs` | `history.jsonl` and `stats`. |
| `doctor.rs` | The `doctor` report. |
| `error.rs` | The one error type. Every message says what to do next. |

All code that differs per OS lives in `runner.rs` (its private `imp` module) and `sys.rs`.

## Data flow of one run

```
CLI / MCP ──Request──▶ queue::run
   prepare      refusal check, detect kind/label/cost, apply hints (argv, env), keys
   nested?      CALMRUN_HELD alive → run at once (the outer run holds the points)
   enqueue      lock → prune dead → add entry (or take back a ticket) → unlock
   wait_turn    loop: lock → prune → publish disk_short → scheduler::schedule → unlock
                admitted → mark running; else Notify gets the waiting line;
                sleep with backoff (250 ms … 1.5 s), cancel checked every 100 ms;
                --wait-timeout → park the entry (ticket) and return 75
   execute      runner::run with a Watcher (cancel, timeout, memory) → leave the queue
   record       append to history.jsonl
```

A `calmrun hold` takes the same path with an entry marked `hold_ttl_ms` that runs nothing: in
the locked step that admits it, the entry becomes a `Lease` in `state.leases`, which has no
owner process and lapses at `until_ms`. The gates count leases' slots and points as taken.

The lock is held only to read, decide and write. calmrun never sleeps, spawns or waits for
the child while holding it. Each tick reads the state file once, and writes it back only
when the bytes changed.

## How to…

### Add a tool or a kind

Add one entry to `RULES` in `src/kinds.rs`:

```rust
rule(&["bazel"], When::Sub(&["build", "test"]), "build"),
```

`When` covers the usual shapes: always, a subcommand, `docker compose <sub>`, package
scripts, and "no subcommand or one of these". A command that never ends belongs in `ENDLESS`
(refused, with a `Fix` to its one-shot form); a dev server belongs in `LIGHT` (runs at once,
outside the queue). A brand-new kind also needs one entry in `KINDS` (name, default cost, exclusive-key
rule). The test `every_rule_names_a_known_kind` checks this. A rule can also set `key` (the
folder it must not share, e.g. `KeyRule::Project` with marker files), `cost` and `needs`. To
size the tool's workers, set `hints`: a `Hint::Arg` adds a flag when the tool is started
directly as words, a `Hint::Env` sets a variable, a `Hint::Replace` turns `-n auto` into a
number; each names what makes it stand back (`opt_out`, `opt_out_env`). Give a hint only to a
tool that already uses every core: turning parallelism on can break a build. Nothing outside
`kinds.rs` matches on tool names.

### Add a gate (a new start condition)

Implement `gates::Gate`:

```rust
struct Battery { on_battery: bool }
impl<'a> Gate<'a> for Battery {
    fn blocks(&self, w: &'a Job, active: bool) -> Option<Block> { … }
    fn admit(&mut self, w: &'a Job) {}
}
```

Then add it to `gates::standard`, in the position its reason should be reported. If users
should see a new kind of reason, add a `Block` variant and its line in `messages.rs`. The
scheduler loop does not change; `scheduler::schedule_with` takes any gate list, as the test
`extra_gates_plug_in_without_touching_the_loop` shows. Gates must be pure functions of the
snapshot: every waiter evaluates them independently and has to reach the same answer. A
fact that only one process can see, such as its own drive, is published into its entry
first (see `disk_short`).

### Add an MCP tool

Add a `Tool` to `TOOLS` in `src/mcp.rs`: a name, a schema function and a handler
`fn(&Call) -> Value`. Set `background: true` for anything that may wait. It then runs on its
own thread and can be cancelled. A job that may outlive its call goes through `Tasks::start`. Use `ctx_for(args)` to honour the `queue` argument. The
test `every_tool_has_a_matching_schema` checks the schemas.

### Add an agent to `setup`

Add an `AgentSpec` to `AGENTS` in `src/setup.rs`, with the folder, the env var that moves it,
the instruction file name, and any file that shadows it. Take the paths from the agent's own
documentation.

## Tests

Unit tests sit next to the code. The scheduler, gates, kinds, detect and shell tests are
pure. Integration tests in `tests/` drive the real binary against a temporary
`CALMRUN_HOME`:

- `queue.rs`: budget, keys, order, prio, bump, tickets, disk, resources, queues, Windows argument escaping.
- `signals.rs`: Ctrl+C and process-tree cleanup.
- `memory.rs`: the memory ceiling.
- `mcp.rs`: the JSON-RPC protocol, and tasks: handles, offsets, cancel, server exit.
- `lease.rs`: `hold` / `release`: lifecycle, expiry, contention with `--needs`, queues.
- `setup.rs`: setup against a temporary home.
- `review.rs`: regression tests for review findings (watch refusal, setup backups, tickets in
  `status`, the hook's absolute path, `npm.cmd` shadowing, a hard-killed calmrun).

The hidden `__probe` subcommand is the test workload. It sleeps, logs start and end times,
allocates memory, prints its arguments or environment, and spawns a daemon.
`__ctrl_c` delivers Ctrl+C. CI runs everything on Windows, macOS and Linux.
