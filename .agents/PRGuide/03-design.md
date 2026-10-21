# 03 — Design: prguide v1

Status: **approved by the owner on 2026-10-21**. Inputs: `00-rough-idea.md`,
the decisions in `01-idea.md` (Q1–Q28), `02-research-live-agent-loop.md`
and `02-research-mermaid-terminal.md`, including the reviewer's second
opinion at the end of each. The first draft of this doc was also reviewed by
the reviewer agent, and its P0 and P1 findings are folded in. §7 lists the
points the owner still has to confirm.

## 1. Overview

prguide walks a human through a GitHub PR the way an engineer sitting next to
them would: what the problem was, how it was solved, a sketch of the
architecture, then the changes in the order they run. It ships as one Rust
binary with two names (`prguide` for agents, `prg` for humans, Q3) and a
skill that teaches any coding agent how to use it (Q9).

The binary never calls a model. The agent does the thinking: it reads the PR,
writes the guide, answers comments and writes deep dives. The binary does
everything deterministic: fetching the PR, the worktree, validating and
storing guides, drawing slides and diffs, the comment loop, and posting to
GitHub.

### 1.1 A review, end to end

1. In the agent's pane the reviewer says "/prguide 1234" (the skill).
2. The agent runs `prguide start 1234`. prguide checks it is inside a clone of
   the PR's repo, fetches the PR, prepares a worktree at the PR head (Q23),
   starts a **review run** and prints it as JSON: the run id, worktree path,
   commits and changed files.
3. The agent runs `prguide guide new`, which creates a draft whose manifest
   prguide has already filled in (PR identity, SHAs). The agent reads the code
   and writes the slides (§3.1), runs `prguide check`, fixes what fails, and
   runs `prguide guide commit`.
4. The agent runs `prguide open`. It runs the reviewer's configured open
   command if there is one, and otherwise prints the viewer command. The
   agent then opens a pane with its own tools, or asks the reviewer to run
   `prg 1234` (Q18, §2.9).
5. The agent starts listening with `prguide wait` (§2.5) and stays in that
   loop until the run ends.
6. The reviewer reads the slides. They comment on a line, a range or a whole
   slide (Q21). The agent receives each comment as it is saved (Q11) and
   either replies in the thread (Q12), writes or extends the slide's deep dive
   (Q26–Q28), or drafts a question for the PR author.
7. The reviewer can pick a range of commits in prg and ask for a guide of just
   those commits (Q16, Q17). The agent writes a new guide version.
8. When ready, the reviewer opens the outbox, edits or drops the drafted
   questions and any agent answers they promoted (Q8), checks where each item
   will land, and posts them as one GitHub review with the "Comment" event
   (Q7, Q24). **prg posts; the agent never posts.**
9. Quitting prg (`q`) stops the run, so `wait` tells the agent to leave the
   loop. Nothing is deleted: `prg 1234` reopens the review as it was left and
   offers the prompt that brings the agent back (§2.5). The agent keeps its
   own context in its pane either way. If prg crashes or its pane is closed,
   nothing is sent, so the agent keeps waiting and reopening just works.

### 1.2 Out of scope for v1

- Local branches and working-tree diffs; GitHub PRs only (Q5). No GitLab.
- Sharing a guide with other reviewers; the format is ready for it (Q15).
- A coverage check (Q19).
- Approving or requesting changes from prg (Q24).
- Replying to GitHub threads from prg; they are read-only (Q25).
- Deep dives of deep dives (Q27). The model allows a tree later.
- Commit selections that aren't one contiguous run, or that contain a merge
  commit (§2.3).
- **Codex support** (owner, 2026-10-21; §7, item 4). v1 supports Claude Code
  and omp. `prguide wait --timeout` still exists, so a polling harness can be
  added later without changing the protocol.
- Any typing into the agent's pane (live-loop research, second opinion).
- prguide calling a model (Q9).

## 2. Architecture

```
 agent pane                                   viewer pane
 ┌──────────────────────────┐                ┌──────────────────────────┐
 │ coding agent + skill     │                │ prg (ratatui TUI)        │
 │  runs `prguide …` (JSON) │                │  slides, diffs, diagrams │
 └────────────┬─────────────┘                │  threads, picker, outbox │
              │                              └────────────┬─────────────┘
              ▼                                           ▼
 ┌──────────────────────────────────────────────────────────────────────┐
 │ prguide library (one binary; argv[0] = prg → viewer)                 │
 │                                                                      │
 │  store: SQLite (WAL) — runs, events, threads, deep dives, outbox,    │
 │         guide-version rows, schema version                           │
 │  guides/: immutable main decks (publishable format)                  │
 │  anchors: code refs, line mapping between revisions                  │
 │  render: mmdflux adapter        git: git CLI (fetch, refs, diff, show)│
 │  open: user command / print     github: gh CLI (PR, review, GraphQL) │
 └──────────────────────────────────────────────────────────────────────┘
              │                                           │
              ▼                                           ▼
     local clone, pinned refs, worktree              GitHub (via gh)
```

One Cargo workspace with a library crate and a thin binary. The binary looks
at `argv[0]`: `prg` starts the viewer, `prguide` is the agent CLI. `prguide
view` starts the viewer too, so the skill never depends on the `prg` name.

### 2.1 Where things live

All state is local to the reviewer's machine, under `$PRGUIDE_HOME`
(default `~/.prguide`; no spaces in the path, which agents handle badly):

```
~/.prguide/prs/github.com/<owner>/<repo>/<n>/
  store.db            SQLite (WAL), versioned with PRAGMA user_version
  guides/v1/ …        committed main decks (§3.1); never edited
  staging/            guide commit's work area (§3.2)
  drafts/             where the agent writes before guide commit
  worktree/           prguide-managed git worktree (§2.2)
```

There is one store per PR per machine. Inside it, each `prguide start`
creates a **run**: one review session with one agent. A new run doesn't
inherit an old run's events or end state. Guide versions, threads and the
outbox are per PR and outlive runs, so a later run sees earlier
conversations.

**A store is bound to one clone.** Its pinned refs and its worktree live in
that clone's Git database, so the first `start` records the clone's Git
common directory in the store. A later `start` from another clone of the same
repo fails with an error naming the bound clone. Moving a store to another
clone is out of v1.

**`prguide gc`** removes a PR's store, its worktree (`git worktree remove`,
then `git worktree prune`) and its pinned refs together, for PRs that are
closed and older than a configurable age, or for one PR given explicitly.
Deleting only some of these would leave the remaining guides unreadable.

### 2.2 Git and GitHub

- **GitHub through `gh`.** prguide shells out to `gh` for PR metadata, the
  commit list, posting the review and reading review threads (GraphQL). The
  PR diff is computed locally (§2.7). The reviewer's existing `gh auth` is reused, so prguide
  stores no tokens. `gh` missing or unauthenticated is a clear startup error.
- **Git through the `git` CLI**, so the reviewer's config, credentials and
  fetch settings apply. The current directory must be inside a clone with a
  remote pointing at the PR's repo (Q23). prguide fetches `pull/<n>/head`
  (this works for PRs from forks) and the base branch.
- **Pinned refs.** For every guide version prguide writes refs under
  `refs/prguide/<n>/v<k>/` for the snapshot's base and head and the
  selection's base and head. Force-pushes and `git gc` therefore can't remove
  the objects an old guide needs. `prguide gc` removes them together with the
  store (§2.1).
- **Reading code.** prg and `check` read code with `git show <sha>:<path>` and
  `git diff` on pinned SHAs, never from the worktree. Diffs always use fixed
  options: `--no-ext-diff --no-textconv --no-color -M -U3`.
- **The worktree** exists only for the agent to read and grep the code. It is
  created once with `git worktree add --detach`. On `start` and `guide new`,
  prguide moves it to the snapshot head with `git checkout --detach --force`
  and `git clean -fd`. The skill tells the agent not to keep files there. The
  reviewer's own working tree and branch are never touched. `prguide end
  --clean` removes the worktree.

### 2.3 Guide versions: snapshot and selection

Every guide version records two things separately:

- **snapshot:** the PR state the agent analysed. That is the PR head SHA at
  `guide new`, and the merge base with the base branch at that time.
- **selection:** the change being explained. For a full guide that is all of
  the PR's commits, and its diff is `snapshot.base...snapshot.head`, which
  matches GitHub's "Files changed". For a range guide (Q16) it is a contiguous
  run of the PR's commits in PR order, and its diff is `<first>^..<last>`.

A range may not contain a merge commit; `guide new` rejects one with a clear
error. Without merges, `<first>^..<last>` is unambiguous. A full guide handles
merges correctly, since it uses the merge base.

How the parts use them:

- `diff` code blocks resolve against the selection's diff.
- `context` code blocks show code at `snapshot.head` by default ("the rest of
  the PR", Q16). A block can ask for another revision with `at:
  selection-head` or `at: selection-base` (§2.4).
- "N new commits" in prg compares the PR's current head with the newest
  version's `snapshot.head`, regardless of which range that version covered.

Ways to create a version:

- **In prg (Q17):** a picker lists the PR's commits. The reviewer selects a
  contiguous run and presses generate, which appends a `generate_request`
  event. A version list switches between versions.
- **From the CLI (Q17 follow-up):** `prguide guide new --commits A..B`, for
  when the reviewer asks in chat.
- When the PR head moves past the newest snapshot, prg shows "3 new commits
  since v2" and opens the picker with them preselected (Q14).

### 2.4 Code anchors

One anchor model is used by code blocks, flags, threads and posting.

- **Code blocks** have an id that is unique within the slide, a path and an
  inclusive line range (`start`, `end`, 1-based). There are two kinds:
  - **`diff`** requires `side: new | old`. `new` means line numbers at
    `selection.head`, and `old` means `selection.base`, used for deleted code.
    The block selects every hunk of that file in the selection's diff that
    overlaps the range. prg always shows whole hunks, so a change can't be
    cut in half. For a renamed file, `path` is the new path, and prguide
    takes the old path from the diff. A deleted file uses `side: old` and the
    old path. Binary and mode-only changes appear as a one-line stub, and can
    be cited only by a `diff` block with no line range.
  - **`context`** has no `side`. It takes `at: snapshot-head |
    selection-head | selection-base`, defaulting to `snapshot-head`, and
    shows those lines of the file at that revision, styled as unchanged.
- **Flags** (Q13) point at a block id and an optional line inside it.
- **Thread anchors** are resolved and **stored in full** when the thread is
  created: version, slide, block id, block kind, path, side (`diff` only),
  the resolved revision SHA, start and end line, and the text of those lines. Posting and remapping use the stored
  anchor, not the slide.
- **Line mapping** between revisions follows the hunks of `git diff <from>
  <to> -- <path>` (with rename detection). If a line inside the anchor changed
  or was deleted, the anchor is unmapped. This is used for threads shown on
  other versions (§2.6) and for posting (§2.7).

### 2.5 The live loop

This follows the live-loop research, as amended by both second opinions.

**Runs and the consumer.** `prguide start` returns a run id. Every agent
command takes `--run <id>`, because shell state doesn't persist between tool
calls in Claude Code and other harnesses. A run has exactly one consumer, its
agent.

A run is `active`, `stopped` or `expired`:

- **The lease.** Every command with `--run` renews a 30-minute lease, and a
  blocked `wait` keeps renewing it. During long work, such as writing a
  guide, the skill has the agent run `prguide status --run R` at least every
  10 minutes as a heartbeat.
- **Stop.** `q` in prg, `prguide end` or a takeover moves the run to
  `stopped`. A lease that runs out moves it to `expired`. Expiry is checked
  whenever any command or prg reads the run.
- **One transaction.** A run leaving `active` sends all its claimed events
  back to `pending` in the same transaction, so the next run receives them.
- **After the end.** Every later command from a stopped or expired run fails
  with `run_stopped` and writes nothing. A late reply from an old agent can't
  land after the next run has already answered.
- **Starting again.** `start` is blocked only by another `active` run with a
  valid lease, and `--takeover` stops that run first. Stopped and expired
  runs never block it, so reopening after `q` just works.

**Events.** prg appends events to the store:

| type | written when | the agent's usual response |
|---|---|---|
| `comment` | a new thread or a new reviewer message in a thread | `reply`, `deepdive` or `draft-question` |
| `deepdive_request` | the reviewer presses "deep dive" on a comment (Q28) | `deepdive` |
| `generate_request` | the picker's generate (Q17) | `guide new` … `guide commit` |

Each event has a sequence number and a stable `event_id`, and is `pending`,
`claimed` (returned by `wait` to the run) or `done`.

**`prguide wait --run R [--timeout D] [--max N]`** blocks until the run has
pending or claimed events, or the run has stopped. It returns a bounded batch,
claimed events first, then pending ones in sequence order. Output is small
JSON: ids, type, sequence, thread or slide refs, and no comment text. The
agent fetches the content with `prguide show`. On timeout it returns
`{"events":[]}`. If the run has stopped (`q` in prg, `prguide end`, or a
takeover), it returns `{"stopped":true}`, and the agent leaves the loop.
Stopping is run state, not an event, so it never waits behind a backlog and
needs no ack. Events written while no run is active stay `pending` in the PR
store, and the next run receives them.

**Delivery is at least once, and results are idempotent.** An event becomes
`done` only when the run writes a result with `--caused-by <event_id>`
(`reply`, `deepdive`, `draft-question`, `guide commit`) or calls `prguide ack`.
A second result for the same event returns the first one and changes nothing.
If a run's lease expires, its claimed events go back to `pending` for the
next run.

**Agent status in prg's header**, worked out from the store:

- **listening:** an active run has a `wait` blocked right now (it holds an
  advisory file lock while blocked);
- **working:** an active run has claimed events and no `wait` blocked;
- **not listening:** anything else. That covers no active run (for example
  after `q` and a reopen), and an active run with no `wait` blocked and no
  claimed events, whether or not events are pending. The agent dropping out
  of the loop therefore shows even before the reviewer comments. To avoid
  flicker in the moment between a result and the next `wait`, prg shows this
  state only after it has lasted 10 seconds.

When not listening, prg shows the pending count and a key that copies a
resume prompt to paste into the agent. There are two prompts:

- **no active run:** "prguide: the reviewer is back on #1234. Run `prguide
  start 1234`, then listen with `prguide wait`."
- **an active run that stopped listening:** "prguide: you stopped listening
  on #1234. Run `prguide wait --run <R>` and continue the loop." A `start`
  would fail with `run_active` here.

prg keeps accepting comments and requests, which wait as pending events.

prguide never types into the agent's pane. Copying the prompt is the only
way back into the loop.

**How each harness runs the loop** (the skill gives each its own
instructions):

| harness | how `wait` runs | idle cost |
|---|---|---|
| Claude Code | `run_in_background: true`, no timeout. The agent ends its turn, and the command exiting starts a new one | none (verified) |
| omp | `bash` with `async: true, timeout: 0`. Job completion starts a follow-up turn | none (verified by the reviewer) |
| later: Codex, others | foreground `prguide wait --timeout 4m`, called again on an empty result. Not in v1 (§1.2) | one model call per timeout while idle |

The skill's rules: never leave the loop on your own; handle each event, write
the result with `--caused-by`, start `wait` again; on an empty result, call
`wait` again without commentary; stop only on `{"stopped":true}`. Never build
or run the PR's code unless the reviewer asks.

### 2.6 Threads and deep dives

- **Threads (Q12, Q21)** keep the anchor they were created on (§2.4), and
  belong to the PR, not to one version. prg shows a thread on its own version.
  On other versions it is mapped to the matching line if the line maps, and
  otherwise listed under "threads from other versions". Switching between a
  full guide and a range guide never hides an active conversation. Messages
  are from `reviewer` or `agent`.
- **Deep dives** are stored in SQLite, not in the guide directory, so writing
  one section and marking its event done is a single transaction. A deep dive
  belongs to (guide version, parent slide) and is shown to the left of its
  parent (Q26). v1 allows one per main slide (Q27). `prguide deepdive <slide>
  --caused-by E --file part.md` creates it, or appends a section if it already
  exists. Each section records the event that caused it, so a redelivered
  event can't add the section twice. When a deep dive is created or grows, prg
  marks the parent "◀ deep dive" and adds a one-line agent message to the
  causing thread, pointing at the new section. A later tree only needs a
  parent column that can point at a deep dive.

### 2.7 The outbox and posting

- **Agent replies** stay local (Q8). The reviewer can mark one "post anyway".
  It is then posted with attribution: a quote of the reviewer's question,
  followed by "Answer from the reviewer's prguide agent:" and the answer. It
  never looks like the reviewer wrote it.
- **Author questions.** The agent drafts one with `prguide draft-question
  <thread>` when it can't answer from the code and PR history. The reviewer can
  also turn any thread into an author question from prg (Q12).
- **The outbox (Q7)** lists every item to be posted, each editable or
  droppable.
- **Posting** happens only in prg:
  1. Fetch the PR's current head SHA from GitHub, and `git fetch` the head
     and the base branch into the bound clone. The canonical PR diff is
     computed locally as the three-dot diff `base...head`, which is what
     GitHub shows. This avoids the API's diff size limits, and lets the line
     mapping of §2.4 run against it.
  2. Map every item's stored anchor to the current head (§2.4). Items that map
     to a line in the PR's diff become inline comments, on one line or a
     range. Everything else goes in the review body with path:line@sha and a
     short quote of the code: slide-level items, context code, unmapped
     anchors, and lines outside the diff (Q21 note).
  3. Show the reviewer the final placement, including every item that moved
     to the body and why, and ask for confirmation.
  4. Submit one review: `event: COMMENT`, `commit_id` set to the current head,
     a non-empty body (a short "Review notes" line when nothing else goes
     there), and a hidden marker `<!-- prguide:<post id> -->`.
- **Posting recovery.** The outbox is locked while a post is in flight. If the
  request times out, prg doesn't retry blindly. It lists the PR's reviews,
  looks for the marker, and either records the success or offers a retry. The
  GitHub ids of posted comments are stored.
- **GitHub threads (Q25)** are read with GraphQL when prg starts, on `r`, and
  on focus at most once a minute. They are shown read-only on the slide and
  line they map to, marked "GitHub", together with replies to the reviewer's
  posted questions. Threads that can't be mapped go in a list on the overview
  slide. The agent reads them with `prguide threads --github`.

### 2.8 Diagrams

From the Mermaid research, as amended by its second opinion:

- `mmdflux`, pinned to an exact version with `default-features = false`,
  behind a private `Renderer` trait. No mmdflux types reach the guide model.
- v1 types: `flowchart`/`graph` and `sequenceDiagram`. Detection uses
  mmdflux's detection and validation API.
- Limits are enforced before rendering: source bytes, nodes, edges,
  participants, nesting, label length and output bytes. The call is wrapped
  in `catch_unwind`.
- **`prguide check`** renders every diagram with the embedded renderer, in
  Unicode and ASCII, at the minimum supported content width of 76 columns (an
  80-column pane minus the frame). A supported type must render. An
  unsupported type passes only when the slide has text art, which then is the
  diagram. Text art for a supported type is optional (§7, item 2).
- **prg fails soft.** On an error or panic it shows the text art if there is
  any, otherwise the raw Mermaid with a one-line error.
- **Size.** prg keeps the authored direction and pans for modest overflow.
  Only above the cap (width `min(2 × viewport width, 200)`, height `3 ×
  viewport height`, using the real viewport) does it try LR→TD, with an
  "auto-layout LR→TD" indicator. `m` cycles diagram → text art → source.
- Renders are cached by (source hash, viewport width and height, charset,
  renderer version).

### 2.9 Opening the viewer

prguide has **no built-in knowledge of any terminal multiplexer** (§7,
item 5). It works the same in herdr, tmux, zellij, WezTerm, or a plain
terminal. Nothing in the library or the store mentions a multiplexer.

`prguide open --run R`:

- If the reviewer configured an open command, it runs that command with `sh
  -c`, with the viewer command in `$PRGUIDE_VIEW_CMD` (for example `prguide
  view --run r_…`). It prints `{"opened": true}`, or the command's error.
  - The command is set in `open_command` in `~/.prguide/config.toml`, and
    `PRGUIDE_OPEN_CMD` overrides it.
  - The README shows examples for common multiplexers. They are
    documentation, not code.
- Otherwise it prints `{"opened": false, "command": "prg 1234"}`.

The skill then tells the agent what to do with `opened: false`: open a pane
with its own tools if it has a way to, such as a multiplexer skill.
Otherwise it asks the reviewer to run the command. prguide never types into
an existing pane.

### 2.10 Trust

The agent reads untrusted PR content, which could carry prompt injection
("post this", "approve"). The design limits the damage: the agent has no way
to post or approve. Its outputs are local slides, replies, deep dives and
outbox drafts, and nothing leaves the machine until the reviewer has seen the
outbox and its placement preview. Every path in a guide is validated: no
absolute paths, no `..`, and no symlinks in the guide directory. The skill
tells the agent not to run the PR's code unless the reviewer asks.

## 3. Low-level details that matter

### 3.1 The guide format

A guide version's main deck is a directory with a manifest and Markdown
slides. Agents write Markdown well and JSON-escaped prose badly, and a file
per slide keeps edits small. **prguide writes the manifest's provenance; the
agent writes only the slide list and the slides.**

`guide.yaml`, as created by `guide new`:

```yaml
format: prguide/v1
pr: { host: github.com, repo: owner/name, number: 1234 }
snapshot: { base: 9f1c…, head: 4e2a… }        # full SHAs, written by prguide
selection:
  kind: commits                                # full | commits
  base: 77ab…
  head: 4e2a…
  commits: [a1b2…, c3d4…, 4e2a…]
slides: []                                     # the agent fills this in
```

A slide, `slides/s2-controller.md`:

```markdown
---
id: s2-controller
kind: step                 # overview | architecture | step
title: "Controller: accept the new `retry` flag"
code:
  - id: handler
    kind: diff
    path: src/api/orders.rs
    side: new
    start: 40
    end: 88
  - id: router
    kind: context          # unchanged code (Q20); no side
    at: snapshot-head      # the default; or selection-head | selection-base
    path: src/router.rs
    start: 12
    end: 20
flags:                     # Q13
  - at: { block: handler, line: 61 }
    text: "No test covers retry=false."
---
The request reaches `OrdersController::create` from the router above …
```

- Ranges are explicit `start`/`end` fields, not `40-88` strings, so `check`
  diagnostics can name the exact field to fix.
- A `diff` block with no hunks in its range is a `check` error. A `context`
  block overlapping a hunk is a warning: it should be a `diff` block.
- `architecture` slides carry a fenced `mermaid` block, plus an optional
  fenced `text` block (§2.8).
- The body is CommonMark without HTML. prg renders headings, emphasis, lists,
  inline code, code blocks and links. Candidate: `tui-markdown`, with
  pulldown-cmark plus our own styling as the fallback (to confirm in
  planning).
- **Portable, not self-contained.** A guide holds no local paths or reviewer
  data, so it can be published later (Q15). It refers to Git objects by SHA,
  though, so the reader needs the repo, and old guides can break after a
  force-push on another machine. Deep dives live in the store (§2.6) and
  would be exported along with the guide when sharing arrives.

### 3.2 Committing a guide

`prguide guide commit <draft> --run R [--caused-by E]`:

1. Reserve the next version number in a store row with state `staging`. The
   row records the `caused-by` event and every ref the version needs, by name
   and SHA.
2. Copy the draft to `staging/v<k>/`, rejecting symlinks and bad paths, and
   run `check` on the copy.
3. Write the pinned refs (§2.2).
4. Rename `staging/v<k>/` to `guides/v<k>/` (atomic on one filesystem).
5. In one transaction: set the row to `ready`, mark the `caused-by` event
   done, and record a notice that prg picks up.

Reconciliation runs on startup and on a repeated `commit`, and handles each
crash point:

- **`guides/v<k>/` exists:** check every ref recorded in the row, and
  recreate any that is missing or wrong. Then run step 5.
- **`guides/v<k>/` doesn't exist:** delete the `staging/` directory and every
  ref recorded in the row that was already written, then delete the row.
- **A repeated `commit` with the same `caused-by`** returns the version if its
  row is `ready`. If the row is still `staging`, the commit reconciles it
  first, then finishes it or starts again.

### 3.3 The agent CLI

Every command prints JSON on stdout. Errors print JSON on stderr with a
stable `code` and exit 1. Everything after `start` takes `--run <id>`.

| command | purpose |
|---|---|
| `start <pr> [--takeover]` | open the PR store, fetch, prepare the worktree, start a run |
| `guide new [--commits A..B]` | create a draft with the provenance filled in (§3.1) |
| `check <draft>` | validate a draft guide |
| `guide commit <draft> [--caused-by E]` | store a new version (§3.2) |
| `open` | open the viewer pane (§2.9) |
| `wait [--timeout D] [--max N]` | the listening loop (§2.5) |
| `show <event_id>` | an event's full content and its thread |
| `reply <thread> --caused-by E (--text … \| --file …)` | an agent message in a thread |
| `deepdive <slide> --caused-by E --file …` | create or extend a deep dive (§2.6) |
| `draft-question <thread> --caused-by E --file …` | an author question into the outbox |
| `ack <event_id>` | mark an event done with no output |
| `threads [--github]` | threads across all versions, plus GitHub's with `--github` |
| `status` | the run, versions, listener state, pending events; also the heartbeat during long work (§2.5) |
| `end [--clean]` | stop the run (the reviewer said "we're done" in chat), optionally removing the worktree |
| `skill install [--harness claude\|agents] [--dir …]` | install the skill files (`agents` = `~/.agents/skills`, read by omp) |

There is deliberately no command that posts to GitHub.

### 3.4 The viewer

```
┌ owner/repo#1234 · v2 (3 commits) · slide 3/9 ────── agent: listening ┐
│ Controller: accept the new `retry` flag             ◀ deep dive        │
│ The request reaches OrdersController::create from the router …         │
│ ── src/api/orders.rs ─────────────────────────────────────────────────  │
│   58   let retry = req.retry.unwrap_or(true);                          │
│ + 61   if retry { schedule_retry(&order)?; }   ⚑ No test covers false  │
│ ── src/router.rs · unchanged ─────────────────────────────────────────  │
│ 💬 2 threads (1 answered) · GitHub: 1                                  │
└ ↑↓ slide  ←→ deep dive  j/k scroll  v select  c comment  o outbox  ? ─┘
```

- ↑/↓ moves through the main deck, and ←/→ goes to a deep dive and back
  (Q26). j/k and PgUp/PgDn scroll within a slide.
- `v` starts a line selection in a code block. `c` comments on the selection,
  or on the slide when nothing is selected. `D` asks for a deep dive.
- `g` opens the commit picker, `V` the version list, `o` the outbox, and `t`
  the session-wide thread list.
- `m` cycles a diagram's views. `p` enters pan mode, where the arrows pan the
  diagram, and Esc leaves it.
- `q` quits and stops the run (§1.1, step 9). Everything is kept.
- Comments are typed in an inline editor, with `$EDITOR` as the fallback for
  long text.
- prg polls the store every 250 ms and redraws when replies, deep dives or
  versions arrive. A redraw never moves the reviewer's cursor or selection.
- Diff lines get +/- colouring and line numbers. Syntax highlighting is a
  nice-to-have (candidate: syntect).

### 3.5 The skill

One `SKILL.md` with standard front matter (name, description), so Claude
Code and omp can load it (and later other harnesses that read the same
format). It covers:

- the session flow of §1.1, with `--run` on every command;
- the per-harness loop instructions and rules (§2.5);
- triage: answer from the code and the PR history when you can; write a deep
  dive when the question is "how does X work"; otherwise draft an author
  question;
- the trust rules (§2.10).

How to write a good guide goes in a second file next to `SKILL.md`, read only
when writing one:

- problem and solution first;
- a small architecture diagram (≤ ~12 nodes, TD for long chains, a sequence
  diagram for call chains);
- then the steps in invocation order, citing unchanged code as context where
  the chain passes through it;
- flags where a second look is worth it (Q13).

`prguide skill install` copies both files into the harness's skills
directory.

## 4. Build order

The order matters because later parts rest on the anchor model and the event
store:

1. The store, runs, events, lease and claims, with recovery tests. This is
   the riskiest part to get wrong.
2. Git: fetch, pinned refs, the snapshot and selection diffs, the anchor
   model and line mapping.
3. The guide format, `guide new`, `check`, `guide commit` and reconciliation.
4. The viewer: slides, code blocks, threads, deep dives, versions, the
   picker.
5. Diagrams.
6. Posting and GitHub threads.
7. The skill, with an end-to-end test on Claude Code and omp.
8. Later: Codex support (starting with a spike on the long loop), syntax
   highlighting.

## 5. Testing

- **The store:**
  - event states, claim expiry, idempotent results;
  - stopping a run, takeover and lease expiry: claims going back to
    pending, late writes from the old run rejected, and pending events
    reaching the next run;
  - a store refusing a `start` from a different clone;
  - concurrent prg and CLI access (WAL);
  - schema migrations.
- **Git and anchors, on fixture repos built in tests:**
  - renames, deletions, binary and mode-only changes;
  - merge commits (a full guide passes, a range is rejected);
  - the base branch moving, force-pushes, `git gc` with pinned refs;
  - line mapping across revisions.
- **Guides:**
  - `check` diagnostics;
  - path hardening;
  - `guide commit` crashes at every step, then reconciliation.
- **Posting, against a fake `gh`:**
  - mixed-version outboxes, mapping and moving items to the body, the
    placement preview;
  - an empty body;
  - timeouts with unknown outcomes, and finding the marker before a retry;
  - attribution of promoted answers.
- **Diagrams:** the fixture corpus from the Mermaid second opinion, in CI.
- **The viewer:**
  - ratatui `TestBackend` snapshot tests per slide kind;
  - narrow (80) and wide panes;
  - the fallback paths;
  - redraws during typing.
- **The skill:** a manual end-to-end checklist per harness (Claude Code and
  omp) on a real agent-written PR, plus an opt-in end-to-end test
  against a scratch GitHub repo.

## 6. Risks

- **Codex is not supported in v1.** Its idle cost and long turns are untested
  hands-on, so supporting it later starts with a spike (§4, step 8).
- **mmdflux has one maintainer.** It sits behind a private trait, pinned. The
  fixture corpus shows when merman catches up.
- **Guide quality depends on the agent and the model.** Without a coverage
  check (Q19), a guide can skip a hunk that matters. The skill is the only
  defence in v1.
- **Line mapping is best-effort.** Threads and posted comments that can't be
  mapped are shown in lists or moved to the review body, never placed on a
  guessed line.

## 7. To confirm with the owner

These change or narrow something in `01-idea.md`:

1. **Commit selection is one contiguous run with no merge commits** (Q16 said
   "a set of commits", Q17 said "range"). *Confirmed by the owner,
   2026-10-21.*
2. **Text art is optional for supported diagram types** (Q22 described text
   art as the fallback). `prguide check` makes the agent fix diagrams before
   the reviewer sees them, so requiring text art too would cost tokens and the
   two could disagree. Text art is still required for unsupported types.
   *Confirmed by the owner, 2026-10-21.*
3. **Quitting prg stops the agent's loop** (Q18 had no end rule). Nothing is
   deleted. Reopening shows "not listening" with a one-key copy of the resume
   prompt, and the agent keeps its context in its pane. A crash or a closed
   pane sends nothing, so the agent keeps waiting. There is no separate "end
   review" step; `prguide gc` cleans up old PR stores. *Confirmed by the
   owner, 2026-10-21* (this replaces an End/Detach dialog from an earlier
   draft).

4. **Codex support is out of v1** (Q9 said "test on Claude Code and
   Codex"). v1 supports Claude Code and omp, both of which wake on events at
   no idle cost. The polling mode (`wait --timeout`) stays in the protocol,
   so Codex can be added later. *Decided by the owner, 2026-10-21.*

5. **No multiplexer integration in prguide** (Q18 said the agent opens prg
   "in a new herdr/tmux pane"). prguide is multiplexer-agnostic. `prguide
   open` runs a user-configured command, or prints the viewer command for
   the agent or the reviewer to use. The agent still opens the pane when it
   can, using its own tools. The herdr nudge from earlier drafts is dropped.
   *Decided by the owner, 2026-10-21.*

Decisions made in this design and kept after review:

6. GitHub through the `gh` CLI, and git through the `git` CLI (§2.2).
7. State under `~/.prguide`: one store per PR, with a run per review session
   (§2.1).
8. The guide format: a YAML manifest whose provenance prguide writes, plus
   Markdown slides (§3.1). Deep dives live in the store (§2.6).
9. Posting is a prg action only, and the agent has no posting command
   (§2.7).
10. Threads belong to the PR and are shown across versions (§2.6). This
   reverses the first draft, where they were tied to one version.
11. `guide commit` refuses a guide that fails `check` (§3.2).
12. Agent status comes from the lease and claims, with no timers (§2.5).
    prguide never types into the agent's pane; the reviewer pastes the
    resume prompt.
