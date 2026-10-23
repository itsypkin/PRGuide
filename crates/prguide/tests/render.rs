//! Renderer tests (T5-1): corpus snapshots, detection, limits, rewrite, cache key.

use std::borrow::Cow;
use std::path::Path;

use prguide::render::limits::{self, Limit};
use prguide::render::{
    Charset, MmdfluxRenderer, RENDERER_VERSION, RenderError, RenderOpts, Renderer, RootDirection,
    TextDiagram, render_cache_key, rewrite_root_direction,
};
use ratatui::text::Line;

fn fixture(name: &str) -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/mermaid")
        .join(name);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

const CHARSETS: [(Charset, &str); 2] = [(Charset::Unicode, "unicode"), (Charset::Ascii, "ascii")];

fn opts(charset: Charset) -> RenderOpts {
    RenderOpts {
        charset,
        root_direction: None,
    }
}

fn flipped(charset: Charset) -> RenderOpts {
    RenderOpts {
        charset,
        root_direction: Some(RootDirection::TopDown),
    }
}

fn render(src: &str, o: &RenderOpts) -> Result<TextDiagram, RenderError> {
    MmdfluxRenderer.render(src, o)
}

fn ok(src: &str, o: &RenderOpts) -> TextDiagram {
    render(src, o).unwrap_or_else(|e| panic!("render failed: {e}"))
}

fn err(src: &str, o: &RenderOpts) -> RenderError {
    match render(src, o) {
        Ok(_) => panic!("expected an error"),
        Err(e) => e,
    }
}

fn snap(d: &TextDiagram) -> String {
    format!("{}x{}\n{}", d.width, d.height, d.lines.join("\n"))
}

fn too_large(e: RenderError) -> (Limit, usize, usize) {
    match e {
        RenderError::TooLarge { limit, actual, max } => (limit, actual, max),
        other => panic!("expected TooLarge, got {other:?}"),
    }
}

const CORPUS: [&str; 6] = [
    "architecture",
    "cross_boundary",
    "lr_chain",
    "td_chain",
    "sequence_alt_loop",
    "wide_labels",
];

#[test]
fn corpus_snapshots() {
    for name in CORPUS {
        let src = fixture(&format!("{name}.mmd"));
        for (cs, label) in CHARSETS {
            let d = ok(&src, &opts(cs));
            insta::assert_snapshot!(format!("{name}_{label}"), snap(&d));
            if name != "wide_labels" {
                let text = d.lines.join("\n");
                assert_eq!(text.is_ascii(), cs == Charset::Ascii, "{name} {label}");
            }
        }
    }
}

#[test]
fn text_diagram_dimensions() {
    for name in CORPUS {
        let src = fixture(&format!("{name}.mmd"));
        for (cs, label) in CHARSETS {
            let d = ok(&src, &opts(cs));
            let widest = d
                .lines
                .iter()
                .map(|l| Line::raw(l.as_str()).width())
                .max()
                .unwrap_or(0);
            assert_eq!(d.height, d.lines.len(), "{name} {label}");
            assert_eq!(d.width, widest, "{name} {label}");
            assert!(!d.direction_overridden, "{name} {label}");
        }
    }
    let d = ok(&fixture("wide_labels.mmd"), &opts(Charset::Unicode));
    assert_eq!(d.width, 29);
    let line = d
        .lines
        .iter()
        .find(|l| l.contains("数据库"))
        .expect("label line");
    assert_eq!(Line::raw(line.as_str()).width(), 29);
    assert_eq!(line.chars().count(), 26);
}

#[test]
fn detects_through_front_matter_comments_and_init() {
    for (cs, label) in CHARSETS {
        let base = ok(&fixture("lr_chain.mmd"), &opts(cs)).lines;
        for name in ["detect_front_matter", "detect_comments", "detect_init"] {
            let d = ok(&fixture(&format!("{name}.mmd")), &opts(cs));
            assert_eq!(d.lines, base, "{name} {label}");
        }
    }
}

#[test]
fn multiline_directive_is_invalid() {
    let e = err(&fixture("multiline_directive.mmd"), &opts(Charset::Unicode));
    assert_eq!(e.code(), "render_invalid");
    assert!(
        e.to_string()
            .contains("put the %%{...}%% directive on one line"),
        "{e}"
    );
}

#[test]
fn mmds_json_is_unsupported() {
    let e = err(&fixture("mmds.json"), &opts(Charset::Unicode));
    assert_eq!(e.code(), "render_unsupported");
}

#[test]
fn unsupported_types() {
    for (file, kind) in [
        ("unsupported_pie.mmd", "pie"),
        ("unsupported_class.mmd", "classDiagram"),
        ("unsupported_state.mmd", "stateDiagram-v2"),
    ] {
        match err(&fixture(file), &opts(Charset::Unicode)) {
            RenderError::Unsupported(k) => assert_eq!(k, kind, "{file}"),
            other => panic!("{file}: {other:?}"),
        }
    }
}

#[test]
fn malformed_and_empty_are_invalid() {
    let e = err(&fixture("malformed.mmd"), &opts(Charset::Unicode));
    assert_eq!(e.code(), "render_invalid");
    assert!(e.to_string().contains("line 2"), "{e}");
    for src in ["", "%% only a comment\n"] {
        let e = err(src, &opts(Charset::Unicode));
        assert_eq!(e.code(), "render_invalid");
        assert!(e.to_string().contains("empty diagram"), "{e}");
    }
}

#[test]
fn panic_is_caught() {
    let r = MmdfluxRenderer;
    let src = fixture("panic_node_in_two_subgraphs.mmd");
    for (cs, _) in CHARSETS {
        match r.render(&src, &opts(cs)) {
            Err(e @ RenderError::Panic(_)) => {
                assert_eq!(e.code(), "render_panic");
                assert!(e.to_string().contains("no entry found for key"), "{e}");
            }
            other => panic!("expected Panic, got {other:?}"),
        }
    }
    assert!(
        r.render(&fixture("lr_chain.mmd"), &opts(Charset::Unicode))
            .is_ok()
    );
}

fn pad_to(base: &str, total: usize) -> String {
    let mut s = base.to_string();
    let mut rest = total - s.len();
    while rest > 0 {
        let mut n = rest.min(80);
        if rest - n > 0 && rest - n < 4 {
            n -= 4;
        }
        s.push_str("%% ");
        s.push_str(&"x".repeat(n - 4));
        s.push('\n');
        rest -= n;
    }
    assert_eq!(s.len(), total);
    s
}

fn l(n: usize) -> String {
    "é".repeat(n)
}

fn nodes(n: usize) -> String {
    let mut s = "flowchart TD\n".to_string();
    for i in 0..n {
        s.push_str(&format!("  n{i}\n"));
    }
    s
}

fn edges(n: usize) -> String {
    format!("flowchart TD\n{}", "  a --> b\n".repeat(n))
}

fn participants(n: usize) -> String {
    let mut s = "sequenceDiagram\n".to_string();
    for i in 0..n {
        s.push_str(&format!("  participant P{i}\n"));
    }
    s.push_str(&format!("  P0->>P{}: x\n", n - 1));
    s
}

fn nesting(deep: bool) -> String {
    if deep {
        "flowchart TD\n  subgraph a\n    subgraph b\n      subgraph c\n        subgraph d\n          x --> y\n        end\n      end\n    end\n  end\n".into()
    } else {
        "flowchart TD\n  subgraph a\n    subgraph b\n      subgraph c\n        x --> y\n      end\n    end\n  end\n".into()
    }
}

/// Flowchart labels with the given lengths: subgraph title, node label, edge label.
fn flow_labels(t: usize, n: usize, e: usize) -> String {
    format!(
        "flowchart TD\n  subgraph sg [{}]\n    A[{}]\n  end\n  A -->|{}| B\n",
        l(t),
        l(n),
        l(e)
    )
}

/// Sequence labels: alias, message, note, alt, else, loop lengths.
fn seq_labels(v: [usize; 6]) -> String {
    format!(
        "sequenceDiagram\n  participant A as {}\n  participant B\n  A->>B: {}\n  Note over A: {}\n  alt {}\n    A->>B: x\n  else {}\n    B->>A: y\n  end\n  loop {}\n    A->>B: z\n  end\n",
        l(v[0]),
        l(v[1]),
        l(v[2]),
        l(v[3]),
        l(v[4]),
        l(v[5])
    )
}

type Row = (String, String, String, Limit, usize, usize);

#[test]
fn limit_pairs() {
    let lr = fixture("lr_chain.mmd");
    let mut rows: Vec<Row> = vec![
        (
            "source".into(),
            pad_to(&lr, 8192),
            pad_to(&lr, 8193),
            Limit::SourceBytes,
            8193,
            8192,
        ),
        ("nodes".into(), nodes(40), nodes(41), Limit::Nodes, 41, 40),
        ("edges".into(), edges(80), edges(81), Limit::Edges, 81, 80),
        (
            "participants".into(),
            participants(12),
            participants(13),
            Limit::Participants,
            13,
            12,
        ),
        (
            "nesting".into(),
            nesting(false),
            nesting(true),
            Limit::Nesting,
            4,
            3,
        ),
    ];
    let at = flow_labels(60, 60, 60);
    for (i, kind) in ["subgraph title", "node label", "edge label"]
        .iter()
        .enumerate()
    {
        let mut v = [60; 3];
        v[i] = 61;
        rows.push((
            kind.to_string(),
            at.clone(),
            flow_labels(v[0], v[1], v[2]),
            Limit::LabelChars,
            61,
            60,
        ));
    }
    let at = seq_labels([60; 6]);
    for (i, kind) in ["alias", "message", "note", "alt", "else", "loop"]
        .iter()
        .enumerate()
    {
        let mut v = [60; 6];
        v[i] = 61;
        rows.push((
            kind.to_string(),
            at.clone(),
            seq_labels(v),
            Limit::LabelChars,
            61,
            60,
        ));
    }
    for (name, at, over, limit, actual, max) in rows {
        for (cs, label) in CHARSETS {
            if let Err(e) = render(&at, &opts(cs)) {
                panic!("{name} at-limit {label}: {e}");
            }
            let got = too_large(match render(&over, &opts(cs)) {
                Err(e) => e,
                Ok(_) => panic!("{name} over-limit {label} rendered"),
            });
            assert_eq!(got, (limit, actual, max), "{name} {label}");
        }
    }
    // The padded "at" source renders exactly like the unpadded chain.
    assert_eq!(
        ok(&pad_to(&lr, 8192), &opts(Charset::Unicode)).lines,
        ok(&lr, &opts(Charset::Unicode)).lines
    );
}

#[test]
fn output_limit() {
    assert!(limits::check(Limit::OutputBytes, 65536).is_ok());
    assert_eq!(
        too_large(limits::check(Limit::OutputBytes, 65537).unwrap_err()),
        (Limit::OutputBytes, 65537, 65536)
    );
    let src = fixture("over_output.mmd");
    let mut sizes = Vec::new();
    for (cs, fmt) in [
        (Charset::Unicode, ::mmdflux::OutputFormat::Text),
        (Charset::Ascii, ::mmdflux::OutputFormat::Ascii),
    ] {
        let want = ::mmdflux::render_diagram(&src, fmt, &::mmdflux::RenderConfig::default())
            .expect("direct render")
            .len();
        let got = too_large(err(&src, &opts(cs)));
        assert_eq!(got, (Limit::OutputBytes, want, 65536));
        sizes.push(want);
    }
    assert_eq!(sizes, vec![306_860, 217_808]);
    assert!(sizes[1] < sizes[0]);
}

#[test]
fn limits_run_before_render_and_detection() {
    let e = err(
        &fixture("over_nodes_would_panic.mmd"),
        &opts(Charset::Unicode),
    );
    assert_eq!(too_large(e), (Limit::Nodes, 42, 40));
    let e = err(
        &fixture("over_source_unsupported.mmd"),
        &opts(Charset::Unicode),
    );
    assert_eq!(too_large(e), (Limit::SourceBytes, 8193, 8192));
}

#[test]
fn rewrite_root_direction_cases() {
    let td = RootDirection::TopDown;
    for (src, want) in [
        ("graph LR\n  A --> B\n", "graph TD\n  A --> B\n"),
        ("flowchart RL\n  A --> B\n", "flowchart TD\n  A --> B\n"),
        ("graph lr\n  A --> B\n", "graph TD\n  A --> B\n"),
        ("\tgraph   LR;A-->B\n", "\tgraph   TD;A-->B\n"),
        ("graph LR; A[LR] --> B\n", "graph TD; A[LR] --> B\n"),
        ("graph LR\r\n  A --> B\r\n", "graph TD\r\n  A --> B\r\n"),
        ("graph LR", "graph TD"),
        ("graph LR A-->B\n", "graph TD A-->B\n"),
        ("graph LR\tA-->B\n", "graph TD\tA-->B\n"),
        (
            "%%{ note\ngraph LR\n  A-->B\n",
            "%%{ note\ngraph TD\n  A-->B\n",
        ),
    ] {
        let r = rewrite_root_direction(src, td);
        assert!(matches!(r, Cow::Owned(_)), "{src:?}");
        assert_eq!(r, want);
    }
    for src in [
        "graph TD\n  A --> B\n",
        "graph TB\n  A --> B\n",
        "graph BT\n  A --> B\n",
        "graph\n",
        "sequenceDiagram\n  A->>B: LR\n",
        "graph LRX\n",
        "",
        "%% c\n",
        "---\ntitle: x\ngraph LR\n  A-->B\n",
        "flowchart TD\n  subgraph s\n    direction LR\n    A --> B\n  end\n",
    ] {
        let r = rewrite_root_direction(src, td);
        assert!(matches!(r, Cow::Borrowed(_)), "{src:?}");
        assert_eq!(r, src);
    }
}

#[test]
fn rewrite_survives_front_matter_and_comments() {
    let src = fixture("lr_nested_dirs.mmd");
    let out = rewrite_root_direction(&src, RootDirection::TopDown);
    assert_eq!(out.len(), src.len());
    let diff: Vec<_> = src
        .lines()
        .zip(out.lines())
        .filter(|(a, b)| a != b)
        .collect();
    assert_eq!(diff, vec![("flowchart LR", "flowchart TD")]);
    assert_eq!(src.lines().count(), out.lines().count());
}

#[test]
fn lr_to_td_keeps_subgraph_directions() {
    let src = fixture("lr_nested_dirs.mmd");
    for (cs, label) in CHARSETS {
        let plain = ok(&src, &opts(cs));
        insta::assert_snapshot!(format!("lr_nested_dirs_{label}"), snap(&plain));
        let d = ok(&src, &flipped(cs));
        insta::assert_snapshot!(format!("lr_nested_dirs_flipped_{label}"), snap(&d));
        assert!(d.direction_overridden, "{label}");
        assert!(d.height > d.width, "{label}: {}x{}", d.width, d.height);
        assert!(
            d.lines.iter().any(|l| l.contains('X') && l.contains('Y')),
            "{label} X Y"
        );
        assert!(
            d.lines.iter().any(|l| l.contains('Q') && l.contains('P')),
            "{label} Q P"
        );
    }
}

#[test]
fn override_render_semantics() {
    let lr = fixture("lr_chain.mmd");
    let rewritten = rewrite_root_direction(&lr, RootDirection::TopDown).into_owned();
    for (cs, label) in CHARSETS {
        let plain = ok(&lr, &opts(cs));
        let d = ok(&lr, &flipped(cs));
        assert_eq!(d.lines, ok(&rewritten, &opts(cs)).lines, "{label}");
        assert!(d.direction_overridden);
        assert!(d.height > plain.height);
        insta::assert_snapshot!(format!("lr_chain_flipped_{label}"), snap(&d));
        for name in ["td_chain", "sequence_alt_loop"] {
            let src = fixture(&format!("{name}.mmd"));
            let d = ok(&src, &flipped(cs));
            assert_eq!(d.lines, ok(&src, &opts(cs)).lines, "{name} {label}");
            assert!(!d.direction_overridden, "{name} {label}");
        }
    }
}

#[test]
fn cache_key() {
    let src = "graph TD\n  A --> B\n";
    let k = render_cache_key(src, 76, 36, Charset::Unicode);
    assert_eq!(k, 0x4bef_a1e4_5c03_797a);
    assert_eq!(
        render_cache_key(src, 76, 36, Charset::Ascii),
        0x4bef_a2e4_5c03_7b2d
    );
    assert_eq!(
        render_cache_key(src, 36, 76, Charset::Unicode),
        0xd6d0_25e0_dfb5_3d7a
    );
    let changed = [
        render_cache_key(&format!("{src}x"), 76, 36, Charset::Unicode),
        render_cache_key(src, 77, 36, Charset::Unicode),
        render_cache_key(src, 75, 36, Charset::Unicode),
        render_cache_key(src, 76, 37, Charset::Unicode),
        render_cache_key(src, 76, 35, Charset::Unicode),
        render_cache_key(src, 76, 36, Charset::Ascii),
    ];
    for c in changed {
        assert_ne!(c, k);
    }
    assert_eq!(RENDERER_VERSION, "mmdflux-2.6.1");
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let t: toml::Table = std::fs::read_to_string(path).unwrap().parse().unwrap();
    let dep = t["dependencies"]["mmdflux"]
        .as_table()
        .expect("mmdflux table");
    let want = format!("={}", RENDERER_VERSION.trim_start_matches("mmdflux-"));
    assert_eq!(dep["version"].as_str(), Some(want.as_str()));
    assert_eq!(dep["default-features"].as_bool(), Some(false));
}

#[test]
fn error_codes_are_stable() {
    assert_eq!(
        RenderError::Unsupported(String::new()).code(),
        "render_unsupported"
    );
    assert_eq!(RenderError::Invalid(String::new()).code(), "render_invalid");
    let tl = RenderError::TooLarge {
        limit: Limit::Nodes,
        actual: 2,
        max: 1,
    };
    assert_eq!(tl.code(), "render_too_large");
    assert_eq!(tl.to_string(), "diagram too large: 2 nodes (max 1)");
    assert_eq!(RenderError::Panic(String::new()).code(), "render_panic");
    for (l, s) in [
        (Limit::SourceBytes, "source_bytes"),
        (Limit::Nodes, "nodes"),
        (Limit::Edges, "edges"),
        (Limit::Participants, "participants"),
        (Limit::Nesting, "nesting"),
        (Limit::LabelChars, "label_chars"),
        (Limit::OutputBytes, "output_bytes"),
    ] {
        assert_eq!(l.as_str(), s);
    }
}

#[test]
fn unrecognised_supported_keyword_is_invalid() {
    let e = err("graph;\n  A --> B\n", &opts(Charset::Unicode));
    assert_eq!(e.code(), "render_invalid");
    let m = e.to_string();
    assert!(m.contains("diagram type line not recognised: graph"), "{m}");
    assert!(!m.contains("directive"), "{m}");
}

#[test]
fn unsupported_kind_is_truncated() {
    match err(&fixture("mmds.json"), &opts(Charset::Unicode)) {
        RenderError::Unsupported(k) => assert_eq!(k, r#"{"version":1,"profiles":["mmds-core-v1"]"#),
        other => panic!("{other:?}"),
    }
}

#[test]
fn width_counts_cells_not_chars_on_the_widest_line() {
    // The edge label sticks out past both boxes, so the widest line is the label line.
    let src = "graph TD\n  A --> B\n  A -->|数据库数据库数据库数据库| B\n";
    let d = ok(src, &opts(Charset::Unicode));
    let max_chars = d.lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    let widest = d
        .lines
        .iter()
        .find(|l| l.contains("数据库数据库"))
        .expect("label line");
    assert_eq!(
        d.width,
        d.lines
            .iter()
            .map(|l| Line::raw(l.as_str()).width())
            .max()
            .unwrap_or(0)
    );
    assert!(
        d.width > max_chars,
        "{}x{} chars {max_chars}\n{}",
        d.width,
        d.height,
        widest
    );
}

#[test]
fn unclosed_directive_opener_is_a_comment_and_override_applies() {
    let src = "%%{ note\ngraph LR\n  A-->B\n";
    let plain = ok(src, &opts(Charset::Unicode));
    let d = ok(src, &flipped(Charset::Unicode));
    assert!(d.direction_overridden);
    assert!(d.height > plain.height);
}

#[test]
fn unclosed_front_matter_is_unsupported() {
    let e = err(
        "---\ntitle: x\ngraph LR\n  A-->B\n",
        &opts(Charset::Unicode),
    );
    assert_eq!(e.code(), "render_unsupported");
}
