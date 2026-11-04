//! Guide checking support: the checks `prguide check` runs on a draft
//! directory (T3-3). T5-2 adds the diagram rules through `check_diagrams`.

use std::{collections::HashMap, path::Path};

use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

use super::{
    GuideError,
    model::{
        DraftLoad, GuideManifest, MANIFEST_FILE, SelectionKind, Slide, SlideKind, block_field,
    },
    new::Provenance,
    parse::load_draft,
    range::{ResolvedSelection, Selection, Snapshot},
};
use crate::{
    Error,
    anchors::{BlockContext, CodeBlock, resolve},
    git::Git,
    render::{
        Charset, MmdfluxRenderer, RenderError, RenderOpts, Renderer, RootDirection, TextDiagram,
    },
    store::{StoreError, runs::RunId},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Diagnostic {
    pub severity: Severity,
    pub file: String,
    pub field: String,
    pub code: &'static str,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckReport {
    pub ok: bool,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftRow {
    pub id: String,
    pub run_id: String,
    pub path: String,
    pub provenance_json: String,
}

/// The string to look up in `drafts.path`, or `None` (-> `draft_unknown`).
/// The parent is canonicalised; the last name is not followed.
pub fn draft_path(draft: &Path, cwd: &Path) -> Option<String> {
    let joined = if draft.is_absolute() {
        draft.to_path_buf()
    } else {
        cwd.join(draft)
    };
    let mut parts: Vec<_> = joined.components().collect();
    let last = parts.pop()?;
    let std::path::Component::Normal(name) = last else {
        return None;
    };
    let parent: std::path::PathBuf = parts.into_iter().collect();
    let parent = std::fs::canonicalize(parent).ok()?;
    parent.join(name).to_str().map(str::to_owned)
}

/// The drafts row for `(path, run_id)`, read inside the caller's transaction.
pub fn draft_row(
    tx: &rusqlite::Transaction<'_>,
    run_id: &RunId,
    path: &str,
) -> Result<Option<DraftRow>, StoreError> {
    Ok(tx
        .query_row(
            "SELECT id, run_id, path, provenance_json FROM drafts WHERE path = ?1 AND run_id = ?2",
            [path, run_id.as_str()],
            |r| {
                Ok(DraftRow {
                    id: r.get(0)?,
                    run_id: r.get(1)?,
                    path: r.get(2)?,
                    provenance_json: r.get(3)?,
                })
            },
        )
        .optional()?)
}

/// `store_meta` owner, repo and git_common_dir.
pub fn store_binding(tx: &rusqlite::Transaction<'_>) -> Result<(String, String, String), Error> {
    let meta = |key: &str| -> Result<String, Error> {
        let value: Option<String> = tx
            .query_row("SELECT value FROM store_meta WHERE key = ?1", [key], |r| {
                r.get(0)
            })
            .optional()
            .map_err(StoreError::from)?;
        match value {
            Some(v) if !v.is_empty() => Ok(v),
            _ => Err(Error::Internal(format!("store_meta has no `{key}`"))),
        }
    };
    Ok((meta("owner")?, meta("repo")?, meta("git_common_dir")?))
}

#[derive(Deserialize)]
struct RowProvenance {
    format: String,
    pr: RowPr,
    snapshot: RowSnapshot,
    selection: RowSelection,
}

#[derive(Deserialize)]
struct RowPr {
    host: String,
    repo: String,
    number: u64,
}

#[derive(Deserialize)]
struct RowSnapshot {
    base: String,
    head: String,
}

#[derive(Deserialize)]
struct RowSelection {
    kind: SelectionKind,
    base: String,
    head: String,
    commits: Vec<String>,
}

fn diag(severity: Severity, error: GuideError) -> Diagnostic {
    Diagnostic {
        severity,
        file: error.file().unwrap_or("").to_owned(),
        field: error.field().unwrap_or("").to_owned(),
        code: error.code(),
        message: error.to_string(),
        preview: None,
    }
}

fn err(error: GuideError) -> Diagnostic {
    diag(Severity::Error, error)
}

fn warn(error: GuideError) -> Diagnostic {
    diag(Severity::Warning, error)
}

/// The first leaf (in a fixed order) where the manifest differs from the row.
fn first_differing_leaf(row: &RowProvenance, m: &GuideManifest) -> &'static str {
    let checks: [(&'static str, bool); 10] = [
        ("format", row.format != m.format),
        ("pr.host", row.pr.host != m.pr.host),
        ("pr.repo", row.pr.repo != m.pr.repo),
        ("pr.number", row.pr.number != m.pr.number),
        ("snapshot.base", row.snapshot.base != m.snapshot.base),
        ("snapshot.head", row.snapshot.head != m.snapshot.head),
        ("selection.kind", row.selection.kind != m.selection.kind),
        ("selection.base", row.selection.base != m.selection.base),
        ("selection.head", row.selection.head != m.selection.head),
        (
            "selection.commits",
            row.selection.commits != m.selection.commits,
        ),
    ];
    checks
        .iter()
        .find(|(_, differs)| *differs)
        .map_or("", |(name, _)| name)
}

type FieldKey = Vec<(Vec<u8>, Option<u64>)>;

fn field_key(field: &str) -> FieldKey {
    if field.is_empty() {
        return Vec::new();
    }
    field
        .split('.')
        .map(|seg| {
            if let Some(inner) = seg.strip_suffix(']')
                && let Some(open) = inner.rfind('[')
            {
                let digits = &inner[open + 1..];
                if !digits.is_empty()
                    && digits.bytes().all(|b| b.is_ascii_digit())
                    && let Ok(n) = digits.parse::<u64>()
                {
                    return (inner.as_bytes()[..open].to_vec(), Some(n));
                }
            }
            (seg.as_bytes().to_vec(), None)
        })
        .collect()
}

fn sort_diagnostics(list: &mut [Diagnostic]) {
    list.sort_by(|a, b| {
        a.file
            .as_bytes()
            .cmp(b.file.as_bytes())
            .then_with(|| field_key(&a.field).cmp(&field_key(&b.field)))
            .then_with(|| a.code.as_bytes().cmp(b.code.as_bytes()))
            .then_with(|| a.message.as_bytes().cmp(b.message.as_bytes()))
    });
}

fn kind_name(kind: SlideKind) -> &'static str {
    match kind {
        SlideKind::Overview => "overview",
        SlideKind::Architecture => "architecture",
        SlideKind::Step => "step",
    }
}

fn deck_rules(m: &GuideManifest, load: &DraftLoad, out: &mut Vec<Diagnostic>) {
    if m.slides.is_empty() {
        out.push(err(GuideError::SlideFileMissing {
            file: MANIFEST_FILE.into(),
            field: "slides".into(),
            message: "`slides` is empty: list the deck's slide ids in order, the overview first"
                .into(),
        }));
        return;
    }
    let mut first_at: HashMap<&str, usize> = HashMap::new();
    for (i, id) in m.slides.iter().enumerate() {
        if !load.slide_files.contains(id) {
            out.push(err(GuideError::SlideFileMissing {
                file: MANIFEST_FILE.into(),
                field: format!("slides[{i}]"),
                message: format!(
                    "slide `{}` is listed, but slides/{}.md does not exist",
                    id.as_str(),
                    id.as_str()
                ),
            }));
        }
        match first_at.get(id.as_str()) {
            Some(j) => out.push(err(GuideError::DuplicateSlideId {
                file: MANIFEST_FILE.into(),
                field: format!("slides[{i}]"),
                message: format!(
                    "slide `{}` is listed more than once (first at slides[{j}]); list each slide once",
                    id.as_str()
                ),
            })),
            None => {
                first_at.insert(id.as_str(), i);
            }
        }
    }
    let by_id: HashMap<&str, &Slide> = load
        .slides
        .iter()
        .map(|s| (s.front.id.as_str(), s))
        .collect();
    let listed: Option<Vec<&Slide>> = m
        .slides
        .iter()
        .map(|id| by_id.get(id.as_str()).copied())
        .collect();
    if let Some(listed) = listed {
        if let Some(first) = listed.first()
            && first.front.kind != SlideKind::Overview
        {
            out.push(err(GuideError::OverviewFirst {
                file: MANIFEST_FILE.into(),
                field: "slides[0]".into(),
                message: format!(
                    "the first slide must be the overview; `{}` has kind {}",
                    first.front.id.as_str(),
                    kind_name(first.front.kind)
                ),
            }));
        }
        if !listed.iter().any(|s| s.front.kind == SlideKind::Step) {
            out.push(err(GuideError::NoSteps {
                file: MANIFEST_FILE.into(),
                field: "slides".into(),
                message: "the deck has no step slide; add at least one slide with kind: step"
                    .into(),
            }));
        }
        if !listed
            .iter()
            .any(|s| s.front.kind == SlideKind::Architecture)
        {
            out.push(warn(GuideError::NoArchitecture {
                file: MANIFEST_FILE.into(),
                field: "slides".into(),
                message: "the deck has no architecture slide".into(),
            }));
        }
    }
    for id in &load.slide_files {
        if !m.slides.contains(id) {
            out.push(warn(GuideError::UnlistedSlide {
                file: id.file(),
                field: String::new(),
                message: format!(
                    "slides/{}.md is not listed in guide.yaml's `slides`; list it or delete it",
                    id.as_str()
                ),
            }));
        }
    }
}

/// The declared line range of a block, if it has one.
fn declared_range(block: &CodeBlock) -> Option<(u32, u32)> {
    match block {
        CodeBlock::Diff { start, end, .. } => start.zip(*end),
        CodeBlock::Context { start, end, .. } => Some((*start, *end)),
    }
}

fn flag_rules(slide: &Slide, out: &mut Vec<Diagnostic>) {
    let file = &slide.file;
    let blocks = &slide.front.code;
    let mut first_at: HashMap<&str, usize> = HashMap::new();
    for (i, b) in blocks.iter().enumerate() {
        match first_at.get(b.id()) {
            Some(j) => out.push(err(GuideError::DuplicateBlockId {
                file: file.clone(),
                field: block_field(i, "id"),
                message: format!(
                    "block id `{}` is used more than once in this slide (first at code[{j}]); \
                     give each block its own id",
                    b.id()
                ),
            })),
            None => {
                first_at.insert(b.id(), i);
            }
        }
    }
    for (k, flag) in slide.front.flags.iter().enumerate() {
        let Some(&bi) = first_at.get(flag.at.block.as_str()) else {
            out.push(err(GuideError::FlagBlockUnknown {
                file: file.clone(),
                field: format!("flags[{k}].at.block"),
                message: format!(
                    "the flag points at block `{}`, which this slide does not have",
                    flag.at.block
                ),
            }));
            continue;
        };
        let Some(line) = flag.at.line else { continue };
        let block = &blocks[bi];
        let message = match declared_range(block) {
            None => format!(
                "block `{}` has no line range, so a flag on it cannot name a line; remove `line`",
                flag.at.block
            ),
            Some((start, end)) if line < start || line > end => format!(
                "line {line} is outside block `{}`, which covers lines {start}-{end}",
                flag.at.block
            ),
            Some(_) => continue,
        };
        out.push(err(GuideError::FlagLineOutOfRange {
            file: file.clone(),
            field: format!("flags[{k}].at.line"),
            message,
        }));
    }
}

/// Reference viewport content width (an 80-column pane minus the frame).
pub const REFERENCE_WIDTH: usize = 76;
/// Reference viewport content height (a 40-row pane minus the frame).
pub const REFERENCE_HEIGHT: usize = 36;
/// Widest diagram the viewer pans before it tries LR→TD: min(2 × 76, 200).
pub const CAP_WIDTH: usize = 152;
/// Tallest diagram the viewer pans before it tries LR→TD: 3 × 36.
pub const CAP_HEIGHT: usize = 108;
/// The most rendering lines a preview carries.
pub const PREVIEW_LINES: usize = 40;

fn fits_cap(d: &TextDiagram) -> bool {
    d.width <= CAP_WIDTH && d.height <= CAP_HEIGHT
}

fn is_wide(d: &TextDiagram) -> bool {
    d.width > REFERENCE_WIDTH
}

fn preview_of(d: &TextDiagram) -> String {
    d.lines
        .iter()
        .take(PREVIEW_LINES)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("\n")
}

fn charset_name(cs: Charset) -> &'static str {
    match cs {
        Charset::Unicode => "unicode",
        Charset::Ascii => "ascii",
    }
}

fn body_diag(
    severity: Severity,
    file: &str,
    message: String,
    preview: Option<String>,
    build: fn(String, String, String) -> GuideError,
) -> Diagnostic {
    let mut d = diag(severity, build(file.to_owned(), "body".to_owned(), message));
    d.preview = preview;
    d
}

/// Runs the diagram rules on one architecture slide with the real renderer.
pub fn check_diagrams(slide: &Slide, out: &mut Vec<Diagnostic>) {
    check_diagrams_with(&MmdfluxRenderer, slide, out);
}

/// The diagram rules (design §2.8) with an injectable renderer.
pub fn check_diagrams_with(renderer: &dyn Renderer, slide: &Slide, out: &mut Vec<Diagnostic>) {
    let file = slide.file.as_str();
    let Some(block) = &slide.mermaid else {
        if slide.text_art.is_none() {
            out.push(body_diag(
                Severity::Error,
                file,
                "an architecture slide needs a diagram: add a ```mermaid block (flowchart or sequenceDiagram), or a ```text block with text art".to_owned(),
                None,
                |file, field, message| GuideError::NoDiagram { file, field, message },
            ));
        }
        return;
    };
    let line = block.line;
    let src = block.source.as_str();
    let render = |charset, root_direction| {
        renderer.render(
            src,
            &RenderOpts {
                charset,
                root_direction,
            },
        )
    };
    let flip = Some(RootDirection::TopDown);
    let u = render(Charset::Unicode, None);
    if let Err(RenderError::Unsupported(kind)) = &u {
        if slide.text_art.is_none() {
            out.push(body_diag(
                Severity::Error,
                file,
                format!("the `mermaid` block (line {line}) is a `{kind}` diagram, which prg cannot draw; add a ```text block with the same diagram as text art, or use flowchart or sequenceDiagram"),
                u.as_ref().err().map(ToString::to_string),
                |file, field, message| GuideError::UnsupportedNeedsText { file, field, message },
            ));
        }
        return;
    }
    let a = render(Charset::Ascii, None);
    let mut failed = false;
    for (cs, result) in [(Charset::Unicode, &u), (Charset::Ascii, &a)] {
        if let Err(e) = result {
            failed = true;
            out.push(body_diag(
                Severity::Error,
                file,
                format!(
                    "the `mermaid` block (line {line}) does not render in {} ({}): {e}",
                    charset_name(cs),
                    e.code()
                ),
                Some(e.to_string()),
                |file, field, message| GuideError::DiagramRender {
                    file,
                    field,
                    message,
                },
            ));
        }
    }
    let (Ok(u), false) = (u, failed) else {
        return;
    };
    let wide = |d: &TextDiagram, after: &str, out: &mut Vec<Diagnostic>| {
        out.push(body_diag(
            Severity::Warning,
            file,
            format!(
                "the `mermaid` block (line {line}) is {} columns wide{after}, wider than the {REFERENCE_WIDTH}-column reference viewport; the reviewer has to pan to see all of it",
                d.width
            ),
            Some(preview_of(d)),
            |file, field, message| GuideError::DiagramWide { file, field, message },
        ));
    };
    if fits_cap(&u) {
        if is_wide(&u) {
            wide(&u, "", out);
        }
        return;
    }
    let too_large = |clause: String, out: &mut Vec<Diagnostic>| {
        out.push(body_diag(
            Severity::Error,
            file,
            format!(
                "the `mermaid` block (line {line}) is {} columns by {} rows, over the {CAP_WIDTH} by {CAP_HEIGHT} cap{clause}; split it into smaller diagrams or simplify it",
                u.width, u.height
            ),
            Some(preview_of(&u)),
            |file, field, message| GuideError::DiagramTooLarge { file, field, message },
        ));
    };
    let fu = match render(Charset::Unicode, flip) {
        Err(e) => {
            too_large(
                format!(
                    ", and after LR→TD it does not render in unicode ({})",
                    e.code()
                ),
                out,
            );
            return;
        }
        Ok(fu) => fu,
    };
    if !fu.direction_overridden {
        too_large(String::new(), out);
        return;
    }
    if !fits_cap(&fu) {
        too_large(
            format!(
                ", and {} columns by {} rows after LR→TD",
                fu.width, fu.height
            ),
            out,
        );
        return;
    }
    match render(Charset::Ascii, flip) {
        Err(e) => too_large(
            format!(
                ", and after LR→TD it does not render in ascii ({})",
                e.code()
            ),
            out,
        ),
        Ok(_) => {
            if is_wide(&fu) {
                wide(&fu, " after LR→TD", out);
            }
        }
    }
}

/// Everything after the drafts row, on any directory.
pub fn check_draft(git: &Git, dir: &Path, provenance_json: &str) -> Result<CheckReport, Error> {
    let row: RowProvenance = serde_json::from_str(provenance_json).map_err(|e| {
        Error::Internal(format!(
            "the drafts row's provenance_json does not parse: {e}"
        ))
    })?;
    let selection = match row.selection.kind {
        SelectionKind::Full => Selection::Full,
        SelectionKind::Commits => {
            match (row.selection.commits.first(), row.selection.commits.last()) {
                (Some(first), Some(last)) => Selection::Commits {
                    first: first.clone(),
                    last: last.clone(),
                },
                _ => {
                    return Err(Error::Internal(
                        "the drafts row's provenance_json has selection kind commits but no commits"
                            .into(),
                    ));
                }
            }
        }
    };
    let load = load_draft(dir);
    let mut out: Vec<Diagnostic> = Vec::new();
    for e in &load.errors {
        out.push(Diagnostic {
            severity: Severity::Error,
            file: e.file().unwrap_or("").to_owned(),
            field: e.field().unwrap_or("").to_owned(),
            code: e.code(),
            message: e.to_string(),
            preview: None,
        });
    }
    if let Some(m) = &load.manifest {
        let rebuilt = serde_json::to_string(&Provenance::from_manifest(m))
            .map_err(|e| Error::Internal(e.to_string()))?;
        if rebuilt != provenance_json {
            let field = first_differing_leaf(&row, m);
            out.push(err(GuideError::ProvenanceChanged {
                file: MANIFEST_FILE.into(),
                field: field.into(),
                message: format!(
                    "guide.yaml's provenance differs from what prguide guide new wrote \
                     (first difference: `{field}`); restore it, and edit only `slides`"
                ),
            }));
        }
        deck_rules(m, &load, &mut out);
    }
    for slide in &load.slides {
        flag_rules(slide, &mut out);
    }
    if load.slides.iter().any(|s| !s.front.code.is_empty()) {
        let cx = BlockContext::new(
            git,
            0,
            Snapshot {
                base: row.snapshot.base.clone(),
                head: row.snapshot.head.clone(),
            },
            ResolvedSelection {
                selection,
                base: row.selection.base.clone(),
                head: row.selection.head.clone(),
                commits: row.selection.commits.clone(),
            },
        )?;
        for slide in &load.slides {
            for (i, block) in slide.front.code.iter().enumerate() {
                match resolve(git, block, &cx) {
                    Ok(r) => {
                        for w in r.warnings {
                            out.push(Diagnostic {
                                severity: Severity::Warning,
                                file: slide.file.clone(),
                                field: block_field(i, w.field),
                                code: w.code,
                                message: w.message,
                                preview: None,
                            });
                        }
                    }
                    Err(Error::Anchor(e)) => out.push(Diagnostic {
                        severity: Severity::Error,
                        file: slide.file.clone(),
                        field: block_field(i, e.field()),
                        code: e.code(),
                        message: e.to_string(),
                        preview: None,
                    }),
                    Err(other) => return Err(other),
                }
            }
        }
    }
    for slide in &load.slides {
        if slide.front.kind == SlideKind::Architecture {
            check_diagrams(slide, &mut out);
        }
    }
    sort_diagnostics(&mut out);
    let ok = !out.iter().any(|d| d.severity == Severity::Error);
    Ok(CheckReport {
        ok,
        diagnostics: out,
    })
}
