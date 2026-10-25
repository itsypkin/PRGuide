//! Tests for the guide model and its parsers (T3-1). Drafts are built in
//! temporary directories; nothing touches git or the network.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::Command;
use std::time::Instant;

use prguide::anchors::{AnchorError, At, CodeBlock, Side};
use prguide::guide::GuideError;
use prguide::guide::model::*;
use prguide::guide::parse::*;

// ---------------------------------------------------------------- fixtures

fn sha(prefix: &str) -> String {
    format!("{prefix}{}", "0".repeat(40 - prefix.len()))
}

fn valid_manifest() -> String {
    [
        "format: prguide/v1".to_string(),
        "pr: { host: github.com, repo: owner/name, number: 1234 }".to_string(),
        format!(
            "snapshot: {{ base: {}, head: {} }}        # full SHAs, written by prguide",
            sha("9f1c"),
            sha("4e2a")
        ),
        "selection:".to_string(),
        "  kind: commits                                # full | commits".to_string(),
        format!("  base: {}", sha("77ab")),
        format!("  head: {}", sha("4e2a")),
        format!(
            "  commits: [{}, {}, {}]",
            sha("a1b2"),
            sha("c3d4"),
            sha("4e2a")
        ),
        "slides: [s1-overview, s2-controller]           # the agent fills this in".to_string(),
        String::new(),
    ]
    .join("\n")
}

const EXAMPLE_SLIDE: &str = "---
id: s2-controller
kind: step                 # overview | architecture | step
title: \"Controller: accept the new `retry` flag\"
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
    text: \"No test covers retry=false.\"
---
The request reaches `OrdersController::create` from the router above.
";

const OVERVIEW_SLIDE: &str = "---\nid: s1-overview\nkind: overview\ntitle: Overview\n---\nHello\n";

const HANDLER: &str = "  - id: handler\n    kind: diff\n    path: src/a.rs\n    side: new\n    start: 1\n    end: 2\n";
const CTX: &str = "  - id: ctx\n    kind: context\n    path: src/b.rs\n    start: 1\n    end: 2\n";

const FRONT: &str = "id: s1\nkind: step\ntitle: T\n";

/// A slide file from front matter lines and a body.
fn slide_text(front: &str, body: &str) -> String {
    format!("---\n{front}---\n{body}")
}

/// A step slide `s1` with the given code list (already indented) and a body.
fn with_code(code: &str) -> String {
    slide_text(&format!("{FRONT}code:\n{code}"), "body\n")
}

fn kind_slide(kind: &str, body: &str) -> String {
    slide_text(&format!("id: s1\nkind: {kind}\ntitle: T\n"), body)
}

fn parse_s1(text: &str) -> Result<Slide, GuideError> {
    parse_slide("slides/s1.md", text)
}

fn s1_err(text: &str) -> GuideError {
    parse_s1(text).unwrap_err()
}

fn manifest_err(text: &str) -> GuideError {
    parse_manifest(text).unwrap_err()
}

/// Asserts the error: code, file, field, and that the message has `needle`.
#[track_caller]
fn assert_err(err: &GuideError, code: &str, file: &str, field: &str, needle: &str) {
    assert_eq!(err.code(), code, "code of {err:?}");
    assert_eq!(err.file(), Some(file), "file of {err:?}");
    assert_eq!(err.field(), Some(field), "field of {err:?}");
    assert!(
        err.to_string().contains(needle),
        "message {:?} lacks {needle:?}",
        err.to_string()
    );
}

fn summary(errors: &[GuideError]) -> Vec<(String, String, String)> {
    errors
        .iter()
        .map(|e| {
            (
                e.code().to_string(),
                e.file().unwrap_or("?").to_string(),
                e.field().unwrap_or("?").to_string(),
            )
        })
        .collect()
}

fn expect(items: &[(&str, &str, &str)]) -> Vec<(String, String, String)> {
    items
        .iter()
        .map(|(a, b, c)| (a.to_string(), b.to_string(), c.to_string()))
        .collect()
}

fn write_files(dir: &Path, files: &[(&str, &str)]) {
    for (name, content) in files {
        let path = dir.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
}

fn draft(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write_files(dir.path(), files);
    dir
}

fn ids(list: &[SlideId]) -> Vec<&str> {
    list.iter().map(SlideId::as_str).collect()
}

fn id(s: &str) -> SlideId {
    SlideId::parse(s).unwrap()
}

// ---------------------------------------------------------------- manifest

/// P1 manifest_design_example_parses
#[test]
fn manifest_design_example_parses() {
    let manifest = parse_manifest(&valid_manifest()).unwrap();
    assert_eq!(manifest.format, "prguide/v1");
    assert_eq!(manifest.pr.host, "github.com");
    assert_eq!(manifest.pr.repo, "owner/name");
    assert_eq!(manifest.pr.number, 1234);
    assert_eq!(manifest.snapshot.base, sha("9f1c"));
    assert_eq!(manifest.snapshot.head, sha("4e2a"));
    assert_eq!(manifest.selection.kind, SelectionKind::Commits);
    assert_eq!(manifest.selection.base, sha("77ab"));
    assert_eq!(manifest.selection.head, sha("4e2a"));
    assert_eq!(
        manifest.selection.commits,
        vec![sha("a1b2"), sha("c3d4"), sha("4e2a")]
    );
    assert_eq!(ids(&manifest.slides), ["s1-overview", "s2-controller"]);

    let empty = valid_manifest().replace("[s1-overview, s2-controller]", "[]");
    assert!(parse_manifest(&empty).unwrap().slides.is_empty());
    // `slides:` with no value is an empty list too.
    let bare = valid_manifest().replace("slides: [s1-overview, s2-controller]", "slides:");
    assert!(parse_manifest(&bare).unwrap().slides.is_empty());
}

/// P2 manifest_round_trips
#[test]
fn manifest_round_trips() {
    let manifest = GuideManifest {
        format: FORMAT.to_string(),
        pr: ManifestPr {
            host: "github.com".into(),
            repo: "owner/name".into(),
            number: 7,
        },
        snapshot: ManifestSnapshot {
            base: sha("9f1c"),
            head: sha("4e2a"),
        },
        selection: ManifestSelection {
            kind: SelectionKind::Full,
            base: sha("9f1c"),
            head: sha("4e2a"),
            commits: vec![],
        },
        slides: vec![id("s1-overview"), id("s2")],
    };
    let text = serde_saphyr::to_string(&manifest).unwrap();
    assert_eq!(parse_manifest(&text).unwrap(), manifest);
}

/// P3 manifest_format_unsupported
#[test]
fn manifest_format_unsupported() {
    let text = valid_manifest().replace("prguide/v1", "prguide/v2");
    let err = manifest_err(&text);
    assert_err(
        &err,
        "format_unsupported",
        "guide.yaml",
        "format",
        "prguide/v1",
    );
    assert!(err.to_string().contains("prguide/v2"));
}

/// P4 manifest_slide_id_invalid
#[test]
fn manifest_slide_id_invalid() {
    let text = valid_manifest().replace("[s1-overview, s2-controller]", "[s1, S2, -x]");
    let err = manifest_err(&text);
    assert_err(&err, "slide_id_invalid", "guide.yaml", "slides[1]", "`S2`");
    assert!(err.to_string().contains("[a-z0-9]"));
}

/// P5 manifest_unknown_field
#[test]
fn manifest_unknown_field() {
    let text = valid_manifest().replace("number: 1234 }", "number: 1234, owner: x }");
    assert_err(
        &manifest_err(&text),
        "field_unknown",
        "guide.yaml",
        "pr.owner",
        "unknown field `owner`",
    );
    let text = format!("{}notes: x\n", valid_manifest());
    assert_err(
        &manifest_err(&text),
        "field_unknown",
        "guide.yaml",
        "notes",
        "unknown field `notes`",
    );
}

/// P6 manifest_missing_field
#[test]
fn manifest_missing_field() {
    let text = valid_manifest().replace(&format!("  head: {}\n", sha("4e2a")), "");
    assert_err(
        &manifest_err(&text),
        "field_missing",
        "guide.yaml",
        "selection.head",
        "missing field `head`",
    );
}

/// P7 manifest_invalid_value
#[test]
fn manifest_invalid_value() {
    let text = valid_manifest().replace("kind: commits", "kind: range");
    assert_err(
        &manifest_err(&text),
        "field_invalid",
        "guide.yaml",
        "selection.kind",
        "invalid value for `kind`",
    );
    let text = valid_manifest().replace("number: 1234", "number: -1");
    assert_err(
        &manifest_err(&text),
        "field_invalid",
        "guide.yaml",
        "pr.number",
        "invalid value for `number`",
    );
}

// ------------------------------------------------------------ front matter

/// P8 example_slide_parses
#[test]
fn example_slide_parses() {
    let slide = parse_slide("slides/s2-controller.md", EXAMPLE_SLIDE).unwrap();
    assert_eq!(slide.file, "slides/s2-controller.md");
    assert_eq!(slide.front.id.as_str(), "s2-controller");
    assert_eq!(slide.front.kind, SlideKind::Step);
    assert_eq!(slide.front.title, "Controller: accept the new `retry` flag");
    assert_eq!(
        slide.front.code,
        vec![
            CodeBlock::Diff {
                id: "handler".into(),
                path: "src/api/orders.rs".into(),
                side: Side::New,
                start: Some(40),
                end: Some(88),
            },
            CodeBlock::Context {
                id: "router".into(),
                path: "src/router.rs".into(),
                at: At::SnapshotHead,
                start: 12,
                end: 20,
            },
        ]
    );
    assert_eq!(
        slide.front.flags,
        vec![Flag {
            at: FlagAt {
                block: "handler".into(),
                line: Some(61)
            },
            text: "No test covers retry=false.".into(),
        }]
    );
    assert_eq!(
        slide.body,
        "The request reaches `OrdersController::create` from the router above.\n"
    );
    assert_eq!(slide.body_line, 22);
    assert!(slide.mermaid.is_none());
    assert!(slide.text_art.is_none());
    insta::assert_snapshot!(
        "example_slide",
        serde_json::to_string_pretty(&slide).unwrap()
    );
}

/// P9 example_slide_front_matter_round_trips
#[test]
fn example_slide_front_matter_round_trips() {
    let slide = parse_slide("slides/s2-controller.md", EXAMPLE_SLIDE).unwrap();
    let mut yaml = serde_saphyr::to_string(&slide.front).unwrap();
    if !yaml.ends_with('\n') {
        yaml.push('\n');
    }
    let text = format!("---\n{yaml}---\n{}", slide.body);
    let again = parse_slide("slides/s2-controller.md", &text).unwrap();
    assert_eq!(again.front, slide.front);
    assert_eq!(again.body, slide.body);
    assert_eq!(again.file, slide.file);
    assert_eq!(again.mermaid, slide.mermaid);
    assert_eq!(again.text_art, slide.text_art);
}

/// P10 slide_kinds
#[test]
fn slide_kinds() {
    for (word, kind) in [
        ("overview", SlideKind::Overview),
        ("architecture", SlideKind::Architecture),
        ("step", SlideKind::Step),
    ] {
        assert_eq!(parse_s1(&kind_slide(word, "x\n")).unwrap().front.kind, kind);
    }
    assert_err(
        &s1_err(&kind_slide("slide", "x\n")),
        "field_invalid",
        "slides/s1.md",
        "kind",
        "unknown variant `slide`",
    );
}

/// P11 slide_id_mismatch
#[test]
fn slide_id_mismatch() {
    let text = slide_text("id: s2\nkind: step\ntitle: T\n", "x\n");
    assert_err(
        &s1_err(&text),
        "slide_id_mismatch",
        "slides/s1.md",
        "id",
        "`s2`",
    );
    let dir = draft(&[("guide.yaml", &valid_manifest()), ("slides/s1.md", &text)]);
    let load = load_draft(dir.path());
    assert_eq!(
        summary(&load.errors),
        expect(&[("slide_id_mismatch", "slides/s1.md", "id")])
    );
    assert!(load.errors[0].to_string().contains("`s2`"));
    assert_eq!(ids(&load.slide_files), ["s1"]);
    assert!(load.slides.is_empty());
}

/// P12 slide_id_invalid_in_front_matter
#[test]
fn slide_id_invalid_in_front_matter() {
    let text = slide_text("id: S1\nkind: step\ntitle: T\n", "x\n");
    assert_err(
        &s1_err(&text),
        "slide_id_invalid",
        "slides/s1.md",
        "id",
        "`S1`",
    );
}

/// P13 block_id_invalid
#[test]
fn block_id_invalid() {
    let code =
        format!("{HANDLER}  - id: Handler\n    kind: diff\n    path: src/a.rs\n    side: new\n");
    assert_err(
        &s1_err(&with_code(&code)),
        "block_id_invalid",
        "slides/s1.md",
        &block_field(1, "id"),
        "`Handler`",
    );
}

/// P14 code_block_unknown_field
#[test]
fn code_block_unknown_field() {
    let code = format!(
        "{HANDLER}  - id: h2\n    kind: diff\n    path: src/a.rs\n    side: new\n    colour: red\n"
    );
    assert_err(
        &s1_err(&with_code(&code)),
        "field_unknown",
        "slides/s1.md",
        &block_field(1, "colour"),
        "unknown field `colour`",
    );
    let code = "  - id: c\n    kind: context\n    path: src/a.rs\n    side: new\n    start: 1\n    end: 2\n";
    assert_err(
        &s1_err(&with_code(code)),
        "field_unknown",
        "slides/s1.md",
        &block_field(0, "side"),
        "unknown field `side`",
    );
}

/// P15 code_block_missing_field
#[test]
fn code_block_missing_field() {
    let code = "  - id: c\n    kind: context\n    path: src/a.rs\n    start: 1\n";
    assert_err(
        &s1_err(&with_code(code)),
        "field_missing",
        "slides/s1.md",
        &block_field(0, "end"),
        "missing field `end`",
    );
}

fn ctx_block(lines: &str) -> String {
    format!("  - id: c\n    kind: context\n    path: src/a.rs\n{lines}")
}

/// P16 code_block_invalid_start_or_end
#[test]
fn code_block_invalid_start_or_end() {
    let cases: [(&str, &str); 3] = [
        ("    start: abc\n    end: 2\n", "start"),
        ("    start: 1\n    end: -2\n", "end"),
        ("    end: xyz\n    start: abc\n", "start"),
    ];
    for (lines, key) in cases {
        assert_err(
            &s1_err(&with_code(&ctx_block(lines))),
            "field_invalid",
            "slides/s1.md",
            &block_field(0, key),
            &format!("invalid value for `{key}`"),
        );
    }
    let text =
        with_code("  - id: [x]\n    kind: context\n    path: [y]\n    start: 1\n    end: 2\n");
    assert_err(
        &s1_err(&text),
        "field_invalid",
        "slides/s1.md",
        &block_field(0, "id"),
        "invalid value for `id`",
    );
}

/// P17 code_block_enum_fields
#[test]
fn code_block_enum_fields() {
    let cases = [
        (
            "  - id: d\n    kind: diff\n    path: p\n    side: left\n",
            "field_invalid",
            "side",
            "unknown variant `left`",
        ),
        (
            &ctx_block("    at: head\n    start: 1\n    end: 2\n"),
            "field_invalid",
            "at",
            "invalid value for `at`",
        ),
        (
            "  - id: d\n    kind: blob\n    path: p\n",
            "field_invalid",
            "kind",
            "invalid value for `kind`",
        ),
        (
            "  - id: d\n    path: p\n",
            "field_missing",
            "kind",
            "missing field `kind`",
        ),
    ];
    for (code, error_code, key, needle) in cases {
        assert_err(
            &s1_err(&with_code(code)),
            error_code,
            "slides/s1.md",
            &block_field(0, key),
            needle,
        );
    }
}

/// P18 code_block_string_fields
#[test]
fn code_block_string_fields() {
    let text = with_code("  - id: [x]\n    kind: diff\n    path: p\n    side: new\n");
    assert_err(
        &s1_err(&text),
        "field_invalid",
        "slides/s1.md",
        &block_field(0, "id"),
        "invalid value for `id`",
    );
    let text = with_code("  - id: x\n    kind: diff\n    path:\n    side: new\n");
    assert_err(
        &s1_err(&text),
        "field_invalid",
        "slides/s1.md",
        &block_field(0, "path"),
        "invalid value for `path`",
    );
}

/// P19 flags_fields
#[test]
fn flags_fields() {
    let flags = |body: &str| slide_text(&format!("{FRONT}flags:\n{body}"), "x\n");
    let cases = [
        (
            flags("  - at: { block: a, line: x }\n    text: t\n"),
            "field_invalid",
            "flags[0].at.line",
            "invalid value for `line`",
        ),
        (
            flags("  - at: { block: a }\n    text: t\n  - at: { block: a }\n"),
            "field_missing",
            "flags[1].text",
            "missing field `text`",
        ),
        (
            flags("  - at: { line: 3 }\n    text: t\n"),
            "field_missing",
            "flags[0].at.block",
            "missing field `block`",
        ),
        (
            flags("  - at: { block: a, col: 1 }\n    text: t\n"),
            "field_unknown",
            "flags[0].at.col",
            "unknown field `col`",
        ),
    ];
    for (text, code, field, needle) in cases {
        assert_err(&s1_err(&text), code, "slides/s1.md", field, needle);
    }
}

/// P20 top_level_fields
#[test]
fn top_level_fields() {
    assert_err(
        &s1_err(&slide_text(&format!("{FRONT}extra: 1\n"), "x\n")),
        "field_unknown",
        "slides/s1.md",
        "extra",
        "unknown field `extra`",
    );
    assert_err(
        &s1_err(&slide_text("id: s1\nkind: step\n", "x\n")),
        "field_missing",
        "slides/s1.md",
        "title",
        "missing field `title`",
    );
    assert_err(
        &s1_err(&slide_text("id: s1\nkind: step\ntitle: ~\n", "x\n")),
        "field_invalid",
        "slides/s1.md",
        "title",
        "invalid value for `title`",
    );
    let slide = parse_s1(&slide_text("id: s1\nkind: step\ntitle: yes\n", "x\n")).unwrap();
    assert_eq!(slide.front.title, "yes");
}

/// P21 duplicate_keys
#[test]
fn duplicate_keys() {
    assert_err(
        &s1_err(&slide_text("id: s1\nid: s1\nkind: step\ntitle: T\n", "x\n")),
        "key_duplicate",
        "slides/s1.md",
        "id",
        "duplicate key `id`",
    );
    let code = "  - id: c\n    kind: diff\n    path: p\n    side: new\n    side: old\n";
    assert_err(
        &s1_err(&with_code(code)),
        "key_duplicate",
        "slides/s1.md",
        &block_field(0, "side"),
        "duplicate key `side`",
    );
    let text = format!("{}slides: []\n", valid_manifest());
    assert_err(
        &manifest_err(&text),
        "key_duplicate",
        "guide.yaml",
        "slides",
        "duplicate key `slides`",
    );
}

/// P22 yaml_features_rejected
#[test]
fn yaml_features_rejected() {
    let cases = [
        (
            slide_text("id: &a s1\nkind: step\ntitle: T\n", "x\n"),
            "anchor",
        ),
        (
            slide_text("id: s1\nkind: step\ntitle: *x\n", "x\n"),
            "alias",
        ),
        (
            slide_text(
                &format!("{FRONT}flags:\n  - at: {{ block: handler }}\n    <<: {{ text: t }}\n"),
                "x\n",
            ),
            "merge key",
        ),
        (
            slide_text("id: !foo s1\nkind: step\ntitle: T\n", "x\n"),
            "tag",
        ),
    ];
    for (text, feature) in cases {
        assert_err(
            &s1_err(&text),
            "yaml_feature_not_allowed",
            "slides/s1.md",
            "",
            feature,
        );
    }
    let text = valid_manifest().replace("format: prguide/v1", "format: &a prguide/v1");
    assert_err(
        &manifest_err(&text),
        "yaml_feature_not_allowed",
        "guide.yaml",
        "",
        "anchor",
    );
    let slide = parse_s1(&slide_text("id: s1\nkind: step\ntitle: !!str T\n", "x\n")).unwrap();
    assert_eq!(slide.front.title, "T");
}

fn alias_bomb(indent: &str) -> String {
    let mut out = format!("{indent}- &a0 [x, x, x, x, x, x, x, x, x]\n");
    for level in 1..9 {
        let refs = vec![format!("*a{}", level - 1); 9].join(", ");
        out.push_str(&format!("{indent}- &a{level} [{refs}]\n"));
    }
    out
}

/// P23 yaml_bomb_fails_fast
#[test]
fn yaml_bomb_fails_fast() {
    let started = Instant::now();
    let text = slide_text(&format!("{FRONT}code:\n{}", alias_bomb("  ")), "x\n");
    assert_err(
        &s1_err(&text),
        "yaml_feature_not_allowed",
        "slides/s1.md",
        "",
        "anchor",
    );
    let manifest = valid_manifest().replace(
        "slides: [s1-overview, s2-controller]",
        &format!("slides:\n{}", alias_bomb("  ")),
    );
    assert_err(
        &manifest_err(&manifest),
        "yaml_feature_not_allowed",
        "guide.yaml",
        "",
        "anchor",
    );
    assert!(started.elapsed().as_secs() < 2);
}

/// P24 yaml_invalid_cases
#[test]
fn yaml_invalid_cases() {
    let nested = format!("{}{}", "[".repeat(100), "]".repeat(100));
    let cases: Vec<(String, &str)> = vec![
        (slide_text("id: s1\n kind: step\n", "x\n"), "line 3"),
        (
            slide_text("id: s1\nkind: step\ntitle: \"abc\n", "x\n"),
            "line",
        ),
        (slide_text("id: s1\n\tkind: step\n", "x\n"), "line"),
        ("---\n---\n".to_string(), "empty"),
        (slide_text("- a\n- b\n", "x\n"), "invalid YAML"),
        (slide_text("just text\n", "x\n"), "invalid YAML"),
        (
            slide_text(&format!("id: s1\nkind: step\ntitle: {nested}\n"), "x\n"),
            "invalid YAML",
        ),
    ];
    for (text, needle) in &cases {
        assert_err(&s1_err(text), "yaml_invalid", "slides/s1.md", "", needle);
    }
    let two = format!("{}---\n{}", valid_manifest(), valid_manifest());
    assert_err(
        &manifest_err(&two),
        "yaml_invalid",
        "guide.yaml",
        "",
        "invalid YAML",
    );
}

/// P25 front_matter_delimiters_accepted
#[test]
fn front_matter_delimiters_accepted() {
    let cases: Vec<(String, &str)> = vec![
        (
            "---  \nid: s1\nkind: step\ntitle: T\n---\t\nbody\n".to_string(),
            "body\n",
        ),
        (slide_text(FRONT, "body\n").replace('\n', "\r\n"), "body\n"),
        (format!("\u{feff}{}", slide_text(FRONT, "body\n")), "body\n"),
        ("---\nid: s1\nkind: step\ntitle: T\n---".to_string(), ""),
        (slide_text(FRONT, "a\n\n---\n\nb\n"), "a\n\n---\n\nb\n"),
        (slide_text(FRONT, "```\n---\n```\n"), "```\n---\n```\n"),
    ];
    for (text, body) in &cases {
        let slide = parse_s1(text).unwrap_or_else(|e| panic!("{text:?}: {e}"));
        assert_eq!(&slide.body, body, "{text:?}");
    }
}

/// P26 front_matter_missing
#[test]
fn front_matter_missing() {
    for text in [
        "",
        "id: s1\n",
        " ---\nid: s1\n---\n",
        "----\nid: s1\n---\n",
        "\u{feff}id: s1\n",
    ] {
        assert_err(
            &s1_err(text),
            "front_matter_missing",
            "slides/s1.md",
            "",
            "must start with a `---` line",
        );
    }
}

/// P27 front_matter_unclosed
#[test]
fn front_matter_unclosed() {
    for text in ["---\nid: s1\n", "---\nid: s1\n...\nbody"] {
        assert_err(
            &s1_err(text),
            "front_matter_unclosed",
            "slides/s1.md",
            "",
            "no closing `---`",
        );
    }
}

/// P28 error_lines_are_file_lines
#[test]
fn error_lines_are_file_lines() {
    let text = slide_text(&format!("{FRONT}# a\n# b\n# c\n"), "x\ny\n<div>\n");
    assert_err(
        &s1_err(&text),
        "html_not_allowed",
        "slides/s1.md",
        "body",
        "line 11",
    );
    let text = slide_text("id: s1\nkind: step\ntitle: ~\n", "x\n");
    assert_err(
        &s1_err(&text),
        "field_invalid",
        "slides/s1.md",
        "title",
        "line 4",
    );
}

// -------------------------------------------------------------------- body

fn html_err(kind: &str, body: &str) -> GuideError {
    s1_err(&kind_slide(kind, body))
}

#[track_caller]
fn assert_html(kind: &str, body: &str) {
    let err = html_err(kind, body);
    assert_err(&err, "html_not_allowed", "slides/s1.md", "body", "raw HTML");
}

/// P29 html_block_rejected
#[test]
fn html_block_rejected() {
    for kind in ["step", "overview", "architecture"] {
        assert_html(kind, "<div>\nx\n</div>\n");
    }
}

/// P30 html_inline_rejected
#[test]
fn html_inline_rejected() {
    assert_html("step", "a <span>b</span> c");
}

/// P31 html_comment_rejected
#[test]
fn html_comment_rejected() {
    for body in ["<!-- x -->", "<!-- x\ny\n-->\n", "text <!-- x --> more"] {
        assert_html("step", body);
    }
}

/// P32 html_in_list_and_quote_rejected
#[test]
fn html_in_list_and_quote_rejected() {
    let cases = [
        ("- item <b>x</b>", 6),
        ("- a\n\n  <div>\n  z\n  </div>", 8),
        ("> <div>q</div>", 6),
        ("> text <i>i</i>", 6),
    ];
    for (body, line) in cases {
        let text = format!("---\nid: s1\nkind: step\ntitle: T\n---\n{body}");
        let err = s1_err(&text);
        assert_err(
            &err,
            "html_not_allowed",
            "slides/s1.md",
            "body",
            &format!("line {line})"),
        );
    }
}

/// P33 html_other_forms_rejected
#[test]
fn html_other_forms_rejected() {
    for body in [
        "<?php echo 1; ?>",
        "<!DOCTYPE html>",
        "<![CDATA[x]]>",
        "line<br/>next",
        "</div>",
        "<div",
        "# T <em>x</em>",
        "[<b>x</b>](http://u)",
        "<b>x</b>\n===",
        "<foo-bar>x</foo-bar>",
        "| <b>x</b> | y |",
    ] {
        assert_html("step", body);
    }
}

/// P34 not_html_accepted
#[test]
fn not_html_accepted() {
    for body in [
        "<https://example.com/a?b=1>",
        "<foo@bar.com>",
        "`<div>`",
        "```html\n<div>x</div>\n```",
        "    <div>x</div>",
        "\\<div>",
        "&lt;div&gt;",
        "a < b > c",
    ] {
        let slide = parse_s1(&kind_slide("step", body)).unwrap_or_else(|e| panic!("{body:?}: {e}"));
        assert_eq!(slide.body, body);
    }
    let slide = parse_s1(&kind_slide(
        "architecture",
        "```mermaid\nA[\"<b>\"]-->B\n```\n",
    ))
    .unwrap();
    assert_eq!(slide.mermaid.unwrap().source, "A[\"<b>\"]-->B\n");
}

/// P35 html_reported_before_diagrams
#[test]
fn html_reported_before_diagrams() {
    let body = "```mermaid\na\n```\n\n```mermaid\nb\n```\n\n<span>x</span>\n";
    assert_html("architecture", body);
}

// ---------------------------------------------------------------- diagrams

/// P36 architecture_blocks_extracted
#[test]
fn architecture_blocks_extracted() {
    let body = "Intro paragraph.\n\n```mermaid\nflowchart LR\n  A-->B\n```\n\nMiddle paragraph.\n\n~~~text\nA -> B\n~~~\n\nClosing paragraph.\n";
    // Probed once: the Start event's byte range runs through the closing
    // fence but not its "\n", so cutting leaves that line break behind.
    let expected = "Intro paragraph.\n\n\n\nMiddle paragraph.\n\n\n\nClosing paragraph.\n";
    let text = kind_slide("architecture", body);
    for text in [text.clone(), text.replace('\n', "\r\n")] {
        let slide = parse_s1(&text).unwrap();
        let mermaid = slide.mermaid.unwrap();
        let art = slide.text_art.unwrap();
        assert_eq!(mermaid.source, "flowchart LR\n  A-->B\n");
        assert_eq!(art.source, "A -> B\n");
        // The body starts on file line 6; the fences are 3 and 9 lines in.
        assert_eq!(mermaid.line, 6 + 2);
        assert_eq!(art.line, 6 + 9);
        assert_eq!(slide.body, expected);
    }
}

fn arch(body: &str) -> Result<Slide, GuideError> {
    parse_s1(&kind_slide("architecture", body))
}

/// P37 duplicate_mermaid_rejected
#[test]
fn duplicate_mermaid_rejected() {
    // body_line is 6: the second fence is on body line 5.
    let err = arch("```mermaid\na\n```\n\n```mermaid\nb\n```\n").unwrap_err();
    assert_err(
        &err,
        "duplicate_diagram_block",
        "slides/s1.md",
        "body",
        "more than one `mermaid` block",
    );
    assert!(err.to_string().contains("line 10"), "{err}");
}

/// P38 duplicate_text_rejected
#[test]
fn duplicate_text_rejected() {
    let err = arch("```mermaid\na\n```\n\n```text\nb\n```\n\n```text\nc\n```\n").unwrap_err();
    assert_err(
        &err,
        "duplicate_diagram_block",
        "slides/s1.md",
        "body",
        "more than one `text` block",
    );
    assert!(err.to_string().contains("line 14"), "{err}");
}

/// P39 diagram_info_string_rules
#[test]
fn diagram_info_string_rules() {
    for body in [
        "```mermaid title\nx\n```\n",
        "```  mermaid\nx\n```\n",
        "~~~mermaid\nx\n~~~\n",
    ] {
        let slide = arch(body).unwrap();
        assert_eq!(slide.mermaid.unwrap().source, "x\n", "{body:?}");
    }
    for body in [
        "```Mermaid\nx\n```\n",
        "```{.mermaid}\nx\n```\n",
        "```mermaidx\nx\n```\n",
        "    mermaid\n    x\n",
    ] {
        let slide = arch(body).unwrap();
        assert!(slide.mermaid.is_none(), "{body:?}");
        assert_eq!(slide.body, body);
    }
}

/// P40 diagram_block_nested_rejected
#[test]
fn diagram_block_nested_rejected() {
    for body in ["- ```mermaid\n  x\n  ```\n", "> ```text\n> q\n> ```\n"] {
        assert_err(
            &arch(body).unwrap_err(),
            "diagram_block_nested",
            "slides/s1.md",
            "body",
            "top level",
        );
        let slide = parse_s1(&kind_slide("step", body)).unwrap();
        assert_eq!(slide.body, body);
    }
}

/// P41 other_kinds_keep_fences
#[test]
fn other_kinds_keep_fences() {
    let body = "```mermaid\na\n```\n\n```mermaid\nb\n```\n\n```text\nc\n```\n\n```text\nd\n```\n";
    for kind in ["step", "overview"] {
        let slide = parse_s1(&kind_slide(kind, body)).unwrap();
        assert!(slide.mermaid.is_none() && slide.text_art.is_none());
        assert_eq!(slide.body, body);
    }
}

/// P42 architecture_without_diagram_parses
#[test]
fn architecture_without_diagram_parses() {
    let slide = arch("Just words.\n").unwrap();
    assert!(slide.mermaid.is_none() && slide.text_art.is_none());
    assert_eq!(slide.body, "Just words.\n");
}

/// P43 unclosed_mermaid_fence
#[test]
fn unclosed_mermaid_fence() {
    let slide = arch("Intro.\n\n```mermaid\nflowchart\n").unwrap();
    assert_eq!(slide.mermaid.unwrap().source, "flowchart\n");
    assert_eq!(slide.body, "Intro.\n\n");
}

// ------------------------------------------------------------- code paths

/// P44 code_path_rules
#[test]
fn code_path_rules() {
    // (YAML double-quoted form, the path it decodes to, B.6's reason)
    let rejected: [(&str, &str, &str); 15] = [
        ("\"\"", "", "empty path"),
        ("\"a\\0b\"", "a\u{0}b", "control character"),
        ("\"a\\nb\"", "a\nb", "control character"),
        ("\"a\\x7Fb\"", "a\u{7f}b", "control character"),
        ("\"a\\\\b\"", "a\\b", "backslash"),
        ("\"/abs\"", "/abs", "absolute"),
        ("\"C:/x\"", "C:/x", "drive prefix"),
        ("\"c:x\"", "c:x", "drive prefix"),
        ("\"a//b\"", "a//b", "empty segment"),
        ("\"a/\"", "a/", "empty segment"),
        ("\".\"", ".", "`.` segment"),
        ("\"a/./b\"", "a/./b", "`.` segment"),
        ("\"..\"", "..", "`..` segment"),
        ("\"a/..\"", "a/..", "`..` segment"),
        ("\"../x\"", "../x", "`..` segment"),
    ];
    for (yaml, raw, reason) in rejected {
        assert_eq!(check_code_path(raw), Err(reason.to_string()), "{raw:?}");
        let code = format!(
            "{CTX}  - id: bad\n    kind: context\n    path: {yaml}\n    start: 1\n    end: 2\n"
        );
        let err = s1_err(&with_code(&code));
        assert_err(
            &err,
            "path_invalid",
            "slides/s1.md",
            &block_field(1, "path"),
            reason,
        );
    }
    for ok in [
        "src/a.rs",
        "a..b/c.rs",
        ".github/ci.yml",
        "dir/file with space.rs",
        "src/é.rs",
        "docs/a:b.md",
    ] {
        assert_eq!(check_code_path(ok), Ok(()), "{ok:?}");
    }
}

/// P45 code_path_is_lexical
#[test]
fn code_path_is_lexical() {
    let code = "  - id: c\n    kind: context\n    path: src/link/x.rs\n    start: 1\n    end: 2\n";
    let dir = draft(&[
        ("guide.yaml", &valid_manifest()),
        (
            "slides/s1.md",
            &slide_text(&format!("{FRONT}code:\n{code}"), "x\n"),
        ),
    ]);
    let load = load_draft(dir.path());
    assert!(load.errors.is_empty(), "{:?}", load.errors);
    assert_eq!(load.slides.len(), 1);
}

// ------------------------------------------------------------------- draft

/// P46 valid_draft_loads
#[test]
fn valid_draft_loads() {
    let dir = draft(&[
        ("guide.yaml", &valid_manifest()),
        ("slides/s1-overview.md", OVERVIEW_SLIDE),
        ("slides/s2-controller.md", EXAMPLE_SLIDE),
    ]);
    let load = load_draft(dir.path());
    assert!(load.errors.is_empty(), "{:?}", load.errors);
    assert!(load.manifest.is_some());
    assert_eq!(ids(&load.slide_files), ["s1-overview", "s2-controller"]);
    let parsed: Vec<&str> = load.slides.iter().map(|s| s.front.id.as_str()).collect();
    assert_eq!(parsed, ["s1-overview", "s2-controller"]);
}

/// P47 draft_root_symlink
#[test]
fn draft_root_symlink() {
    let real = draft(&[
        ("guide.yaml", &valid_manifest()),
        ("slides/s1-overview.md", OVERVIEW_SLIDE),
    ]);
    let tmp = tempfile::tempdir().unwrap();
    let link = tmp.path().join("link");
    symlink(real.path(), &link).unwrap();
    let load = load_draft(&link);
    assert_eq!(load.errors.len(), 1);
    assert_err(&load.errors[0], "symlink_not_allowed", "", "", "symlink");
    assert!(load.manifest.is_none() && load.slides.is_empty() && load.slide_files.is_empty());
}

/// P48 draft_root_invalid
#[test]
fn draft_root_invalid() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("file");
    fs::write(&file, "x").unwrap();
    for path in [tmp.path().join("missing"), file] {
        let load = load_draft(&path);
        assert_eq!(load.errors.len(), 1, "{:?}", load.errors);
        assert_err(&load.errors[0], "draft_invalid", "", "", "draft");
        assert!(load.manifest.is_none() && load.slides.is_empty());
    }
}

/// P49 draft_under_symlinked_ancestor_loads
#[test]
fn draft_under_symlinked_ancestor_loads() {
    let tmp = tempfile::tempdir().unwrap();
    write_files(
        &tmp.path().join("real/draft"),
        &[
            ("guide.yaml", &valid_manifest()),
            ("slides/s1-overview.md", OVERVIEW_SLIDE),
        ],
    );
    symlink(tmp.path().join("real"), tmp.path().join("link")).unwrap();
    let load = load_draft(&tmp.path().join("link/draft"));
    assert!(load.errors.is_empty(), "{:?}", load.errors);
    assert_eq!(load.slides.len(), 1);
}

/// P50 symlinked_slides_dir
#[test]
fn symlinked_slides_dir() {
    let tmp = tempfile::tempdir().unwrap();
    write_files(&tmp.path().join("outside"), &[("s1.md", OVERVIEW_SLIDE)]);
    let dir = draft(&[("guide.yaml", &valid_manifest())]);
    symlink(tmp.path().join("outside"), dir.path().join("slides")).unwrap();
    let load = load_draft(dir.path());
    assert_eq!(load.errors.len(), 1, "{:?}", load.errors);
    assert_err(
        &load.errors[0],
        "symlink_not_allowed",
        "slides",
        "",
        "symlink",
    );
    assert!(load.slide_files.is_empty() && load.slides.is_empty());
}

/// P51 symlinked_slide_file
#[test]
fn symlinked_slide_file() {
    let tmp = tempfile::tempdir().unwrap();
    let outside = tmp.path().join("outside");
    write_files(
        &outside,
        &[("s2.md", &OVERVIEW_SLIDE.replace("s1-overview", "s2"))],
    );
    let dir = draft(&[
        ("guide.yaml", &valid_manifest()),
        ("slides/s1.md", &kind_slide("step", "x\n")),
    ]);
    symlink(outside.join("s2.md"), dir.path().join("slides/s2.md")).unwrap();
    symlink(
        dir.path().join("slides/s1.md"),
        dir.path().join("slides/s3.md"),
    )
    .unwrap();
    let load = load_draft(dir.path());
    assert_eq!(
        summary(&load.errors),
        expect(&[
            ("symlink_not_allowed", "slides/s2.md", ""),
            ("symlink_not_allowed", "slides/s3.md", ""),
        ])
    );
    assert!(
        load.errors
            .iter()
            .all(|e| e.to_string().contains("symlink"))
    );
    assert_eq!(ids(&load.slide_files), ["s1"]);
    assert_eq!(load.slides.len(), 1);
}

/// P52 symlinked_guide_yaml
#[test]
fn symlinked_guide_yaml() {
    let tmp = tempfile::tempdir().unwrap();
    write_files(tmp.path(), &[("outside.yaml", &valid_manifest())]);
    let dir = draft(&[]);
    symlink(
        tmp.path().join("outside.yaml"),
        dir.path().join("guide.yaml"),
    )
    .unwrap();
    let load = load_draft(dir.path());
    assert_eq!(load.errors.len(), 1, "{:?}", load.errors);
    assert_err(
        &load.errors[0],
        "symlink_not_allowed",
        "guide.yaml",
        "",
        "symlink",
    );
    assert!(load.manifest.is_none());
}

/// P53 dangling_symlink
#[test]
fn dangling_symlink() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = draft(&[
        ("guide.yaml", &valid_manifest()),
        ("slides/s1-overview.md", OVERVIEW_SLIDE),
    ]);
    symlink(tmp.path().join("missing"), dir.path().join("slides/s2.md")).unwrap();
    let load = load_draft(dir.path());
    assert_eq!(load.errors.len(), 1, "{:?}", load.errors);
    assert_err(
        &load.errors[0],
        "symlink_not_allowed",
        "slides/s2.md",
        "",
        "symlink",
    );
}

/// P54 unexpected_root_entries
#[test]
fn unexpected_root_entries() {
    let dir = draft(&[
        ("guide.yaml", &valid_manifest()),
        ("notes.md", "x"),
        ("extra/f.txt", "x"),
        ("Slides/s1.md", OVERVIEW_SLIDE),
    ]);
    let load = load_draft(dir.path());
    assert_eq!(
        summary(&load.errors),
        expect(&[
            ("unexpected_file", "Slides", ""),
            ("unexpected_file", "extra", ""),
            ("unexpected_file", "notes.md", ""),
        ])
    );
    let notes = load.errors[2].to_string();
    assert!(notes.contains("only guide.yaml and slides/<id>.md are allowed in a draft"));
    assert!(notes.contains("remove `notes.md`"));

    let dir = draft(&[("guide.yaml/inner.txt", "x")]);
    let load = load_draft(dir.path());
    assert_eq!(
        summary(&load.errors),
        expect(&[("unexpected_file", "guide.yaml", "")])
    );

    let dir = draft(&[("GUIDE.yaml", &valid_manifest())]);
    let load = load_draft(dir.path());
    assert_eq!(
        summary(&load.errors),
        expect(&[
            ("unexpected_file", "GUIDE.yaml", ""),
            ("manifest_missing", "guide.yaml", ""),
        ])
    );
}

/// P55 slide_file_names
#[test]
fn slide_file_names() {
    let long65 = format!("{}.md", "a".repeat(65));
    let bad = [
        "S1.md",
        "s1.MD",
        "s1.txt",
        ".s1.md",
        "s1.md~",
        "-s1.md",
        "s_1.md",
        long65.as_str(),
        "\u{0455}1.md",
        "s\u{ff11}.md",
    ];
    for name in bad {
        let file = format!("slides/{name}");
        let dir = draft(&[("guide.yaml", &valid_manifest()), (&file, OVERVIEW_SLIDE)]);
        let load = load_draft(dir.path());
        assert_eq!(
            summary(&load.errors),
            expect(&[("slide_file_name_invalid", &file, "")]),
            "{name}"
        );
        assert!(load.errors[0].to_string().contains("slides/<id>.md"));
        assert!(load.slide_files.is_empty());
    }
    let long64 = "a".repeat(64);
    for stem in ["a-".to_string(), long64] {
        let text = kind_slide("step", "x\n").replace("id: s1", &format!("id: {stem}"));
        let dir = draft(&[
            ("guide.yaml", &valid_manifest()),
            (&format!("slides/{stem}.md"), &text),
        ]);
        let load = load_draft(dir.path());
        assert!(load.errors.is_empty(), "{stem}: {:?}", load.errors);
        assert_eq!(ids(&load.slide_files), [stem.as_str()]);
    }
    let dir = draft(&[("guide.yaml", &valid_manifest()), ("slides/sub/x.md", "x")]);
    let load = load_draft(dir.path());
    assert_eq!(
        summary(&load.errors),
        expect(&[("unexpected_file", "slides/sub", "")])
    );
}

/// P56 id_rules
#[test]
fn id_rules() {
    let long64 = "a".repeat(64);
    let long65 = "a".repeat(65);
    let good = ["a", "0", "a-", "a--b", long64.as_str()];
    let bad = [
        "",
        "-a",
        "A",
        "a_b",
        "a.b",
        "\u{e9}",
        "\u{0455}",
        "\u{ff11}",
        "a\n",
        "a ",
        long65.as_str(),
    ];
    for s in good {
        assert!(is_valid_id(s), "{s:?}");
        assert_eq!(SlideId::parse(s).unwrap().file(), format!("slides/{s}.md"));
    }
    for s in bad {
        assert!(!is_valid_id(s), "{s:?}");
        assert!(SlideId::parse(s).is_none(), "{s:?}");
    }
}

/// P57 manifest_missing
#[test]
fn manifest_missing() {
    let dir = draft(&[(
        "slides/s1.md",
        OVERVIEW_SLIDE.replace("s1-overview", "s1").as_str(),
    )]);
    let load = load_draft(dir.path());
    assert_eq!(load.errors.len(), 1, "{:?}", load.errors);
    assert_err(
        &load.errors[0],
        "manifest_missing",
        "guide.yaml",
        "",
        "guide.yaml",
    );
    assert_eq!(load.slides.len(), 1);
}

/// P58 missing_slides_dir_is_empty
#[test]
fn missing_slides_dir_is_empty() {
    let dir = draft(&[("guide.yaml", &valid_manifest())]);
    let load = load_draft(dir.path());
    assert!(load.errors.is_empty(), "{:?}", load.errors);
    assert!(load.manifest.is_some());
    assert!(load.slide_files.is_empty() && load.slides.is_empty());
}

/// P59 errors_one_per_file_sorted
#[test]
fn errors_one_per_file_sorted() {
    let valid = |name: &str| kind_slide("step", "x\n").replace("id: s1", &format!("id: {name}"));
    let html = kind_slide("step", "<div>\nx\n</div>\n").replace("id: s1", "id: b");
    let manifest = format!("{}notes: x\n", valid_manifest());
    let dir = draft(&[
        ("guide.yaml", &manifest),
        ("slides/b.md", &html),
        ("slides/a.md", &valid("a")),
    ]);
    symlink(
        dir.path().join("slides/a.md"),
        dir.path().join("slides/z.md"),
    )
    .unwrap();
    let load = load_draft(dir.path());
    assert_eq!(
        summary(&load.errors),
        expect(&[
            ("field_unknown", "guide.yaml", "notes"),
            ("html_not_allowed", "slides/b.md", "body"),
            ("symlink_not_allowed", "slides/z.md", ""),
        ])
    );
    assert!(load.manifest.is_none());
    assert_eq!(ids(&load.slide_files), ["a", "b"]);
    let parsed: Vec<&str> = load.slides.iter().map(|s| s.front.id.as_str()).collect();
    assert_eq!(parsed, ["a"]);

    // No guide.yaml: manifest_missing can only be emitted after the root is
    // read, so an unsorted result would not come out in this order.
    let dir = draft(&[("notes.md", "x"), ("slides/b.md", &html)]);
    let load = load_draft(dir.path());
    assert_eq!(
        summary(&load.errors),
        expect(&[
            ("manifest_missing", "guide.yaml", ""),
            ("unexpected_file", "notes.md", ""),
            ("html_not_allowed", "slides/b.md", "body"),
        ])
    );
}

/// P60 file_not_utf8
#[test]
fn file_not_utf8() {
    let dir = draft(&[("guide.yaml", &valid_manifest())]);
    fs::create_dir(dir.path().join("slides")).unwrap();
    fs::write(dir.path().join("slides/s1.md"), b"---\xff\n").unwrap();
    let load = load_draft(dir.path());
    assert_eq!(load.errors.len(), 1, "{:?}", load.errors);
    assert_err(
        &load.errors[0],
        "file_not_utf8",
        "slides/s1.md",
        "",
        "UTF-8",
    );

    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("guide.yaml"), b"---\xff\n").unwrap();
    let load = load_draft(dir.path());
    assert_eq!(load.errors.len(), 1, "{:?}", load.errors);
    assert_err(&load.errors[0], "file_not_utf8", "guide.yaml", "", "UTF-8");
}

/// P61 file_unreadable
#[test]
fn file_unreadable() {
    let dir = draft(&[
        ("guide.yaml", &valid_manifest()),
        ("slides/s1.md", OVERVIEW_SLIDE),
    ]);
    let path = dir.path().join("slides/s1.md");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
    assert!(fs::read(&path).is_err(), "run the tests as a non-root user");
    let load = load_draft(dir.path());
    assert_eq!(load.errors.len(), 1, "{:?}", load.errors);
    assert_err(
        &load.errors[0],
        "file_unreadable",
        "slides/s1.md",
        "",
        "slides/s1.md",
    );
    assert_eq!(ids(&load.slide_files), ["s1"]);
}

/// P62 non_regular_file_not_opened
#[test]
fn non_regular_file_not_opened() {
    let dir = draft(&[("guide.yaml", &valid_manifest())]);
    fs::create_dir(dir.path().join("slides")).unwrap();
    let status = Command::new("mkfifo")
        .arg(dir.path().join("slides/s1.md"))
        .status()
        .unwrap();
    assert!(status.success());
    let load = load_draft(dir.path());
    assert_eq!(load.errors.len(), 1, "{:?}", load.errors);
    assert_err(
        &load.errors[0],
        "unexpected_file",
        "slides/s1.md",
        "",
        "remove `slides/s1.md`",
    );
}

/// P63 parse_slide_rejects_bad_file_argument
#[test]
fn parse_slide_rejects_bad_file_argument() {
    let upper = slide_text("id: S1\nkind: step\ntitle: T\n", "x\n");
    assert_err(
        &parse_slide("slides/S1.md", &upper).unwrap_err(),
        "slide_id_invalid",
        "slides/S1.md",
        "id",
        "`S1`",
    );
    assert_err(
        &parse_slide("other/s1.md", &kind_slide("step", "x\n")).unwrap_err(),
        "slide_file_name_invalid",
        "other/s1.md",
        "",
        "slides/<id>.md",
    );
}

// ----------------------------------------------------------------- helpers

/// P64 block_field_builds_on_anchor_field
#[test]
fn block_field_builds_on_anchor_field() {
    let end = AnchorError::LinesOutOfRange {
        field: "end",
        message: String::new(),
    };
    assert_eq!(block_field(1, end.field()), "code[1].end");
    let side = AnchorError::WrongSide(String::new());
    assert_eq!(block_field(0, side.field()), "code[0].side");
}

/// P65 markdown_options_are_commonmark
#[test]
fn markdown_options_are_commonmark() {
    assert_eq!(markdown_options(), pulldown_cmark::Options::empty());
}
