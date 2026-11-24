# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.5.0] - 2026-11-24

### Added

- A hook for the [Pi coding agent](https://github.com/earendil-works/pi): `calmrun hook pi`.
  `calmrun setup --write --hook pi` writes a small Pi extension,
  `~/.pi/agent/extensions/calmrun.js` (under `$PI_CODING_AGENT_DIR` when that is set), that asks
  calmrun before Pi's `bash` tool (or the optional `powershell` tool on Windows) runs a command.
  Heavy commands are blocked with the calmrun command to run instead, or changed in place with
  `CALMRUN_HOOK_MODE=rewrite`; if calmrun cannot be asked, the command runs unchanged. The wait
  cap is added only when the call sets a `timeout`. `--hook all` now includes Pi.
- `calmrun setup --agents pi` adds the calmrun block to `~/.pi/agent/AGENTS.md`. Pi reads only
  one instructions file per folder, so when that folder has a `CLAUDE.md` and no `AGENTS.md`,
  the block goes into `CLAUDE.md`; an `AGENTS.override.md` is reported, as for Codex.

## [0.4.0] - 2026-11-24

### Added

- `calmrun bump` picks jobs by `--pos N` (the `#` column of `calmrun status`), `--ticket ID`
  (job id, also a place kept by `--wait-timeout`) or `--dir PATH` (jobs in that folder or
  below), besides a label substring; exactly one. The MCP `bump` tool takes `label`, `pos`,
  `ticket` or `dir` (exactly one; `label` is no longer required) and `back`.
- `--prio low` (MCP `run_task` `prio: "low"`): after every normal and high job, also ones that
  come later. `calmrun bump --back <selector>` moves waiting jobs to the end with prio low.
- `calmrun status` marks places with `^` (bumped) and `-` (prio low) next to `!` (high);
  `status --json` and MCP `queue_status` show `low` and `bumped` per job.
- Working or hung? A running job's owner now adds up the CPU time of its whole process tree
  with the memory sample (Windows, Linux, macOS). `calmrun status` shows the recent CPU use,
  `idle 5m` once the tree has used under 2% of one core for 2 minutes, the usual run time
  from the history (`usually 40s`, median of the newest successful runs of the label in the
  same folder, else anywhere) and `longer than usual`. Idle is a hint only, never a stop.
- The waiting line names these hints for the running jobs (`running: npx vitest in lane
  (12m00s, usually 40s, idle 5m)`) and, when one looks stuck, a `calmrun: next:` line with
  `calmrun status` and `calmrun stop <id>`. `status --json`, MCP `queue_status` and
  `task_status` carry the same data (`cpu_pct`, `idle_ms`, `usual_ms`, `timeout_ms`, ...).
- Run time limit: `--timeout SECS` (0 = no limit for this run; MCP `timeout_s`, same 0),
  `max_run_min` in the config (env `CALMRUN_MAX_RUN_MIN`) and `[kinds.<name>] max_run_min`.
  By default test, vitest, jest, pytest, typecheck, tsc, lint and eslint jobs stop after
  60 minutes; builds and the other kinds have no limit. Exit 124 (history `timeout`); the
  message names the setting.
- `calmrun stop <id>` / `calmrun stop --label TEXT`: the job's calmrun stops its command tree
  (or leaves the line), prints `calmrun: stopped by calmrun stop` and exits 130 (history
  `stopped`). An owner that does not react within `--wait` seconds (default 10), such as a
  0.3 calmrun, is killed with its tree after a pid and start time check; one that runs other
  jobs too (an MCP server) keeps running and only the job's command tree is killed. CLI only,
  no MCP tool.
- `calmrun status` has an `id` column for running and waiting jobs.
- History lines carry `cwd` (the folder as a path key).
- `calmrun --log -- <cmd>` copies the command's stdout and stderr to the terminal and to a log
  file under `logs/` in the state directory (same names and retention as MCP task logs). stdin,
  the exit code and Ctrl+C work as before. The command writes to a pipe then, so many tools
  drop colours and progress bars; that is why it is opt-in. `log = true` in the config (or
  `CALMRUN_LOG=1`) turns it on for every run, `--no-log` off for one.
- `calmrun logs <id> [--follow] [--tail N]` prints the log of a running or recently finished
  job (by the `id` column of `calmrun status`, or `--label TEXT` matching one job, else exit 2
  with the candidates); `--follow` streams new output and exits 0 when the job ends.
- `status` shows a running job's log and when it last printed (`last output 6m10s ago`; in
  JSON `log_path`, `output_ms`, `output_ago_ms`), for MCP tasks too. Waiting lines name
  `calmrun logs <id>` for a running job with a log. History lines record the job id and log.

### Changed

- A job resumed with `--ticket` keeps whether it was bumped or pushed back.
- A config file with `max_run_min` (or `log`) is an unknown key to calmrun 0.3.x, which then
  ignores the whole file: restart long-lived 0.3 processes (`calmrun mcp` servers) after
  upgrading.

### Fixed

- A bumped job holds the front: while it waits for budget points or RAM, later jobs no longer
  pass it (before, a smaller job that came later could start first and delay it). Waiters
  behind it say so: `bumped job "npx vitest" ahead (1st in line) to start`. While it waits for
  an exclusive key, a resource or disk space, others may still pass it. A 0.3.x calmrun
  waiting in the same queue does not know this rule and may still pass it.
- Linux: the memory ceiling counted every thread of a process as a process of its own, with
  the whole process's memory, so a multi-threaded tree looked many times bigger than it was.

## [0.3.1] - 2026-11-24

### Fixed

- The one-line MCP result summary uses plain ASCII separators (`exit code 0, npx vitest, kind test,
  ...`) like the CLI messages, so it shows correctly in terminals without UTF-8.
- Antigravity: the sandbox refusal and the README no longer suggest a `command(...)` allow rule,
  which only skips the prompt and keeps the command inside the sandbox. They point to a mode
  other than `auto` (such as `--mode accept-edits`), tested with agy 1.5.15 on Windows.

## [0.3.0] - 2026-11-23

### Added

- Hooks for Codex (`calmrun hook codex`), Antigravity (`calmrun hook antigravity`) and OpenCode
  (`calmrun hook opencode`, through a small plugin), with the same decision as the Claude Code
  hook: deny with the wrapped command, or rewrite it with `CALMRUN_HOOK_MODE=rewrite`.
- `calmrun setup --hook claude,codex,antigravity,opencode` (or `--hook all`) installs them in
  each agent's user-level file, keeping other tools' hooks there; bare `--hook` still means
  Claude Code.
- Suggested commands carry a `--wait-timeout` just below the agent's shell time limit (Claude
  Code, OpenCode), so a long wait ends with exit 75 and a kept place instead of a killed call.
  Deny texts show the queue's load and, for long kinds, the background form; Claude Code's
  rewrite mode runs long kinds with `run_in_background`.
- The hooks find heavy commands inside `if`/`for`/`while` blocks, PowerShell `if ($?) { … }`
  and `try { … }` blocks, `$( … )`, subshells, `bash -c "…"` / `pwsh -Command '…'` scripts and
  after `timeout`. `npm --prefix web test` (and `-w`, `pnpm -C`, `--filter`) and `node --test`
  are detected; `node --watch` is refused with its one-shot form.
- MCP tasks outlive tool timeouts: `run_task` takes `wait_seconds` (default 25). A job that has
  not ended by then keeps its place and goes on in the server; the call answers at once with a
  `task_id`, its state (`waiting` with place and points, or `running`) and the output so far.
- MCP tools `task_status` (state, exit code when done, wait and run time, new output since
  `output_offset`, at most 32 KB, optional `wait_seconds`) and `cancel_task` (leaves the queue
  or stops the whole process tree).
- `calmrun hold <resource>[,…]` takes resource slots without running a command (for long
  device or emulator claims): it waits like `--needs`, prints a lease id and exits. Leases
  lapse after `--ttl` (default 4 h), take no points unless `--cost` is given, and are extended
  with `hold --renew <id>`. `calmrun release <id>` gives them back. `status` and MCP
  `queue_status` list them. `--wait-timeout 0` tries once.
- A lease does not block its holder: `hold --for <dir>` lets jobs in that folder use the leased
  resources, and `CALMRUN_LEASE=<id>[,…]` does the same for a job anywhere.
- `status`, `status --json` and MCP `queue_status` say why each waiting job waits (`why`,
  `why_code`: budget, order, ram, disk, key, resource).
- `calmrun release --ticket <id>` gives up a place kept by `--wait-timeout`.
- `setup --hook codex` adds calmrun's state directory to Codex's
  `sandbox_workspace_write.writable_roots` (adding lines only), or prints the lines to add.
- `rtk <tool> …` is seen through like `npx`: `rtk cargo test`, `rtk vitest`, `rtk npm test`,
  `rtk tsc` are queued and the hooks suggest `calmrun -- rtk cargo test`.
- A calmrun killed hard leaves a history line (`killed` or `abandoned`), written by the next
  calmrun that finds it gone; Ctrl+C while waiting records `cancelled`. `stats` shows waits that
  gave up before starting.

### Changed

- The `next:` line of a refused watch mode suggests the same one-shot form as the hooks
  (`npx vitest --watch` -> `npx vitest run`).
- `next:` lines and the hook commands `setup` writes name calmrun's absolute path when
  `calmrun` is not on `PATH`.
- A job waiting only for a resource that a lease holds never stops later jobs from passing.
- MCP results carry calmrun's notes (an expired ticket that joined the end of the line), a
  refusal names the `run_task` call to make instead of the CLI form, and the summary line drops
  empty fields and a status that only repeats the exit code.
- Resource names (`--needs`, `hold`) must be plain names (letters, digits, `-`, `_`, `.`);
  others exit 2.
- Exclusive keys and the RAM and disk gates are machine-wide: a job in an `--id` queue waits for
  a `node_modules` or `target` used in another queue, and the RAM check applies while anything
  runs in any queue. Budget and order stay per queue.
- A queue decides with the budget, `min_free_gb` and pass limit of the job that joined it while
  it was empty; a caller with other settings gets a note and `status` warns.
- Defaults: `min_free_gb` is max(1.5, RAM / 10) (was max(3, RAM / 8)) and the budget is capped
  at floor(RAM GB / 2.5) (was / 4), so a 16 GB machine runs jobs side by side.
- The RAM waiting line shows free RAM as read and what is held back for jobs that just started.
- `doctor` prints the suggested budget in its `setx`/`export` lines and warns when free RAM is
  below `min_free_gb`.
- `--id` may come before the subcommand (`calmrun --id q status`).
- CLI messages are ASCII, as Windows PowerShell 5.1 garbles an em dash in pipes and files.
- `history.jsonl` rotates to `history.1.jsonl` past 4 MB; queues left empty for 7 days are
  removed.
- Codex (Windows) and Antigravity (Windows) run commands in PowerShell: a quoted calmrun path is
  called with `&`.
- Antigravity's hook is experimental: its terminal sandbox hides other processes and blocks the
  state folder, so `setup --hook all` leaves it out, and calmrun refuses to use the queue from
  inside a Windows AppContainer instead of dropping jobs it cannot see.

## [0.2.0] - 2026-11-23

### Added

- Detection for builds and tests across ecosystems: C/C++ (make, cmake --build, ctest, ninja,
  meson, bazel, buck2, scons, xmake, compilers), JVM (gradle, maven, mvnd, sbt, ant), .NET
  (dotnet, msbuild), Go, Swift and xcodebuild, Flutter/Dart (incl. build_runner), Ruby,
  PHP (incl. composer test, artisan test), Elixir/Erlang, Haskell, OCaml, Zig, monorepo
  runners (turbo, nx, lerna, pnpm -r, rush, moon) and containers (podman, buildah).
- New kinds `monorepo` (cost 3), `typecheck` and `lint`; a single compiler call costs 1.
- Exclusive keys per project folder for these tools (Gradle settings root, `pom.xml`, `*.sln`,
  `Package.swift`, `pubspec.yaml`, …), so two builds of one project never share its output.
- Worker hints sized to the job's share of the cores for tools that use every core by default:
  `cmake --build --parallel`, `ninja -j`, `meson`, `bazel --jobs`, `buck2 -j`, `dotnet -m`,
  `go -p`, `golangci-lint --concurrency`, `swift -j`, `xcodebuild -jobs`, `flutter/dart test
  --concurrency`, `paratest --processes`, `mix test --max-cases`, `stack`/`cabal`/`dune`/`zig
  -j`, `mvnd -T`, monorepo runner concurrency, `PARALLEL_WORKERS`, `PARALLEL_TEST_PROCESSORS`,
  `MOON_CONCURRENCY`, `DENO_JOBS`, `NEXTEST_TEST_THREADS`, and `pytest -n auto` / `tox -p auto`
  turned into a number. nx, turbo and pnpm are never raised above their own defaults. A flag
  or variable you set yourself always wins. `calmrun doctor` lists them.
- Gradle `connected*` tasks also take the `emulator` resource.
- More watch modes refused with their one-shot form (`gradle --continuous`, `dotnet watch`,
  `turbo watch`, `mix test.watch`, `parcel watch`, `sbt ~`, `cargo watch`, …), and more dev
  servers and app runs started outside the queue (`turbo run dev`, `nx serve`, `make dev`,
  `bootRun`, `spring-boot:run`, `rails server`, `zig build run`, …).
- Launchers `bundle exec`, `dotnet tool run` and `xcrun` are seen through.

### Changed

- Releases publish the npm package from CI through npm trusted publishing (with provenance).
- Dev servers and other commands that run outside the queue keep their arguments unchanged,
  and the Claude Code hook no longer wraps them.
- The Claude Code hook follows the runner's command rules: a watch mode gets its one-shot form
  (`npx vitest` → `calmrun -- npx vitest run`, `docker compose up` → `… up -d`), and one
  without such a form (`sbt shell`, `cargo watch`) is left alone instead of being sent to a
  calmrun that refuses it.

## [0.1.0] - 2026-11-23

### Added

- `calmrun -- <command>`: a machine-wide queue for heavy commands, with a point budget, a
  free-RAM gate, first-come order with limited passing, `--prio high`, and exclusive keys per
  `node_modules` / Cargo `target` folder.
- Kind detection for vitest, jest, tsc, Playwright-style capture, docker, eslint-style linters,
  pytest, builds and test runners, with default costs and a `[kinds]` config table.
- `--maxWorkers` for direct vitest calls; refusal of attached `docker compose up`.
- Low CPU priority for the command (below-normal class on Windows, `nice +10` on Unix) and
  process-tree cleanup (Job Object on Windows, process group on Unix).
- `status`, `stats`, `bump`, `doctor`, `init` subcommands and `history.jsonl`.
- `calmrun hook claude`: a Claude Code PreToolUse hook that sends heavy shell commands
  through calmrun.
- `--wait-timeout SECS`: exit 75 when not started in time and keep the place in line for 10
  minutes; `--ticket <id>` resumes it. The `next:` line gives the exact command.
- `calmrun guide`, and `next:` hints on refusals and missing commands.
- Memory ceiling: a job whose process tree passes `ceiling_gb` (default half the RAM), or the
  biggest job when free RAM stays below `critical_free_gb`, is stopped with exit 137.
- Disk gate: a job waits while its working directory's drive has less than `min_free_disk_gb`
  (default 5) free; it starts anyway, with a warning, when nothing else runs.
- `CARGO_BUILD_JOBS` for cargo and `--max-workers` for gradle invoked directly, sized to the
  job's share of the cores.
- The waiting line names what is running: `running: npx vitest in web (3m10s), …`.
- `calmrun setup`: a dry-run-by-default writer for the calmrun block in the global instruction
  files of Claude Code, Codex, Gemini CLI and OpenCode (marker comments, re-runnable,
  `--remove`), plus `--hook` to merge the Claude Code hook into `settings.json` with a backup.
- Named resources: `--needs db` takes a slot of `db` (`[resources] db = 1`, `CALMRUN_RESOURCES`).
- Separate queues: `--id QUEUE` for running, `status` and `bump` (GNU sem style).
- Relative programs (`./gradlew`, `tool`) are looked up in the job's working directory, also for
  MCP tasks with a `cwd`.
- Endless commands (watch modes, attached `docker compose up`, interactive `docker run`) are
  refused with the one-shot form; dev servers and `--help` / `--version` run outside the queue.
- Safety: bare program names never come from the working directory on Windows; `setup` backs
  up every file it rewrites and refuses non-UTF-8 or unreadable files; a corrupt
  `state.json` is set aside, an unreadable one is an error instead of an empty queue.
- The RAM gate shares one reading per second across waiters and reserves memory for jobs
  that just started. Unix: the command dies with calmrun (Linux parent-death signal, macOS
  watchdog), Ctrl+Z suspends both, and `calmrun mcp` stops its tasks on SIGTERM.
- `npx tsc@5`-style package specs are detected; `status` shows places kept by a ticket; the
  hook suggests calmrun's absolute path when it is not on PATH.
- `calmrun mcp`: an MCP server over stdio with `run_task` (same queue as the CLI, captured
  log, last 200 lines, progress notifications, timeout, cancellation), `queue_status` and
  `bump`.
- Daemons a command leaves running on purpose (Gradle, Kotlin) survive a normal exit; the
  tree is ended only after Ctrl+C or when calmrun is killed.
- A command ended by Ctrl+C on Windows exits 130; Ctrl+C works even when the parent started
  calmrun with Ctrl+C disabled.
