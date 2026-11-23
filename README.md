# calmrun

[![CI](https://github.com/vannt-dev/calmrun/actions/workflows/ci.yml/badge.svg)](https://github.com/vannt-dev/calmrun/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Release](https://img.shields.io/github/v/release/vannt-dev/calmrun)](https://github.com/vannt-dev/calmrun/releases/latest)
[![crates.io](https://img.shields.io/crates/v/calmrun)](https://crates.io/crates/calmrun)
[![npm](https://img.shields.io/npm/v/calmrun)](https://www.npmjs.com/package/calmrun)

A machine-wide queue for heavy commands, so several coding agents and terminals on one computer
don't run `vitest`, `tsc`, builds, Playwright and `docker compose` all at once and freeze it.

```sh
calmrun -- npx vitest run
```

calmrun waits until the machine has room (budget points, free RAM, free disk), then runs the
command at low CPU priority and passes its exit code back. There is no daemon: every `calmrun`
process coordinates through a small state file guarded by an OS file lock.

Works on Windows, macOS and Linux. Website: <https://vannt-dev.github.io/calmrun/>

**Contents:** [Why](#why) · [Install](#install) · [Quick start](#quick-start) ·
[How it decides](#how-it-decides) · [Commands](#commands) · [Config](#config) ·
[Coding agents](#coding-agents) · [Exit codes](#exit-codes) · [Platform notes](#platform-notes) ·
[Troubleshooting](#troubleshooting) · [Contributing](#contributing)

## Why

One test run is fine. Four agents each starting `vitest` with a worker per core, plus a `tsc`
and a build, is not: the machine swaps, the editor stalls, tests time out for reasons that have
nothing to do with the code, and every run gets slower than if they had taken turns. calmrun
makes them take turns:

- Jobs start only while budget points are free, and wait while free RAM or disk is low.
- Two test runs or builds on the same `node_modules` / Cargo `target` folder never run together.
- Direct vitest, gradle and cargo calls get a worker count that fits their share of the machine.
- Commands run at low CPU priority, so your editor and the agents stay responsive.
- A job whose process tree grows past a memory ceiling is stopped instead of taking the machine.
- A killed terminal does not leave orphaned workers: the command's tree dies with calmrun.

## Install

Prebuilt binaries for x64 and arm64 are attached to each
[GitHub release](https://github.com/vannt-dev/calmrun/releases/latest).

```sh
# macOS / Linux
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/vannt-dev/calmrun/releases/latest/download/calmrun-installer.sh | sh
```

```powershell
# Windows (PowerShell)
powershell -ExecutionPolicy Bypass -c "irm https://github.com/vannt-dev/calmrun/releases/latest/download/calmrun-installer.ps1 | iex"
```

The installers put `calmrun` in `$CARGO_HOME/bin` (by default `~/.cargo/bin`, or
`%USERPROFILE%\.cargo\bin` on Windows) and add that folder to `PATH`. Open a new terminal
afterwards.

```sh
# npm (any OS; downloads the matching binary)
npm install -g calmrun

# from crates.io (Rust 1.95 or newer)
cargo install calmrun --locked
```

**Manual download.** Each release has one archive per platform, each with a `.sha256` file:

| Platform | Archive |
| --- | --- |
| Windows x64 | `calmrun-x86_64-pc-windows-msvc.zip` |
| Windows arm64 | `calmrun-aarch64-pc-windows-msvc.zip` |
| macOS Intel | `calmrun-x86_64-apple-darwin.tar.xz` |
| macOS Apple silicon | `calmrun-aarch64-apple-darwin.tar.xz` |
| Linux x64 | `calmrun-x86_64-unknown-linux-gnu.tar.xz` |
| Linux arm64 | `calmrun-aarch64-unknown-linux-gnu.tar.xz` |

Unpack it and put `calmrun` (`calmrun.exe`) in a folder on `PATH`. Check with
`calmrun --version` and `calmrun doctor`.

## Quick start

```sh
calmrun doctor                   # what this machine has and the suggested budget
calmrun -- npx vitest run        # queue, then run at low priority
calmrun -- cargo build --release
calmrun --cost 3 --label e2e -- npx playwright test
calmrun "npm ci && npm test"     # one quoted string runs through the shell
calmrun status                   # what runs, what waits, and why
calmrun stats --days 30          # how long jobs waited and ran
calmrun bump e2e                 # move a waiting job to the front
calmrun init                     # write a commented config file
calmrun setup --write --hook     # tell your coding agents to use calmrun
```

While waiting, calmrun prints one line to stderr, then one every 30 seconds:

```
calmrun: waiting — 4 of 6 points in use, 2nd in line (needs 2); running: npx vitest in web (3m10s), cargo build in api (1m02s)
calmrun: waiting for RAM: 2.1 GB free, needs 3.0 GB; running: npx playwright in e2e (4m55s)
```

## How it decides

### Budget and kinds

The machine has `budget` points (default: logical cores − 2, capped at floor(RAM GB / 4), at
least 2). Each job costs points by kind; `--cost` overrides, and a cost above the budget counts
as the whole budget.

| Kind | Detected from | Default cost |
| --- | --- | --- |
| `vitest` | `vitest` | 2 |
| `jest` | `jest` | 2 |
| `tsc` | `tsc`, `vue-tsc`, `tsgo`, package scripts `typecheck` / `type-check` / `tsc` | 2 |
| `build` | `vite/next/ng build`, `webpack`, `rollup`, `esbuild`, package script `build`, and the build tools in the [table below](#build-tools) | 2 (1 for one compiler call: `gcc`, `clang`, `cl`, …) |
| `monorepo` | `turbo run`, `nx run-many/affected`, `lerna run`, `pnpm -r`, `rush build`, `moon run` | 3 |
| `capture` | `playwright`, `puppeteer`, `cypress` | 2 |
| `docker` | `docker compose up/build/run/create`, `docker-compose …` (same subcommands), `docker build/run/buildx`; the same for `podman` and `podman-compose`; `buildah build`, `packer build` | 2 |
| `typecheck` | `mypy`, `pyright`, `deno check`, `phpstan`, `psalm` | 2 |
| `eslint` | `eslint`, `oxlint`, `biome`, package script `lint` | 1 |
| `lint` | `ruff`, `pylint`, `flake8`, `go vet`, `golangci-lint run`, `rubocop` | 1 |
| `pytest` | `pytest`, `py.test`, `python -m pytest` | 1 |
| `test` | `npm/pnpm/yarn/bun test`, package script `test`, `mocha`, `tox`, `nox`, and the test runners in the [table below](#build-tools) | 1 |
| `other` | anything else | 1 |

"Package script" means `npm run <name>` / `npm run-script <name>` (also `<name>:suffix`, such as
`test:unit`), and `pnpm|yarn|bun <name>` directly. Detection looks at the command position
only: the program name (basename, without `.cmd/.bat/.exe/.ps1/.js`) after `VAR=value` prefixes
and launchers such as `npx`, `pnpx`, `bunx`, `pnpm exec`, `yarn exec`, `npm exec`, `uv run`,
`python -m`, `bundle exec`, `dotnet tool run`, `xcrun`. So `git commit -m "fix vitest"` is not a vitest run. `--kind` overrides detection;
`[kinds.<name>]` in the config changes a cost or adds a kind with its own patterns.

<a id="build-tools"></a>**Build tools.** What calmrun knows per tool beyond JavaScript and
Python. *Key* is the marker file of the project folder the job takes an exclusive key on
(see [Gates](#gates-when-a-job-starts)). *Hint* is the worker count calmrun passes, N being
the job's share of the machine (logical cores × cost ÷ budget, at least 1); in brackets, what
makes it leave the count alone because you already chose one.

| Tools | Kind | Key | Hint |
| --- | --- | --- | --- |
| `make`, `gmake`, `mingw32-make`, `nmake` | build | `Makefile` | none (serial unless you pass `-j`) |
| `cmake --build` | build | `CMakeCache.txt`, `CMakeLists.txt` | `--parallel N` (`-j`, `--parallel`, `CMAKE_BUILD_PARALLEL_LEVEL`) |
| `ctest` | test | as cmake | none (serial unless you pass `-j`) |
| `ninja` | build | `build.ninja` | `-jN` (`-j`) |
| `meson compile` / `meson test` | build / test | `meson.build` | `-j N` / `--num-processes N` (`MESON_TESTTHREADS`) |
| `bazel`, `bazelisk` `build` / `test`, `coverage` | build / test | `MODULE.bazel`, `WORKSPACE` | `--jobs=N` (`--jobs`, `-j`) |
| `buck2`, `buck` `build` / `test` | build / test | `.buckconfig` | `-jN` (`-j`, `--num-threads`) |
| `scons`, `xmake` | build | `SConstruct`, `xmake.lua` | none, `-j N` for xmake |
| `gcc`, `g++`, `clang`, `clang++`, `cl`, `cc` | build, cost 1 | none | none |
| `gradle`, `gradlew` | test for `test*`/`check*`/`connected*` tasks (`connected*` also takes the `emulator` resource), else build | `settings.gradle(.kts)` | `--max-workers=N` (`--max-workers`, `-Dorg.gradle.workers.max`) |
| `mvn`, `mvnw` / `mvnd` | test for `test`/`verify`, else build | `pom.xml` | none / `-T N` for mvnd (`-T`) |
| `sbt`, `ant` | test for `sbt test`/`testOnly`, else build | `build.sbt`, `build.xml` | none |
| `dotnet build`, `publish`, `pack`, `msbuild`, `restore` / `dotnet test` | build / test | `*.sln`, `*.slnx`, `*.csproj`, `*.fsproj`, … | `-m:N` (`-m`, `-maxcpucount`) |
| `msbuild` | build | as dotnet | none (serial unless you pass `-m`) |
| `go build`, `install` / `test` / `vet` | build / test / lint | none | `-p N` after the subcommand (`-p`, `GOFLAGS=-p…`) |
| `golangci-lint run` | lint | none | `--concurrency N` (`--concurrency`, `-j`) |
| `cargo build`, `check`, `clippy`, `doc` / `test`, `bench`, `nextest` | build / test | `target` folder | `CARGO_BUILD_JOBS=N`, also `NEXTEST_TEST_THREADS=N` for nextest |
| `maturin build`, `develop` | build | `target` folder | `CARGO_BUILD_JOBS=N` |
| `swift build` / `test` | build / test | `Package.swift` | `-j N` (`-j`, `--jobs`) |
| `xcodebuild` (also through `xcrun`) | test for test actions, else build | `*.xcworkspace`, `*.xcodeproj` | `-jobs N` (`-jobs`) |
| `flutter build` | build | `pubspec.yaml` | none |
| `flutter test`, `dart test` | test | `pubspec.yaml` | `--concurrency=N` (`--concurrency`, `-j`) |
| `dart compile`, `dart run build_runner` | build | `pubspec.yaml` | none |
| `rspec`, `cucumber`, `rake test`, `rake spec` | test | none | none |
| `rails test` | test | none | `PARALLEL_WORKERS=N` |
| `parallel_rspec`, `parallel_test` | test | none | `PARALLEL_TEST_PROCESSORS=N` |
| `phpunit`, `pest`, `composer test`, `php artisan test` | test | none | none |
| `paratest` | test | none | `--processes=N` (`-p`, `--processes`) |
| `mix test` / `mix compile`, `release` | test / build | `mix.exs` | `--max-cases N` (`--max-cases`) / none |
| `rebar3 eunit`, `ct` / `compile`, `release` | test / build | `rebar.config` | none |
| `stack build`, `install`, `test` | build / test | `stack.yaml` | `-j N` (`-j`, `--jobs`) |
| `cabal build`, `install`, `test` | build / test | `cabal.project`, `*.cabal` | `-jN` (`-j`, `--jobs`) |
| `dune build` / `test` | build / test | `dune-project` | `-j N` (`-j`) |
| `zig build` | build | `build.zig` | `-jN` (`-j`) |
| `turbo run` | monorepo | `node_modules` | `--concurrency=N`, at most 10, turbo's default (`--concurrency`) |
| `nx run-many`, `nx affected` | monorepo | `node_modules` | `--parallel=N`, at most 3, nx's default (`--parallel`) |
| `lerna run`, `exec` | monorepo | `node_modules` | `--concurrency N` (`--concurrency`) |
| `pnpm -r` (not `install`, `add`, `update`, …) | monorepo | `node_modules` | `--workspace-concurrency=N`, at most 4, pnpm's default (`--workspace-concurrency`) |
| `rush build`, `rebuild`, `test` | monorepo | `node_modules` | `--parallelism N` (`-p`, `--parallelism`) |
| `moon run`, `ci`, `check` | monorepo | `node_modules` | `MOON_CONCURRENCY=N` |
| `pytest -n auto`, `tox -p auto` | pytest / test | none, `tox.ini` | `auto` becomes N |
| `deno test`, `bench` | test | none | `DENO_JOBS=N` |
| `docker`, `podman`, `buildah`, `packer` builds | docker | none | none |

A variable in the Hint column is set only when it is not set already, and an argument is
added only when the tool is started directly as words (also after `VAR=value`, `npx`,
`bundle exec`, `xcrun`, …), never to package scripts or shell strings. `calmrun doctor` prints
the numbers for this machine. Tools that run one job at a time unless asked (make, ctest,
scons, plain Maven, msbuild without `-m`) get no hint on purpose: turning parallelism on can
break a build that was never written for it. Pass the flag yourself (`make -j8`) and calmrun
leaves it alone.

### Gates: when a job starts

A job starts when **all** of these hold:

- **Points.** Its cost fits in the free points of its queue.
- **RAM.** Free RAM is at least `min_free_gb` (default max(3, RAM / 8) GB; `0` disables). The
  check is skipped when nothing is running, so a job never waits forever on an idle machine.
  Waiters share one RAM reading per second, and memory is reserved for jobs that just started.
- **Disk.** The drive of its working directory has at least `min_free_disk_gb` free (default 5;
  `0` turns it off): `calmrun: waiting for disk space: 3.2 GB free on C:\, needs 5.0 GB`. When
  nothing else runs it starts anyway, with a warning.
- **Same folder, one at a time.** vitest, jest and JavaScript build jobs take an exclusive key
  for their nearest `node_modules` folder; Cargo builds and tests take one for their `target`
  folder (or `CARGO_TARGET_DIR`); other build tools take one for their project folder, the
  nearest folder holding the marker file in the [build tools table](#build-tools) (two Gradle
  runs in one build share `build/`, two `dotnet` runs share `obj/`). Two jobs with the same key
  never run together.
- **Named resources.** `--needs db` also takes one slot of the resource `db`. Configure slots
  with `[resources] db = 1` or `CALMRUN_RESOURCES="db=1,gpu=2"`; a resource that is not
  configured has one slot. Use it for things that cannot be shared: one test database, one
  emulator, one GPU. Waiters say who holds it:
  `calmrun: waiting for resource "db" (1 of 1 slot(s) held by npm test), 2nd in line`.

Points, keys and resource slots are taken all-or-nothing.

### Order and passing

`--prio high` first, then first come. A later job may pass earlier ones only if it fits right
now and no job ahead of it has waited longer than `pass_limit_min` (default 10; `0` = strict
order). Running jobs are never stopped to make room. `calmrun bump <label>` moves matching
waiting jobs to the front.

### Separate queues

`--id QUEUE` uses its own queue with its own budget, like GNU `sem --id` (for example one queue
per project, or one for long e2e runs). `status --id QUEUE` and `bump … --id QUEUE` act on it.
The default queue and named queues do not see each other's points; history and config are
shared. A queue id uses letters, digits, `-`, `_` and `.` (not first).

### Tickets: capped waits

`--wait-timeout SECS` caps the wait. If the job has not started by then, calmrun exits **75**
(EX_TEMPFAIL) and keeps its place in line for 10 minutes. It prints the exact command that
resumes that place, the same arguments with `--ticket <id>` in front:

```
calmrun: not started within 120s (waiting — 6 of 6 points in use, 2nd in line (needs 2)); your place in line is kept for 10 min.
calmrun: next: calmrun --ticket 17 --wait-timeout 120 -- npx vitest run
```

While the owner is away, the kept place does not block anyone; when the ticket comes back it
rejoins at its old position. An expired or unknown ticket joins the end of the line. `status`
shows places kept by a ticket.

### Command rules

- **Endless commands are refused** (exit 2), because they would hold their points forever.
  calmrun prints the one-shot form to run instead (`calmrun: next: …`):
  - `docker compose up` / `docker-compose up` without `-d`, `--detach` or `--wait` → add `-d`;
  - `tsc --watch` / `-w` (also `vue-tsc`, `tsgo`), `jest --watch` / `--watchAll`,
    `vitest --watch` / `-w` → without the flag;
  - `vitest watch` / `vitest dev`, and plain `vitest` when stdin is a terminal (it watches
    then) → `vitest run`;
  - `gradle --continuous` / `-t`, `webpack|rollup|esbuild|mocha --watch`,
    `vite build --watch`, `deno|bun --watch`, `stack --file-watch`, `dune -w`,
    `zig build --watch` → without the flag;
  - `dotnet watch <cmd>` → `dotnet <cmd>`; `turbo watch` → `turbo run`; `mix test.watch` →
    `mix test`; `parcel watch` → `parcel build`;
  - `docker run` / `docker exec` (also `podman`) with `-i`, `-t`, `--interactive` or `--tty`,
    bare `sbt` and `sbt shell|console`, `sbt ~task`, `cargo watch`, `nx watch`, `lerna watch`,
    `dart run build_runner watch|serve`, and watchers (`ibazel`, `nodemon`, `watchexec`, `ptw`,
    `bacon`, `air`, `guard`) → run it outside calmrun.
- **Light commands run at once**, outside the queue and with their arguments unchanged: dev
  servers and app runs (`vite`, `vite dev|serve|preview`, `next|nuxt|astro dev|start|preview`,
  `ng serve`, `webpack serve`, `npm run dev|start|serve|preview` and the pnpm/yarn/bun forms,
  `parcel <entry>`, `turbo run dev`, `nx serve`, `make dev|run|serve|start|watch`, `cargo run`,
  `go run`, `dotnet run`, `flutter run`, `swift run`, `zig build run`, `bazel run`, `sbt run`,
  `gradle bootRun`, `mvn spring-boot:run`, `rails server`, `mix phx.server`,
  `php artisan serve`, `symfony serve`, …), installs and infrastructure commands
  (`pod install`, `composer install`, `terraform plan|apply`, `pulumi up`), and commands that
  only print help, a version or project information (`--help`, `-h`, `--version`, `-V`,
  `xcodebuild -list`). The Claude Code hook leaves them alone.
- **Worker counts.** vitest started directly as separate words (`vitest …`,
  `npx|pnpx|bunx vitest …`, `pnpm|yarn exec vitest …`) gets `--maxWorkers=N` unless it already
  passes `--maxWorkers`, `--pool` or `--no-file-parallelism`; N is `vitest_workers` (default 1
  for up to 4 cores, 2 below 12, else 3). Build tools get the hint in the
  [build tools table](#build-tools). Cargo gets `CARGO_BUILD_JOBS=N` unless it is already set;
  a lower `build.jobs` in a cargo config (`.cargo/config.toml` up from the working directory,
  then `$CARGO_HOME/config.toml`) wins. `npm run …` scripts and shell strings are never
  changed.
- **Nesting.** calmrun sets `CALMRUN_HELD=<pid>` for the command. A calmrun started inside it
  (for example a script that calls calmrun again) runs at once instead of queuing behind its
  own parent.

### While it runs

- **Low priority.** Below normal priority on Windows, `nice +10` in its own process group on
  Unix. `CALMRUN_LOW_CPU=0` (or `false`, `no`, `off`) turns this off.
- **Memory ceiling.** About once a second calmrun adds up the memory of the job's whole process
  tree (including nested calmrun runs). If the tree goes above `ceiling_gb` (default half the
  RAM; `0` turns it off), or free RAM stays below `critical_free_gb` (default 5% of RAM, at most
  1 GB) for 3 seconds and this job's tree is the largest of the running jobs (on a tie, the
  newer one), calmrun stops the tree (gracefully, then forced) and exits **137**. A tree smaller
  than max(1 GB, 10% of RAM) is never stopped for low free RAM: calmrun warns once instead,
  since something outside the queue is using the memory.

  ```
  calmrun: stopped npx vitest run: its processes used 9.1 GB, above the 8.0 GB ceiling
  ```

- **Process tree.** When the command exits normally, processes it deliberately left running (a
  Gradle or Kotlin daemon, a dev server started in the background) are left alone. When the run
  ends abnormally (Ctrl+C, or calmrun itself being killed) the whole tree is ended.
- **History.** Each run appends one JSON line to `history.jsonl`: `at`, `label`, `kind`, `cost`,
  `wait_ms`, `run_ms`, `status` (`ceiling` or `low-memory` for memory stops). The label is
  `--label` or the tool and subcommand only (`npx vitest run`, `cargo build`), never the
  arguments, which may contain secrets.

## Commands

```
calmrun [--kind K] [--cost N] [--prio normal|high] [--label L] [--needs NAME,…] [--id QUEUE]
        [--wait-timeout SECS] [--ticket ID] -- <command...>
calmrun status [--json] [--id QUEUE]
calmrun stats [--days N] [--json]
calmrun bump <label-substring> [--id QUEUE]
calmrun doctor
calmrun init [--force]
calmrun guide
calmrun setup [--write] [--remove] [--hook] [--agents claude,codex,gemini,opencode] [--home DIR]
calmrun hook claude
calmrun mcp
```

### Running a command

| Flag | Meaning |
| --- | --- |
| `--kind K` | Kind of job (a built-in kind or one from the config); detected when omitted |
| `--cost N` | Points this job takes (clamped to the budget) |
| `--prio normal\|high` | Queue priority; `high` goes before every `normal` job |
| `--label L` | Name in `status` and `stats` (default: tool + subcommand) |
| `--needs NAME` | Also take a slot of this named resource; repeat or comma-separate (`--needs db,gpu`) |
| `--id QUEUE` | Use a separate queue instead of the machine-wide one |
| `--wait-timeout SECS` | Give up waiting after this long: exit 75, place kept for 10 minutes |
| `--ticket ID` | Resume the place kept by an earlier `--wait-timeout` run |

- Separate words (`calmrun -- npx vitest run`) run the program directly. On Windows calmrun
  looks the name up on `PATH` with `PATHEXT`, so `npm`, `npx` and other `.cmd` shims work, and
  arguments are escaped for `cmd.exe`. A bare name is never taken from the working directory
  (a repository's own `npm.cmd` cannot stand in for npm); `./gradlew` or `.\tool.cmd` with a
  path runs from the job's folder.
- One string containing spaces (`calmrun "a && b"`) runs through the shell: `cmd /d /s /c` on
  Windows, `sh -c` on Unix. calmrun never rewrites such a string.
- Ctrl+C reaches the command; a job still waiting leaves the queue and exits 130.

### Subcommands

| Command | What it does |
| --- | --- |
| `status [--json]` | Running and waiting jobs with the reason each one waits, the budget, free RAM and kept tickets. `--json` (with `mem_bytes` per running job) for tools. |
| `stats [--days N] [--json]` | Runs, failures, average and maximum wait, average run time per label and kind, over the last N days (default 7). |
| `bump <text>` | Move waiting jobs whose label contains the text (case-insensitive) to the front. |
| `doctor` | Cores, RAM, disk type, battery, the state directory and config file, the settings in effect and the suggested ones. |
| `init [--force]` | Write a commented `config.toml` with this machine's defaults and print its path; `--force` overwrites. |
| `guide` | Short instructions for coding agents, to paste into their context. |
| `setup` | Add or remove the calmrun block in agents' global instruction files; see [below](#calmrun-setup). |
| `hook claude` | The Claude Code PreToolUse hook; reads the hook JSON on stdin. |
| `mcp` | An MCP server on stdin/stdout with `run_task`, `queue_status` and `bump`. |

`--id QUEUE` is accepted by every subcommand; it matters for running, `status` and `bump`.

## Config

`calmrun init` writes a commented `config.toml` and prints its path. Every key is optional;
environment variables override the file.

```toml
budget = 6
min_free_gb = 4
pass_limit_min = 10
vitest_workers = 2
ceiling_gb = 8          # 0 = off
min_free_disk_gb = 5    # 0 = off
critical_free_gb = 0.8  # 0 = off

[resources]                      # named resources for --needs, with their slots
db = 1
emulator = 1

[kinds.vitest]
cost = 3                         # override a built-in cost

[kinds.heavy]                    # add a kind with command patterns (first words)
cost = 2
patterns = ["make", "bazel build"]
```

| Key | Variable | Default | Meaning |
| --- | --- | --- | --- |
| `budget` | `CALMRUN_BUDGET` | cores − 2, capped at floor(RAM GB / 4), at least 2 | Points that may run at once |
| `min_free_gb` | `CALMRUN_MIN_FREE_GB` | max(3, RAM / 8) | Free RAM needed to start while something runs; `0` = off |
| `min_free_disk_gb` | `CALMRUN_MIN_FREE_DISK_GB` | 5 | Free space needed on the working directory's drive; `0` = off |
| `pass_limit_min` | `CALMRUN_PASS_MIN` | 10 | Minutes a waiter may be passed; `0` = strict order |
| `vitest_workers` | `CALMRUN_VITEST_WORKERS` | 1 (≤ 4 cores), 2 (< 12), else 3 | `--maxWorkers` for direct vitest calls |
| `ceiling_gb` | `CALMRUN_CEILING_GB` | half the RAM | Memory ceiling for one job's process tree; `0` = off |
| `critical_free_gb` | `CALMRUN_CRITICAL_FREE_GB` | 5% of RAM, at most 1 GB | Free-RAM floor that stops the biggest job; `0` = off |
| `[resources]` | `CALMRUN_RESOURCES` | none (1 slot each) | Named resource slots, e.g. `db=1,gpu=2`; the variable adds to the table |
| `[kinds.<name>]` | | built-in costs | `cost` and `patterns` (command's first words) |
| | `CALMRUN_LOW_CPU` | on | `0`, `false`, `no` or `off` keeps normal priority |
| | `CALMRUN_HOOK_MODE` | deny | `rewrite` makes the Claude Code hook wrap commands itself |
| | `CALMRUN_HOME` | see below | State directory |
| | `CALMRUN_CONFIG` | `<state dir>/config.toml` | Config file path |
| | `CALMRUN_HELD` | | Set by calmrun for the command it runs (nesting); do not set it yourself |

An unknown key or a syntax error makes calmrun ignore the whole file with a warning
(`calmrun: ignoring config file: …`) and use the defaults; `calmrun doctor` shows what is in
effect.

### Where state lives

| OS | Default state directory |
| --- | --- |
| Windows | `%LOCALAPPDATA%\calmrun` |
| macOS | `~/Library/Application Support/calmrun` |
| Linux | `$XDG_STATE_HOME/calmrun` or `~/.local/state/calmrun` |

It holds `state.json` (running and waiting jobs), `state.lock`, `history.jsonl`, `config.toml`
and `logs/` (MCP task output). Entries of processes that died are removed automatically:
calmrun checks the pid together with the process start time, so a reused pid is not mistaken for
a live job. A corrupt `state.json` is set aside; an unreadable one is an error.

## Coding agents

### Claude Code hook

`calmrun hook claude` is a [PreToolUse hook](https://code.claude.com/docs/en/hooks). For the
`Bash` and `PowerShell` tools it checks each part of the command (split on `&&`, `||`, `;`, `|`,
`&` and new lines). If a part runs a heavy kind and is not already wrapped (`calmrun …`,
`npx calmrun …`, `lane with …`), the hook denies the call and tells Claude the exact command to
run instead, with each heavy part wrapped in place:

```
cd web && npm ci && npx vitest run      →      cd web && npm ci && calmrun -- npx vitest run
```

Light commands (`git`, `ls`, `cat`, `npm install`, …) are never blocked, heredoc bodies (for
example a commit message written with `<<'EOF'`) are not treated as commands, and any input the
hook does not understand is allowed (no output, exit 0). Wrappers such as `VAR=1`, `time` or
`nice` stay in front: `time npx vitest` becomes `time calmrun -- npx vitest`. When `calmrun` is
not on the agent's `PATH`, the suggestion uses calmrun's absolute path.

Add it to `.claude/settings.json` in a project, or to `~/.claude/settings.json` for every
project (or let `calmrun setup --write --hook` do it):

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash|PowerShell",
        "hooks": [{ "type": "command", "command": "calmrun hook claude" }]
      }
    ]
  }
}
```

With `CALMRUN_HOOK_MODE=rewrite` the hook instead returns `updatedInput` with the wrapped
command and no permission decision, so your normal permission rules still apply to the wrapped
command. In auto mode Claude Code may decline a changed input, in which case the original
command runs unwrapped; the default deny mode does not have that gap.

### MCP server

`calmrun mcp` is an [MCP](https://modelcontextprotocol.io) server over stdio. Agents that speak
MCP can queue work without a shell wrapper. It offers three tools:

| Tool | Arguments | What it does |
| --- | --- | --- |
| `run_task` | `command`, and optionally `cwd`, `kind`, `cost`, `label`, `timeout_s`, `prio`, `wait_timeout_s`, `ticket`, `needs`, `queue` | Waits in the same queue as the CLI, runs the command at low priority and returns the exit code, wait and run time, the last 200 lines of output and the path of the full log. |
| `queue_status` | `queue` | The same data as `calmrun status --json`. |
| `bump` | `label`, `queue` | Move matching waiting jobs to the front. |

- While a task waits, the server sends progress notifications if the client passed a
  `progressToken`.
- `timeout_s` stops the command (exit 124). `wait_timeout_s` gives up waiting (exit 75) and
  returns a `ticket`; pass it back as `ticket` to resume the same place in line.
- Cancelling the request (`notifications/cancelled`) leaves the queue or stops the command, and,
  as the MCP spec asks, the cancelled request gets no response.
- Full logs are kept under `logs/` in the state directory; the newest 200 are kept.
- A `command` made of plain words runs the program directly (and gets the worker rules);
  leading `VAR=value` words become the command's environment, so `CI=1 npm test` works on
  Windows too. One that uses shell syntax (`&&`, pipes, redirects, `$VAR`, …) runs in `sh -c`
  on macOS/Linux and `cmd /d /s /c` on Windows. The output tail is in the text content and, for
  clients that show structured results, in `output_tail`.
- `calmrun mcp` stops its running tasks on Ctrl+C / SIGTERM and when its client closes stdin.

Register it with your agent:

```sh
# Claude Code
claude mcp add calmrun -- calmrun mcp
```

```toml
# Codex: ~/.codex/config.toml
[mcp_servers.calmrun]
command = "calmrun"
args = ["mcp"]
tool_timeout_sec = 3600   # tasks may wait in the queue and run for a while
```

```json
// Cursor: ~/.cursor/mcp.json
{
  "mcpServers": {
    "calmrun": { "type": "stdio", "command": "calmrun", "args": ["mcp"] }
  }
}
```

```json
// Gemini CLI: ~/.gemini/settings.json (timeout in ms; the default is 10 minutes)
{
  "mcpServers": {
    "calmrun": { "command": "calmrun", "args": ["mcp"], "timeout": 3600000 }
  }
}
```

Long test runs can outlast a client's default tool timeout; raise it where the client allows.

### `calmrun setup`

`calmrun setup` adds a short calmrun section to each coding agent's **global** instruction file.
It is a dry run by default: it prints what it would change and the exact block. Add `--write`
to apply.

| Agent | File |
| --- | --- |
| `claude` | `~/.claude/CLAUDE.md` (or `$CLAUDE_CONFIG_DIR/CLAUDE.md`) |
| `codex` | `~/.codex/AGENTS.md` (or `$CODEX_HOME/AGENTS.md`) |
| `gemini` | `~/.gemini/GEMINI.md` |
| `opencode` | `~/.config/opencode/AGENTS.md` |

```sh
calmrun setup                         # show what would change (agents whose folder exists)
calmrun setup --write                 # apply
calmrun setup --write --agents codex  # only some agents
calmrun setup --write --hook          # also add the Claude Code hook to ~/.claude/settings.json
calmrun setup --write --remove --hook # take both out again
```

- The block sits between `<!-- calmrun:start … -->` and `<!-- calmrun:end -->`. Running setup
  again replaces the block in place instead of adding a second one, and `--remove` deletes it
  and leaves the rest of the file as it was.
- Every file setup rewrites is first copied to `<file>.calmrun.bak`; a file that cannot be read
  or is not UTF-8 text is left alone (exit 1).
- `--hook` merges the PreToolUse entry into `settings.json`, keeps every other setting, and
  leaves a file it cannot parse untouched.
- When Codex has an `AGENTS.override.md`, Codex reads that file instead; setup says so, and you
  copy the block there too.
- `--home DIR` uses another home directory (for tests, or another user's profile).

### `calmrun guide` and hand-written instructions

`calmrun guide` prints short instructions to paste into an agent's context. To write them by
hand instead, add this to `CLAUDE.md`, `AGENTS.md` or your agent's instruction file:

```markdown
## Heavy commands

This machine is shared by several agents. Run heavy commands through the calmrun queue:
`calmrun -- <command>` (tests, type checks, linters, builds, Playwright, docker compose,
pytest, cargo/gradle/make/dotnet/go builds). Example: `calmrun -- npx vitest run`.
Run anything that may take more than a minute or two in the background and check on it later.
Use `calmrun status` to see what is running or waiting. Do not wrap light commands
(git, ls, cat, package installs).
```

Messages meant for agents say what happened and what to run next (`calmrun: next: …`), for
example after a refused `docker compose up` or a command that is not on `PATH`.

## Exit codes

| Exit code | Meaning |
| --- | --- |
| the command's | It ran. On Unix a command ended by signal `n` gives `128 + n`. |
| 1 | calmrun itself failed (state directory, lock, a file `setup` could not change) |
| 2 | Refused (watch mode, attached `docker compose up`, interactive `docker run`), bad arguments, or an invalid `--id` |
| 75 | Not started within `--wait-timeout`; place kept, see the `next:` line |
| 124 | Stopped by the MCP `timeout_s` |
| 126 | The command was found but could not be started |
| 127 | Command not found |
| 130 | Ctrl+C (while waiting or running) |
| 137 | Stopped by the memory ceiling or the critical free-RAM floor |

## Platform notes

- **Windows** is where calmrun is built and checked by hand, as well as in CI. `.cmd` and
  `.bat` programs run through `cmd.exe /d /s /c` with every argument quoted and caret-escaped
  for both cmd parses a batch shim performs, so arguments such as `a&b`, `100%` or `"quoted"`
  arrive unchanged. `.ps1` files run with `powershell -NoProfile -ExecutionPolicy Bypass -File`.
  The command runs in a Job Object, so its tree ends even after `taskkill /F` on calmrun.
  Ctrl+C reaches the command through the shared console; calmrun turns Ctrl+C processing back
  on if its parent had it disabled. A command ended by Ctrl+C exits 130.
- **macOS / Linux** are covered by the test suite in CI, not by hand yet. The command gets its
  own process group. When calmrun runs in the foreground of a terminal, that group becomes the
  terminal's foreground group (so the command can read input and gets Ctrl+C directly) and the
  terminal is handed back afterwards; SIGINT, SIGTERM and SIGHUP that calmrun receives are
  forwarded to the group as SIGINT. Ctrl+Z stops the command and calmrun together; `fg`
  continues both. On Linux the command gets SIGTERM when calmrun dies, even by `kill -9`
  (parent-death signal); on macOS a small `calmrun __watchdog` process does the same within a
  second.
- **Memory figures.** calmrun counts resident memory: the working set on Windows and the
  resident set size (RSS) on macOS and Linux. Shared pages count once per process, so the tree
  total can be higher than what the OS reports as used; the default ceiling leaves room.
- **What CI covers.** CI runs the whole suite on `windows-latest` (x64), `ubuntu-latest` (x64)
  and `macos-latest` (arm64): queue rules, exit codes, Ctrl+C while running and while waiting,
  a SIGKILLed calmrun taking its command with it (Unix), SIGTERM to the MCP server (Unix),
  daemons surviving a normal exit, the memory ceiling stopping a nested tree, the MCP server,
  tickets, the disk gate, named resources, separate queues and `setup`. Killing calmrun ending
  the tree is tested on Windows only. The critical free-RAM floor is covered by unit tests only
  (it depends on the machine). Ctrl+Z and the terminal hand-over are not covered (runners have
  no terminal). Release binaries for Windows arm64, Linux arm64 and macOS x64 are built but not
  tested.
- **Battery.** `calmrun doctor` reports whether the machine runs on battery; consider a smaller
  budget then.

## Troubleshooting

**`calmrun: command not found` after installing.** The installers put the binary in
`~/.cargo/bin` (`%USERPROFILE%\.cargo\bin` on Windows). Open a new terminal, or add that folder
to `PATH`. Coding agents started before the install need a restart too; the Claude Code hook
falls back to calmrun's absolute path in its suggestions.

**A job waits although nothing seems to run.** `calmrun status` says why each job waits. On a
busy machine the RAM gate is the usual reason (`waiting for RAM: … free, needs …`): lower
`min_free_gb`, or set it to `0`. Entries of dead processes are removed automatically.

**A job I need now is stuck behind others.** `calmrun bump <label>`, or run it with
`--prio high`.

**My agent's tool call timed out while waiting.** Use `--wait-timeout` (CLI) or
`wait_timeout_s` (MCP) and resume with the ticket, or raise the client's tool timeout (see the
MCP section).

**`refused: … is watch mode`.** Queued jobs must end. Run the `calmrun: next:` command it
prints, or run the watcher outside calmrun.

**A job was stopped with exit 137.** Its process tree passed `ceiling_gb`, or free RAM stayed
below `critical_free_gb` and it was the biggest job. Raise or disable the limit
(`CALMRUN_CEILING_GB=0`), or give the job fewer workers.

**My config seems ignored.** An unknown key or a TOML error makes calmrun skip the whole file
with a warning on stderr. `calmrun doctor` shows the path it reads and the values in effect.

**I want a separate budget for one project.** Use `--id <name>` on its runs (and on `status` /
`bump`).

## Contributing

[ARCHITECTURE.md](ARCHITECTURE.md) maps the modules and explains how to add a tool, a gate, an
MCP tool or an agent. Changes are listed in [CHANGELOG.md](CHANGELOG.md).

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Bug reports and pull requests are welcome at
<https://github.com/vannt-dev/calmrun/issues>.

## License

[MIT](LICENSE)
