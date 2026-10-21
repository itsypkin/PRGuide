# Repository rules

These are the rules every change to this repo follows.

[`.agents/PRGuide/03-design.md`](.agents/PRGuide/03-design.md) is the source of truth for behaviour; [`.agents/PRGuide/04-plan.md`](.agents/PRGuide/04-plan.md) has the tasks, the layout and the waves. Linking to `.agents/` is allowed; editing or committing anything there is not.

## Toolchain

The toolchain is pinned in `rust-toolchain.toml` to 1.99.1 with rustfmt and clippy. Local builds and CI both use that file; a bump is its own PR.

## Gate

Run these commands from the repository root:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## Rules for every task

- **The gate:** `cargo fmt --check`, `cargo clippy --all-targets -- -D
  warnings`, `cargo test`. All three must pass.
- **No `unwrap`/`expect` outside tests.**
- **Errors are module-local.** Each module has its own `thiserror` enum that
  converts into the top-level `Error`. Every variant maps to a stable
  snake_case `code` via `code()`. Codes are part of the agent contract:
  never rename one.
- **Agent CLI output:** JSON on stdout. Errors are JSON on stderr with
  `{"code": "...", "message": "..."}`, and exit 1. The only exception is
  `check` (T3-3). Never print prose to stdout from an agent command.
- **No network in tests.** GitHub goes through the `GitHub` trait's fake.
  Git uses fixture repos built inside the test (T2-2's `FixtureRepo`).
- **Facts and event state are written in one SQLite transaction.** Any
  write that records a fact and changes an event's state does both in one
  transaction (§2.5, §2.6).
- **Stay inside your slot.** A task edits its own module files plus the
  stub slot that T0-1 created for it. If a task needs to change a shared
  file (`Cargo.toml`, `lib.rs`, `cli/mod.rs`, `error.rs`,
  `viewer/app.rs`), keep the change minimal and say so in the PR.
- Don't commit anything under `.agents/`.

## Fixed choices

These are decided, so tasks don't stop to ask:

- `clap` (derive), `serde`, `serde_json`, `thiserror`.
- `rusqlite` with the `bundled` feature.
- `fd-lock`, `ulid`, `toml`, `tempfile`, `assert_cmd`, `insta`.
- YAML: **`serde-saphyr`** (2.0.x). `serde_yaml` is deprecated, and
  `serde_yml` is pre-1.0.
- Markdown: **`pulldown-cmark`** with our own ratatui renderer. No
  `tui-markdown`.
- Diagrams: **`mmdflux = "=2.6.1"`**, `default-features = false`.
- Viewer: `ratatui` + `crossterm`.
- GitHub: **github.com only in v1.** Any other host fails with
  `unsupported_host`. The REST API version header is pinned to the version
  current at implementation time, which was `2026-03-10` when this plan was
  written.
- **One executable, two names.** Cargo builds a single `[[bin]]`,
  `prguide`. The `prg` name is a symlink: release archives and the Homebrew
  formula ship it, and `prguide link-prg` creates it for `cargo install`
  users (T8-2).
- **Skill directories:**
  - Claude Code: `~/.claude/skills/prguide/`;
  - omp: `~/.agents/skills/prguide/`. omp scans `~/.agents/skills` by
    default, and Codex reads the same directory, which helps L-2 later.

The exact version requirements live in `crates/prguide/Cargo.toml`.
