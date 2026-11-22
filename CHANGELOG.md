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
