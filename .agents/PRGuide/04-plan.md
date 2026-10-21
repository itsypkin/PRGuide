# 04 — Implementation plan: prguide v1

Based on `03-design.md`, approved by the owner on 2026-10-21. These were
made to the design after approval, and are reflected here:
- two resume prompts in §2.5;
- the PR diff for posting is computed locally in §2.7;
- **Codex support is out of v1** (§1.2 and §7, item 4). v1 targets Claude
  Code and omp. Codex is L-2.
- **No multiplexer integration** (§2.9 and §7, item 5). `prguide open` runs a
  user-configured command or prints the viewer command; the store records
  nothing about the terminal.

The repo is greenfield: only `LICENSE`, `README.md` and a Rust `.gitignore`
exist at `main`. This plan was reviewed by the reviewer agent and revised.

Tasks follow the design's build order ([§4](03-design.md#4-build-order)).
Each task names its design sections, its dependencies, the files it mainly
creates, and the tests that mark it done. Paths are relative to the repo
root.

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

## Layout

T0-1 creates all of this as stubs:

```
Cargo.toml                    workspace, member crates/prguide
crates/prguide/
  Cargo.toml                  lib + one [[bin]] prguide; all dependencies declared up front
  src/main.rs                 argv[0] dispatch: "prg" → viewer, else CLI
  src/lib.rs                  declares every module below
  src/error.rs                top-level Error, code()
  src/cli/mod.rs              the full clap tree from design §3.3; one file per command
  src/store/                  schema, migrations, runs, events, threads, deepdives, outbox, versions, posts, status
  src/git/                    runner, refs, worktree, diff (spec + parser)
  src/github/                 trait, gh impl, fake
  src/anchors/                blocks, stored anchors, mapping, inline locations
  src/guide/                  model, parse, range, new, check, commit
  src/render/                 Renderer trait, mmdflux adapter, limits
  src/config.rs               ~/.prguide/config.toml (open_command)
  src/launcher.rs             runs the configured open command; no multiplexer code
  src/viewer/                 app (state + key routing), components…
  tests/                      integration tests; tests/support/ helpers
skills/prguide/SKILL.md
skills/prguide/writing-guides.md
```

## Waves

At most 3 tasks run in parallel, and each wave needs the previous one
merged. The viewer tasks (T6-x, T7-1) form one serial chain, because they
all touch the app state and key routing. Everything else runs alongside
them.

| Wave | Tasks |
|---|---|
| A | T0-1 |
| B | T1-1, T2-1, T2-2 |
| C | T1-2, T2-3, T5-1 |
| D | T1-3, T2-4, T2-5 |
| E | T1-4, T3-1, T4-5 |
| F | T3-2, T7-2 |
| G | T3-3 |
| H | T3-4, T5-2 |
| I | T4-1, T4-4, T6-1 |
| J | T4-2, T4-3, T6-2 |
| K | T6-3, T8-1 |
| L | T6-4 |
| M | T6-5, T4-6 |
| N | T6-6 |
| O | T6-7 |
| P | T6-8 |
| Q | T6-9 |
| R | T6-10, T8-2 |
| S | T7-1 |
| T | T7-3 |
| U | T7-4 |
| V | T7-5 |
| W | T8-3 |

Later, not v1 blockers: L-1 (syntax highlighting) and L-2 (Codex support).

---

## Phase 0 — Scaffold

### T0-1. Workspace scaffold, CLI tree and conventions

- Create the workspace, `crates/prguide` with a library and one `[[bin]]`
  `prguide`, and every module file from the layout as a stub.
- Declare every dependency from Fixed choices in `Cargo.toml` now, so
  parallel tasks don't edit it.
- `main.rs` dispatches on the file name of `argv[0]`. `prg` runs the viewer
  entry point (a stub for now). Anything else runs the clap CLI, where
  `prguide view` also calls the viewer entry point.
- **The whole clap tree from design §3.3**, with every argument and flag:
  - `start`, `guide new`, `check`, `guide commit`, `open`, `wait`, `show`;
  - `reply`, `deepdive`, `draft-question`, `ack`, `threads`, `status`;
  - `end`, `skill install`, `gc`, `link-prg`, `view`.
  Each command has its own file in `src/cli/` with a handler that returns
  `not_implemented`. Later tasks fill in a handler without touching
  `cli/mod.rs`.
- `error.rs`: the top-level `Error`, with `code()`, and `From` impls for each
  module error (stub enums are fine). Start with the codes
  `not_implemented`, `usage` and `internal`.
- `cli/output.rs`: `print_json(&T)`, and `fail(err) -> !`, which writes the
  JSON error to stderr and exits 1. clap usage errors are mapped to
  `code: "usage"`.
- CI (GitHub Actions): the gate on `ubuntu-latest` and `macos-latest`.
- README: one paragraph on what prguide is, and "work in progress".

Files: everything in the layout, `.github/workflows/ci.yml`, `README.md`.

Tests:
- a symlink named `prg` pointing at the built binary runs the viewer stub;
- every subcommand from the list parses and returns `not_implemented` as
  JSON;
- an unknown command gives `code: "usage"` and exit 1.

- Design ref: [§2 Architecture](03-design.md#2-architecture), [§3.3](03-design.md#33-the-agent-cli)
- Depends on: none

---

## Phase 1 — Store, runs and events

### T1-1. Store: paths, the full v1 schema and migrations

**Paths.**
- A PR store lives at `$PRGUIDE_HOME` (default `~/.prguide`) +
  `/prs/github.com/<owner>/<repo>/<n>/`. Directories are created on demand.
- Reject owner and repo names that contain `/`, `..` or NUL.

**Connection.**
- WAL on, `busy_timeout` 5 s, `foreign_keys` on.
- `Store::open(pr_ref)` opens or creates the store, `store.tx(|tx| …)` runs
  a transaction, and `store.data_version()` returns `PRAGMA data_version`
  (T6-1 uses it).

**Migrations.**
- An ordered list of SQL scripts, keyed by `PRAGMA user_version`.
- A store whose version is higher than the binary knows fails with
  `store_too_new`.
- This task writes migration 1, the whole v1 schema below. Later tasks add
  no tables, unless the schema has a gap, in which case they add a new
  migration and say why.

Schema v1 (`TEXT` timestamps are RFC 3339 UTC; ids are ULIDs unless
stated):

```sql
store_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL)
  -- keys: host, owner, repo, number, git_common_dir

runs(id TEXT PRIMARY KEY,                       -- "r_" + 12 base32 chars
     state TEXT NOT NULL CHECK (state IN ('active','stopped','expired')),
     created_at TEXT NOT NULL, lease_until TEXT NOT NULL,
     ended_at TEXT, end_reason TEXT)            -- quit | end | takeover | expired
CREATE UNIQUE INDEX one_active_run ON runs(state) WHERE state = 'active';

guide_versions(version INTEGER PRIMARY KEY,
     state TEXT NOT NULL CHECK (state IN ('staging','ready')),
     caused_by TEXT UNIQUE REFERENCES events(id),
     snapshot_base TEXT NOT NULL, snapshot_head TEXT NOT NULL,
     selection_kind TEXT NOT NULL CHECK (selection_kind IN ('full','commits')),
     selection_base TEXT NOT NULL, selection_head TEXT NOT NULL,
     commits_json TEXT NOT NULL,                -- ordered SHAs
     refs_json TEXT NOT NULL,                   -- [{name, sha}] the version pins
     created_at TEXT NOT NULL, ready_at TEXT)

drafts(id TEXT PRIMARY KEY, run_id TEXT NOT NULL REFERENCES runs(id),
     path TEXT NOT NULL, provenance_json TEXT NOT NULL, created_at TEXT NOT NULL)

threads(id TEXT PRIMARY KEY, created_at TEXT NOT NULL,
     origin_version INTEGER NOT NULL REFERENCES guide_versions(version),
     slide_id TEXT NOT NULL,
     anchor_json TEXT)                          -- StoredAnchor (T2-5); NULL = whole slide

messages(id TEXT PRIMARY KEY, thread_id TEXT NOT NULL REFERENCES threads(id),
     author TEXT NOT NULL CHECK (author IN ('reviewer','agent')),
     body TEXT NOT NULL, created_at TEXT NOT NULL)

events(seq INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT NOT NULL UNIQUE,
     type TEXT NOT NULL CHECK (type IN ('comment','deepdive_request','generate_request')),
     state TEXT NOT NULL CHECK (state IN ('pending','claimed','done')),
     claimed_by TEXT REFERENCES runs(id), claimed_at TEXT,
     thread_id TEXT REFERENCES threads(id),     -- comment, deepdive_request
     message_id TEXT REFERENCES messages(id),   -- comment
     range_first TEXT, range_last TEXT,         -- generate_request; both NULL = full
     result_kind TEXT, result_id TEXT, done_at TEXT,
     created_at TEXT NOT NULL)
CREATE INDEX events_open ON events(state, seq);

deepdives(id TEXT PRIMARY KEY,
     version INTEGER NOT NULL REFERENCES guide_versions(version),
     slide_id TEXT NOT NULL, created_at TEXT NOT NULL,
     UNIQUE (version, slide_id))
deepdive_sections(id TEXT PRIMARY KEY,
     deepdive_id TEXT NOT NULL REFERENCES deepdives(id),
     ord INTEGER NOT NULL, body TEXT NOT NULL,
     caused_by TEXT NOT NULL UNIQUE REFERENCES events(id),
     created_at TEXT NOT NULL, UNIQUE (deepdive_id, ord))

outbox_items(id TEXT PRIMARY KEY,
     kind TEXT NOT NULL CHECK (kind IN ('author_question','promoted_answer')),
     thread_id TEXT NOT NULL REFERENCES threads(id),
     message_id TEXT REFERENCES messages(id),   -- the promoted agent message
     body TEXT NOT NULL,
     state TEXT NOT NULL CHECK (state IN ('draft','dropped','posting','posted')),
     post_id TEXT REFERENCES posts(id), gh_comment_id INTEGER,
     created_at TEXT NOT NULL, updated_at TEXT NOT NULL)

posts(id TEXT PRIMARY KEY,
     state TEXT NOT NULL CHECK (state IN ('in_flight','unknown','done','failed')),
     head_sha TEXT NOT NULL, plan_json TEXT NOT NULL,   -- T7-3's placement plan
     gh_review_id INTEGER, error TEXT,
     created_at TEXT NOT NULL, updated_at TEXT NOT NULL)
CREATE UNIQUE INDEX one_open_post ON posts((1)) WHERE state IN ('in_flight','unknown');

notices(seq INTEGER PRIMARY KEY AUTOINCREMENT, kind TEXT NOT NULL,   -- guide_ready
     ref TEXT NOT NULL, created_at TEXT NOT NULL)

gh_threads(id TEXT PRIMARY KEY, updated_at TEXT NOT NULL, json TEXT NOT NULL,
     fetched_at TEXT NOT NULL)
```

**Results.** An event's result is recorded on the event row itself
(`result_kind`, `result_id`). The values of `result_kind` are `message`,
`deepdive_section`, `outbox_item`, `guide_version` and `ack`. That is how a
repeated command finds what it produced the first time (T1-3).

Files: `src/store/{mod,schema,migrations}.rs`.

Tests:
- a fresh store reaches version 1;
- reopening is a no-op;
- `store_too_new`;
- each `CHECK` and unique index rejects a bad row (a second active run, a
  second open post);
- path rejection.

- Design ref: [§2.1](03-design.md#21-where-things-live), [§2.5](03-design.md#25-the-live-loop), [§2.6](03-design.md#26-threads-and-deep-dives), [§2.7](03-design.md#27-the-outbox-and-posting)
- Depends on: T0-1

### T1-2. Runs and the lease

These are library functions only; the CLI comes in T2-4.

- **`start_run(store, takeover) -> Run`.**
  - If an `active` run has a valid lease and `takeover` is false, fail with
    `run_active`, including that run's id.
  - With `takeover`, stop the old run (`end_reason = takeover`) in the same
    transaction.
  - An `active` run whose lease has passed is expired first.
- **The lease** is 30 minutes. `require_active(run_id)` is the gate for every
  call that carries a run id, all in one transaction:
  - lease passed → set `expired` and send its claims back to `pending`;
  - stopped or expired → `run_stopped`;
  - otherwise → renew `lease_until`.
- **`stop_run(run_id, reason)`** sets `stopped`, `ended_at` and `end_reason`,
  and sends its claims back to `pending`, in one transaction.
- **Time** comes from an injectable `Clock`, so tests can move it forward.

Files: `src/store/runs.rs`.

Tests:
- start, then a second start (`run_active`), then takeover;
- calls after a stop return `run_stopped`;
- the fake clock expires a run and sends its claims back;
- a start after a stopped run works without takeover;
- an expired active run doesn't block a start.

- Design ref: [§2.5](03-design.md#25-the-live-loop) (Runs and the consumer)
- Depends on: T1-1

### T1-3. Events, claims, `wait`, results and `ack`

**Writing events.** `append_comment(thread, message)`,
`append_deepdive_request(thread)` and `append_generate_request(first?,
last?)` are what prg uses. They insert `pending` rows.

**`wait(run, timeout, max)`**:
- loop every 250 ms; call `require_active` at most every 60 s, which also
  renews the lease;
- if the run has stopped: return `{"stopped": true}`;
- otherwise, collect this run's `claimed` events plus `pending` ones, in
  `seq` order, up to `max` (default 10). If there are any, mark the pending
  ones `claimed_by` this run and return them;
- if the timeout passes: return `{"events": []}`;
- while blocked, hold an exclusive `fd-lock` on `<store>/wait.lock`. If it's
  held already: `wait_active`.

Each returned event is:

```json
{"id": "…", "seq": 7, "type": "comment",
 "thread": "…", "message": "…", "range": null}
```

**The result protocol.** `with_result(run, event_id, kind, |tx| ->
result_id)`:
- check `require_active`;
- the event must exist (`event_unknown`) and be claimed by this run
  (`event_not_claimed`);
- if it is already `done`: return the stored `(result_kind, result_id)`
  without calling the closure. The command then loads that object and
  prints it as if it had just been created;
- otherwise run the closure, and set `done`, `result_kind`, `result_id` and
  `done_at`, in the same transaction.

**`ack(run, event_id)`** goes through `with_result` with `kind = ack`.

**CLI:** `prguide wait --run R [--timeout D] [--max N]` (durations like
`4m` and `30s`; no timeout means forever) and `prguide ack --run R <event>`.

Files: `src/store/events.rs`, `src/cli/{wait,ack}.rs`.

Tests:
- ordering and the batch bound;
- the timeout;
- stopping the run while `wait` is blocked returns `stopped` within 1 s;
- a concurrent second `wait` gets `wait_active`;
- a duplicate result returns the first one and doesn't call the closure;
- claims go back to pending on stop and on expiry, and the next run gets
  them;
- events appended with no active run reach the next run.

- Design ref: [§2.5](03-design.md#25-the-live-loop)
- Depends on: T1-2

### T1-4. Agent status and `prguide status`

- **`agent_status(store) -> Listening | Working | NotListening { pending,
  active_run: Option<RunId> }`.**
  - **Listening:** `wait.lock` is held. Probe it with a non-blocking
    try-lock, released at once.
  - **Working:** an active run has claimed events.
  - **NotListening:** anything else. `active_run` tells T6-10 which resume
    prompt to show. The 10-second debounce is the viewer's job.
- **`prguide status --run R`** prints the run, the versions, the status and
  the pending count. It goes through `require_active`, so it is also the
  heartbeat.

Files: `src/store/status.rs`, `src/cli/status.rs`.

Tests: each status, driven by holding the lock, claiming events, and
stopping or expiring the run.

- Design ref: [§2.5](03-design.md#25-the-live-loop) (Agent status)
- Depends on: T1-3

---

## Phase 2 — Git, GitHub and anchors

### T2-1. GitHub core: trait, `gh` implementation and fake

- **The trait.** `trait GitHub` with the read methods T2-4, T6-7 and T8-1
  need. T7-2 and T7-5 extend it.
  - `pr(repo, n) -> Pr`: `gh api repos/{o}/{r}/pulls/{n}`. `Pr` has `number`,
    `state` (open or closed), `merged_at`, `closed_at`, `head_sha`,
    `head_repo`, `base_ref`, `base_sha`, `title`, `body`, `author` and
    `url`.
  - `commits(repo, n) -> Vec<Commit>`: `gh api --paginate
    repos/{o}/{r}/pulls/{n}/commits`. `Commit` has `sha`, `parents`,
    `message` (first line plus body) and `author`. GitHub caps this list at
    250 commits; at 250 fail with `pr_too_large`.
- **Every call** sends `-H "X-GitHub-Api-Version: <pinned>"` and
  `--hostname github.com`. Other hosts fail with `unsupported_host`.
- **`GhCli`** parses the JSON with serde structs.
  - Missing `gh`: `gh_missing`.
  - `gh auth status` failing: `gh_unauthenticated`.
  - Exit code non-zero with HTTP 404: `pr_not_found`.
  - Anything else: `gh_failed`, with stderr.
- **`FakeGitHub`** is in memory and scriptable: canned responses, injected
  errors, delays.

Files: `src/github/{mod,gh,fake}.rs`, `tests/fixtures/gh/*.json`.

Tests:
- parsing the recorded fixtures;
- error mapping, using a fake `gh` script on `PATH` that exits with chosen
  codes and output;
- `pr_too_large`.

- Design ref: [§2.2](03-design.md#22-git-and-github)
- Depends on: T0-1

### T2-2. Git wrapper: clones, fetch, pinned refs, show and the diff spec

- **Running git.** `Git` runs `git -c core.quotepath=off -C <dir> …` and
  captures stdout and stderr. A non-zero exit becomes `git_failed`, with the
  command and stderr.
- **Clone detection.** `git rev-parse --git-common-dir --show-toplevel`.
  `remote_for(repo)` matches remotes by URL, in https and ssh forms, with or
  without `.git`, case-insensitively for the owner and repo. No match:
  `not_in_clone`.
- **`fetch(remote, refspecs)`.** The PR head is
  `pull/<n>/head:refs/prguide/<n>/fetch/head`, and the base is
  `<base_ref>:refs/prguide/<n>/fetch/base`. Both are force-updated.
- **Refs.** `write_refs`, `verify_refs` (returns which are missing or wrong)
  and `delete_refs`, using `git update-ref --stdin`.
- **Reading.** `show(sha, path) -> Option<Vec<u8>>`, `merge_base(a, b)` and
  `rev_parents(sha)`.
- **The diff spec.**
  `DiffSpec::ThreeDot { base_tip, head } | TwoDot { from, to }`, with an
  optional path filter. It always adds `--no-ext-diff --no-textconv
  --no-color -M -U3`, and `-z` is not used.
- **`FixtureRepo`**, a test builder: scripted commits, branches, renames,
  deletions, binary files, mode changes, merges, a bare "remote" with
  `refs/pull/<n>/head`, and a second clone of the same remote.

Files: `src/git/{mod,runner,refs}.rs`, `tests/support/fixture_repo.rs`.

Tests:
- the remote-URL variants;
- fetching from the fixture remote;
- writing, verifying and deleting refs;
- `git gc --prune=now` keeps the objects that a pinned ref points to.

- Design ref: [§2.2](03-design.md#22-git-and-github)
- Depends on: T0-1

### T2-3. The diff model, snapshot and selection

- **The patch parser.** It accepts git's output for the `DiffSpec` options:
  - `diff --git` headers, with paths quoted C-style;
  - `new file mode`, `deleted file mode`, and `old mode`/`new mode`;
  - `similarity index` with `rename from` and `rename to`;
  - `index` lines;
  - `Binary files … differ`;
  - `Subproject commit` (submodules, treated like a binary stub);
  - `@@` hunk headers with optional counts;
  - `\ No newline at end of file`.
  It produces files with `path`, `old_path`, `status` (added, modified,
  deleted, renamed), `binary`, `mode_only` and `submodule`, and hunks with
  their starts, counts and lines (kind plus text). Anything else is
  `diff_parse`, with the line number.
- **`Snapshot { base, head }`.** `head` is the fetched PR head, and `base` is
  `merge_base(head, fetched base tip)`.
- **`Selection::Full`.** The diff is `ThreeDot { base_tip, head }`.
- **`Selection::Commits { first, last }`.**
  - Both must be in the PR's commit list, with `first` at or before `last`
    (`range_not_in_pr`).
  - No commit in the run may have more than one parent
    (`range_has_merge`).
  - `selection.base = first^`, and the diff is `TwoDot { first^, last }`.

Files: `src/git/diff.rs`, `src/guide/range.rs`.

Tests:
- one parser test per grammar item;
- full and range diffs on the fixture repo;
- a merge commit is fine in a full guide and rejected in a range;
- a reversed or foreign range is rejected.

- Design ref: [§2.3](03-design.md#23-guide-versions-snapshot-and-selection)
- Depends on: T2-2

### T2-4. `prguide start`

`prguide start <pr> [--takeover]`:

1. Find the clone and the remote for the PR's repo (T2-2). Get `pr` and
   `commits` (T2-1).
2. Open or create the store (T1-1). On create, write the `store_meta` keys,
   including the git common dir. If the stored common dir differs from the
   current one, fail with `store_bound`, including the bound path.
3. Fetch the head and the base (T2-2).
4. Worktree:
   - `<store>/worktree` missing: `git worktree add --detach <path> <head>`;
   - otherwise, in it: `git checkout --detach --force <head>`, then
     `git clean -fd`.
5. `start_run(takeover)` (T1-2).
6. Print the run id, the store path, the worktree path, the PR title, body
   and author, the commits, and the changed files of the full selection
   (path, status, +/-), using T2-3, plus the ready guide versions.

Files: `src/cli/start.rs`, `src/git/worktree.rs`.

Tests (fixture repo + fake GitHub):
- a first start and a resume;
- a second clone fails with `store_bound`;
- a dirty worktree is reset;
- the reviewer's own working tree and HEAD are untouched;
- `run_active` and `--takeover`.

- Design ref: [§1.1](03-design.md#11-a-review-end-to-end), [§2.1](03-design.md#21-where-things-live), [§2.2](03-design.md#22-git-and-github)
- Depends on: T1-2, T2-1, T2-2, T2-3

### T2-5. Anchors, line mapping and GitHub inline locations

**Code blocks.** `CodeBlock` is one of:
- `Diff { id, path, side, start?, end? }`. `start` and `end` may be left out
  only for binary, mode-only and submodule files.
- `Context { id, path, at, start, end }`, where `at` is `snapshot-head`
  (the default), `selection-head` or `selection-base`.

**`resolve(block, version) -> ResolvedBlock`.**
- A diff block gives its whole overlapping hunks. A context block gives the
  file's lines at the revision.
- Errors: `path_not_in_diff`, `no_hunks_in_range`, `lines_out_of_range`,
  `range_required`, `range_not_allowed`.
- Warning: `context_overlaps_hunk`.

**`StoredAnchor`**:

```
{version, slide, block, kind,
 path, old_path?, side?,     // side for diff blocks only
 rev,                        // resolved SHA
 start, end,
 lines: [text]}
```

**`map_lines(anchor, to_rev) -> Option<MappedRange { path, start, end,
rev }>`.**
- It follows the hunks of `git diff anchor.rev to_rev` with rename detection
  for that path, so the path is updated across renames.
- `None` if any line in the range was changed or deleted, or if the file
  was deleted.

**`inline_location(mapped, side, pr_diff) -> Option<InlineLoc { path,
line, side, start_line?, start_side? }>`** gives a GitHub-valid inline
position:
- every line must be inside one hunk of the PR's three-dot diff (added or
  context lines for `RIGHT`, removed or context for `LEFT`);
- a range must lie within a single hunk, on one side;
- otherwise `None`.

Files: `src/anchors/{mod,block,mapping,inline}.rs`.

Tests:
- each resolution result;
- mapping across an insertion above the range, an edit inside it, a rename
  (the path changes) and a deletion;
- the identity mapping;
- `inline_location` for a single line, a range inside one hunk, a range
  across two hunks (`None`), a line outside every hunk (`None`), and the
  `LEFT` side.

- Design ref: [§2.4](03-design.md#24-code-anchors), [§2.7](03-design.md#27-the-outbox-and-posting)
- Depends on: T2-3

---

## Phase 3 — Guides

### T3-1. Guide model and parsing

- **`guide.yaml`**, parsed with `serde-saphyr` into `GuideManifest {
  format, pr, snapshot, selection, slides: Vec<SlideId> }`.
- **Slide files.**
  - YAML front matter between `---` lines: `id`, `kind` (`overview`,
    `architecture` or `step`), `title`, `code[]` (T2-5's `CodeBlock`) and
    `flags[] { at: { block, line? }, text }`.
  - Then a CommonMark body.
- **The body, parsed with `pulldown-cmark`.**
  - Any raw HTML (block or inline) is the error `html_not_allowed`.
  - `architecture` slides may contain at most one fenced `mermaid` block and
    at most one fenced `text` block. More is `duplicate_diagram_block`.
    Those blocks are pulled out of the body.
  - Other kinds treat those fences as ordinary code blocks.
- **Path hardening.**
  - Slide files must be exactly `slides/<id>.md`, where an `id` matches
    `[a-z0-9][a-z0-9-]{0,63}`.
  - No absolute paths, no `..` and no symlinks anywhere in the draft
    (checked with `symlink_metadata`).
  - Code-block paths are relative, normalised, without `..`.
- **Errors.** Each one carries `file`, `field` (for example `code[1].start`)
  and `code`.

Files: `src/guide/{model,parse}.rs`.

Tests: round-tripping the design's example slide; each error code; path
and symlink cases; HTML in the body; duplicate diagram blocks.

- Design ref: [§3.1](03-design.md#31-the-guide-format)
- Depends on: T2-5

### T3-2. `prguide guide new`

`prguide guide new --run R [--commits A..B]`:
- computes the snapshot and selection (T2-3) from freshly fetched refs (it
  fetches first);
- moves the worktree to the snapshot head;
- inserts a `drafts` row;
- creates `drafts/<draft id>/guide.yaml` with the provenance and `slides:
  []`, plus an empty `slides/`;
- prints the draft path, the selection's changed files, and the selected
  commits.

Files: `src/cli/guide_new.rs`, `src/guide/new.rs`.

Tests: full and range drafts have the right provenance; range errors pass
through; the worktree moves.

- Design ref: [§1.1](03-design.md#11-a-review-end-to-end), [§2.3](03-design.md#23-guide-versions-snapshot-and-selection), [§3.1](03-design.md#31-the-guide-format)
- Depends on: T2-4, T3-1

### T3-3. `prguide check` (without diagrams)

**Output.** `prguide check <draft> --run R` prints `{"ok": bool,
"diagnostics": [{severity, file, field, code, message}]}` on stdout,
whatever the outcome. It exits 0 when there are no errors, and 1
otherwise.

**Checks.**
- Parse errors (T3-1).
- The provenance must equal the draft's `drafts` row (`provenance_changed`).
- Deck shape:
  - `slides` is non-empty and every entry has a file
    (`slide_file_missing`);
  - ids are unique (`duplicate_slide_id`);
  - the first slide is `overview` (`overview_first`);
  - there is at least one `step` (`no_steps`);
  - there is no `architecture` slide (warning `no_architecture`);
  - a slide file not listed in `slides` (warning `unlisted_slide`).
- Within a slide, block ids are unique (`duplicate_block_id`).
- Flags point at existing blocks (`flag_block_unknown`), with lines inside
  the block (`flag_line_out_of_range`).
- Every code block resolves (T2-5).

Leave a `check_diagrams` hook for T5-2.

Files: `src/guide/check.rs`, `src/cli/check.rs`.

Tests: a valid draft passes; one failing fixture per code; a tampered
provenance is caught.

- Design ref: [§3.1](03-design.md#31-the-guide-format)
- Depends on: T2-5, T3-2

### T3-4. `prguide guide commit` and reconciliation

`prguide guide commit <draft> --run R [--caused-by E]` implements §3.2:

1. **Reserve a version.** In one transaction, check the run, and if
   `caused_by` is given, check that it is a claimed `generate_request`
   whose range equals the draft's selection
   (`event_command_mismatch`). Then insert the `guide_versions` row in
   state `staging`, with `refs_json` listing the four pinned refs
   `refs/prguide/<n>/v<k>/{snapshot-base,snapshot-head,selection-base,selection-head}`.
2. **Stage and check.** Copy the draft to `staging/v<k>/` and run `check` on
   the copy. On errors, delete the row and the copy, and fail with
   `check_failed` and the diagnostics.
3. **Pin.** Write the refs.
4. **Publish.** Rename `staging/v<k>` to `guides/v<k>`.
5. **Finish.** In one transaction:
   - the row becomes `ready`;
   - the event, if any, becomes `done` with `result_kind = guide_version`;
   - a `notices(kind = guide_ready, ref = k)` row is inserted.
   Without `caused_by`, the transaction is the same, minus the event.

**Reconciliation** runs on every `Store::open` and on a repeated commit with
the same `caused_by`, for each `staging` row:
- if `guides/v<k>` exists, verify the refs, re-create any that are missing,
  and run step 5;
- otherwise, delete `staging/v<k>` and every ref in `refs_json`, then delete
  the row.

A repeated commit whose event is already `done` returns that version. A
test-only fault hook (`cfg(test)`) crashes after each step.

Files: `src/guide/commit.rs`, `src/cli/guide_commit.rs`.

Tests:
- a crash after each of steps 1–4, followed by reconciliation, ends either
  `ready` or cleanly rolled back, with no orphan refs and no stray
  directories;
- the repeat returns the same version;
- a failing check refuses the commit;
- `event_command_mismatch`.

- Design ref: [§3.2](03-design.md#32-committing-a-guide), [§2.2](03-design.md#22-git-and-github)
- Depends on: T1-3, T2-2, T3-3

---

## Phase 4 — Agent result commands, launcher and skill

**Rules for every result command:**
- Each one passes through `require_active` and `with_result` (T1-3).
- The command must match its event, or fail with `event_command_mismatch`:
  - `reply` and `draft-question` take the thread of a claimed `comment` or
    `deepdive_request` event;
  - `deepdive` takes the slide of that event's thread, in that thread's
    origin version.

### T4-1. `show`, `reply` and local `threads`

- **`show --run R <event>`.** The event must be claimed by this run. It
  prints the event, its thread with all messages, the thread's stored
  anchor, and the slide id. For a `generate_request`, it prints the range.
- **`reply --run R <thread> --caused-by E (--text T | --file F)`.** It
  inserts an `agent` message, with `result_kind = message`.
- **`threads --run R`.** All threads in the store, with their origin version,
  anchor and messages. `--github` returns `not_implemented` until T7-5.

Files: `src/cli/{show,reply,threads}.rs`, `src/store/threads.rs`.

Tests: happy paths; a repeat returns the same message; replying on another
thread is `event_command_mismatch`; `show` of an unclaimed event fails.

- Design ref: [§2.6](03-design.md#26-threads-and-deep-dives), [§3.3](03-design.md#33-the-agent-cli)
- Depends on: T1-3, T2-5, T3-4

### T4-2. `deepdive`

`deepdive --run R <slide> --caused-by E --file F`, in one transaction:
- the slide must be a main slide of a ready version, and match the event
  (Phase 4 rules);
- create the `deepdives` row if it is missing;
- append a section at `ord = max + 1`;
- append an agent message to the causing thread: "Added to the deep dive of
  '<slide title>' (section N)";
- `result_kind = deepdive_section`.

Files: `src/cli/deepdive.rs`, `src/store/deepdives.rs`.

Tests: creating, then appending; the repeat; a non-main slide; a slide
mismatch.

- Design ref: [§2.6](03-design.md#26-threads-and-deep-dives)
- Depends on: T4-1

### T4-3. `draft-question`

`draft-question --run R <thread> --caused-by E --file F`:
- inserts an `outbox_items` row with `kind = author_question` and `state =
  draft`;
- appends an agent message to the thread: "Drafted a question for the author
  (in the outbox)";
- `result_kind = outbox_item`.

Files: `src/cli/draft_question.rs`, `src/store/outbox.rs`.

Tests: happy path; the repeat; a mismatch.

- Design ref: [§2.7](03-design.md#27-the-outbox-and-posting)
- Depends on: T4-1

### T4-4. `end`

- `end --run R [--clean]` runs `stop_run(reason = end)`.
- With `--clean`, it also runs `git worktree remove --force <store>/worktree`
  and `git worktree prune`.

Tests: a blocked `wait` returns `stopped`; every later command gives
`run_stopped`; `--clean` removes the worktree.

- Design ref: [§2.5](03-design.md#25-the-live-loop), [§3.3](03-design.md#33-the-agent-cli)
- Depends on: T1-3, T2-4

### T4-5. Config and `open`

prguide has no built-in knowledge of any multiplexer (design §2.9).
**Nothing in `src/` may name a multiplexer, read a multiplexer-specific
environment variable, or call a multiplexer binary.** The test below
enforces this.

- **Config.** `$PRGUIDE_HOME/config.toml` (default `~/.prguide/config.toml`)
  is optional and parsed with `toml` into `Config { open_command:
  Option<String> }`. Unknown keys are a warning on stderr, not an error. The
  env var `PRGUIDE_OPEN_CMD` overrides `open_command`.
- **`open --run R`:**
  - with an open command: run it with `sh -c`, with `PRGUIDE_VIEW_CMD` set to
    the viewer command, for example `prguide view --run r_…` (absolute path
    of the running binary, shell-quoted). It is non-blocking: it waits at
    most 5 s for the command to exit.
    - exit 0 → `{"opened": true}`;
    - non-zero → `open_failed`, with the exit code and stderr;
    - still running after 5 s → `{"opened": true}`, with the process
      detached;
  - without one: `{"opened": false, "command": "prg <n>", "view_command":
    "prguide view --run <R>"}`.

Files: `src/config.rs`, `src/launcher.rs`, `src/cli/open.rs`.

Tests:
- the configured command receives `PRGUIDE_VIEW_CMD` (a script that writes
  its env to a file);
- the env var overrides the config file;
- a failing command gives `open_failed`;
- a command still running after 5 s;
- no config prints the command;
- **a grep test fails if any file under `crates/prguide/src/` contains
  `herdr`, `tmux`, `zellij`, `wezterm` or `TMUX`/`HERDR_` (case-insensitive
  for the names).**

- Design ref: [§2.9](03-design.md#29-opening-the-viewer)
- Depends on: T2-4

### T4-6. The skill and `prguide skill install`

- **`skills/prguide/SKILL.md`** has front matter (`name: prguide`, and a
  description that triggers on "review / guide me through PR …"). Its body
  covers, in this order:
  - the session flow (§1.1), with `--run` on every command;
  - opening the viewer: run `prguide open`. On `opened: false`, open a pane
    running `view_command` with whatever tools you have for the reviewer's
    terminal, such as a multiplexer skill. If you have none, ask the reviewer
    to run `command` in another pane. Never type into a pane the reviewer is
    using;
  - the per-harness loop instructions, with exact parameters:
    - Claude Code: a Bash tool call with `run_in_background: true` and no
      timeout;
    - omp: `bash` with `async: true, timeout: 0`;
    - any other harness: tell the reviewer that this harness isn't supported
      yet, and stop. Don't fall back to polling in v1;
  - the loop rules, and stopping only on `{"stopped": true}`;
  - the heartbeat: `prguide status` at least every 10 minutes during long
    work;
  - triage;
  - the trust rules (§2.10);
  - never post and never approve.
- **`skills/prguide/writing-guides.md`** covers:
  - the format by example;
  - the narrative order;
  - the diagram rules (≤ 12 nodes, TD for long chains, a sequence diagram
    for call chains);
  - context blocks and flags;
  - running `check` until it is clean.
- **`skill install [--harness claude|agents] [--dir PATH] [--force]`.**
  - The files are embedded with `include_str!`.
  - Default (no flag): install to both `~/.claude/skills/prguide/` and
    `~/.agents/skills/prguide/`. `--harness claude` or `--harness agents`
    installs to just one; `agents` is the directory omp reads.
  - If the target already has files: when they are identical, it's a no-op;
    when they differ, fail with `skill_conflict` unless `--force` is given.
  - Print what was written.

Files: `skills/prguide/*`, `src/cli/skill.rs`.

Tests:
- `--dir` writes both files byte-for-byte;
- the conflict and force cases;
- the front matter parses;
- every `prguide` command and flag that `SKILL.md` names exists in the clap
  tree (introspect `Command`).

- Design ref: [§3.5](03-design.md#35-the-skill), [§2.5](03-design.md#25-the-live-loop), [§2.10](03-design.md#210-trust)
- Depends on: T4-1, T4-2, T4-3, T4-4, T4-5, T5-2

---

## Phase 5 — Diagrams

### T5-1. Renderer trait, mmdflux adapter and limits

- **The trait.** `trait Renderer { fn render(&self, src, opts) ->
  Result<TextDiagram, RenderError> }`.
  - `RenderOpts` holds the charset (unicode or ascii) and an optional
    `root_direction` override.
  - `TextDiagram` holds the lines, width and height.
- **`MmdfluxRenderer`.** It detects and validates the diagram type through
  mmdflux's API, which handles front matter, comments and directives. The
  supported types are `flowchart`/`graph` and `sequenceDiagram`; anything
  else is `Unsupported(kind)`.
- **Limits**, checked before rendering (constants in `render/limits.rs`):
  - 8 KiB of source;
  - 40 nodes and 80 edges;
  - 12 participants;
  - subgraph nesting of 3;
  - labels of 60 characters;
  - 64 KiB of output.
  Exceeding any of them is `TooLarge(which)`. Rendering runs under
  `catch_unwind`, and a panic becomes `Panic`.
- **LR→TD.** The override changes only the root diagram's direction
  (`LR`/`RL` → `TD`), never a subgraph's own `direction`. Implement it as a
  source rewrite of the root header if mmdflux has no option for it.
- **`render_cache_key(src, w, h, charset)`** hashes these together with
  `"mmdflux-2.6.1"`.
- **The fixture corpus**, `tests/fixtures/mermaid/`:
  - an architecture diagram with nested subgraphs and labels;
  - cross-boundary edges;
  - LR and TD chains;
  - a sequence diagram with `alt` and `loop`;
  - malformed input;
  - an unsupported type;
  - one over each limit.
  Every rendering is an `insta` snapshot, in both charsets.

Files: `src/render/{mod,mmdflux,limits}.rs`, `tests/fixtures/mermaid/*`.

Tests: the corpus; each limit; detection with front matter and comments;
subgraph directions preserved under LR→TD.

- Design ref: [§2.8](03-design.md#28-diagrams), `02-research-mermaid-terminal.md`
- Depends on: T0-1

### T5-2. Diagrams in `check`

- **The reference viewport** is 76 × 36 content cells: an 80 × 40 pane minus
  the frame. prg uses the real viewport (T6-9).
- **Rules for each `architecture` slide:**
  - neither a `mermaid` nor a `text` block: error `no_diagram`;
  - a supported type must render in both charsets (`diagram_render`, with
    the renderer's error);
  - wider than 76: warning `diagram_wide`, with the width;
  - over the cap (wider than `min(2 × 76, 200)` = 152, or taller than `3 ×
    36` = 108) even after LR→TD: error `diagram_too_large`;
  - an unsupported type passes only with a `text` block
    (`unsupported_needs_text`).
- **The preview.** Each diagnostic includes up to 40 lines of the
  rendering, or the error.

Files: `src/guide/check.rs` (the `check_diagrams` hook).

Tests: one draft fixture per rule.

- Design ref: [§2.8](03-design.md#28-diagrams)
- Depends on: T5-1, T3-3

---

## Phase 6 — The viewer (`prg`), one serial chain

### T6-1. Viewer runtime

- **Entry.** `prg [<n>]` and `prguide view [--run R]`.
  - Find the store from the current clone. With a number, use that PR's
    store. Without one, use the most recently used store bound to this
    clone. None found: exit with a message saying how to start a review.
- **The terminal.** crossterm alternate screen and raw mode, with a panic
  hook and signal handling that always restore the terminal.
  - On SIGHUP or SIGTERM, restore and exit without stopping the run.
  - `q` calls `stop_run(reason = quit)`, then exits.
- **App architecture**, which every later viewer task plugs into:
  - an `App` state;
  - a `Component` trait (`handle_key`, `render`, `on_store_change`);
  - a key-routing table where components register their bindings;
  - a modal focus stack (editor, pan mode, overlays).
  Document it in `src/viewer/README.md`.
- **Store polling** every 250 ms on `PRAGMA data_version`. On a change, the
  store is reloaded into `App`, then components get `on_store_change`.
  Nothing redraws while the version stays the same.
- **Header:**
  - the repo and PR;
  - the version and its range;
  - the slide position;
  - the agent status (T1-4). "Not listening" shows only after the state has
    lasted 10 s.
- **Footer:** the key hints. `?` opens a help overlay generated from the
  routing table.

Files: `src/viewer/{mod,app,component,keys,header,footer,terminal}.rs`,
`src/viewer/README.md`.

Tests (`TestBackend`):
- header snapshots for each status;
- the debounce, with a fake clock;
- `q` stops the run;
- a simulated SIGHUP leaves it active;
- the store version change triggers exactly one reload.

- Design ref: [§3.4](03-design.md#34-the-viewer), [§1.1](03-design.md#11-a-review-end-to-end) step 9
- Depends on: T1-4, T3-4

### T6-2. Markdown and slide navigation

- **The Markdown renderer** turns `pulldown-cmark` events into ratatui
  `Text`. It covers:
  - headings (levels 1–3 styled, deeper ones as level 3);
  - emphasis and strong;
  - inline code;
  - ordered and unordered lists, nested two levels;
  - fenced and indented code blocks;
  - links (text plus a dim URL);
  - block quotes;
  - horizontal rules.
  Tables are shown as their raw source, monospaced. Text wraps at the pane
  width.
- **Navigation.** ↑/↓ moves through the main deck, j/k and PgUp/PgDn scroll
  within a slide, and Home/End go to the first and last slide. ←/→ are
  reserved for deep dives (T6-6).

Files: `src/viewer/{markdown,slide,nav}.rs`, `tests/fixtures/markdown/*`.

Tests: a fixture per construct, snapshotted at 80 and 120 columns;
navigation through a 3-slide deck; scrolling a long slide.

- Design ref: [§3.4](03-design.md#34-the-viewer), [§3.1](03-design.md#31-the-guide-format)
- Depends on: T6-1

### T6-3. Code blocks: diffs, context and flags

- **Diff hunks:** a file header (with `old → new` for renames), old and new
  line numbers, and +/- colouring.
- **Context:** a file header marked "unchanged @ <rev label>" and dimmed
  line numbers.
- **Stubs:** binary, mode-only and submodule files get a one-line stub.
- **Flags** show as `⚑ text` at the end of the flagged line, or under the
  block when they have no line.
- **Colour** is off under `NO_COLOR`, and markers keep the meaning without
  it.

Files: `src/viewer/code.rs`.

Tests: snapshots of each block kind, a rename header, line and block flags,
and `NO_COLOR`.

- Design ref: [§2.4](03-design.md#24-code-anchors), [§3.4](03-design.md#34-the-viewer)
- Depends on: T6-2, T2-5

### T6-4. Writing comments

- **Selecting.** `v` starts a line selection inside a code block, j/k extend
  it, and Esc cancels.
- **Writing.** `c` opens the inline editor for the selection, or for the
  slide when nothing is selected. The editor is multi-line: Ctrl-S submits,
  Esc cancels after confirming, and Ctrl-E moves the text to `$EDITOR` in a
  temp file and back.
- **Submitting** writes, in one transaction:
  - a new thread with its `StoredAnchor` (T2-5), or no anchor for a slide
    comment;
  - the reviewer message;
  - a `comment` event (T1-3).
- **`D`** on a code line or slide with an existing thread asks for a deep
  dive (`deepdive_request`). Without a thread, it first opens the editor to
  write the question.
- **A redraw never changes the editor's buffer, cursor or the selection.**
  Store changes only update other components.

Files: `src/viewer/{select,editor,compose}.rs`.

Tests:
- the submitted rows for a line, a range and a slide comment;
- the `D` events;
- a store change between key events leaves the editor untouched.

- Design ref: [§2.6](03-design.md#26-threads-and-deep-dives), [§3.4](03-design.md#34-the-viewer)
- Depends on: T6-3, T4-1

### T6-5. Threads panel and live replies

- **Markers.** Anchored lines show a 💬 marker with a count, and a slide
  shows its slide-level thread count.
- **The thread view.** Enter on a marker, or `t` then a selection, opens
  it:
  - the messages, with the author;
  - live updates as agent messages arrive;
  - `r` to reply, which goes through T6-4's editor and writes a `comment`
    event on the existing thread.
- **`t`** opens the session-wide list of threads across versions, with
  their origin version.

Files: `src/viewer/threads.rs`.

Tests: thread-view snapshots; an agent message arriving updates the open
view; a reply writes a message plus an event.

- Design ref: [§2.6](03-design.md#26-threads-and-deep-dives)
- Depends on: T6-4

### T6-6. Deep dives

- **Navigation.** ← from a main slide opens its deep dive, if one exists,
  with its sections in order. → returns.
- **The marker.** The parent shows "◀ deep dive".
- **Growth.** When a section is added while the reviewer is viewing the deep
  dive, the scroll position is kept and "new section ↓" is shown.

Files: `src/viewer/deepdive.rs`.

Tests: navigation snapshots; the scroll position kept on append.

- Design ref: [§2.6](03-design.md#26-threads-and-deep-dives)
- Depends on: T6-5, T4-2

### T6-7. Versions, the commit picker and new guides

- **The GitHub refresh service** (shared with T7-5).
  - When it runs: on start, on `r` (when no thread view is open), and on
    terminal focus gained (crossterm focus events), at most once every 60
    s.
  - What it fetches: `pr` and `commits` (T2-1).
  - On failure, it keeps the last result and shows "offline".
- **`V`** opens the version list (number, range, slide count, created), and
  Enter switches to a version.
- **`g`** opens the commit picker, which lists the PR's commits.
  - The reviewer selects a contiguous run with Space and shift-arrows;
    merge commits can't be selected.
  - Enter appends a `generate_request` with the range. Choosing "all
    commits" sends one with no range.
- **The new-commits banner.** When the refreshed head is past the newest
  version's `snapshot_head`, show "N new commits since vK". Its key opens
  the picker with those commits preselected.
- **New guides open automatically.** A new `guide_ready` notice whose
  version was caused by this viewer's own `generate_request` switches to it
  at once. Any other new version shows "vK ready — press V".

Files: `src/viewer/{versions,picker,refresh}.rs`.

Tests:
- non-contiguous runs and merges are refused;
- the `generate_request` payloads;
- the banner logic;
- the auto-open on the matching notice;
- the 60 s throttle;
- the offline state.

- Design ref: [§2.3](03-design.md#23-guide-versions-snapshot-and-selection), [§1.1](03-design.md#11-a-review-end-to-end) step 7
- Depends on: T6-6, T2-1, T3-4

### T6-8. Threads from other versions

- **Mapping.** For the version being viewed, map each other version's thread
  with `map_lines(anchor, current version's rev for that side)` (T2-5).
  Mapped threads show on their line, with an origin tag ("from v1").
  Slide-level threads from other versions are never mapped.
- **The list.** Threads that don't map are listed under "threads from other
  versions" on the overview slide, and selecting one switches to its origin
  version and slide.
- **Caching.** Mapping results are cached per (thread, version) until the
  store changes.

Files: `src/viewer/crossversion.rs`.

Tests: mapped, unmapped and renamed cases; jumping to the origin.

- Design ref: [§2.6](03-design.md#26-threads-and-deep-dives), [§2.4](03-design.md#24-code-anchors)
- Depends on: T6-7, T2-5

### T6-9. Diagram slides

- **Rendering.** The `mermaid` block is rendered with T5-1 at the real
  content viewport, cached by T5-1's key.
- **Overflow below the cap:** `p` enters pan mode, where the arrows and
  h/j/k/l pan and Esc leaves, and a "→ more" hint shows.
- **Above the cap:** re-render with LR→TD and show an "auto-layout LR→TD"
  tag. If it's still over the cap, fall back.
- **The fallback:** the text block, otherwise the raw Mermaid with a one-line
  error.
- **`m`** cycles diagram → text (if any) → source.

Files: `src/viewer/diagram.rs`.

Tests: snapshots of a fitting diagram, a pannable one, an auto-flipped
one, each fallback, and the `m` cycle.

- Design ref: [§2.8](03-design.md#28-diagrams)
- Depends on: T6-8, T5-1

### T6-10. Not listening and the resume prompts

- **The banner.** When the status is `NotListening` (debounced), show the
  pending count, and a key `y` that copies the right resume prompt from
  design §2.5 with the real values:
  - `active_run = None`: "prguide: the reviewer is back on #<n>. Run
    `prguide start <n>`, then listen with `prguide wait`."
  - `active_run = Some(R)`: "prguide: you stopped listening on #<n>. Run
    `prguide wait --run <R>` and continue the loop."
- **Copying.** Try `pbcopy`, `wl-copy`, then `xclip -selection clipboard`.
  If none works, use OSC 52. Then show "copied".
- **With no active run**, commenting and generate requests still work (T6-4
  and T6-7), and the events wait as pending.

Files: `src/viewer/{listening,clipboard}.rs`.

Tests: both prompt texts; clipboard selection with fake binaries on
`PATH`; a comment written with no run leaves a pending event.

- Design ref: [§2.5](03-design.md#25-the-live-loop) (Agent status)
- Depends on: T6-9, T1-2

---

## Phase 7 — Outbox, posting and GitHub threads

### T7-1. The outbox

- **`o`** opens the outbox: author questions (T4-3) and promoted answers.
  Each item can be edited (T6-4's editor), dropped or restored. Items in
  `posting` or `posted` are read-only.
- **`P`** in a thread view promotes the selected agent message. It creates a
  `promoted_answer` item whose body is fixed as:

  ```
  > <the reviewer's message that the answer replies to>

  Answer from the reviewer's prguide agent:

  <agent message>
  ```

- **`A`** in a thread view turns the thread into an author question. It
  opens the editor with a draft made of the thread's last reviewer message,
  plus a quote of the anchored code.

Files: `src/viewer/outbox.rs`, `src/store/outbox.rs` (extending T4-3).

Tests: promote, edit, drop and restore; the attribution text exactly as
above; read-only states.

- Design ref: [§2.7](03-design.md#27-the-outbox-and-posting)
- Depends on: T6-10, T4-3

### T7-2. GitHub posting API

Extend `GitHub` (T2-1):

- **`create_review(repo, n, ReviewRequest { commit_id, body, event:
  "COMMENT", comments: [{path, body, line, side, start_line?, start_side?}]
  }) -> ReviewCreated { id }`.** `POST repos/{o}/{r}/pulls/{n}/reviews`.
  An HTTP timeout or 5xx is `gh_unknown_outcome`. A 4xx is
  `gh_rejected`, with GitHub's message.
- **`review_comments(repo, n, review_id) -> Vec<{id, path, line, body}>`.**
  `GET …/pulls/{n}/reviews/{id}/comments`, paginated.
- **`list_reviews(repo, n) -> Vec<{id, body, commit_id, submitted_at}>`.**
  `GET …/pulls/{n}/reviews`, paginated, with the raw `body`.
- **Fake support.** `FakeGitHub` records requests and can simulate success,
  rejection, and "timeout, but the review was created".

Files: `src/github/{mod,gh,fake}.rs`, `tests/fixtures/gh/*`.

Tests: request serialisation; pagination; error mapping.

- Design ref: [§2.7](03-design.md#27-the-outbox-and-posting)
- Depends on: T2-1

### T7-3. Placement plan and preview

**`plan_post(items, head, pr_diff) -> PostPlan`** is pure and
deterministic, given these inputs:
- `head` is the PR head from a fresh `pr` call (T2-1), after a `git fetch`
  of the head and the base (T2-2);
- `pr_diff` is the three-dot diff `base...head`, computed locally (T2-3);
- for each item with a code anchor, `map_lines(anchor, head)` (T2-5)
  followed by `inline_location` (T2-5):
  - `Some` → inline;
  - `None` → body, with a reason: `slide_comment`, `context_code`,
    `changed_since_vK`, `deleted_since_vK` or `outside_pr_diff`;
- items whose anchor revision isn't `head` are tagged "mapped from vK@sha7",
  which compares each item's own origin with the head.

**The body**, in order:
- the slide-level and moved items, each with `path:line@sha7` (when it has
  a code anchor), up to 5 quoted code lines, and the item body;
- `Review notes from prguide.`, used only when the body would otherwise be
  empty;
- the marker `<!-- prguide:<post id> -->`.

**The preview** in prg lists the inline items and the body items, each with
its reason. Enter confirms, and Esc cancels.

Files: `src/github/plan.rs`, `src/viewer/post_preview.rs`.

Tests:
- a mixed-version outbox, covering every reason;
- the empty-body default;
- the marker;
- the preview snapshot;
- determinism (the same inputs give the same plan).

- Design ref: [§2.7](03-design.md#27-the-outbox-and-posting), [§2.4](03-design.md#24-code-anchors)
- Depends on: T7-1, T2-2, T2-3, T2-5

### T7-4. Submitting and recovering a post

The post state machine:

- **Confirm** (one transaction): insert `posts(state = in_flight, head_sha,
  plan_json)`, and set every planned item to `posting` with its `post_id`.
  The `one_open_post` index refuses a second open post (`post_in_progress`).
- **Submit** with `create_review` (T7-2):
  - success → `review_comments` to fetch comment ids by `(path, line,
    body)`. Then, in one transaction, the post becomes `done` and the items
    `posted`, with their `gh_comment_id`;
  - `gh_rejected` → the post becomes `failed`, with the error, and the items
    go back to `draft`;
  - `gh_unknown_outcome` → the post becomes `unknown`.
- **Reconcile an `unknown` post.** This happens automatically when prg
  starts, and on demand. Call `list_reviews` and look for the marker:
  - found → treat it as success;
  - not found → offer a retry, which reuses the same post id and plan.
  Items stay `posting` (read-only) the whole time.

Files: `src/viewer/post.rs`, `src/store/posts.rs`.

Tests (fake GitHub):
- success stores the ids;
- rejection returns the items to `draft`;
- timeout, then marker found, ends `done`;
- timeout, no marker, then retry sends the same post id;
- a second post while one is open is refused;
- prg starting with an `unknown` post reconciles it.

- Design ref: [§2.7](03-design.md#27-the-outbox-and-posting)
- Depends on: T7-3, T7-2

### T7-5. GitHub threads, read-only

- **The API.** Extend `GitHub` with `review_threads(repo, n)`, using GraphQL
  through `gh api graphql`, paginated on threads and on comments:
  - `pullRequest(number).reviewThreads(first: 100, after: $c)`, with
    `pageInfo`;
  - for each thread: `id`, `isResolved`, `isOutdated`, `path`, `line`,
    `startLine`, `diffSide`, `originalLine`;
  - for its comments (`first: 100`, `after`): `id`, `databaseId`,
    `author.login`, `body`, `createdAt`, `replyTo.databaseId`,
    `originalCommit.oid`, `commit.oid`.
  Check the field names against the current schema and record the date.
- **Caching.** Cache the threads in `gh_threads`. They are fetched by T6-7's
  refresh service.
- **Mapping.** Threads whose commit is the version's head, or which map to it
  through `map_lines`, show read-only on their line, marked "GitHub". Replies
  to the reviewer's posted comments are linked through
  `outbox_items.gh_comment_id` and shown under that item in the outbox and
  the thread view. Threads that don't map are listed on the overview slide.
- **The CLI.** `prguide threads --github` returns the cached threads with
  their mapping.

Files: `src/github/{threads,mod,gh,fake}.rs`,
`src/viewer/github_threads.rs`, `src/cli/threads.rs`.

Tests: pagination; mapping cases; reply linkage to a posted item; the CLI
output.

- Design ref: [§2.7](03-design.md#27-the-outbox-and-posting) (GitHub threads)
- Depends on: T7-4, T6-7

---

## Phase 8 — Housekeeping, packaging and end to end

### T8-1. `prguide gc`

- **Which PRs.**
  - Default: stores whose PR is closed, with `closed_at` (from `pr`, T2-1)
    more than 30 days ago.
  - `--closed-older-than D` changes the age.
  - `--pr N` selects one store, whatever its state.
  - `--dry-run` lists what would be removed.
- **Never** touch a store with an `active` run whose lease is valid.
- **Order**, where each step is idempotent so a rerun finishes the job:
  1. `git worktree remove --force` in the bound clone;
  2. `git worktree prune`;
  3. delete `refs/prguide/<n>/**`;
  4. remove the store directory, last.
- **A missing bound clone.** Skip steps 1–3 with a warning, and keep the
  store, unless `--force` is given, in which case delete it.

Files: `src/cli/gc.rs`.

Tests: everything is removed; a rerun after a failure at each step; an
active run blocks it; a missing clone with and without `--force`; dry run.

- Design ref: [§2.1](03-design.md#21-where-things-live)
- Depends on: T2-4, T3-4

### T8-2. Packaging and release

- **`prguide link-prg [--dir D]`** creates the `prg` symlink next to the
  running executable (or in `D`). An existing `prg` that points elsewhere is
  `link_conflict`.
- **Release workflow** (GitHub Actions, on a tag):
  - targets `aarch64-apple-darwin`, `x86_64-apple-darwin`,
    `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl`;
  - each archive contains `prguide` and a `prg -> prguide` symlink, plus
    checksums.
- **The Homebrew formula** template, for a tap repo named by the owner:
  `bin.install "prguide"` and `bin.install_symlink "prguide" => "prg"`.
- **`Cargo.toml` metadata** for crates.io: description, license,
  repository, keywords. Publishing is the owner's job.
- **The README:**
  - install (brew, release archive, `cargo install prguide` followed by
    `prguide link-prg`);
  - `prguide skill install`;
  - configuring `open_command`, with example commands for herdr, tmux,
    zellij and WezTerm. These are documentation only, and each is checked by
    hand;
  - a quick start;
  - the key bindings;
  - the harness notes.

Files: `src/cli/link_prg.rs`, `.github/workflows/release.yml`,
`packaging/homebrew/prguide.rb`, `README.md`.

Tests: `link-prg` creates the link, is idempotent, and reports conflicts;
the release workflow is validated with `actionlint` in CI.

- Design ref: [§2 Architecture](03-design.md#2-architecture)
- Depends on: T4-6

### T8-3. End-to-end checklist per harness

- **`docs/e2e-checklist.md`.** Run one full review, §1.1 steps 1–9, on a
  real agent-written PR, once each on Claude Code and omp. Check that:
  - the loop survives 30 minutes idle;
  - comments get live answers;
  - a deep dive appears;
  - a range guide comes from the picker;
  - posting matches the preview;
  - `q` stops the loop, and reopening plus the resume prompt works.
- **An opt-in test.** `cargo test --features e2e` drives the CLI against a
  scratch GitHub repo named in `PRGUIDE_E2E_REPO`, and skips when it isn't
  set.
- **Recording.** The first run's results go in the PR description.

Files: `docs/e2e-checklist.md`, `crates/prguide/tests/e2e.rs`.

- Design ref: [§5 Testing](03-design.md#5-testing)
- Depends on: all v1 tasks

---

## Later (not v1 blockers)

### L-1. Syntax highlighting

- Highlight diff and context lines with `syntect`, keeping the +/-
  background. Off with `NO_COLOR`.
- Design ref: [§3.4](03-design.md#34-the-viewer)
- Depends on: T6-3

### L-2. Codex support

Out of v1 by the owner's decision (design §1.2, §7 item 4). When it's picked
up:

1. **A spike first.** A throwaway `spike/fake-wait.sh` blocks until a file
   `spike/event` appears or 4 minutes pass, then prints a JSON line and
   removes the file. Give a Codex agent the loop rules from design §2.5 and
   let it run for 60 minutes, dropping events at random times and chatting
   once mid-turn. Record whether the loop survives, the model calls while
   idle, whether chatting mid-turn works, context growth, and any rate-limit
   or timeout messages. Write it up in
   `.agents/PRGuide/02-research-codex-spike.md`.
2. **If it's viable:**
   - add a Codex section to `SKILL.md` (a foreground `prguide wait --timeout
     4m`, called again on an empty result);
   - make `skill install --harness agents` mention Codex;
   - add Codex to the end-to-end checklist (T8-3).
   The protocol needs no change: `wait --timeout` already exists (T1-3).

- Design ref: [§2.5](03-design.md#25-the-live-loop), [§6 Risks](03-design.md#6-risks)
- Depends on: T4-6, T8-3
