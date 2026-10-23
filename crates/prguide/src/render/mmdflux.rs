//! The mmdflux-backed renderer: detection, limits, panic guard and the LR→TD rewrite.

use std::any::Any;
use std::borrow::Cow;
use std::panic::{AssertUnwindSafe, catch_unwind};

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
        limits::check(Limit::SourceBytes, src.len())?;
        match catch_unwind(AssertUnwindSafe(|| pipeline(src, opts))) {
            Ok(result) => result,
            Err(payload) => Err(RenderError::Panic(panic_message(payload.as_ref()))),
        }
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
    Ok(TextDiagram {
        height: lines.len(),
        width,
        lines,
        direction_overridden: overridden,
    })
}

/// Explains why mmdflux's registry did not recognise the source.
fn classify_undetected(src: &str) -> RenderError {
    let Some(header) = find_header(src) else {
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
    let participant_labels = seq.participants.iter().map(|p| p.label.as_str());
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
fn find_header(src: &str) -> Option<Header<'_>> {
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
        if t.starts_with("%%{") && !t.ends_with("}%%") {
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
    find_header(src).map(|h| kind_of(h.line))
}

/// Rewrites a root `LR`/`RL` direction to `TD`; everything else stays byte-identical.
pub fn rewrite_root_direction(src: &str, dir: RootDirection) -> Cow<'_, str> {
    let RootDirection::TopDown = dir;
    let Some(header) = find_header(src) else {
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
    use super::panic_message;

    #[test]
    fn panic_message_covers_every_payload_kind() {
        assert_eq!(panic_message(&"a"), "a");
        assert_eq!(panic_message(&String::from("b")), "b");
        assert_eq!(panic_message(&1u8), "unknown panic payload");
    }
}
