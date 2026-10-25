//! Parsing of `guide.yaml`, slide files and whole draft directories.
//!
//! Every problem comes back as a [`GuideError`] with a stable code, the file
//! and the field path. YAML is read with strict options (no anchors, aliases,
//! merge keys or custom tags, no duplicate keys, one document), and the field
//! path of a failure is tracked while the structs are read, so errors name
//! the key and not just a line.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

use pulldown_cmark::{CodeBlockKind, Event, Parser, Tag, TagEnd};
use serde::Deserialize;
use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde_saphyr::Error as YamlError;
use serde_saphyr::budget::BudgetBreach;

use super::GuideError;
use super::model::{
    DiagramBlock, DraftLoad, FORMAT, Flag, FlagAt, FrontMatter, GuideManifest, MANIFEST_FILE,
    ManifestPr, ManifestSelection, ManifestSnapshot, SLIDES_DIR, SelectionKind, Slide, SlideId,
    SlideKind, block_field, is_valid_id, markdown_options,
};
use crate::anchors::CodeBlock;

/// What `check_body` returns: the body with the diagram blocks cut out, then
/// the mermaid block and the text block.
pub type BodyParts = (String, Option<DiagramBlock>, Option<DiagramBlock>);

macro_rules! ge {
    ($variant:ident, $file:expr, $field:expr, $message:expr) => {
        GuideError::$variant {
            file: ($file).to_string(),
            field: ($field).to_string(),
            message: $message,
        }
    };
}

// ---------------------------------------------------------------- YAML reading

/// One step of the field path being read.
#[derive(Clone, Copy)]
enum Seg {
    Key(&'static str),
    Index(usize),
}

type Trail = RefCell<Vec<Seg>>;

fn strict_options() -> serde_saphyr::Options {
    serde_saphyr::options! {
        duplicate_keys: serde_saphyr::DuplicateKeyPolicy::Error,
        merge_keys: serde_saphyr::MergeKeyPolicy::Error,
        reject_unsupported_tags: true,
        with_snippet: false,
        budget: serde_saphyr::budget! { max_anchors: 0, max_aliases: 0, max_documents: 1 },
    }
}

/// Read the keys of a mapping. `read` gets each known key (pushed on the trail
/// while it runs; on an error the trail keeps it) and the map. An unknown key
/// fails with the trail at the map's own path.
fn walk<'de, A, F>(
    map: &mut A,
    trail: &Trail,
    fields: &'static [&'static str],
    mut read: F,
) -> Result<(), A::Error>
where
    A: MapAccess<'de>,
    F: FnMut(&'static str, &mut A) -> Result<(), A::Error>,
{
    while let Some(key) = map.next_key::<String>()? {
        let Some(name) = fields.iter().copied().find(|field| *field == key) else {
            return Err(de::Error::unknown_field(&key, fields));
        };
        trail.borrow_mut().push(Seg::Key(name));
        read(name, map)?;
        trail.borrow_mut().pop();
    }
    Ok(())
}

macro_rules! need {
    ($value:expr, $name:literal) => {
        match $value {
            Some(value) => value,
            None => return Err(de::Error::missing_field($name)),
        }
    };
}

macro_rules! map_seed {
    ($seed:ident, $out:ty, |$trail:ident, $map:ident| $body:block) => {
        struct $seed<'t>(&'t Trail);

        impl<'de> DeserializeSeed<'de> for $seed<'_> {
            type Value = $out;

            fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<$out, D::Error> {
                struct V<'t>(&'t Trail);

                impl<'de> Visitor<'de> for V<'_> {
                    type Value = $out;

                    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                        f.write_str("a mapping")
                    }

                    fn visit_map<A: MapAccess<'de>>(self, mut $map: A) -> Result<$out, A::Error> {
                        let $trail = self.0;
                        $body
                    }
                }

                d.deserialize_map(V(self.0))
            }
        }
    };
}

/// A sequence whose elements are read by the seed `make` returns; the trail
/// holds the element's index while it is read.
struct SeqSeed<'t, F> {
    trail: &'t Trail,
    make: F,
}

impl<'de, F, S> DeserializeSeed<'de> for SeqSeed<'_, F>
where
    F: Fn() -> S,
    S: DeserializeSeed<'de>,
{
    type Value = Vec<S::Value>;

    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        struct V<'t, F> {
            trail: &'t Trail,
            make: F,
        }

        impl<'de, F, S> Visitor<'de> for V<'_, F>
        where
            F: Fn() -> S,
            S: DeserializeSeed<'de>,
        {
            type Value = Vec<S::Value>;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a sequence")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut out = Vec::new();
                loop {
                    self.trail.borrow_mut().push(Seg::Index(out.len()));
                    match seq.next_element_seed((self.make)())? {
                        Some(value) => out.push(value),
                        None => break,
                    }
                    self.trail.borrow_mut().pop();
                }
                self.trail.borrow_mut().pop();
                Ok(out)
            }
        }

        d.deserialize_seq(V {
            trail: self.trail,
            make: self.make,
        })
    }
}

struct RawManifest {
    format: String,
    pr: ManifestPr,
    snapshot: ManifestSnapshot,
    selection: ManifestSelection,
    slides: Vec<String>,
}

struct RawFront {
    id: String,
    kind: SlideKind,
    title: String,
    code: Vec<CodeBlock>,
    flags: Vec<Flag>,
}

map_seed!(PrSeed, ManifestPr, |trail, map| {
    let (mut host, mut repo, mut number) = (None, None, None);
    walk(&mut map, trail, &["host", "repo", "number"], |key, m| {
        match key {
            "host" => host = Some(m.next_value::<String>()?),
            "repo" => repo = Some(m.next_value::<String>()?),
            "number" => number = Some(m.next_value::<u64>()?),
            _ => {}
        }
        Ok(())
    })?;
    Ok(ManifestPr {
        host: need!(host, "host"),
        repo: need!(repo, "repo"),
        number: need!(number, "number"),
    })
});

map_seed!(SnapshotSeed, ManifestSnapshot, |trail, map| {
    let (mut base, mut head) = (None, None);
    walk(&mut map, trail, &["base", "head"], |key, m| {
        match key {
            "base" => base = Some(m.next_value::<String>()?),
            "head" => head = Some(m.next_value::<String>()?),
            _ => {}
        }
        Ok(())
    })?;
    Ok(ManifestSnapshot {
        base: need!(base, "base"),
        head: need!(head, "head"),
    })
});

map_seed!(SelectionSeed, ManifestSelection, |trail, map| {
    let (mut kind, mut base, mut head, mut commits) = (None, None, None, None);
    walk(
        &mut map,
        trail,
        &["kind", "base", "head", "commits"],
        |key, m| {
            match key {
                "kind" => kind = Some(m.next_value::<SelectionKind>()?),
                "base" => base = Some(m.next_value::<String>()?),
                "head" => head = Some(m.next_value::<String>()?),
                "commits" => {
                    commits = Some(m.next_value_seed(SeqSeed {
                        trail,
                        make: PhantomData::<String>::default,
                    })?)
                }
                _ => {}
            }
            Ok(())
        },
    )?;
    Ok(ManifestSelection {
        kind: need!(kind, "kind"),
        base: need!(base, "base"),
        head: need!(head, "head"),
        commits: need!(commits, "commits"),
    })
});

map_seed!(ManifestSeed, RawManifest, |trail, map| {
    let (mut format, mut pr, mut snapshot, mut selection, mut slides) =
        (None, None, None, None, None);
    walk(
        &mut map,
        trail,
        &["format", "pr", "snapshot", "selection", "slides"],
        |key, m| {
            match key {
                "format" => format = Some(m.next_value::<String>()?),
                "pr" => pr = Some(m.next_value_seed(PrSeed(trail))?),
                "snapshot" => snapshot = Some(m.next_value_seed(SnapshotSeed(trail))?),
                "selection" => selection = Some(m.next_value_seed(SelectionSeed(trail))?),
                "slides" => {
                    slides = Some(m.next_value_seed(SeqSeed {
                        trail,
                        make: PhantomData::<String>::default,
                    })?)
                }
                _ => {}
            }
            Ok(())
        },
    )?;
    Ok(RawManifest {
        format: need!(format, "format"),
        pr: need!(pr, "pr"),
        snapshot: need!(snapshot, "snapshot"),
        selection: need!(selection, "selection"),
        slides: need!(slides, "slides"),
    })
});

map_seed!(FlagAtSeed, FlagAt, |trail, map| {
    let (mut block, mut line) = (None, None);
    walk(&mut map, trail, &["block", "line"], |key, m| {
        match key {
            "block" => block = Some(m.next_value::<String>()?),
            "line" => line = m.next_value::<Option<u32>>()?,
            _ => {}
        }
        Ok(())
    })?;
    Ok(FlagAt {
        block: need!(block, "block"),
        line,
    })
});

map_seed!(FlagSeed, Flag, |trail, map| {
    let (mut at, mut text) = (None, None);
    walk(&mut map, trail, &["at", "text"], |key, m| {
        match key {
            "at" => at = Some(m.next_value_seed(FlagAtSeed(trail))?),
            "text" => text = Some(m.next_value::<String>()?),
            _ => {}
        }
        Ok(())
    })?;
    Ok(Flag {
        at: need!(at, "at"),
        text: need!(text, "text"),
    })
});

map_seed!(FrontSeed, RawFront, |trail, map| {
    let (mut id, mut kind, mut title, mut code, mut flags) = (None, None, None, None, None);
    walk(
        &mut map,
        trail,
        &["id", "kind", "title", "code", "flags"],
        |key, m| {
            match key {
                "id" => id = Some(m.next_value::<String>()?),
                "kind" => kind = Some(m.next_value::<SlideKind>()?),
                "title" => title = Some(m.next_value::<String>()?),
                "code" => {
                    code = Some(m.next_value_seed(SeqSeed {
                        trail,
                        make: PhantomData::<CodeBlock>::default,
                    })?)
                }
                "flags" => {
                    flags = Some(m.next_value_seed(SeqSeed {
                        trail,
                        make: || FlagSeed(trail),
                    })?)
                }
                _ => {}
            }
            Ok(())
        },
    )?;
    Ok(RawFront {
        id: need!(id, "id"),
        kind: need!(kind, "kind"),
        title: need!(title, "title"),
        code: code.unwrap_or_default(),
        flags: flags.unwrap_or_default(),
    })
});

fn read_manifest(text: &str) -> Result<RawManifest, Box<(Vec<Seg>, YamlError)>> {
    let trail = RefCell::new(Vec::new());
    serde_saphyr::with_deserializer_from_str_with_options(text, strict_options(), |de| {
        ManifestSeed(&trail).deserialize(de)
    })
    .map_err(|error| Box::new((trail.into_inner(), error)))
}

fn read_front(text: &str) -> Result<RawFront, Box<(Vec<Seg>, YamlError)>> {
    let trail = RefCell::new(Vec::new());
    serde_saphyr::with_deserializer_from_str_with_options(text, strict_options(), |de| {
        FrontSeed(&trail).deserialize(de)
    })
    .map_err(|error| Box::new((trail.into_inner(), error)))
}

// ---------------------------------------------------------------- YAML errors

fn path_string(trail: &[Seg]) -> String {
    let mut out = String::new();
    for seg in trail {
        match seg {
            Seg::Key(key) => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(key);
            }
            Seg::Index(index) => out.push_str(&format!("[{index}]")),
        }
    }
    out
}

fn join_path(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_string()
    } else {
        format!("{path}.{key}")
    }
}

fn last_key(trail: &[Seg]) -> &str {
    trail
        .iter()
        .rev()
        .find_map(|seg| match seg {
            Seg::Key(key) => Some(*key),
            Seg::Index(_) => None,
        })
        .unwrap_or("")
}

/// The detail of a wrong-type, null, bad-value or bad-variant error, without
/// its location. `None` for every other error.
fn invalid_detail(error: &YamlError) -> Option<String> {
    Some(match error {
        YamlError::SerdeInvalidType {
            unexpected,
            expected,
            ..
        } => format!("invalid type: {unexpected}, expected {expected}"),
        YamlError::SerdeInvalidValue {
            unexpected,
            expected,
            ..
        } => format!("invalid value: {unexpected}, expected {expected}"),
        YamlError::SerdeInvalidLength { len, expected, .. } => {
            format!("invalid length {len}, expected {expected}")
        }
        YamlError::SerdeUnknownVariant {
            variant, expected, ..
        } => {
            let list: Vec<String> = expected.iter().map(|name| format!("`{name}`")).collect();
            format!(
                "unknown variant `{variant}`, expected one of {}",
                list.join(", ")
            )
        }
        YamlError::NullIntoString { .. } => "null is not allowed".to_string(),
        YamlError::Unexpected { expected, .. } => format!("expected {expected}"),
        YamlError::InvalidScalar { ty, .. } => format!("invalid {ty}"),
        YamlError::InvalidBooleanStrict { .. } => "invalid boolean".to_string(),
        YamlError::InvalidBinaryBase64 { .. } => "invalid base64 in a !!binary value".to_string(),
        YamlError::BinaryNotUtf8 { .. } => "a !!binary value is not UTF-8".to_string(),
        YamlError::TaggedScalarCannotDeserializeIntoString { .. } => {
            "a tagged value cannot be read as a string".to_string()
        }
        YamlError::NonFiniteFloat { value, .. } => format!("non-finite number `{value}`"),
        YamlError::TaggedEnumMismatch { tagged, target, .. } => {
            format!("tag `{tagged}` does not match `{target}`")
        }
        _ => return None,
    })
}

/// What a yaml_invalid message says. Only `Eof` says "empty".
fn yaml_phrase(error: &YamlError) -> &'static str {
    match error {
        YamlError::Eof { .. } => "empty YAML: there is no document",
        YamlError::MultipleDocuments { .. }
        | YamlError::Budget {
            breach: BudgetBreach::Documents { .. },
            ..
        } => "invalid YAML: more than one document",
        YamlError::IndentationError { .. } => "invalid YAML: bad indentation",
        YamlError::Budget { .. } => "invalid YAML: too large or too deeply nested",
        _ if invalid_detail(error).is_some() => {
            "invalid YAML: the top level must be a mapping with plain string keys"
        }
        _ => "invalid YAML: it does not parse",
    }
}

/// Which key of the `code[index]` block fails its type, tried in the order
/// kind, id, path, side, at, start, end. The block is deserialised as one
/// unit, so serde-saphyr can't say; read the same text again as plain values.
fn failing_block_key(text: &str, index: usize) -> Option<&'static str> {
    #[derive(Deserialize)]
    struct Probe {
        #[serde(default)]
        code: Option<Vec<BTreeMap<String, serde_json::Value>>>,
    }

    use serde_json::Value;
    let probe: Probe = serde_saphyr::from_str_with_options(text, strict_options()).ok()?;
    let block = probe.code?.into_iter().nth(index)?;
    let one_of = |key: &str, set: &[&str]| matches!(block.get(key), Some(Value::String(s)) if set.contains(&s.as_str()));
    let string = |key: &str| matches!(block.get(key), Some(Value::String(_)));
    // A diff block's `start` and `end` are optional, so `~` is valid there.
    let is_diff = matches!(block.get("kind"), Some(Value::String(s)) if s == "diff");
    let number = |key: &str| {
        matches!(block.get(key), Some(Value::Null) if is_diff)
            || matches!(block.get(key), Some(Value::Number(n))
                if n.as_u64().is_some_and(|n| n <= u64::from(u32::MAX)))
    };
    let checks: [(&'static str, bool); 7] = [
        ("kind", one_of("kind", &["diff", "context"])),
        ("id", string("id")),
        ("path", string("path")),
        ("side", one_of("side", &["old", "new"])),
        (
            "at",
            one_of("at", &["snapshot-head", "selection-head", "selection-base"]),
        ),
        ("start", number("start")),
        ("end", number("end")),
    ];
    checks
        .into_iter()
        .find(|(key, ok)| block.contains_key(*key) && !ok)
        .map(|(key, _)| key)
}

/// Turn a serde-saphyr failure into the guide error with the right code. The
/// variant decides the code, never the message text. `line_offset` turns a
/// YAML line into a file line.
fn yaml_error(
    file: &str,
    line_offset: u64,
    text: &str,
    trail: &[Seg],
    error: &YamlError,
) -> GuideError {
    let mut error = error;
    while let YamlError::WithSnippet { error: inner, .. } = error {
        error = inner;
    }
    let at = error
        .location()
        .map(|location| format!(" (line {})", location.line() + line_offset))
        .unwrap_or_default();
    let path = path_string(trail);
    let feature = |what: &str| {
        ge!(
            YamlFeatureNotAllowed,
            file,
            "",
            format!("YAML {what} is not allowed{at}")
        )
    };
    match error {
        YamlError::Budget {
            breach: BudgetBreach::Anchors { .. },
            ..
        } => feature("anchor"),
        YamlError::Budget {
            breach: BudgetBreach::Aliases { .. } | BudgetBreach::AliasAnchorRatio { .. },
            ..
        }
        | YamlError::UnknownAnchor { .. }
        | YamlError::AliasError { .. }
        | YamlError::AliasReplayCounterOverflow { .. }
        | YamlError::AliasReplayLimitExceeded { .. }
        | YamlError::AliasReplayStackDepthExceeded { .. }
        | YamlError::AliasExpansionLimitExceeded { .. } => feature("alias"),
        YamlError::MergeKeyNotAllowed { .. }
        | YamlError::MergeValueNotMapOrSeqOfMaps { .. }
        | YamlError::Budget {
            breach: BudgetBreach::MergeKeys { .. },
            ..
        } => feature("merge key (`<<`)"),
        YamlError::UnsupportedTag { tag, .. } => feature(&format!("tag `{tag}`")),
        YamlError::SerdeUnknownField { field, .. } => ge!(
            FieldUnknown,
            file,
            join_path(&path, field),
            format!("unknown field `{field}`{at}")
        ),
        YamlError::SerdeMissingField { field, .. } => ge!(
            FieldMissing,
            file,
            join_path(&path, field),
            format!("missing field `{field}`{at}")
        ),
        YamlError::DuplicateMappingKey { key: Some(key), .. } => ge!(
            KeyDuplicate,
            file,
            join_path(&path, key),
            format!("duplicate key `{key}`{at}")
        ),
        YamlError::DuplicateMappingKey { key: None, .. } => {
            ge!(KeyDuplicate, file, path, format!("duplicate key{at}"))
        }
        _ => match invalid_detail(error) {
            Some(detail) if !trail.is_empty() => {
                // A syntax error, budget breach or forbidden feature that this
                // path-less pass meets before any other value error takes
                // over. Any other error from it (a binary decoding error, a
                // duplicate key) would lose the field path, so it is ignored
                // and the original error is reported. A syntax error that
                // sits behind such a value error shows up once that is fixed.
                if let Err(whole) = serde_saphyr::from_str_with_options::<serde::de::IgnoredAny>(
                    text,
                    strict_options(),
                ) && invalid_detail(&whole).is_none()
                {
                    let reported = yaml_error(file, line_offset, text, &[], &whole);
                    if matches!(
                        reported,
                        GuideError::YamlInvalid { .. } | GuideError::YamlFeatureNotAllowed { .. }
                    ) {
                        return reported;
                    }
                }
                let (mut field, mut key) = (path, last_key(trail));
                if let [Seg::Key("code"), Seg::Index(index)] = trail
                    && let Some(block_key) = failing_block_key(text, *index)
                {
                    field = block_field(*index, block_key);
                    key = block_key;
                }
                ge!(
                    FieldInvalid,
                    file,
                    field,
                    format!("invalid value for `{key}`: {detail}{at}")
                )
            }
            _ => ge!(YamlInvalid, file, "", format!("{}{at}", yaml_phrase(error))),
        },
    }
}

// ------------------------------------------------------------------ manifest

fn bad_slide_id(value: &str) -> String {
    format!("invalid slide id `{value}`: an id matches [a-z0-9][a-z0-9-]{{0,63}}")
}

pub fn parse_manifest(text: &str) -> Result<GuideManifest, GuideError> {
    let raw = read_manifest(text)
        .map_err(|failure| yaml_error(MANIFEST_FILE, 0, text, &failure.0, &failure.1))?;
    if raw.format != FORMAT {
        return Err(ge!(
            FormatUnsupported,
            MANIFEST_FILE,
            "format",
            format!(
                "unsupported format `{}`: this prguide reads `{FORMAT}`",
                raw.format
            )
        ));
    }
    let mut slides = Vec::new();
    for (index, id) in raw.slides.iter().enumerate() {
        let Some(id) = SlideId::parse(id) else {
            return Err(ge!(
                SlideIdInvalid,
                MANIFEST_FILE,
                format!("slides[{index}]"),
                bad_slide_id(id)
            ));
        };
        slides.push(id);
    }
    Ok(GuideManifest {
        format: raw.format,
        pr: raw.pr,
        snapshot: raw.snapshot,
        selection: raw.selection,
        slides,
    })
}

// -------------------------------------------------------------------- slides

fn is_delimiter(line: &str) -> bool {
    line.strip_prefix("---")
        .is_some_and(|rest| rest.chars().all(|c| c == ' ' || c == '\t'))
}

struct Split<'a> {
    yaml: &'a str,
    body: &'a str,
    body_line: u32,
}

/// Cut a normalised slide file into its YAML and its body (B.4 steps 2-4).
fn split_front_matter<'a>(file: &str, text: &'a str) -> Result<Split<'a>, GuideError> {
    let mut lines = text.split_inclusive('\n');
    let first = lines.next().unwrap_or("");
    if !is_delimiter(first.trim_end_matches('\n')) {
        return Err(ge!(
            FrontMatterMissing,
            file,
            "",
            "a slide must start with a `---` line, then the YAML front matter".to_string()
        ));
    }
    let yaml_start = first.len();
    let mut pos = yaml_start;
    let mut line_no: u32 = 1;
    for line in lines {
        line_no = line_no.saturating_add(1);
        if is_delimiter(line.trim_end_matches('\n')) {
            return Ok(Split {
                yaml: &text[yaml_start..pos],
                body: &text[pos + line.len()..],
                body_line: line_no.saturating_add(1),
            });
        }
        pos += line.len();
    }
    Err(ge!(
        FrontMatterUnclosed,
        file,
        "",
        "the front matter has no closing `---` line".to_string()
    ))
}

/// `Err(reason)` when `path` can't be a code-block path. Lexical only: it
/// never touches the filesystem or git.
pub fn check_code_path(path: &str) -> Result<(), String> {
    let err = |reason: &str| Err(reason.to_string());
    if path.is_empty() {
        return err("empty path");
    }
    if path.chars().any(|c| c <= '\u{1f}' || c == '\u{7f}') {
        return err("control character");
    }
    if path.contains('\\') {
        return err("backslash");
    }
    if path.starts_with('/') {
        return err("absolute");
    }
    let bytes = path.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return err("drive prefix");
    }
    if path.split('/').any(str::is_empty) {
        return err("empty segment");
    }
    if path.split('/').any(|segment| segment == ".") {
        return err("`.` segment");
    }
    if path.split('/').any(|segment| segment == "..") {
        return err("`..` segment");
    }
    Ok(())
}

/// Parse `text`, the content of the slide file `file` (`slides/<id>.md`).
pub fn parse_slide(file: &str, text: &str) -> Result<Slide, GuideError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let text = text.replace("\r\n", "\n");
    let split = split_front_matter(file, &text)?;
    // The YAML starts on file line 2.
    let raw = read_front(split.yaml)
        .map_err(|failure| yaml_error(file, 1, split.yaml, &failure.0, &failure.1))?;

    let Some(id) = SlideId::parse(&raw.id) else {
        return Err(ge!(SlideIdInvalid, file, "id", bad_slide_id(&raw.id)));
    };
    let stem = file
        .strip_prefix("slides/")
        .and_then(|rest| rest.strip_suffix(".md"))
        .filter(|stem| is_valid_id(stem));
    let Some(stem) = stem else {
        return Err(ge!(
            SlideFileNameInvalid,
            file,
            "",
            format!("`{file}` is not a slide file: slides are named slides/<id>.md")
        ));
    };
    if stem != id.as_str() {
        return Err(ge!(
            SlideIdMismatch,
            file,
            "id",
            format!(
                "the id `{}` does not match the file `{file}`: use id `{stem}`",
                id.as_str()
            )
        ));
    }
    for (index, block) in raw.code.iter().enumerate() {
        if !is_valid_id(block.id()) {
            return Err(ge!(
                BlockIdInvalid,
                file,
                block_field(index, "id"),
                bad_slide_id(block.id()).replace("slide id", "block id")
            ));
        }
        if let Err(reason) = check_code_path(block.path()) {
            return Err(ge!(
                PathInvalid,
                file,
                block_field(index, "path"),
                format!("invalid code path `{}`: {reason}", block.path())
            ));
        }
    }
    let (body, mermaid, text_art) = check_body(file, raw.kind, split.body, split.body_line)?;
    Ok(Slide {
        file: file.to_string(),
        front: FrontMatter {
            id,
            kind: raw.kind,
            title: raw.title,
            code: raw.code,
            flags: raw.flags,
        },
        body,
        body_line: split.body_line,
        mermaid,
        text_art,
    })
}

fn html_error(file: &str, line: u32) -> GuideError {
    ge!(
        HtmlNotAllowed,
        file,
        "body",
        format!("raw HTML is not allowed in a slide body (line {line}); use Markdown")
    )
}

/// Check a slide body (no raw HTML) and, on an architecture slide, pull its
/// diagram blocks out. `body_line` is the file line the body starts on.
pub fn check_body(
    file: &str,
    kind: SlideKind,
    body: &str,
    body_line: u32,
) -> Result<BodyParts, GuideError> {
    let events: Vec<_> = Parser::new_ext(body, markdown_options())
        .into_offset_iter()
        .collect();
    let line_of = |offset: usize| {
        let newlines = body[..offset].matches('\n').count();
        body_line.saturating_add(u32::try_from(newlines).unwrap_or(u32::MAX))
    };

    for (event, range) in &events {
        if matches!(event, Event::Html(_) | Event::InlineHtml(_)) {
            return Err(html_error(file, line_of(range.start)));
        }
    }
    if kind != SlideKind::Architecture {
        return Ok((body.to_string(), None, None));
    }

    let mut depth = 0usize;
    let mut mermaid = None;
    let mut text_art = None;
    let mut cuts = Vec::new();
    let mut i = 0;
    while i < events.len() {
        let (event, range) = &events[i];
        match event {
            Event::Start(Tag::Item | Tag::BlockQuote(_)) => depth += 1,
            Event::End(TagEnd::Item | TagEnd::BlockQuote(_)) => depth = depth.saturating_sub(1),
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info))) => {
                let word = info.split_whitespace().next().unwrap_or("");
                if word == "mermaid" || word == "text" {
                    let line = line_of(range.start);
                    if depth > 0 {
                        return Err(ge!(
                            DiagramBlockNested,
                            file,
                            "body",
                            format!(
                                "the `{word}` block must be at the top level, not inside a list or quote (line {line})"
                            )
                        ));
                    }
                    let slot = if word == "mermaid" {
                        &mut mermaid
                    } else {
                        &mut text_art
                    };
                    if slot.is_some() {
                        return Err(ge!(
                            DuplicateDiagramBlock,
                            file,
                            "body",
                            format!(
                                "more than one `{word}` block (line {line}); an architecture slide has at most one"
                            )
                        ));
                    }
                    let mut source = String::new();
                    let mut j = i + 1;
                    while j < events.len() {
                        match &events[j].0 {
                            Event::Text(text) => source.push_str(text),
                            Event::End(TagEnd::CodeBlock) => break,
                            _ => {}
                        }
                        j += 1;
                    }
                    *slot = Some(DiagramBlock { source, line });
                    cuts.push(range.clone());
                    i = j;
                }
            }
            _ => {}
        }
        i += 1;
    }

    let mut out = body.to_string();
    for range in cuts.iter().rev() {
        out.replace_range(range.clone(), "");
    }
    // Cutting a block can join what was around it into raw HTML (an indented
    // code block after a list item turns into an HTML block, say). The body
    // the model returns must be free of HTML too, so run the rule again.
    if !cuts.is_empty() {
        for (event, range) in Parser::new_ext(&out, markdown_options()).into_offset_iter() {
            if matches!(event, Event::Html(_) | Event::InlineHtml(_)) {
                // Map the offset in the cut body back to the original body by
                // adding back every cut that lies before it.
                let mut original = range.start;
                let mut removed = 0;
                for cut in &cuts {
                    if range.start >= cut.start - removed {
                        removed += cut.end - cut.start;
                        original = range.start + removed;
                    }
                }
                return Err(html_error(file, line_of(original)));
            }
        }
    }
    Ok((out, mermaid, text_art))
}

// --------------------------------------------------------------------- draft

const UNEXPECTED: &str = "only guide.yaml and slides/<id>.md are allowed in a draft";

fn unexpected_file(file: &str) -> GuideError {
    ge!(
        UnexpectedFile,
        file,
        "",
        format!("{UNEXPECTED}; remove `{file}`")
    )
}

fn symlink_error(file: &str) -> GuideError {
    ge!(
        SymlinkNotAllowed,
        file,
        "",
        format!("`{file}` is a symlink; symlinks are not allowed in a draft, use a real file")
    )
}

fn unreadable(file: &str, error: &std::io::Error) -> GuideError {
    ge!(
        FileUnreadable,
        file,
        "",
        format!("cannot read `{file}`: {error}")
    )
}

fn read_utf8(path: &Path, file: &str) -> Result<String, GuideError> {
    let bytes = fs::read(path).map_err(|error| unreadable(file, &error))?;
    String::from_utf8(bytes).map_err(|_| {
        ge!(
            FileNotUtf8,
            file,
            "",
            format!("`{file}` is not valid UTF-8")
        )
    })
}

/// Read a draft directory: `guide.yaml` and `slides/<id>.md`, nothing else.
/// Symlinks are never followed and non-regular files are never opened; the
/// ancestors of `dir` are not checked. At most one error per file, sorted by
/// file.
pub fn load_draft(dir: &Path) -> DraftLoad {
    // `lstat("link/")` follows the link, so drop a trailing `/` or `/.` first.
    let dir: PathBuf = dir.components().collect();
    let dir = dir.as_path();
    let mut load = DraftLoad {
        manifest: None,
        slide_files: Vec::new(),
        slides: Vec::new(),
        errors: Vec::new(),
    };
    match fs::symlink_metadata(dir) {
        Err(error) => {
            load.errors.push(ge!(
                DraftInvalid,
                "",
                "",
                format!(
                    "cannot read the draft directory `{}`: {error}",
                    dir.display()
                )
            ));
            return load;
        }
        Ok(meta) if meta.file_type().is_symlink() => {
            load.errors.push(ge!(
                SymlinkNotAllowed,
                "",
                "",
                format!("the draft directory `{}` is a symlink", dir.display())
            ));
            return load;
        }
        Ok(meta) if !meta.is_dir() => {
            load.errors.push(ge!(
                DraftInvalid,
                "",
                "",
                format!("the draft `{}` is not a directory", dir.display())
            ));
            return load;
        }
        Ok(_) => {}
    }

    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            load.errors.push(unreadable("", &error));
            return load;
        }
    };
    let mut has_manifest_entry = false;
    let mut manifest_path = None;
    let mut slides_path = None;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                load.errors.push(unreadable("", &error));
                break;
            }
        };
        let name = entry.file_name();
        let shown = name.to_string_lossy().into_owned();
        let is_manifest = name.to_str() == Some(MANIFEST_FILE);
        has_manifest_entry |= is_manifest;
        let meta = match fs::symlink_metadata(entry.path()) {
            Ok(meta) => meta,
            Err(error) => {
                load.errors.push(unreadable(&shown, &error));
                continue;
            }
        };
        if meta.file_type().is_symlink() {
            load.errors.push(symlink_error(&shown));
        } else if is_manifest && meta.is_file() {
            manifest_path = Some(entry.path());
        } else if name.to_str() == Some(SLIDES_DIR) && meta.is_dir() {
            slides_path = Some(entry.path());
        } else {
            load.errors.push(unexpected_file(&shown));
        }
    }

    match manifest_path {
        Some(path) => {
            match read_utf8(&path, MANIFEST_FILE).and_then(|text| parse_manifest(&text)) {
                Ok(manifest) => load.manifest = Some(manifest),
                Err(error) => load.errors.push(error),
            }
        }
        None if !has_manifest_entry => load.errors.push(ge!(
            ManifestMissing,
            MANIFEST_FILE,
            "",
            "the draft has no guide.yaml".to_string()
        )),
        None => {}
    }

    if let Some(slides_path) = slides_path {
        load_slides(&slides_path, &mut load);
    }
    load.slide_files.sort();
    load.slides.sort_by(|a, b| a.front.id.cmp(&b.front.id));
    load.errors
        .sort_by(|a, b| a.file().unwrap_or("").cmp(b.file().unwrap_or("")));
    load
}

fn load_slides(slides_path: &Path, load: &mut DraftLoad) {
    let entries = match fs::read_dir(slides_path) {
        Ok(entries) => entries,
        Err(error) => {
            load.errors.push(unreadable(SLIDES_DIR, &error));
            return;
        }
    };
    let mut found: Vec<(SlideId, PathBuf)> = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                load.errors.push(unreadable(SLIDES_DIR, &error));
                break;
            }
        };
        let name = entry.file_name();
        let file = format!("{SLIDES_DIR}/{}", name.to_string_lossy());
        let meta = match fs::symlink_metadata(entry.path()) {
            Ok(meta) => meta,
            Err(error) => {
                load.errors.push(unreadable(&file, &error));
                continue;
            }
        };
        if meta.file_type().is_symlink() {
            load.errors.push(symlink_error(&file));
        } else if !meta.is_file() {
            load.errors.push(unexpected_file(&file));
        } else if let Some(id) = name
            .to_str()
            .and_then(|name| name.strip_suffix(".md"))
            .and_then(SlideId::parse)
        {
            found.push((id, entry.path()));
        } else {
            load.errors.push(ge!(
                SlideFileNameInvalid,
                file,
                "",
                format!("`{file}` is not a slide file: slides are named slides/<id>.md")
            ));
        }
    }
    for (id, path) in found {
        let file = id.file();
        load.slide_files.push(id);
        match read_utf8(&path, &file).and_then(|text| parse_slide(&file, &text)) {
            Ok(slide) => load.slides.push(slide),
            Err(error) => load.errors.push(error),
        }
    }
}
