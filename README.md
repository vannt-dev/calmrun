# calmrun

[![CI](https://github.com/vannt-dev/calmrun/actions/workflows/ci.yml/badge.svg)](https://github.com/vannt-dev/calmrun/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Release](https://img.shields.io/github/v/release/vannt-dev/calmrun)](https://github.com/vannt-dev/calmrun/releases/latest)
[![crates.io](https://img.shields.io/crates/v/calmrun)](https://crates.io/crates/calmrun)
[![npm](https://img.shields.io/npm/v/calmrun)](https://www.npmjs.com/package/calmrun)

A machine-wide queue for heavy commands, so several coding agents and terminals on one computer
don't run their builds and tests all at once and freeze it, in any stack: `vitest`, `pytest`,
`cargo`, `gradle`, `dotnet`, `go`, `cmake`, Playwright, `docker compose` and more.

```sh
calmrun -- npx vitest run
```

calmrun waits until the machine has room (budget points, free RAM, free disk), then runs the
command at low CPU priority and passes its exit code back. There is no daemon: every `calmrun`
process coordinates through a small state file guarded by an OS file lock.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/vannt-dev/calmrun/main/docs/diagrams/overview-dark.svg">
  <img alt="Several agents and terminals on one machine send heavy commands to calmrun, either directly as calmrun -- cmd or through the MCP tool run_task; the agent hooks (Claude Code, Codex, OpenCode, and Antigravity as experimental) catch a heavy command started without calmrun and denies it with the wrapped form, or rewrites it. calmrun first applies its command rules: a watch mode is refused with exit 2 and the one-shot form to run instead, and a dev server or app run starts at once outside the queue. Any other command is detected for its kind and cost, its exclusive key and a worker hint, then waits in the machine-wide queue until all gates hold: free budget points, enough free RAM and disk, its exclusive key and any named resource. With --wait-timeout it gives up with exit 75 and keeps its place as a ticket. Once admitted it runs at low CPU priority under a process-tree memory ceiling, and its exit code is passed back and recorded in the history." src="https://raw.githubusercontent.com/vannt-dev/calmrun/main/docs/diagrams/overview-light.svg">
</picture>

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
- Two test runs or builds on the same `node_modules`, Cargo `target` or project folder (Gradle,
  Maven, .NET, CMake, Swift, Flutter, …) never run together.
- Direct calls of vitest, cargo, gradle, dotnet, go, cmake, ninja, bazel and other tools that
  use every core get a worker count that fits their share of the machine.
- Commands run at low CPU priority, so your editor and the agents stay responsive.
- A job whose process tree grows past a memory ceiling is stopped instead of taking the machine.
- A killed terminal does not leave orphaned workers: the command's tree dies with calmrun.
- A stuck job is visible and short-lived: `status` and the waiting lines show its CPU, `idle 5m`
  and its usual time, test-like jobs stop after 60 minutes, and `calmrun stop <id>` ends one now.
- You steer the order: `calmrun bump` puts a job next, `--prio low` and `bump --back` send long
  jobs to the end, and `calmrun logs <id> --follow` shows what the job ahead prints.

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
calmrun --log -- npm test        # also keep a log of the output
calmrun logs 3 --follow          # watch the output of job 3 (one with a log)
calmrun stats --days 30          # how long jobs waited and ran
calmrun bump e2e                 # move a waiting job to the front
calmrun bump --back --pos 2      # or to the end of the line
calmrun stop 12                  # stop a hung job (id from `calmrun status`)
calmrun --timeout 600 -- npm test  # stop it if it runs longer than 10 minutes
calmrun init                     # write a commented config file
calmrun setup --write --hook     # tell your coding agents to use calmrun
```

While waiting, calmrun prints one line to stderr, then one every 30 seconds:

```
calmrun: waiting — 4 of 6 points in use, 2nd in line (needs 2); running: npx vitest in web (3m10s), cargo build in api (1m02s)
calmrun: waiting for RAM: 2.1 GB free, needs 3.0 GB; running: npx playwright in e2e (4m55s)
```

The running jobs carry hints about whether they are working (see
[Working or hung?](#working-or-hung)). When one looks stuck, a next step follows:

```
calmrun: waiting - 6 of 6 points in use (needs 2), 1st in line; running: npx vitest in lane (12m00s, usually 40s, longer than usual, idle 5m)
calmrun: next: "npx vitest" (job 12) has used no CPU for 5m; check it with `calmrun status`, and if it hangs stop it with `calmrun stop 12`
```

## How it decides

### Budget and kinds

The machine has `budget` points (default: logical cores − 2, capped at floor(RAM GB / 2.5), at
least 2; the RAM gate below is what keeps a RAM-poor machine responsive). Each job costs points by kind; `--cost` overrides, and a cost above the budget counts
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
- **RAM.** Free RAM is at least `min_free_gb` (default max(1.5, RAM / 10) GB; `0` disables). The
  check is skipped when nothing is running in any queue, so a job never waits forever on an idle
  machine. Waiters share one RAM reading per second, and 1 GB per point is held back for jobs
  that started in the last 20 s:
  `calmrun: waiting for RAM: 2.0 GB free, needs 3.6 GB (1.6 GB minimum + 2.0 GB held back for jobs that just started)`.
  `calmrun doctor` warns when less RAM is free than `min_free_gb`.
- **Disk.** The drive of its working directory has at least `min_free_disk_gb` free (default 5;
  `0` turns it off): `calmrun: waiting for disk space: 3.2 GB free on C:\, needs 5.0 GB`. When
  nothing else runs it starts anyway, with a warning.
- **Same folder, one at a time.** vitest, jest and JavaScript build jobs take an exclusive key
  for their nearest `node_modules` folder; Cargo builds and tests take one for their `target`
  folder (or `CARGO_TARGET_DIR`); other build tools take one for their project folder, the
  nearest folder holding the marker file in the [build tools table](#build-tools) (two Gradle
  runs in one build share `build/`, two `dotnet` runs share `obj/`). Two jobs with the same key
  never run together, whatever queue they are in.
- **Named resources.** `--needs db` also takes one slot of the resource `db`. Configure slots
  with `[resources] db = 1` or `CALMRUN_RESOURCES="db=1,gpu=2"`; a resource that is not
  configured has one slot. Use it for things that cannot be shared: one test database, one
  emulator, one GPU. Waiters say who holds it:
  `calmrun: waiting for resource "db" (1 of 1 slot(s) held by npm test), 2nd in line`.
  A resource name uses letters, digits, `-`, `_` and `.` (not first); anything else exits 2.
  A [lease](#leases-holding-a-resource-without-a-command) also holds a slot. A resource can
  also list its [instances](#resources-with-instances-devices) (two emulators, three
  simulators): each job then gets its own free one.

Points, keys and resource slots are taken all-or-nothing.

### Resources with instances (devices)

When a resource is a set of things that each serve one job at a time, such as emulators,
devices or simulators, list them. Each job that needs the resource gets one free instance, and
calmrun tells it which:

```toml
[resources]
android = ["emulator-5554", "emulator-5556"]     # a fixed list

[resources.emulator]                             # or a table
discover = "adb devices"                         # list them with a command
health = "adb -s {instance} get-state"           # skip an instance whose check fails
env = "ANDROID_SERIAL"                           # the variable to set (default below)
```

```sh
calmrun --needs android -- ./gradlew connectedAndroidTest   # ANDROID_SERIAL=emulator-5554
calmrun --needs android -- ./gradlew connectedAndroidTest   # at the same time: emulator-5556
```

- The job gets the instance in `CALMRUN_<NAME>` (`CALMRUN_ANDROID`; characters other than
  letters and digits become `_`) and in the variable named by `env`. For `android` and
  `emulator` that variable is `ANDROID_SERIAL` by default, which `adb` and Gradle's
  `connected*` tasks follow. Gradle's `connected*` tasks need `emulator` by themselves, so
  `[resources.emulator]` gives them a device each without `--needs`.
- `calmrun` prints `calmrun: using android emulator-5554 (CALMRUN_ANDROID, ANDROID_SERIAL)`
  when the job starts. `status` shows it under the job (`uses: android=emulator-5554`);
  `status --json`, MCP `run_task`, `task_status` and `queue_status` carry `instances`.
- `discover` is a command whose output lists instances, one per line: the first word of each
  line counts. Blank lines, `#` comments, `*` notes, the `List of devices attached` header of
  `adb devices`, and devices that are `offline`, `unauthorized` or otherwise not ready are
  skipped. The fixed `instances` come first, then the discovered ones.
- `health` runs for each instance with `{instance}` replaced (also in `CALMRUN_INSTANCE`); an
  instance whose check does not exit 0 within 10 s is skipped. An instance name may only use
  letters, digits and `- _ . : @ / +`, so it is safe in a command line.
- The waiting job runs these commands itself, outside every lock, and looks again every 5 s
  while it waits. When no instance is ready, it waits with
  `calmrun: waiting for a ready instance of "android" (…)`, and it never holds back the jobs
  behind it. When every ready instance is in use, it waits like any resource:
  `waiting for resource "android" (all 2 ready instance(s) in use by …)`.
- A [lease](#leases-holding-a-resource-without-a-command) holds one instance; jobs that use
  the lease get that instance. A resource without instances (`db = 1`) works as before.

Instances need calmrun 0.6 or newer in every process that shares the queue: an older calmrun
counts the resource as one slot and drops the instance records when it rewrites the state.

### Order and passing

`--prio high` first, then normal, then `--prio low`; within each, first come. A later job may
pass earlier ones only if it fits right now and no job ahead of it has waited longer than
`pass_limit_min` (default 10; `0` = strict order). Running jobs are never stopped to make room.

`calmrun bump` moves waiting jobs to the front: before every other waiter, with prio high.
Pick them with exactly one of a label substring (`calmrun bump e2e`), `--pos N` (the place in
the `#` column of `calmrun status`), `--ticket ID` (the job id from `status --json`, or a
ticket kept by `--wait-timeout`) or `--dir PATH` (jobs whose folder is that one or inside it).
A bumped job also holds the front: while it waits for budget points or free RAM, no later job
may pass it, since that would take the room it waits for (the others wait with
`bumped job "npx vitest" ahead (1st in line) to start`). While it waits for an exclusive key,
a resource slot or disk space, later jobs may still pass it (until it has waited past the pass
limit, as for any job): those can last long, and passing does not delay it.

`calmrun bump --back <selector>` moves jobs to the very end instead: prio low, after every
other waiter, in their order. A low job waits behind every normal job, also ones that come
later, so a steady stream of normal jobs can keep it waiting; that is what low is for. The
pass limit does not change that: normal jobs are ahead of a low one in line, they do not pass it.

In `calmrun status` the place is marked: `!` prio high, `^` bumped (holds the front), `-` prio
low; for example `1!^` or `4-`.

### Separate queues

`--id QUEUE` uses its own queue with its own budget and order, like GNU `sem --id` (for example
one queue per project, or one for long e2e runs). `status --id QUEUE` and `bump … --id QUEUE`
act on it. Queues do not share points, but the machine is one: exclusive keys and the RAM and
disk gates see the jobs running in every queue (a key held elsewhere is named:
`same dependency folder is in use by "npx vitest (queue kx)"`). History and config are shared.
A queue id uses letters, digits, `-`, `_` and `.` (not first); a queue left empty for 7 days is
removed.

A queue decides with the settings (budget, `min_free_gb`, `pass_limit_min`) of the job that
joined it while it was empty, so two agents with different `CALMRUN_BUDGET` never decide
differently. A caller with other settings gets a note; `status` shows the queue's settings and
warns about the difference. New settings apply once the queue is empty.

### Tickets: capped waits

`--wait-timeout SECS` caps the wait. If the job has not started by then, calmrun exits **75**
(EX_TEMPFAIL) and keeps its place in line for 10 minutes. It prints the exact command that
resumes that place, the same arguments with `--ticket <id>` in front:

```
calmrun: not started within 120s (waiting - 6 of 6 points in use (needs 2), 2nd in line); your place in line is kept for 10 min.
calmrun: next: calmrun --ticket 17 --wait-timeout 120 -- npx vitest run
```

While the owner is away, the kept place does not block anyone; when the ticket comes back it
rejoins at its old position. An expired or unknown ticket joins the end of the line. `status`
shows places kept by a ticket; `calmrun release --ticket <id>` gives one up.

### Leases: holding a resource without a command

Some claims outlive one command: an agent that works with the emulator for an hour runs many
short commands against it. `calmrun hold` takes slots of named resources without running
anything, and gives them back with `calmrun release`:

```sh
lease=$(calmrun hold emulator --label "web-2" --for ~/work/web-2)   # waits like --needs, prints the id
calmrun -- ./gradlew connectedAndroidTest   # in ~/work/web-2: uses the leased emulator
calmrun release "$lease"
```

- `hold` waits in the normal queue until every named resource has a free slot (the same rule
  as `--needs`), records a lease, prints its id (six hex digits) as the only line on stdout and
  exits 0. No process stays behind.
- A lease takes no budget points unless `--cost N` is given, so it never waits for points or
  RAM by itself. It lapses after `--ttl SECS` (default 4 h): any calmrun that reads the queue
  drops expired leases. `calmrun hold --renew <id> [--ttl SECS]` extends it from now (default:
  its own ttl), and exits 1 if it is gone.
- `--wait-timeout SECS` gives up waiting: exit 75, and unlike a job the place is not kept.
  `--wait-timeout 0` tries once and exits 75 at once when a slot is not free.
- A lease does not block its own holder. With `--for DIR`, jobs whose folder is `DIR` or inside
  it use the leased resources: they neither wait for them nor take another slot (a Gradle
  `connected*` task there, which needs the emulator by itself, just runs). A job started with
  `CALMRUN_LEASE=<id>[,<id>…]` may use those leases' resources from any folder. Another
  `hold` always takes slots of its own.
- For a [resource with instances](#resources-with-instances-devices), the lease holds one free
  instance and its jobs get that one (`ANDROID_SERIAL`, `CALMRUN_ANDROID`). `hold` names it on
  stderr (`calmrun: holding android=emulator-5556 for 4h00m (lease 3fa2c1) …`), and `status`
  shows it. `hold --print-env` prints `NAME=VALUE` lines on stdout instead of the bare id, for
  a script: `CALMRUN_LEASE=3fa2c1`, then `CALMRUN_ANDROID=emulator-5556` and
  `ANDROID_SERIAL=emulator-5556`.
- `release` exits 0 when the lease is released and also when it is already gone (a note on
  stderr). An id of the wrong shape exits 2.
- Jobs with `--needs emulator` wait while a lease holds the emulator's only slot. Such a job
  never holds back the jobs behind it, however long it waits: a lease may last hours, so the
  pass limit does not apply to a wait that only a lease causes.
- Leases belong to a queue: `hold … --id QUEUE` and `release … --id QUEUE`. `status` lists
  them apart from jobs (resource, label, age, expiry, owning folder); `status --json` and MCP `queue_status`
  have them under `leases`.

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
  `xcodebuild -list`). The agent hooks leave them alone.
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

- **Run time limit.** A job of a test-like kind (`test`, `vitest`, `jest`, `pytest`,
  `typecheck`, `tsc`, `lint`, `eslint`) still running after 60 minutes is stopped (its tree,
  like a memory stop) and calmrun exits **124**; builds, monorepo runs, docker, captures and
  other kinds have no limit. `max_run_min` in the config sets it for every kind,
  `[kinds.<name>] max_run_min` for one (0 = no limit), `--timeout SECS` for one run
  (`--timeout 0` = no limit). The message names the setting to change. A lease or a waiting
  job is never limited, and a command that runs outside the queue (a dev server, a nested
  calmrun) only gets an explicit `--timeout`.

  ```
  calmrun: stopped npx vitest run: still running after its 1h run time limit (the default for kind vitest)
  calmrun: next: if it was still working, set [kinds.vitest] max_run_min (or max_run_min for every kind, or CALMRUN_MAX_RUN_MIN) in the config; 0 = no limit; or --timeout SECS for one run (--timeout 0 = no limit)
  ```

- **Output and logs.** By default the command writes straight to your terminal and no copy is
  kept. With `--log` (or `log = true` in the config) calmrun copies its stdout and stderr, each
  chunk as it arrives, to the terminal and to a log file under `logs/` in the state directory
  (shared with MCP tasks; the newest 200 are kept). stdin, the exit code and Ctrl+C work as
  without it, and each stream keeps its order. The trade-off: the command now writes to a pipe,
  not a terminal, so many tools turn off colours, progress bars and prompts (some take
  `--color=always` or `FORCE_COLOR=1`), and stdout and stderr lines may interleave differently.
  On a Windows console the copy is shown as UTF-8. A process the command leaves running (a
  daemon) keeps the pipe open: calmrun reads on for at most 2 s after the command exits and
  then drops that process's output, so run such commands without `--log`. While a job with a
  log runs, `calmrun status` shows the file and when the job last printed (`last output 6m10s
  ago`), waiting lines end with `output of npx vitest: calmrun logs 3`, and
  `calmrun logs 3 --follow` prints its output as it comes.

- **Process tree.** When the command exits normally, processes it deliberately left running (a
  Gradle or Kotlin daemon, a dev server started in the background) are left alone. When the run
  ends abnormally (Ctrl+C, or calmrun itself being killed) the whole tree is ended.
- **History.** Each run appends one JSON line to `history.jsonl`: `at`, `label`, `kind`, `cost`,
  `wait_ms`, `run_ms`, `status` (`ceiling` or `low-memory` for memory stops, `timeout` for the
  run time limit, `stopped` for `calmrun stop`, `cancelled` for Ctrl+C while waiting), and `cwd`
  (the folder as a path key, so usual run times are per project), plus `id` and `log` for a job
  with a log (so `calmrun logs <id>` finds it afterwards). A calmrun killed hard
  (`taskkill /F`, `kill -9`) is recorded by the next calmrun that finds it gone: `killed` if its
  command was running, `abandoned` if it was still waiting. Past 4 MB the file becomes
  `history.1.jsonl`; `stats` reads both. The label is `--label` or the tool and subcommand only
  (`npx vitest run`, `cargo build`), never the arguments, which may contain secrets.

### Working or hung?

A long job looks the same whether it works or hangs, so `calmrun status` (and the waiting
line of every job behind it) says what can be measured:

- **CPU.** With the memory sample (about once a second) calmrun adds up the CPU time of the
  job's whole process tree, on Windows, Linux and macOS. `status` shows the recent CPU use in
  percent of one core (`cpu 85%`; above 100% means several cores) and, once the tree has used
  less than 2% of one core for 2 minutes, `idle 5m`. Idle is only a hint and never stops a job:
  a tree that waits for docker, an emulator capture or the network does its work outside the
  tree and uses no CPU itself.
- **Usual run time.** From the history: the median run time of the newest successful runs of
  the same label in the same folder (at least 3), else of the label anywhere (at least 5),
  else nothing. `usually 40s`, and `longer than usual` once the job has run more than
  max(3 x usual, usual + 2 min).
- **Limit.** The run time limit in force (`limit 60m`), see [While it runs](#while-it-runs).
- **Output.** For a job with a log (run with `--log`, or an MCP task): its file and when it last
  printed, on a `log:` line (`last output 6m10s ago`), and `calmrun logs <id> --follow` shows
  what it prints. See [Output and logs](#while-it-runs).

```
running (1):
          id label                    kind     cost     pid      for  cwd
          12 npx vitest run           vitest      2   20412   12m03s  F:\src\lane
       now: cpu 0%, usually 40s, longer than usual, idle 5m, limit 60m
```

When a running job looks stuck (idle, or longer than usual), the jobs waiting behind it print
a `calmrun: next:` line that names `calmrun status` and `calmrun stop <id>`, and `status`
lists it under `warning:`. `status --json`, MCP `queue_status` and `task_status` carry the
same data: `cpu_pct`, `cpu_ms`, `idle_ms`, `usual_ms`, `longer_than_usual`, `timeout_ms`,
`stop_requested` and the `hint` text.

**Stopping a job.** `calmrun stop <id>` (the `id` column of `status`) or
`calmrun stop --label TEXT` (the exact label; it must match one job, else calmrun lists the
candidates and exits 2) asks the calmrun that owns the job to stop it: a running command's
tree is stopped the way a timeout stops it, a waiting job leaves the line, and that calmrun
prints `calmrun: stopped by calmrun stop` and exits **130** (history status `stopped`). If
the owner does not react within 10 seconds (`--wait SECS`), for example a calmrun older than
0.4 or a wedged process, `stop` kills the owner and its process tree itself, after checking
that the pid still belongs to that owner (same start time, as the cleanup of dead entries
does), removes the entry and says which way it went. A child process only counts as part of
the tree when it started after its parent (Windows keeps a dead parent's pid, which may be
reused). An owner that also runs other jobs (an MCP server runs every task under its own
pid) is never killed: for a running job only its command tree is, and a waiting job of such
an owner is refused with exit 1. An owner that cannot be killed leaves the entry as it is
(exit 1). A place kept for a ticket is just dropped. `stop` is a CLI command only: the MCP server has no such tool, so an agent cannot
clear other agents' jobs out of its way (`cancel_task` stops its own tasks).

## Commands

```
calmrun [--kind K] [--cost N] [--prio normal|high|low] [--label L] [--needs NAME,…] [--id QUEUE]
        [--wait-timeout SECS] [--ticket ID] [--timeout SECS] [--log | --no-log]
        -- <command...>
calmrun status [--json] [--id QUEUE]
calmrun logs <id> | --label TEXT [--follow] [--tail N] [--id QUEUE]
calmrun stats [--days N] [--json]
calmrun bump <label-substring> | --pos N | --ticket ID | --dir PATH [--back] [--id QUEUE]
calmrun stop <id> | --label TEXT [--wait SECS] [--id QUEUE]
calmrun hold <resource>[,<resource>…] [--ttl SECS] [--label TEXT] [--for DIR] [--cost N] [--wait-timeout SECS] [--id QUEUE]
calmrun hold --renew <lease> [--ttl SECS] [--id QUEUE]
calmrun release <lease> [--id QUEUE]
calmrun release --ticket <id> [--id QUEUE]
calmrun doctor
calmrun init [--force]
calmrun guide
calmrun setup [--write] [--remove] [--hook[=AGENTS]] [--agents claude,codex,gemini,opencode,pi] [--home DIR]
calmrun hook claude|codex|antigravity|opencode|pi
calmrun mcp
```

### Running a command

| Flag | Meaning |
| --- | --- |
| `--kind K` | Kind of job (a built-in kind or one from the config); detected when omitted |
| `--cost N` | Points this job takes (clamped to the budget) |
| `--prio normal\|high\|low` | Queue priority; `high` goes before every `normal` job, `low` after every one (see [Order and passing](#order-and-passing)) |
| `--label L` | Name in `status` and `stats` (default: tool + subcommand) |
| `--needs NAME` | Also take a slot of this named resource; repeat or comma-separate (`--needs db,gpu`) |
| `--id QUEUE` | Use a separate queue instead of the machine-wide one |
| `--wait-timeout SECS` | Give up waiting after this long: exit 75, place kept for 10 minutes |
| `--ticket ID` | Resume the place kept by an earlier `--wait-timeout` run |
| `--timeout SECS` | Stop the command once it has run this long: exit 124. `0` = no limit for this run. Default: `max_run_min` from the config (60 min for test-like kinds, else none) |
| `--log` | Also copy stdout and stderr to a log file that `calmrun logs` shows; the command then writes to a pipe (see [Output and logs](#while-it-runs)) |
| `--no-log` | No log for this run, even with `log = true` in the config |

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
| `status [--json]` | Running and waiting jobs (with their `id`) with the reason each one waits (`waits for: RAM: ...`; in JSON `why` and `why_code`: `budget`, `order`, `ram`, `disk`, `key`, `resource`, or `starting`), the queue's budget, free RAM, kept tickets and leases. Running jobs show CPU use, `idle`, their usual run time and limit (see [Working or hung?](#working-or-hung)). `--json` (with `mem_bytes`, `cpu_pct`, `idle_ms`, `usual_ms`, `timeout_ms` per running job) for tools. A running job with a log gets a `log:` line (in JSON `log_path`, `output_ms` and `output_ago_ms`: when it last printed). |
| `logs <id>` | Print the last `--tail N` lines (default 50) of a job's log; `--follow` keeps printing new output until the job ends, then exits 0. `<id>` is the job's `id` in `calmrun status`; `--label TEXT` picks the job by part of its label instead and must match one job (else exit 2 with the candidates). Works for jobs run with `--log` and MCP tasks, while they run and afterwards while the file is kept; a job without a log says how to get one. Read-only. |
| `stats [--days N] [--json]` | Runs, failures, average and maximum wait, average run time per label and kind, and waits that gave up before starting, over the last N days (default 7). |
| `bump <text>` | Move waiting jobs whose label contains the text (case-insensitive) to the front, where later jobs may not pass them while they wait for points or RAM. Instead of the text: `--pos N`, `--ticket ID` or `--dir PATH` (exactly one). `--back` moves them to the end with prio low. See [Order and passing](#order-and-passing). |
| `stop <id>` | Stop a running or waiting job, or `--label TEXT` (exact, one job); its calmrun exits 130. An owner that does not react within `--wait SECS` (default 10) is killed with its tree. CLI only. See [Stopping a job](#working-or-hung). |
| `hold <resource>[,…]` | Wait for a slot of each resource, record a lease, print its id and exit; `--ttl`, `--label`, `--for`, `--cost`, `--wait-timeout`, `--renew <id>`. See [Leases](#leases-holding-a-resource-without-a-command). |
| `release <lease>` | Give a lease back (exit 0 also when it is already gone). `release --ticket <id>` gives up a place kept by `--wait-timeout`. |
| `doctor` | Cores, RAM, disk type, battery, the state directory and config file, the settings in effect and the suggested ones. |
| `init [--force]` | Write a commented `config.toml` with this machine's defaults and print its path; `--force` overwrites. |
| `guide` | Short instructions for coding agents, to paste into their context. |
| `setup` | Add or remove the calmrun block in agents' global instruction files; see [below](#calmrun-setup). |
| `hook <agent>` | The PreToolUse hook of `claude`, `codex`, `antigravity`, `opencode` or `pi`; reads the hook JSON on stdin. See [Coding agents](#coding-agents). |
| `mcp` | An MCP server on stdin/stdout with `run_task`, `task_status`, `cancel_task`, `queue_status` and `bump`. |

`--id QUEUE` is accepted by every subcommand, before or after its name; it matters for running,
`status`, `logs`, `bump`, `stop`, `hold` and `release`.

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
max_run_min = 120       # run time limit for every kind; 0 = no limit
log = false             # true: every run keeps a log, as with --log

[resources]                      # named resources for --needs, with their slots
db = 1
android = ["emulator-5554", "emulator-5556"]   # or instances: one each per job

[kinds.vitest]
cost = 3                         # override a built-in cost
max_run_min = 90                 # and its run time limit (0 = none)

[kinds.heavy]                    # add a kind with command patterns (first words)
cost = 2
patterns = ["make", "bazel build"]
```

| Key | Variable | Default | Meaning |
| --- | --- | --- | --- |
| `budget` | `CALMRUN_BUDGET` | cores − 2, capped at floor(RAM GB / 2.5), at least 2 | Points that may run at once |
| `min_free_gb` | `CALMRUN_MIN_FREE_GB` | max(1.5, RAM / 10) | Free RAM needed to start while something runs; `0` = off |
| `min_free_disk_gb` | `CALMRUN_MIN_FREE_DISK_GB` | 5 | Free space needed on the working directory's drive; `0` = off |
| `pass_limit_min` | `CALMRUN_PASS_MIN` | 10 | Minutes a waiter may be passed; `0` = strict order |
| `vitest_workers` | `CALMRUN_VITEST_WORKERS` | 1 (≤ 4 cores), 2 (< 12), else 3 | `--maxWorkers` for direct vitest calls |
| `ceiling_gb` | `CALMRUN_CEILING_GB` | half the RAM | Memory ceiling for one job's process tree; `0` = off |
| `critical_free_gb` | `CALMRUN_CRITICAL_FREE_GB` | 5% of RAM, at most 1 GB | Free-RAM floor that stops the biggest job; `0` = off |
| `max_run_min` | `CALMRUN_MAX_RUN_MIN` | 60 for test, vitest, jest, pytest, typecheck, tsc, lint, eslint; none for other kinds | Run time limit in minutes for every kind without its own; `0` = no limit (exit 124 when it passes) |
| `log` | `CALMRUN_LOG` | false | `true` (variable `1`) gives every CLI run a log, as `--log` does; `--no-log` or `CALMRUN_LOG=0` turns it off |
| `[resources]` | `CALMRUN_RESOURCES` | none (1 slot each) | Named resource slots, e.g. `db=1,gpu=2`, or [instances](#resources-with-instances-devices) (a list, or a table with `instances`, `discover`, `health`, `env`); the variable adds to the table, and a resource it names has slots, not instances |
| `[kinds.<name>]` | | built-in costs | `cost`, `patterns` (command's first words) and `max_run_min` (wins over the global one) |
| | `CALMRUN_LOW_CPU` | on | `0`, `false`, `no` or `off` keeps normal priority |
| | `CALMRUN_HOOK_MODE` | deny | `rewrite` makes the agent hooks wrap commands themselves |
| | `CALMRUN_HOME` | see below | State directory |
| | `CALMRUN_CONFIG` | `<state dir>/config.toml` | Config file path |
| | `CALMRUN_HELD` | | Set by calmrun for the command it runs (nesting); do not set it yourself |

An unknown key or a syntax error makes calmrun ignore the whole file with a warning
(`calmrun: ignoring config file: …`) and use the defaults; `calmrun doctor` shows what is in
effect. A config with `max_run_min` is such an unknown key for calmrun 0.3.x: a long-lived
0.3 process (a `calmrun mcp` server started before the upgrade) uses its defaults until it is
restarted.

### Where state lives

| OS | Default state directory |
| --- | --- |
| Windows | `%LOCALAPPDATA%\calmrun` |
| macOS | `~/Library/Application Support/calmrun` |
| Linux | `$XDG_STATE_HOME/calmrun` or `~/.local/state/calmrun` |

It holds `state.json` (running and waiting jobs), `state.lock`, `history.jsonl`, `config.toml`
and `logs/` (output of MCP tasks and `--log` runs). Entries of processes that died are removed
automatically: calmrun checks the pid together with the process start time, so a reused pid is
not mistaken for a live job. A corrupt `state.json` is set aside; an unreadable one is an error.

## Coding agents

calmrun has a PreToolUse hook for Claude Code, Codex, OpenCode, the
[Pi coding agent](https://github.com/earendil-works/pi) and (experimental) Antigravity. All five
make the same decision; only the way each agent sends a tool call and reads the answer
differs.
`calmrun setup --write --hook <agents>` installs them in each agent's user-level file (see
[`calmrun setup`](#calmrun-setup)).

### What the hooks do

For each shell command the hook checks every part of it (split on `&&`, `||`, `;`, `|`, `&`,
new lines, and the braces and parentheses of blocks). If a part runs a heavy kind and is not
already wrapped (`calmrun …`, `npx calmrun …`, `lane with …`), the hook denies the call and
tells the agent the exact command to run instead, with each heavy part wrapped in place:

```
cd web && npm ci && npx vitest run      →  cd web && npm ci && calmrun -- npx vitest run
cd web; if ($?) { npx tsc --noEmit }    →  cd web; if ($?) { calmrun -- npx tsc --noEmit }
for d in a b; do npm test; done         →  for d in a b; do calmrun -- npm test; done
bash -c "npm test"                      →  bash -c "calmrun -- npm test"
```

Light commands (`git`, `ls`, `cat`, `npm install`, …) are never blocked, heredoc bodies (for
example a commit message written with `<<'EOF'`) are not treated as commands, and any input the
hook does not understand is allowed (no output, exit 0). Wrappers such as `VAR=1`, `time`,
`nice` or `timeout 600` stay in front: `time npx vitest run` becomes
`time calmrun -- npx vitest run`. Commands run through rtk (a token-saving CLI proxy)
are seen through and keep their rtk form: `rtk cargo test` becomes `calmrun -- rtk cargo test`
(`rtk vitest`, `rtk tsc`, `rtk npm test`, `rtk test <command>` likewise; `rtk git` and other
light subcommands stay light). Commands inside `if`/`then`/`do` blocks, PowerShell
`if ($?) { … }` and `try { … }` blocks, `$( … )`, subshells and `bash -c "…"` or
`pwsh -Command '…'` scripts are wrapped where they run. When `calmrun` is not on the agent's
`PATH`, the suggestion uses calmrun's absolute path.

The hooks follow the same [command rules](#command-rules) as `calmrun --`, so they never
suggest a command calmrun would refuse. A watch mode with a one-shot form gets that form
(`dotnet watch test` → `calmrun -- dotnet test`, `npx vitest` → `calmrun -- npx vitest run`,
`docker compose up` → `calmrun -- docker compose up -d`); `calmrun --` prints the same form in
its `next:` line. A watch mode or interactive session without one (`sbt shell`, `cargo watch`,
`nx watch`, `docker run -it`), dev servers and app runs are left alone.

**Long waits.** An agent's shell tool stops a command after its time limit (Claude Code: the
call's `timeout`, 2 minutes by default; OpenCode the same; Pi only when the call sets a
`timeout`). A heavy command waiting in a busy
queue could be stopped there and lose its place. So for those tools the suggestion carries a
`--wait-timeout` a little below the limit (120 s → 108 s): calmrun exits 75 before the tool
stops it, keeps the place in line, and prints the `calmrun: next:` command that resumes it. The
deny text says so, shows the queue's load and, for kinds that usually take minutes (tests,
builds, containers, monorepo runs), offers the background form without the cap:

```
Heavy command (vitest) must go through the machine-wide calmrun queue so parallel agents do not freeze this computer. Run this instead:
calmrun --wait-timeout 108 -- npx vitest run
Queue now: 3 of 3 points in use, 1 waiting.
Exit 75 means it is still waiting in line: run the `calmrun: next:` command it prints to keep its place.
It may take minutes: better run it with run_in_background: true, without --wait-timeout:
calmrun -- npx vitest run
```

**Rewrite mode.** With `CALMRUN_HOOK_MODE=rewrite` in the agent's environment the hook changes
the command instead of denying it (all five agents support that). The default deny mode works
everywhere and keeps the change visible to the agent.

**Sandboxes.** calmrun keeps the queue in its state directory (`calmrun doctor` shows it). An
agent whose sandbox lets commands write only inside the project stops calmrun with `cannot lock
or update the queue state: Access is denied`. Allow that one folder (for Codex, see below)
rather than pointing `CALMRUN_HOME` into the project, which would give each project a queue of
its own.

### Claude Code

`calmrun hook claude` reads Claude Code's [PreToolUse](https://code.claude.com/docs/en/hooks)
input for the `Bash` and `PowerShell` tools. `calmrun setup --write --hook` adds it to
`~/.claude/settings.json` (or `$CLAUDE_CONFIG_DIR/settings.json`); for one project, add it to
`.claude/settings.json`:

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

- Deny: `permissionDecision: "deny"` with the text above. The wait cap follows the call's
  `timeout` (2 minutes when it has none); a call with `run_in_background: true` gets no cap.
- Rewrite: `updatedInput` with the wrapped command and no permission decision, so your normal
  permission rules still apply. Kinds that take minutes also get `run_in_background: true` and
  no cap; others get the capped command. In auto mode Claude Code may decline a changed input,
  in which case the original command runs unwrapped; the default deny mode does not have that
  gap.

### Codex

`calmrun hook codex` reads [Codex CLI hook](https://developers.openai.com/codex/hooks) input:
`tool_name` `Bash` with `tool_input.command` (a string; an argv array is accepted too).
`apply_patch` edits and MCP tools pass untouched. `calmrun setup --write --hook codex` adds
this to `~/.codex/hooks.json` (or `$CODEX_HOME/hooks.json`), next to any hooks already there
(for one project: `.codex/hooks.json`):

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [{ "type": "command", "command": "calmrun hook codex", "timeout": 30 }]
      }
    ]
  }
}
```

- Codex runs a new or changed hook only after you trust it: review it with `/hooks` in Codex.
- Deny: `permissionDecision: "deny"` with the reason. Codex's shell calls carry no time limit
  in the hook input, so no `--wait-timeout` is added.
- Rewrite: Codex takes a changed command only with `permissionDecision: "allow"` and
  `updatedInput`. That answer may also count as approval of the wrapped command, so use
  rewrite mode only if that is fine for you.
- The `workspace-write` sandbox blocks calmrun's state directory. `calmrun setup --hook codex`
  adds it to `~/.codex/config.toml` (only adding lines; the dry run shows them), or prints the
  lines to add when the file's layout is one it does not edit:

  ```toml
  [sandbox_workspace_write]
  writable_roots = ['C:\Users\you\AppData\Local\calmrun']
  ```
- On Windows Codex runs commands in PowerShell, so a quoted calmrun path is called with `&`.

### Antigravity (experimental)

**Experimental.** The hook's deny and rewrite work, but Antigravity runs `run_command` in a
terminal sandbox (a Windows AppContainer; on by default) that hides every other process and
blocks calmrun's state folder, so the wrapped command cannot join the machine-wide queue
there: calmrun refuses with a `next:` line rather than drop other agents' jobs it cannot see.
`setup --hook all` leaves Antigravity out; name it (`--hook antigravity`) to install it.

To use it, run Antigravity without the terminal sandbox: `auto` mode turns the sandbox on, so pick
another mode (for example `agy --mode accept-edits`) and keep `enableTerminalSandbox` off (the
default). Then the wrapped command joins the queue; add
`"permissions": {"allow": ["command(regex:.*calmrun.*)"]}` to
`~/.gemini/antigravity-cli/settings.json` so it runs without a prompt. Tested with agy 1.5.15 on
Windows 11. An allow rule alone does not help inside the sandbox: a `command` rule only skips the
prompt, and the per-command `unsandboxed(...)` rule is ignored on personal accounts (see
Antigravity's [sandbox](https://antigravity.google/docs/sandbox?tab=cli) and
[permissions](https://antigravity.google/docs/permissions) docs).

`calmrun hook antigravity` reads [Antigravity hook](https://antigravity.google/docs/hooks)
input for the `run_command` tool (`toolCall.args.CommandLine`). `calmrun setup --write --hook
antigravity` adds a `calmrun` entry to `~/.gemini/config/hooks.json`, next to the other named
entries (for one project: `.agents/hooks.json`):

```json
{
  "calmrun": {
    "PreToolUse": [
      {
        "matcher": "run_command",
        "hooks": [{ "type": "command", "command": "calmrun hook antigravity", "timeout": 30 }]
      }
    ]
  }
}
```

- Deny: `{"decision": "deny", "reason": "…"}`. A command still running after
  `WaitMsBeforeAsync` goes on in the background instead of being stopped, so no
  `--wait-timeout` is added.
- Rewrite: `{"decision": "allow", "overwrite": {"CommandLine": "…"}}`.
- On Windows `run_command` runs PowerShell, so a quoted calmrun path is called with `&`.

### OpenCode

OpenCode hooks are JavaScript plugins, so `calmrun setup --write --hook opencode` writes a small
one, `~/.config/opencode/plugins/calmrun.js`. Before the `bash` tool runs, the plugin sends the
call to `calmrun hook opencode`; on a deny it throws, and OpenCode shows the reason to the
model; in rewrite mode it changes `command`. If calmrun cannot be asked, the command runs
unchanged. The wait cap follows the call's `timeout` (2 minutes when it has none). Setup
replaces or removes the file only when it starts with calmrun's marker line.

### Pi

Pi hooks are extensions (TypeScript or JavaScript modules), so `calmrun setup --write --hook pi`
writes a small one, `~/.pi/agent/extensions/calmrun.js` (under `$PI_CODING_AGENT_DIR` when that
is set), where Pi finds it at its next start or `/reload`. Before the `bash` tool runs (or the
optional `powershell` tool on Windows), the extension's `tool_call` handler sends the call to
`calmrun hook pi`:

- Deny: `{"decision": "deny", "reason": "…"}`. The extension blocks the call and Pi shows the
  reason to the model.
- Rewrite: `{"decision": "rewrite", "input": {"command": "…"}}`. The extension changes the
  command in place.
- If calmrun cannot be asked, the command runs unchanged.

Pi's shell tool has no time limit unless the call sets `timeout` (in seconds), so the wait cap is
added only then. For the `powershell` tool a quoted calmrun path is called with `&`. Setup
replaces or removes the file only when it starts with calmrun's marker line.

### MCP server

`calmrun mcp` is an [MCP](https://modelcontextprotocol.io) server over stdio. Agents that speak
MCP can queue work without a shell wrapper. It offers five tools:

| Tool | Arguments | What it does |
| --- | --- | --- |
| `run_task` | `command`, and optionally `cwd`, `kind`, `cost`, `label`, `timeout_s`, `prio`, `wait_timeout_s`, `ticket`, `needs`, `wait_seconds`, `queue` | Waits in the same queue as the CLI, runs the command at low priority and returns the exit code, wait and run time, the last 200 lines of output and the path of the full log. When the job has not ended within `wait_seconds` (default 25), it answers at once with a `task_id` instead (see below). |
| `task_status` | `task_id`, optionally `output_offset`, `wait_seconds` | Where the task is: `waiting` (place in line, waiting count, points in use and budget, the waiting line, `running_jobs` with their hints, and `next` steps for one that looks stuck), `running` (with `cpu_pct`, `idle_ms`, `usual_ms`, `timeout_ms`), or `done` (exit code and status as `run_task` gives them); wait and run time; the output written since `output_offset` (at most the newest 32 KB) and `next_offset` for the next call. With `wait_seconds` it first waits up to that long for the end. |
| `cancel_task` | `task_id` | A waiting task leaves the queue; a running one is stopped with its whole process tree. Answers with how it ended. A finished task is left alone. |
| `queue_status` | `queue` | The same data as `calmrun status --json`, leases included. |
| `bump` | exactly one of `label`, `pos`, `ticket`, `dir`; optionally `back`, `queue` | Move matching waiting jobs to the front (or with `back`, to the end with prio low), as `calmrun bump` does. |

**Long jobs and tool timeouts.** Agents give an MCP call a few minutes at most; a job that waits
behind others or runs long would be cut off with its place lost. So `run_task` returns within
`wait_seconds`: with the result when the job ended by then (as before), else with
`{task_id, state: "waiting"|"running", place, used, budget, waiting_line, output_tail,
next_offset, …}` while the job keeps its place and goes on inside the server. Follow it with
`task_status` until `state` is `done`, or stop it with `cancel_task`. Short jobs still answer
in one call. A client that has no tool timeout and wants the old blocking call passes a large
`wait_seconds` (e.g. `86400`); `0` always returns a task id at once. Tasks live as long as the
server process; the last 100 finished ones stay queryable.

- While a task waits, the server sends progress notifications if the client passed a
  `progressToken`.
- `timeout_s` stops the command (exit 124; `0` = no limit); without it the config's `max_run_min` applies, as
  for the CLI. `wait_timeout_s` gives up waiting (exit 75) and
  returns a `ticket`; pass it back as `ticket` to resume the same place in line.
- Cancelling a `run_task` request (`notifications/cancelled`) while it still waits for its
  answer leaves the queue or stops the command, and, as the MCP spec asks, the cancelled
  request gets no response. Once it has answered with a task id, use `cancel_task`.
- Notes for the agent travel in the result (`notes`, and first in the text): for example a
  `ticket` that has expired, so the job joined the end of the line. A refused command names
  the `run_task` call to make instead.
- Full logs are kept under `logs/` in the state directory; the newest 200 are kept. While a
  task runs, `calmrun status` shows its log and when it last printed, and
  `calmrun logs <id> --follow` prints its output from a terminal.
- A `command` made of plain words runs the program directly (and gets the worker rules);
  leading `VAR=value` words become the command's environment, so `CI=1 npm test` works on
  Windows too. One that uses shell syntax (`&&`, pipes, redirects, `$VAR`, …) runs in `sh -c`
  on macOS/Linux and `cmd /d /s /c` on Windows. The output tail is in the text content and, for
  clients that show structured results, in `output_tail`.
- `calmrun mcp` stops its tasks (waiting and running, including those that already answered
  with a task id) on Ctrl+C / SIGTERM and when its client closes stdin. If it is killed outright,
  the command's process tree ends with it (a Job Object on Windows, the parent-death signal on
  Linux, a watchdog on macOS) and the next calmrun that reads the queue drops its entries.

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
| `pi` | `~/.pi/agent/AGENTS.md` (or `$PI_CODING_AGENT_DIR/AGENTS.md`) |

```sh
calmrun setup                         # show what would change (agents whose folder exists)
calmrun setup --write                 # apply
calmrun setup --write --agents codex  # only some agents
calmrun setup --write --hook          # also add the Claude Code hook to ~/.claude/settings.json
calmrun setup --write --hook codex,opencode,pi  # hooks of other agents (or --hook all)
calmrun setup --write --remove --hook # take both out again
```

- The block sits between `<!-- calmrun:start … -->` and `<!-- calmrun:end -->`. Running setup
  again replaces the block in place instead of adding a second one, and `--remove` deletes it
  and leaves the rest of the file as it was.
- Every file setup rewrites is first copied to `<file>.calmrun.bak`; a file that cannot be read
  or is not UTF-8 text is left alone (exit 1).
- `--hook` installs hooks in user-level files: `claude` (the default) in `settings.json`,
  `codex` in `hooks.json`, `antigravity` in `~/.gemini/config/hooks.json`, `opencode` as a
  plugin, `pi` as an extension; `all` installs every one but the experimental Antigravity hook (`--remove --hook all`
  removes it too). With `codex` it also lets Codex's sandbox write calmrun's state directory. Only calmrun's own entry is added, replaced or removed (a
  hook running `calmrun … hook <agent>`, Antigravity's `calmrun` entry, the plugin and extension
  files with calmrun's marker), so hooks of other tools stay as they are. A file it cannot parse is left
  untouched. An agent whose folder does not exist gets no hook (an error when named, a note
  under `all`). The hook command is `calmrun`, or calmrun's absolute path when it is not on
  `PATH`.
- When Codex or Pi has an `AGENTS.override.md`, the agent reads that file instead; setup says so,
  and you copy the block there too. Pi reads only one instructions file per folder, so when
  `~/.pi/agent` has a `CLAUDE.md` and no `AGENTS.md`, setup puts the block in `CLAUDE.md`.
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
| 124 | Stopped by its run time limit (`--timeout`, MCP `timeout_s`, `max_run_min`) |
| 126 | The command was found but could not be started |
| 127 | Command not found |
| 130 | Ctrl+C (while waiting or running), or stopped by `calmrun stop` |
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

**Is the job ahead working or hung?** If it runs with a log (`--log`, or an MCP task),
`calmrun status` says when it last printed and `calmrun logs <id> --follow` shows its output.
Run long jobs with `--log` (or `log = true`) to have that.

**A job I need now is stuck behind others.** `calmrun bump <label>` (or `--pos N` from
`calmrun status`), or run it with `--prio high`. A job that can wait: `calmrun bump --back
<label>`, or run it with `--prio low`.

**My agent's tool call timed out while waiting.** Use `--wait-timeout` (CLI) or
`wait_timeout_s` (MCP) and resume with the ticket, or raise the client's tool timeout (see the
MCP section).

**`refused: … is watch mode`.** Queued jobs must end. Run the `calmrun: next:` command it
prints, or run the watcher outside calmrun.

**A job was stopped with exit 137.** Its process tree passed `ceiling_gb`, or free RAM stayed
below `critical_free_gb` and it was the biggest job. Raise or disable the limit
(`CALMRUN_CEILING_GB=0`), or give the job fewer workers.

**A job runs for ages; is it hung?** `calmrun status` shows its CPU use, `idle` once its
processes have used no CPU for 2 minutes, and its usual run time. Idle with no output for a
long time usually means it hangs (unless it waits for docker or the network); stop it with
`calmrun stop <id>`.

**A job was stopped with exit 124.** It ran past its run time limit: 60 minutes for test-like
kinds by default. The message names the setting; raise `max_run_min` (globally or in
`[kinds.<name>]`), or pass `--timeout SECS` (`--timeout 0` = no limit) for that run.

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
