//! The mmdflux-backed renderer: detection, limits, panic guard and the LR→TD rewrite.

use std::any::Any;
use std::borrow::Cow;
use std::cell::Cell;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Once;

use ::mmdflux::graph::Graph;
use ::mmdflux::payload::Diagram;
use ::mmdflux::timeline::Sequence;
use ::mmdflux::timeline::sequence::model::SequenceEvent;
use ::mmdflux::{OutputFormat, RenderConfig};
use ratatui::text::Line;

use super::limits::{self, Limit};
use super::{Charset, RenderError, RenderOpts, Renderer, RootDirection, TextDiagram};

/// The longest kind name quoted in an `Unsupported` error.
const MAX_KIND_CHARS: usize = 40;

/// Renders flowcharts and sequence diagrams with mmdflux.
#[derive(Debug, Default, Clone, Copy)]
pub struct MmdfluxRenderer;

impl Renderer for MmdfluxRenderer {
    fn render(&self, src: &str, opts: &RenderOpts) -> Result<TextDiagram, RenderError> {
        INSTALL.call_once(|| {
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                if !QUIET.try_with(Cell::get).unwrap_or(false) {
                    previous(info);
                }
            }));
        });
        limits::check(Limit::SourceBytes, src.len())?;
        let src = normalise_source(src);
        let guard = QuietGuard::new();
        let caught = catch_unwind(AssertUnwindSafe(|| pipeline(&src, opts)));
        drop(guard);
        match caught {
            Ok(result) => result,
            Err(payload) => Err(RenderError::Panic(panic_message(payload.as_ref()))),
        }
    }
}

static INSTALL: Once = Once::new();

thread_local! {
    /// True while this thread is inside a render's `catch_unwind`.
    static QUIET: Cell<bool> = const { Cell::new(false) };
}

/// Silences the panic hook for the render on this thread; resets on drop.
struct QuietGuard;

impl QuietGuard {
    fn new() -> Self {
        QUIET.with(|q| q.set(true));
        QuietGuard
    }
}

impl Drop for QuietGuard {
    fn drop(&mut self) {
        let _ = QUIET.try_with(|q| q.set(false));
    }
}

/// Strips one leading BOM, then turns every CRLF into LF; borrows when unchanged.
fn normalise_source(src: &str) -> Cow<'_, str> {
    let stripped = src.strip_prefix('\u{feff}').unwrap_or(src);
    if stripped.contains("\r\n") {
        Cow::Owned(stripped.replace("\r\n", "\n"))
    } else {
        Cow::Borrowed(stripped)
    }
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

/// Everything after the source-size check; runs inside `catch_unwind`.
fn pipeline(src: &str, opts: &RenderOpts) -> Result<TextDiagram, RenderError> {
    let registry = ::mmdflux::builtins::default_registry();
    let Some(id) = registry.detect(src) else {
        return Err(classify_undetected(src));
    };
    let unsupported =
        || RenderError::Unsupported(first_keyword(src).unwrap_or_else(|| id.to_string()));
    if id != "flowchart" && id != "sequence" {
        return Err(unsupported());
    }

    if id == "flowchart" {
        let subgraphs = src.to_ascii_lowercase().matches("subgraph").count();
        limits::check(Limit::Subgraphs, subgraphs)?;
    }

    let text: Cow<'_, str> = match opts.root_direction {
        Some(dir) => rewrite_root_direction(src, dir),
        None => Cow::Borrowed(src),
    };
    let overridden = matches!(text, Cow::Owned(_));

    // unreachable with mmdflux 2.6.1: the registry has every id we pass.
    let instance = registry.create(id).ok_or_else(unsupported)?;
    let parsed = instance
        .parse(&text)
        .map_err(|e| RenderError::Invalid(e.to_string()))?;
    let payload = parsed
        .into_payload()
        .map_err(|e| RenderError::Invalid(e.to_string()))?;
    match &payload {
        Diagram::Flowchart(graph) => check_graph(graph)?,
        Diagram::Sequence(seq) => check_sequence(seq)?,
        // unreachable with mmdflux 2.6.1: only flowchart/sequence ids get here.
        _ => return Err(unsupported()),
    }

    let format = match opts.charset {
        Charset::Unicode => OutputFormat::Text,
        Charset::Ascii => OutputFormat::Ascii,
    };
    let out = ::mmdflux::render_diagram(&text, format, &RenderConfig::default())
        .map_err(|e| RenderError::Invalid(e.message))?;
    limits::check(Limit::OutputBytes, out.len())?;

    let lines: Vec<String> = out.lines().map(str::to_string).collect();
    let width = lines
        .iter()
        .map(|l| Line::raw(l.as_str()).width())
        .max()
        .unwrap_or(0);
    if width == 0 {
        return Err(RenderError::Invalid(
            "empty diagram: nothing to draw; add nodes or messages".to_string(),
        ));
    }
    Ok(TextDiagram {
        height: lines.len(),
        width,
        lines,
        direction_overridden: overridden,
    })
}

/// Explains why mmdflux's registry did not recognise the source.
fn classify_undetected(src: &str) -> RenderError {
    let Some(header) = find_header(src, true) else {
        return RenderError::Invalid("empty diagram: no diagram type line".to_string());
    };
    let kind = kind_of(header.line);
    let supported = ["graph", "flowchart", "sequencediagram"]
        .iter()
        .any(|k| kind.eq_ignore_ascii_case(k));
    if !supported {
        return RenderError::Unsupported(kind);
    }
    if header.multiline_directive {
        RenderError::Invalid(
            "multi-line %%{...}%% directive is not supported: put the %%{...}%% directive on one line"
                .to_string(),
        )
    } else {
        RenderError::Invalid(format!("diagram type line not recognised: {kind}"))
    }
}

fn check_graph(graph: &Graph) -> Result<(), RenderError> {
    if graph
        .nodes
        .keys()
        .any(|k| k.eq_ignore_ascii_case("subgraph"))
    {
        return Err(RenderError::Invalid(
            "`subgraph` was read as a node id: write `subgraph id[Title]` and its closing `end` each on their own line, or rename the node".to_string(),
        ));
    }
    limits::check(Limit::Nodes, graph.nodes.len())?;
    limits::check(Limit::Edges, graph.edges.len())?;
    let mut depth_max = 0;
    for start in graph.subgraphs.values() {
        let mut depth = 1;
        let mut cur = start;
        for _ in 0..graph.subgraphs.len() {
            match cur.parent.as_ref().and_then(|p| graph.subgraphs.get(p)) {
                Some(parent) => {
                    depth += 1;
                    cur = parent;
                }
                None => break,
            }
        }
        depth_max = depth_max.max(depth);
    }
    limits::check(Limit::Nesting, depth_max)?;
    let labels = graph
        .nodes
        .values()
        .map(|n| n.label.as_str())
        .chain(graph.edges.iter().filter_map(|e| e.label.as_deref()))
        .chain(graph.subgraphs.values().map(|s| s.title.as_str()));
    limits::check(Limit::LabelChars, max_chars(labels))
}

fn check_sequence(seq: &Sequence) -> Result<(), RenderError> {
    limits::check(Limit::Participants, seq.participants.len())?;
    let participant_labels = seq
        .participants
        .iter()
        .map(|p| p.label.as_str())
        .chain(seq.title.as_deref())
        .chain(
            seq.participant_boxes
                .iter()
                .filter_map(|b| b.label.as_deref()),
        );
    let event_labels = seq.events.iter().filter_map(|e| match e {
        SequenceEvent::Message { text, .. } | SequenceEvent::Note { text, .. } => {
            Some(text.as_str())
        }
        SequenceEvent::BlockStart { label, .. } | SequenceEvent::BlockDivider { label, .. } => {
            Some(label.as_str())
        }
        _ => None,
    });
    limits::check(
        Limit::LabelChars,
        max_chars(participant_labels.chain(event_labels)),
    )
}

fn max_chars<'a>(labels: impl Iterator<Item = &'a str>) -> usize {
    labels.map(|l| l.chars().count()).max().unwrap_or(0)
}

/// The located diagram header line.
struct Header<'a> {
    /// Byte offset of the line start in the source.
    offset: usize,
    /// The line, including any line ending.
    line: &'a str,
    /// Whether a multi-line `%%{` directive was skipped before it.
    multiline_directive: bool,
}

/// Finds the first line that is not front matter, blank, a comment or a directive.
fn find_header(src: &str, swallow_directives: bool) -> Option<Header<'_>> {
    let mut lines: Vec<(usize, &str)> = Vec::new();
    let mut offset = 0;
    for line in src.split_inclusive('\n') {
        lines.push((offset, line));
        offset += line.len();
    }
    let mut i = 0;
    let lead = src.len() - src.trim_start().len();
    if src[lead..].starts_with("---") {
        // The opener is the line holding the first non-blank text.
        if let Some(open) = lines.iter().position(|(o, l)| o + l.len() > lead)
            && let Some(close) = lines[open + 1..]
                .iter()
                .position(|(_, l)| l.trim() == "---")
        {
            i = open + 1 + close + 1;
        }
    }
    let mut multiline_directive = false;
    while i < lines.len() {
        let (offset, line) = lines[i];
        let t = line.trim();
        if swallow_directives && t.starts_with("%%{") && !t.ends_with("}%%") {
            // mmdflux treats an opener that never closes as a plain comment line.
            match lines[i + 1..]
                .iter()
                .position(|(_, l)| l.trim().ends_with("}%%"))
            {
                Some(close) => {
                    multiline_directive = true;
                    i += close + 2;
                }
                None => i += 1,
            }
        } else if t.is_empty() || t.starts_with("%%") {
            i += 1;
        } else {
            return Some(Header {
                offset,
                line,
                multiline_directive,
            });
        }
    }
    None
}

/// The first word of a header line, `;` trimmed and capped at 40 chars.
fn kind_of(line: &str) -> String {
    let word = line.split_whitespace().next().unwrap_or("");
    word.trim_end_matches(';')
        .chars()
        .take(MAX_KIND_CHARS)
        .collect()
}

/// Names the diagram kind for errors; `None` when there is no header line.
fn first_keyword(src: &str) -> Option<String> {
    find_header(src, true).map(|h| kind_of(h.line))
}

/// Rewrites a root `LR`/`RL` direction to `TD`; everything else stays byte-identical.
pub fn rewrite_root_direction(src: &str, dir: RootDirection) -> Cow<'_, str> {
    let RootDirection::TopDown = dir;
    let Some(header) = find_header(src, false) else {
        return Cow::Borrowed(src);
    };
    let line = header.line;
    let bytes = line.as_bytes();
    let start = line.len() - line.trim_start().len();
    let word_len = line[start..]
        .find(char::is_whitespace)
        .unwrap_or(line.len() - start);
    let word = &line[start..start + word_len];
    if !(word.eq_ignore_ascii_case("graph") || word.eq_ignore_ascii_case("flowchart")) {
        return Cow::Borrowed(src);
    }
    let mut pos = start + word_len;
    while pos < bytes.len() && (bytes[pos] == b' ' || bytes[pos] == b'\t') {
        pos += 1;
    }
    let Some(token) = bytes.get(pos..pos + 2) else {
        return Cow::Borrowed(src);
    };
    let is_dir = token.eq_ignore_ascii_case(b"LR") || token.eq_ignore_ascii_case(b"RL");
    let ends = match bytes.get(pos + 2) {
        None => true,
        Some(b) => matches!(b, b'\n' | b'\r' | b' ' | b'\t' | b';'),
    };
    if !(is_dir && ends) {
        return Cow::Borrowed(src);
    }
    let at = header.offset + pos;
    let mut out = String::with_capacity(src.len());
    out.push_str(&src[..at]);
    out.push_str("TD");
    out.push_str(&src[at + 2..]);
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::{MmdfluxRenderer, normalise_source, panic_message};
    use crate::render::limits::Limit;
    use crate::render::{Charset, RenderError, RenderOpts, Renderer, RootDirection, TextDiagram};

    #[test]
    fn panic_message_covers_every_payload_kind() {
        assert_eq!(panic_message(&"a"), "a");
        assert_eq!(panic_message(&String::from("b")), "b");
        assert_eq!(panic_message(&1u8), "unknown panic payload");
    }

    const CS: [Charset; 2] = [Charset::Unicode, Charset::Ascii];

    fn go(src: &str, cs: Charset, flip: bool) -> Result<TextDiagram, RenderError> {
        MmdfluxRenderer.render(
            src,
            &RenderOpts {
                charset: cs,
                root_direction: flip.then_some(RootDirection::TopDown),
            },
        )
    }

    fn assert_label_limit(r: Result<TextDiagram, RenderError>, actual: usize) {
        match r {
            Err(RenderError::TooLarge {
                limit: Limit::LabelChars,
                actual: a,
                max: 60,
            }) => assert_eq!(a, actual),
            other => panic!("{other:?}"),
        }
    }

    /// R1 sequence_title_label_limit
    #[test]
    fn sequence_title_label_limit() {
        let src = |t: &str| format!("sequenceDiagram\n  title {t}\n  A->>B: hi\n");
        for cs in CS {
            assert!(go(&src(&"t".repeat(60)), cs, false).is_ok());
            assert!(go(&src(&"é".repeat(60)), cs, false).is_ok());
            assert_label_limit(go(&src(&"t".repeat(61)), cs, false), 61);
        }
    }

    /// R2 participant_box_label_limit
    #[test]
    fn participant_box_label_limit() {
        let src = |b: &str| {
            format!("sequenceDiagram\n  box Aqua {b}\n  participant A\n  end\n  A->>B: hi\n")
        };
        for cs in CS {
            assert!(go(&src(&"b".repeat(60)), cs, false).is_ok());
            assert_label_limit(go(&src(&"b".repeat(61)), cs, false), 61);
        }
        let two = format!(
            "sequenceDiagram\n  box Aqua one\n  participant A\n  end\n  box Aqua {}\n  participant B\n  end\n  A->>B: hi\n",
            "b".repeat(61)
        );
        let first_long = format!(
            "sequenceDiagram\n  box Aqua {}\n  participant A\n  end\n  box Aqua one\n  participant B\n  end\n  A->>B: hi\n",
            "b".repeat(61)
        );
        for cs in CS {
            assert_label_limit(go(&two, cs, false), 61);
            assert_label_limit(go(&first_long, cs, false), 61);
        }
    }

    /// R3 crlf_front_matter_matches_lf
    #[test]
    fn crlf_front_matter_matches_lf() {
        let lf = "---\nconfig:\n  flowchart:\n    x: 1\n---\ngraph LR\n  A --> B\n";
        let crlf = lf.replace('\n', "\r\n");
        for src in [
            (lf, &crlf[..]),
            ("graph LR\n  A --> B\n", "graph LR\r\n  A --> B\r\n"),
        ] {
            for cs in CS {
                for flip in [false, true] {
                    assert_eq!(go(src.0, cs, flip).unwrap(), go(src.1, cs, flip).unwrap());
                }
            }
        }
    }

    /// R4 bom_stripped_once
    #[test]
    fn bom_stripped_once() {
        for cs in CS {
            for flip in [false, true] {
                assert_eq!(
                    go("\u{feff}graph LR\n  A --> B\n", cs, flip).unwrap(),
                    go("graph LR\n  A --> B\n", cs, flip).unwrap()
                );
            }
            match go("\u{feff}\u{feff}graph LR\n  A --> B\n", cs, false) {
                Err(RenderError::Unsupported(k)) => assert_eq!(k, "\u{feff}graph"),
                other => panic!("{other:?}"),
            }
        }
    }

    /// R5 size_limit_before_normalising
    #[test]
    fn size_limit_before_normalising() {
        let mut s = String::from("graph LR\r\n  A --> B\r\n%%");
        while s.len() < 8193 {
            s.push('x');
        }
        for cs in CS {
            match go(&s, cs, false) {
                Err(RenderError::TooLarge {
                    limit: Limit::SourceBytes,
                    actual: 8193,
                    max: 8192,
                }) => {}
                other => panic!("{other:?}"),
            }
        }
    }

    /// R6 unclosed_directive_then_pie
    #[test]
    fn unclosed_directive_then_pie() {
        let e = go(
            "%%{init: {\"theme\":\"dark\"}\npie\n  \"a\" : 1\n",
            Charset::Unicode,
            false,
        )
        .unwrap_err();
        assert!(matches!(&e, RenderError::Unsupported(k) if k == "pie"));
        assert_eq!(e.to_string(), "unsupported diagram type: pie");
    }

    /// R7 front_matter_closer_exact
    #[test]
    fn front_matter_closer_exact() {
        let src = |c: &str| format!("---\ntitle: x\n{c}\ngraph LR\n  A --> B\n");
        for c in ["----", "--- x"] {
            let e = go(&src(c), Charset::Unicode, false).unwrap_err();
            assert!(
                matches!(&e, RenderError::Unsupported(k) if k == "---"),
                "{c}: {e}"
            );
        }
        for c in ["---", "  ---  "] {
            let d = go(&src(c), Charset::Unicode, false).unwrap();
            assert_eq!((d.width, d.height), (14, 3), "{c:?}");
        }
    }

    /// R8 normalise_source_cases
    #[test]
    fn normalise_source_cases() {
        let owned = |s: &str, want: &str| {
            let got = normalise_source(s);
            assert!(matches!(got, Cow::Owned(_)), "{s:?}");
            assert_eq!(got, want);
        };
        owned("\u{feff}a\r\nb\rc", "a\nb\rc");
        assert_eq!(normalise_source("\u{feff}\u{feff}x"), "\u{feff}x");
        owned("\r\n\r\n", "\n\n");
        for s in ["abc", "", "x\u{feff}", "a\rb"] {
            let got = normalise_source(s);
            assert!(matches!(got, Cow::Borrowed(_)), "{s:?}");
            assert_eq!(got, s);
        }
    }
}
