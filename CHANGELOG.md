# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

## [0.1.0] - Unreleased

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
- `calmrun mcp`: an MCP server over stdio with `run_task` (same queue as the CLI, captured
  log, last 200 lines, progress notifications, timeout, cancellation), `queue_status` and
  `bump`.
- Daemons a command leaves running on purpose (Gradle, Kotlin) survive a normal exit; the
  tree is ended only after Ctrl+C or when calmrun is killed.
- A command ended by Ctrl+C on Windows exits 130; Ctrl+C works even when the parent started
  calmrun with Ctrl+C disabled.
