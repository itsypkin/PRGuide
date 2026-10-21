# 02 — Research: live agent loop

Q11 says a comment reaches the agent as soon as it is saved, and the answer
appears while the reviewer keeps reading. That only works if the agent stays
in a listening loop for a 30-90 minute session, in Claude Code and in Codex
(Q9), without the reviewer typing into the agent's pane. This doc checks
what each harness allows: how long a command can block, whether a finished
background command can start a new agent turn, and what idling costs.
Researched 2026-10-21. "Verified locally" means run on this machine.

## Claude Code (local: v2.1.306)

- **Bash timeout.** Foreground default 2 min, max 10 min
  (`BASH_DEFAULT_TIMEOUT_MS`, `BASH_MAX_TIMEOUT_MS`). At its timeout a
  foreground command moves to the background instead of being killed.
- **`run_in_background`.** Returns a task id at once. In a local interactive
  session background commands have no time limit. Unattended runs (`-p`,
  SDK, cloud) cap them at 30 min by default, 2 h max.
- **Wake on exit: yes, verified locally.** In tmux, the agent started
  `sleep 25; echo EVENT` in the background and ended its turn. 25 s later,
  with no input, the session started a new turn on its own, read the
  output and replied. A rerun with a half-typed draft in the prompt box
  behaved the same, and the draft was kept, not submitted.
- **Monitor tool.** Each stdout line (or WebSocket frame) becomes an event
  in the chat. Each watch has a deadline (5 min default, 30 min max) and
  must be re-armed. Not on Bedrock/Vertex/Foundry or with telemetry off.
- **Hooks.** A `Stop` hook can return `{"decision":"block","reason":...}`
  to continue (capped at 8 blocks in a row without a tool call; default
  hook timeout 600 s). `asyncRewake` hooks wake Claude on exit code 2.
  `Notification` hooks cannot inject anything.
- **Channels** push MCP messages into a running session, but they are a
  research preview with an allowlist; custom ones need
  `--dangerously-load-development-channels`. Not for v1.
- **Idle limits.** No session idle timeout documented. Background shells
  are reaped only under critical memory pressure after 30 min idle.

## Codex CLI (not installed locally; openai/codex `main`, release rust-v0.163.0 of 2026-10-15)

- **exec tool.** `exec_command` (unified exec, always on) waits
  `yield_time_ms` (default 10 s, clamped to 250 ms–30 s), then returns a
  `session_id` if the process still runs. The process keeps running.
- **Long waits.** The agent polls with an empty `write_stdin`. Each poll
  waits up to `background_terminal_max_timeout` (default 300000 ms). So a
  blocking `prguide wait` costs one model call per poll, inside one turn.
- **Background terminals** survive across turns (`/ps`, `/stop`). Their
  exit raises a UI event (`ExecCommandEnd`). I found no code path where the
  exit starts a model turn (source read, not run).
- **Hooks.** A synchronous `Stop` hook returning `{"decision":"block",
  "reason":"..."}` "creates a new continuation prompt that acts as a new
  user prompt". Default timeout 600 s; the docs give no max for Stop. Async
  hooks cannot wake: "Finishing a background hook doesn't start a new
  turn." Needs `features.hooks`; project hooks need a trusted project.
- **Other.** `notify` is outbound only. `stream_idle_timeout_ms` (5 min)
  covers the model stream, not session idling. No session idle limit found.

## Other harnesses (brief)

- **Gemini CLI:** `run_shell_command` has `is_background` and
  `tools.shell.inactivityTimeout`; nothing documented about wake on exit.
- **pi:** an extension can call `pi.sendUserMessage()`, which "always
  triggers a turn when not streaming". A small pi extension could watch the
  prguide log, but the skill alone can't do it.
- **opencode and others:** not checked. herdr detects 20+ agent kinds, so
  the herdr nudge below covers them.

## Cross-harness patterns

- **Long-poll (baseline).** `prguide wait --timeout 9m` blocks until at
  least one unhandled event exists, prints them as JSON, exits 0. On
  timeout it prints `{"event":"none"}`, and the skill says "call wait
  again". Works in any harness with a shell tool.
- **Durable log and cursor.** prg appends events to a session log. `wait`
  returns everything after the agent's cursor, so events that arrive while
  the agent is busy are not lost.
- **Listener heartbeat.** A running `wait` holds a pid/lock file, so prg can
  show "listening / busy / not listening". Otherwise a dead loop looks like
  a slow agent.
- **Idle cost.** Each `none` return is a full model request over the whole
  context. With an 80k-token context, a 9 min poll over 90 idle minutes is
  about 10 requests (~0.8M input tokens). Anthropic's default cache TTL is
  5 min, so a 9 min poll always misses the cache. Codex's 5 min poll cap
  gives ~18 calls per 90 min. Event-driven wake costs nothing while idle.
- **Keep wake output small** (ids, slide ref, text). Each wake grows the
  transcript; the agent can fetch details with a separate command.
- **The skill must say:** never leave the loop on your own; after each
  event, handle it, write the result with prguide, call `wait` again; on
  `none`, call `wait` again at once without commentary; stop only on a
  `session_end` event.

## Waking the agent from outside (herdr / tmux)

- **tmux:** `send-keys -l "<text>"` plus `Enter` submitted a prompt into
  Claude Code (verified locally). It types blindly: a reviewer's draft in
  that pane gets merged into it and sent.
- **herdr** (local v0.9.3; `herdr --help`, local herdr SKILL.md):
  `herdr agent prompt <target> <text>` sends bracketed-paste text plus Enter
  as one submission, and refuses with `agent_blocked` at an approval dialog.
  `herdr agent get` reports `idle/working/blocked/done`. Panes carry
  `HERDR_PANE_ID`, so prguide can record the agent's pane at start.
- **Pros:** any harness, no hooks or background support needed, zero idle
  cost, and herdr knows when the agent is idle.
- **Cons:** needs herdr or tmux; shows up as a user message; can collide
  with the reviewer typing in the agent pane (herdr's draft handling is
  unverified); vim mode or open dialogs can swallow keys; typing into
  another process is a trust question.

## Recommendation

1. **One contract:** a durable event log plus `prguide wait [--timeout D]`,
   which returns all pending events or `{"event":"none"}` and holds a
   listener heartbeat while it runs.
2. **Claude Code:** the skill runs `prguide wait` with
   `run_in_background: true` and no timeout, and starts it again after each
   event. Event-driven, zero idle cost, verified on v2.1.306. Fallback when
   background tasks are disabled: foreground `wait --timeout 9m` in a loop.
3. **Codex:** long-poll in one turn (`prguide wait --timeout 30m`, polled
   with `write_stdin`). No setup; known idle cost. Offer an optional
   `prguide setup codex-hook` that installs a Stop hook for event-driven
   wake, doing nothing unless a prguide session is live.
4. **Fallback for other harnesses and dead loops:** when prg has pending
   events and no live listener, it offers a nudge (a key in prg;
   auto-nudge is opt-in config): `herdr agent prompt` if the agent pane is
   idle, else `tmux send-keys`, with "prguide: N new events, run `prguide
   wait`". Without either, prg shows "agent not listening" and the
   reviewer prompts the agent by hand.

## Open questions for design

- Codex spike: is a 30-90 min single turn with `write_stdin` polls stable
  (context, rate limits, chatting with the agent mid-turn)? Does a long
  Stop hook block the TUI?
- Claim events on return, or after the agent acks? Ack survives a crash but
  can deliver twice.
- How does the agent order a "guide for X..Y" request against pending
  thread replies in one batch?
- Can a Claude Code skill-frontmatter Stop hook act as a backstop when the
  agent forgets to re-arm?
- Nudge safety: detect a non-empty draft before typing.
- How the session ends (`session_end`), and what happens to a background
  `wait` when the agent exits.

## Sources

- [Claude Code tools reference: Bash, background commands, Monitor](https://code.claude.com/docs/en/tools-reference) (fetched 2026-10-21)
- [Claude Code interactive mode: background Bash](https://code.claude.com/docs/en/interactive-mode) (fetched 2026-10-21)
- [Claude Code environment variables](https://code.claude.com/docs/en/env-vars) (fetched 2026-10-21)
- [Claude Code hooks: Stop, asyncRewake, timeouts](https://code.claude.com/docs/en/hooks) (fetched 2026-10-21)
- [Claude Code channels](https://code.claude.com/docs/en/channels) (fetched 2026-10-21)
- [Codex config reference](https://learn.chatgpt.com/docs/config-file/config-reference) (fetched 2026-10-21)
- [Codex hooks](https://learn.chatgpt.com/docs/hooks) (fetched 2026-10-21)
- [openai/codex unified_exec/mod.rs](https://github.com/openai/codex/blob/main/codex-rs/core/src/unified_exec/mod.rs) and [shell_spec.rs](https://github.com/openai/codex/blob/main/codex-rs/core/src/tools/handlers/shell_spec.rs) (main, 2026-10-21)
- [openai/codex unified_exec_footer.rs](https://github.com/openai/codex/blob/main/codex-rs/tui/src/bottom_pane/unified_exec_footer.rs) (main, 2026-10-21)
- [Gemini CLI shell tool](https://geminicli.com/docs/tools/shell/) (fetched 2026-10-21)
- [pi send-user-message example](https://app.unpkg.com/@earendil-works/pi-coding-agent@0.80.9/files/examples/extensions/send-user-message.ts) (v0.80.9)
- herdr v0.9.3: local `herdr --help`, `herdr agent`, `herdr pane`, `~/.claude/skills/herdr/SKILL.md`
- Local test: Claude Code v2.1.306 in tmux, background wake with and without a draft (2026-10-21)

## Second opinion (reviewer agent on omp, 2026-10-21)

An independent review of the recommendation above, summarised.

- **Keep the event log and `prguide wait`, but deliver at least once.**
  Don't move the cursor just because `wait` returned; the agent can die
  before it writes the answer. Every event gets a sequence number and a
  stable `event_id`. Result writes carry `caused_by: event_id` and are
  idempotent, so a redelivered event can't produce a duplicate reply or
  deep dive. One active consumer per session in v1. Suggested store: a
  session-local SQLite file in WAL mode, kept separate from the publishable
  guide (Q15).
- **omp wakes on events natively (verified by the reviewer in its own
  session).** A `bash` call with `async: true, timeout: 0` runs as a managed
  background job. When it completes, the session gets the result and starts
  a follow-up turn. No extension is needed; pi/omp `sendUserMessage()` is a
  later option.
- **The heartbeat overclaims.** A lock held only during `wait` disappears
  while the agent handles an event, which looks the same as a dead loop.
  Use a consumer lease plus unacked-delivery state: waiting / handling /
  disconnected. Use an advisory lock held by the process, not a PID file.
- **Drop the Codex Stop-hook installer from v1.** It needs project trust
  and feature flags, its default 600 s timeout conflicts with a long wait,
  and its effect on the TUI is unknown. Ship the long-poll baseline. A
  30 min `wait` does not avoid Codex's ~5 min `write_stdin` model calls.
- **Drop raw `tmux send-keys` nudging.** Draft merging, modal editors and
  dialogs make it unsafe even as opt-in. Keep "agent not listening" status
  and a copyable prompt; allow `herdr agent prompt` only on an explicit
  reviewer action, and only when herdr reports the pane idle.
- **Protocol details:** `wait` prints ids, type, sequence and routing data
  only, and content is fetched separately. Batches are bounded. Events are
  ordered by sequence, but `session_end` acts as a cancellation and does
  not queue behind a backlog. Closing the viewer appends `session_end`,
  wakes `wait`, and releases the consumer lock. "Zero idle cost" holds for
  Claude Code and omp, not for the contract in general.

Bottom line from the reviewer: approve the log/`wait` architecture; in v1,
deliver at least once with idempotent writes, use background-job wake on
Claude Code and omp, and defer Codex hooks and all keystroke injection.
