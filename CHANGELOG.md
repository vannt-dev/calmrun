# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

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

### Changed

- A job waiting only for a resource that a lease holds never stops later jobs from passing.
- MCP results carry calmrun's notes (an expired ticket that joined the end of the line), a
  refusal names the `run_task` call to make instead of the CLI form, and the summary line drops
  empty fields and a status that only repeats the exit code.
- Resource names (`--needs`, `hold`) must be plain names (letters, digits, `-`, `_`, `.`);
  others exit 2.

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
