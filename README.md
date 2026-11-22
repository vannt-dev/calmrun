# calmrun

A machine-wide queue for heavy commands, so several coding agents and terminals on one computer
don't run `vitest`, `tsc`, builds, Playwright and `docker compose` all at once and freeze it.

```sh
calmrun -- npx vitest run
```

calmrun waits until the machine has room (budget points and free RAM), then runs the command
at low CPU priority and passes its exit code back. There is no daemon: every `calmrun` process
coordinates through a small state file guarded by an OS file lock.

Works on Windows, macOS and Linux (x64 and arm64).

## Why not just run the commands?

One test run is fine. Four agents each starting `vitest` with a worker per core, plus a `tsc`
and a build, is not: the machine swaps, the editor stalls, and every run gets slower than if
they had taken turns. calmrun makes them take turns:

| Without calmrun | With calmrun |
| --- | --- |
| Every agent starts its heavy command at once | Jobs start only while budget points are free |
| Runs pile up until RAM runs out | A job waits when free RAM is below a threshold |
| Two test runs share one `node_modules` / `target` and fight over caches | Same folder means one at a time |
| Test runners spawn a worker per core | Direct vitest calls get a small `--maxWorkers` |
| Commands compete with your editor at normal priority | Commands run at low CPU priority |
| A killed terminal can leave orphaned workers | The command's process tree dies with calmrun |

## Install

Prebuilt binaries are attached to each GitHub release, with installers:

```sh
# macOS / Linux
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/vannt-dev/calmrun/releases/latest/download/calmrun-installer.sh | sh
```

```powershell
# Windows (PowerShell)
powershell -ExecutionPolicy Bypass -c "irm https://github.com/vannt-dev/calmrun/releases/latest/download/calmrun-installer.ps1 | iex"
```

```sh
# npm (any OS)
npm install -g calmrun

# from source
cargo install --git https://github.com/vannt-dev/calmrun --locked
```

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
```

While waiting, calmrun prints one line to stderr, then one every 30 seconds:

```
calmrun: waiting — 4 of 6 points in use, 2nd in line (needs 2); running: npx vitest in web (3m10s), cargo build in api (1m02s)
calmrun: waiting for RAM: 2.1 GB free, needs 3.0 GB; running: npx playwright in e2e (4m55s)
```

## CLI

```
calmrun [--kind K] [--cost N] [--prio high] [--label L] [--wait-timeout SECS] [--ticket ID] -- <command...>
calmrun status [--json]
calmrun stats [--days N] [--json]
calmrun bump <label-substring>
calmrun doctor
calmrun hook claude
calmrun mcp
calmrun guide
calmrun init [--force]
```

- Separate words (`calmrun -- npx vitest run`) run the program directly. On Windows calmrun
  looks the name up on `PATH` with `PATHEXT`, so `npm`, `npx` and other `.cmd` shims work, and
  arguments are escaped for `cmd.exe`.
- One string containing spaces (`calmrun "a && b"`) runs through the shell: `cmd /d /s /c` on
  Windows, `sh -c` on Unix. calmrun never rewrites such a string.
- The exit code is the command's exit code (on Unix a signal `n` becomes `128 + n`). A missing
  command exits 127; a refused command exits 2.
- Ctrl+C reaches the command; a job still waiting leaves the queue and exits 130.
- `--wait-timeout SECS` caps the wait. If the job has not started by then, calmrun exits **75**
  (EX_TEMPFAIL) and keeps its place in line for 10 minutes. It prints the exact command that
  resumes that place, the same arguments with `--ticket <id>` in front:

  ```
  calmrun: not started within 120s (waiting — 6 of 6 points in use, 2nd in line (needs 2)); your place in line is kept for 10 min.
  calmrun: next: calmrun --ticket 17 --wait-timeout 120 -- npx vitest run
  ```

  While the owner is away, the kept place does not block anyone; when the ticket comes back
  it rejoins at its old position. An expired or unknown ticket joins the end of the line.
- Messages meant for agents say what happened and what to run next (`calmrun: next: …`), for
  example after a refused `docker compose up` or a command that is not on `PATH`.
  `calmrun guide` prints short instructions to paste into an agent's context.

| Exit code | Meaning |
| --- | --- |
| the command's | it ran |
| 2 | refused (e.g. attached `docker compose up`) |
| 75 | not started within `--wait-timeout`; place kept, see the `next:` line |
| 124 | stopped by the MCP `timeout_s` |
| 127 | command not found |
| 130 | Ctrl+C |
| 137 | stopped by the memory ceiling or the critical free-RAM floor |

## Rules

**Budget.** The machine has `budget` points (default: logical cores − 2, capped at
floor(RAM GB / 4), at least 2). Each job costs points by kind; `--cost` overrides, and a cost
above the budget counts as the whole budget.

| Kind | Detected from | Default cost |
| --- | --- | --- |
| `vitest` | `vitest` | 2 |
| `jest` | `jest` | 2 |
| `tsc` | `tsc`, `vue-tsc`, `tsgo`, `npm run typecheck` | 2 |
| `build` | `vite/next/ng build`, `webpack`, `rollup`, `esbuild`, `gradle(w)`, `mvn`, `cargo build/check/clippy`, `flutter build`, `dotnet build`, `go build`, `npm run build` | 2 |
| `capture` | `playwright`, `puppeteer`, `cypress` | 2 |
| `docker` | `docker compose up/build/run`, `docker-compose …`, `docker build/run` | 2 |
| `eslint` | `eslint`, `oxlint`, `biome`, `npm run lint` | 1 |
| `pytest` | `pytest`, `python -m pytest` | 1 |
| `test` | `cargo test`, `go test`, `flutter test`, `dotnet test`, `npm test` | 1 |
| `other` | anything else | 1 |

Detection looks at the command position only: the program name (basename, without
`.cmd/.bat/.exe/.ps1/.js`) after `VAR=value` prefixes and launchers such as `npx`, `pnpx`,
`bunx`, `pnpm exec`, `yarn exec`, `npm exec`, `uv run`, `python -m`. So
`git commit -m "fix vitest"` is not a vitest run. `--kind` overrides detection.

**Starting.** A job starts when its cost fits in the free points **and** free RAM is at least
`min_free_gb` (default max(3, RAM / 8) GB; 0 disables). The RAM check is skipped when nothing
is running, so a job never waits forever on an idle machine.

**Disk.** A job also waits while the drive of its working directory has less than
`min_free_disk_gb` free (default 5; `0` turns it off), and says so:
`calmrun: waiting for disk space: 3.2 GB free on C:\, needs 5.0 GB`. When nothing else is
running it starts anyway, with a warning, so it never waits for a drive it cannot clean up.

**Order.** `--prio high` first, then first come. A later job may pass earlier ones only if it
fits right now and no job ahead of it has waited longer than `pass_limit_min` (default 10;
0 = strict order). Running jobs are never stopped.

**Same folder, one at a time.** vitest, jest and build jobs take an exclusive key for their
nearest `node_modules` folder; Cargo builds and tests take one for their `target` folder (or
`CARGO_TARGET_DIR`). Two jobs with the same key never run together. Points and keys are taken
all-or-nothing.

**Command rules.**

- `docker compose up` / `docker-compose up` without `-d`, `--detach` or `--wait` is refused:
  it would hold points for as long as the containers run.
- vitest started directly as separate words (`vitest …`, `npx|pnpx|bunx vitest …`,
  `pnpm|yarn exec vitest …`) gets `--maxWorkers=N` appended unless it already passes
  `--maxWorkers`, `--pool` or `--no-file-parallelism`. N is `vitest_workers` (default 1 for
  up to 4 cores, 2 below 12, else 3). `npm run …` scripts and shell strings are never changed.
- gradle / gradlew started directly as words gets `--max-workers=N` unless it already sets
  `--max-workers` or `-Dorg.gradle.workers.max`.
- Cargo commands get `CARGO_BUILD_JOBS=N` unless it is already set.
- For both, N is the job's share of the machine: logical cores × cost ÷ budget (at least 1).

**Nesting.** calmrun sets `CALMRUN_HELD=<pid>` for the command. A calmrun started inside it
(for example a script that calls calmrun again) runs at once instead of queuing behind its own
parent.

**Low priority.** The command runs below normal priority on Windows and with `nice +10` in its
own process group on Unix. `CALMRUN_LOW_CPU=0` turns the priority change off.

**Memory ceiling.** While a job runs, calmrun adds up the memory of its whole process tree
(the command and every process below it, including nested calmrun runs) about once a second.
If the tree goes above `ceiling_gb` (default half the RAM; `0` turns it off), or free RAM
stays below `critical_free_gb` (default 5% of RAM, at most 1 GB) for 3 seconds and this job's
tree is the largest of the running jobs, calmrun stops the tree (gracefully, then forced) and
exits **137** with a message like:

```
calmrun: stopped npx vitest run: its processes used 9.1 GB, above the 8.0 GB ceiling
```

The stop is recorded in `history.jsonl` (`"status": "ceiling"` or `"low-memory"`), and
`status --json` shows each running job's `mem_bytes`.

**Process tree.** When the command exits normally, processes it deliberately left running
(a Gradle or Kotlin daemon, a dev server started in the background) are left alone. When the
run ends abnormally — Ctrl+C, or calmrun itself being killed — the whole tree is ended: on
Windows the command runs in a Job Object that kills the tree when calmrun's handle closes; on
Unix the command's process group is killed after Ctrl+C.

**History.** Each run appends one JSON line to `history.jsonl`: `at`, `label`, `kind`, `cost`,
`wait_ms`, `run_ms`, `status`. The label is `--label` or the tool and subcommand only
(`npx vitest run`, `cargo build`), never the arguments, which may contain secrets.

## Config

`calmrun init` writes a commented `config.toml` and prints its path. Every key is optional.

```toml
budget = 6
min_free_gb = 4
pass_limit_min = 10
vitest_workers = 2
ceiling_gb = 8          # 0 = off
min_free_disk_gb = 5    # 0 = off
critical_free_gb = 0.8  # 0 = off

[kinds.vitest]
cost = 3                         # override a built-in cost

[kinds.heavy]                    # add a kind with command patterns (first words)
cost = 2
patterns = ["make", "bazel build"]
```

### Environment

| Variable | Meaning |
| --- | --- |
| `CALMRUN_BUDGET` | Budget points (overrides `budget`) |
| `CALMRUN_MIN_FREE_GB` | Free RAM needed to start while busy; `0` disables |
| `CALMRUN_PASS_MIN` | Pass limit in minutes; `0` = strict order |
| `CALMRUN_VITEST_WORKERS` | `--maxWorkers` value for vitest |
| `CALMRUN_LOW_CPU` | `0` keeps normal priority |
| `CALMRUN_MIN_FREE_DISK_GB` | Free space needed on the cwd's drive; `0` = off |
| `CALMRUN_CEILING_GB` | Memory ceiling for one job's process tree; `0` = off |
| `CALMRUN_CRITICAL_FREE_GB` | Free-RAM floor that stops the biggest job; `0` = off |
| `CALMRUN_HOME` | State directory |
| `CALMRUN_CONFIG` | Config file path |
| `CALMRUN_HOOK_MODE` | `rewrite` makes the Claude Code hook wrap commands itself |

### Where state lives

| OS | Default state directory |
| --- | --- |
| Windows | `%LOCALAPPDATA%\calmrun` |
| macOS | `~/Library/Application Support/calmrun` |
| Linux | `$XDG_STATE_HOME/calmrun` or `~/.local/state/calmrun` |

It holds `state.json` (running and waiting jobs), `state.lock`, `history.jsonl` and
`config.toml`. Entries of processes that died are removed automatically: calmrun checks the
pid together with the process start time, so a reused pid is not mistaken for a live job.

## Coding agents

### Claude Code hook

`calmrun hook claude` is a
[PreToolUse hook](https://code.claude.com/docs/en/hooks). For the `Bash` and `PowerShell`
tools it checks each part of the command (split on `&&`, `||`, `;`, `|`, `&` and new lines).
If a part runs a heavy kind and is not already wrapped (`calmrun …`, `npx calmrun …`,
`lane with …`), the hook denies the call and tells Claude the exact command to run instead,
with each heavy part wrapped in place:

```
cd web && npm ci && npx vitest run      →      cd web && npm ci && calmrun -- npx vitest run
```

Light commands (`git`, `ls`, `cat`, `npm install`, …) are never blocked, heredoc bodies (for
example a commit message written with `<<'EOF'`) are not treated as commands, and any input
the hook does not understand is allowed (no output, exit 0). Wrappers such as `VAR=1`, `time`
or `nice` stay in front: `time npx vitest` becomes `time calmrun -- npx vitest`.

Add it to `.claude/settings.json` in a project, or to `~/.claude/settings.json` for every
project:

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

`calmrun mcp` is an [MCP](https://modelcontextprotocol.io) server over stdio. Agents that
speak MCP can queue work without a shell wrapper. It offers three tools:

| Tool | What it does |
| --- | --- |
| `run_task` | `{command, cwd?, kind?, cost?, label?, timeout_s?, prio?, wait_timeout_s?, ticket?}`. Waits in the same queue as the CLI, runs the command at low priority and returns the exit code, wait and run time, the last 200 lines of output and the path of the full log (kept under `logs/` in the state directory; the newest 200 are kept). While it waits, the server sends progress notifications if the client passed a `progressToken`. `timeout_s` stops the command (exit 124). `wait_timeout_s` gives up waiting (exit 75) and returns a `ticket`; pass it back as `ticket` to resume the same place in line. Cancelling the request leaves the queue or stops the command. |
| `queue_status` | The same data as `calmrun status --json`. |
| `bump` | `{label}`: move matching waiting jobs to the front. |

A `command` made of plain words runs the program directly (and gets the vitest
`--maxWorkers` rule); one that uses shell syntax (`&&`, pipes, redirects, `$VAR`, …) runs in
`sh -c` on macOS/Linux and `cmd /d /s /c` on Windows.

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
// Cursor: ~/.cursor/mcp.json   ·   Gemini CLI: ~/.gemini/settings.json
{
  "mcpServers": {
    "calmrun": { "command": "calmrun", "args": ["mcp"] }
  }
}
```

Long test runs can outlast a client's default tool timeout; raise it where the client allows
(Gemini CLI: a `timeout` in milliseconds on the server entry).

### Instructions for agents

Add this to `CLAUDE.md`, `AGENTS.md` or your agent's instruction file:

```markdown
## Heavy commands

This machine is shared by several agents. Run heavy commands through the calmrun queue:
`calmrun -- <command>` (tests, type checks, linters, builds, Playwright, docker compose,
pytest, cargo/gradle builds). Example: `calmrun -- npx vitest run`.
Run anything that may take more than a minute or two in the background and check on it later.
Use `calmrun status` to see what is running or waiting. Do not wrap light commands
(git, ls, cat, package installs).
```

## Per-OS notes

- **Windows.** `.cmd` and `.bat` programs run through `cmd.exe /d /s /c` with every argument
  quoted and caret-escaped for both cmd parses a batch shim performs, so arguments such as
  `a&b`, `100%` or `"quoted"` arrive unchanged. `.ps1` files run with
  `powershell -NoProfile -ExecutionPolicy Bypass -File`. Ctrl+C reaches the command through the
  shared console; calmrun turns Ctrl+C processing back on if its parent had it disabled. A
  command ended by Ctrl+C exits 130.
- **macOS / Linux.** The command gets its own process group. When calmrun runs in the
  foreground of a terminal, that group becomes the terminal's foreground group (so the command
  can read input and gets Ctrl+C directly) and the terminal is handed back afterwards; SIGINT,
  SIGTERM and SIGHUP that calmrun receives are forwarded to the group as SIGINT.
- **Memory figures.** calmrun counts resident memory: the working set on Windows and the
  resident set size (RSS) on macOS and Linux. Shared pages count once per process, so the
  tree total can be higher than what the OS reports as used; the default ceiling leaves room.
- **What CI covers.** The test suite runs on Windows, macOS and Linux (GitHub-hosted x64 and
  arm64 runners): queue rules, exit codes, Ctrl+C while running and while waiting, and daemons
  surviving a normal exit, and the memory ceiling stopping a nested tree. The critical
  free-RAM floor is covered by unit tests only (it depends on the machine). Killing calmrun ending the tree is tested on Windows only. The
  terminal hand-over is not covered by CI (runners have no terminal).
- **Battery.** `calmrun doctor` reports whether the machine runs on battery; consider a smaller
  budget then.

## Development

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## License

[MIT](LICENSE)
