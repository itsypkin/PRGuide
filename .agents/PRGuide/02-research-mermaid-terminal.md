# 02 — Research: Mermaid in the terminal

Q22 says the agent writes Mermaid for the architecture slide, `prg` draws it
with box-drawing characters in a ratatui pane, and an agent-written text-art
version is shown when `prg` can't draw it. This doc checks what can turn
Mermaid into terminal text from inside a single static Rust binary, with no
Node, browser or network at runtime. It also sets the v1 subset and the
fallback rules. State of the ecosystem as of 2026-10-21.

## Renderers

"Tested" means we built the crate in a scratch project and rendered four
samples: an architecture flowchart TD with two subgraphs and edge labels, a
7-node `graph LR` chain, a sequence diagram with `alt/else`, and broken input.

| Tool | Lang, license | Activity | Diagram types (text output) | Subgraphs / edge labels / width | Notes |
|---|---|---|---|---|---|
| [mmdflux](https://github.com/kevinswiber/mmdflux) | Rust, MIT | 2.6.1 (2026-08-09), ★75, 27 commits in 3 months, essentially one author (1274 of ~1277 commits) | flowchart, class, state, sequence | yes / yes / no max-width option | **Tested.** Best flowchart output of the three: compound layout, orthogonal routing. One edge ran through a subgraph title. Clean sequence output. Clear errors for broken input and unknown types. 37 deps, about +4 MB to a release binary. |
| [merman](https://github.com/Latias94/merman) (`merman-ascii`) | Rust, MIT/Apache-2.0 | 0.9.1 (2026-10-18), ★622, 16 contributors, very active | flowchart, sequence, state, class, ER, xychart; outline text for gantt, mindmap, timeline and others | yes / yes / max-width with Error or Fallback policy, compact profile | **Tested.** Good sequence and LR output. **Refused** our architecture flowchart ("routes crossing reserved graph geometry"). Strict errors. Heavy, policy-rich API. 120 deps for flowchart + sequence. Zed uses merman for SVG. |
| [mermaid-text](https://crates.io/crates/mermaid-text) | Rust, MIT | 0.57.0 (2026-07-21), 81 releases since 2026-04, part of [markdown-reader](https://github.com/leboiko/markdown-reader) (★91) | claims ~20 types (flowchart, sequence, state, class, ER, pie, mindmap...) | yes / yes / `render_with_width` | **Tested.** Messy flowchart: labels detached from edges, many crossings. Width 80 broke words in the middle and still came out 100 cols wide. **Renders broken input as nodes with no error**, so we can't detect when to fall back. 7 deps. |
| Other Rust crates: figurehead, meraid, mermansi, boxmaid, termermaid, termiflow, graphs-tui (AGPL), termaid (Rust crate) | Rust | each <3k downloads, 0–25 stars | mostly flowchart, some sequence | varies | Not evaluated. Too new or too small to depend on. The `termaid` crate's repo link points to a Python-only repo (unverified origin). |
| [mermaid-ascii](https://github.com/AlexanderGrooff/mermaid-ascii) | Go, MIT | 1.6.1 (2026-09-08), ★1.6k, 20 contributors | flowchart (`graph`/`flowchart` LR/TD), sequence, ER | yes / yes / `--max-width` compacts, never truncates | Not tested (Go binary). Rectangles only, no diagonal edges. The reference implementation that others port. About 400 KB of Go source. |
| [beautiful-mermaid](https://github.com/lukilabs/beautiful-mermaid) | TypeScript, MIT | npm 1.1.3 (2026-02-26), ★11.2k, no commits since 2026-05 | flowchart, state, sequence, class, ER, xychart | yes / yes / — | Port of mermaid-ascii plus SVG. Needs a JS runtime. Out for us. |
| [termaid](https://github.com/fasouto/termaid) | Python, MIT | 0.9.1 (2026-10-18), ★558 | 18 types | nested, per-subgraph direction / yes / `--width` auto-fit | Needs Python. Out, but its auto-fit behaviour is a good model. |
| [Diagon](https://github.com/ArthurSonzogni/Diagon) | C++, MIT | last release 2023-05, ★2.2k | its own input languages, **not Mermaid** | — | Reference only. |
| [Graph::Easy](https://metacpan.org/dist/Graph-Easy) | Perl, GPL | 0.76 (2016) | own syntax + DOT, not Mermaid | — | Reference only. |
| [svgbob](https://github.com/ivanceras/svgbob) | Rust, Apache-2.0 | ★4.2k | ASCII art → SVG (the reverse direction) | — | Not relevant. |
| [mermaid-rs-renderer](https://github.com/1jehuang/mermaid-rs-renderer) | Rust, MIT | 0.3.1, ★1.8k | SVG/PNG only | — | No text output. |

Measured on the samples (Unicode, default settings):

- 7-node `graph LR` chain: 140–142 cols in all three Rust crates. As `graph TD`: 33–35 cols wide, 39–65 lines tall.
- Sequence with 3 participants: 45–73 cols. All three were readable. mmdflux and merman were the most compact.
- Render time: under 11 ms for every sample. Caching is still worth it.

## Integration options

| Option | Effort | Risk |
|---|---|---|
| (a) Native crate (mmdflux, or merman) | 2–4 days: a wrapper, error mapping, ratatui pane, caching | A third-party crate with a single author (mmdflux; bus factor 1, so be ready to fork) or a fast-moving 0.x API (merman). Pin exact versions. Wrap calls in `catch_unwind`, because layout code in young crates can panic. |
| (b) Port or vendor a small renderer (e.g. mermaid-ascii Go → Rust) | 3–6 weeks for flowchart + sequence at mermaid-ascii's quality | We own all its bugs. A port falls behind upstream. No clear gain over (a), since Rust crates already exist. |
| (c) Shell out to an optional binary (`mmdflux`, `mermaid-ascii`) | 1 day | Breaks the "single binary" promise if it's required. Fine only as an opt-in override in config. |
| (d) Own minimal renderer | Sequence: about 1 week (no layout problem, just columns and rows). Flowchart with subgraphs, labels and orthogonal routing: 1–3 months | The hard part is routing on a character grid, not parsing or ranking. Parts exist (`tree-sitter-mermaid`, `rust-sugiyama`, `dugong` (dagre port), `ascii-dag`, `layout-rs`), but the crates above needed 40k–150k lines to do it well. |

Put any choice behind an internal trait, e.g.
`fn render(src: &str, opts) -> Result<TextDiagram, RenderError>`, so the
backend can change without touching the guide format or the viewer.

## v1 subset

What the slides need:

- **Architecture sketch**: components and data flow. A flowchart with
  subgraphs (crate/module/service boundaries) and edge labels ("writes",
  "calls").
- **Call chain**: who calls whom, in order, with returns and error branches.
  A sequence diagram fits. A short `graph TD` chain also works.

v1 supports `flowchart`/`graph` (TD, TB, BT, LR, RL; subgraphs; edge labels;
the common node shapes) and `sequenceDiagram` (participants, aliases,
messages, notes, `alt/opt/loop`). `stateDiagram` and `classDiagram` render
through the same backend in mmdflux, but the skill should not offer them
until we have tested them. Every other type goes to the text-art fallback.

Skill guidance for the agent, so most diagrams fit a pane:
at most about 12 nodes; labels at most about 20 chars; use TD for chains
longer than 3–4 nodes; LR only for short chains; prefer a sequence diagram
for call chains.

## Terminal concerns

- **Width.** A pane is 80–120 cols, minus slide chrome. LR diagrams overflow
  quickly (a 7-node chain needs about 140 cols). mmdflux has no max-width
  option, so `prg` must measure the result itself (`unicode-width`) and act
  on it (see fallback rules).
- **Panning.** Render once to a `String`, then draw it with `Paragraph`
  without `.wrap()` and with `.scroll((y, x))`, which offsets on both axes.
  Bind h/j/k/l or the arrow keys while the diagram has focus. Left/right
  already navigate deep dives (Q26), so panning needs a focus mode or
  shift+arrows. `tui-scrollview` 0.6.8 is an option if we need scrollbars.
  Cache per (slide, pane width).
- **Unicode vs ASCII.** Box-drawing characters are the default. Offer an
  ASCII mode (`--ascii`/config). mmdflux and merman both have one. Box
  glyphs are East Asian Ambiguous width and render double-width in some CJK
  terminal setups; merman's docs call this out. ASCII mode covers it.
- **Color.** Render plain text and let `prg` add its own styling (e.g. dim
  edges, accent the nodes a slide is about) instead of trusting Mermaid
  `style`/`classDef`. If we use backend ANSI output, `ansi-to-tui` converts
  it to ratatui `Text`. Respect `NO_COLOR`. The skill should not encode
  meaning in color.

## Recommendation

Use **mmdflux as a library** (option a) behind an internal renderer trait,
with exact version pinning. It gave the best flowchart output in our tests,
handles subgraphs and edge labels, renders sequence diagrams cleanly, fails
loudly on bad input, and is pure Rust with a moderate dependency tree.
Keep **merman** as the alternative to re-check during design: it is more
active, has more contributors, and has a built-in width policy, but it
refused a typical architecture diagram and costs about 3× the dependencies.
Reject mermaid-text: it accepts broken input silently, which defeats the
fallback rule. No shelling out and no own renderer in v1.

Fallback rules. `prg` shows the agent's text art when:

1. the slide has no Mermaid block;
2. the first keyword is not in the v1 subset (checked before rendering);
3. the renderer returns an error or panics (caught);
4. the rendered result is wider than a hard cap (e.g. 2× pane width or 200
   cols) or taller than about 3 screens. Before falling back, re-render an
   LR/RL flowchart as TD once, since that fixes most width problems.

Below the cap, a diagram wider than the pane is shown with horizontal
panning and a "→ more" hint. The reviewer can toggle between the diagram,
the text art and the raw Mermaid source with one key. If the text art is
missing and rendering fails, show the Mermaid source as a code block with
the error on one status line.

## Open questions for design

- Is the text-art fallback required whenever a slide has Mermaid, or
  optional? Required is safer, but every diagram costs the agent twice.
- Should `prguide` have a `render`/`check` subcommand, so the agent sees the
  exact terminal output (and errors) and fixes the diagram before the
  reviewer opens it?
- Is the automatic LR → TD flip acceptable, or must `prg` keep the
  agent's direction?
- Concrete cap values (width, height), and whether they depend on pane size.
- Which keys pan the diagram, given that left/right navigate deep dives?
- How large a binary-size increase is acceptable (mmdflux adds about 4 MB
  unstripped)?
- When to re-check merman (its ASCII flowchart support is moving fast).

## Sources

- [mmdflux repo](https://github.com/kevinswiber/mmdflux), [crate](https://crates.io/crates/mmdflux)
- [merman repo](https://github.com/Latias94/merman), [merman-ascii crate](https://crates.io/crates/merman-ascii), [Zed integration PR #57644](https://github.com/zed-industries/zed/pull/57644)
- [mermaid-text crate](https://crates.io/crates/mermaid-text), [markdown-reader repo](https://github.com/leboiko/markdown-reader)
- [mermaid-ascii](https://github.com/AlexanderGrooff/mermaid-ascii)
- [beautiful-mermaid](https://github.com/lukilabs/beautiful-mermaid), [npm](https://www.npmjs.com/package/beautiful-mermaid)
- [termaid](https://github.com/fasouto/termaid), [PyPI](https://pypi.org/project/termaid/)
- [Diagon](https://github.com/ArthurSonzogni/Diagon), [Graph::Easy](https://metacpan.org/dist/Graph-Easy), [svgbob](https://github.com/ivanceras/svgbob), [mermaid-rs-renderer](https://github.com/1jehuang/mermaid-rs-renderer)
- Layout building blocks: [rust-sugiyama](https://crates.io/crates/rust-sugiyama), [dugong](https://crates.io/crates/dugong), [ascii-dag](https://crates.io/crates/ascii-dag), [layout-rs](https://crates.io/crates/layout-rs), [tree-sitter-mermaid](https://crates.io/crates/tree-sitter-mermaid)
- ratatui: [Paragraph::scroll](https://docs.rs/ratatui/latest/ratatui/widgets/struct.Paragraph.html#method.scroll), [tui-scrollview](https://crates.io/crates/tui-scrollview), [ansi-to-tui](https://crates.io/crates/ansi-to-tui)
- crates.io search API (`/api/v1/crates?q=mermaid`, `ascii diagram`, `graph layout`, `sugiyama`, `dagre`), queried 2026-10-21

## Second opinion (reviewer agent on omp, 2026-10-21)

An independent review of the recommendation above, summarised.

- **mmdflux for v1.** Its bus factor is a maintenance risk, but merman
  rejecting a typical architecture diagram is a product failure. Mitigations:
  - a private adapter, with no mmdflux types in the guide model;
  - pin the exact version and commit `Cargo.lock`;
  - `default-features = false` (the defaults pull in CLI dependencies);
  - a fixture corpus of real prguide diagrams (nested subgraphs,
    cross-boundary edges, labels, LR/TD, sequence branches, malformed
    input), checked against canonical Mermaid so mmdflux isn't accepting
    syntax Mermaid rejects;
  - re-run the corpus against merman from time to time, but don't ship two
    backends. MIT allows an emergency fork.
- **`prguide check` instead of mandatory text art.** Generation fails fast:
  the skill must run `prguide check` after writing or updating a guide and
  fix the diagrams before opening prg. The viewer fails soft: it still
  catches errors and panics, and shows text art if supplied, otherwise the
  raw Mermaid plus a one-line error. `check` uses the exact embedded
  renderer and validates the type, syntax, size limits, render success,
  fitted size at a conservative viewport, Unicode and ASCII modes, and turns
  a panic into a diagnostic; it exits nonzero with a preview and actionable
  errors. Text art is required only for unsupported diagram types or when it
  is the primary diagram. Requiring both wastes tokens, and the two can
  disagree.
- **Width.** Keep the authored direction and pan for modest overflow. Try
  LR→TD only when the diagram exceeds the cap, and show an
  "auto-layout LR→TD" indicator, with the authored version behind the
  toggle. Define the caps on the content viewport, for example width
  `min(2 × viewport_width, 200)` and height `3 × viewport_height`, plus byte
  limits. Panning lives in an explicit focus mode, so left/right still
  moves to deep dives. Upstream mmdflux `main` now documents a `--max-width`
  option; check whether a later release adds it.
- **Harden the renderer boundary.** `catch_unwind` doesn't cover hangs,
  abort-mode panics, stack overflow or OOM, and mmdflux documents no
  resource limits. Enforce limits before calling it: source bytes, nodes,
  edges, participants, nesting, label length, output bytes. The skill's
  ~12-node guidance becomes an enforced limit or warning.
- **Corrections.** Detect the diagram type with mmdflux's
  detection/validation API, not by reading the first keyword (frontmatter,
  comments, directives). Cache renders by (source hash, viewport width,
  charset, renderer version/options), not by (slide, pane width), because
  slides change live.
