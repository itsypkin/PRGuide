//! Typed parsing for the fixed, deterministic `git diff` output.

use std::fmt;

use super::GitError;

/// A repository path as git printed it, unquoted, without the a/ or b/ prefix.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GitPath(Vec<u8>);

impl GitPath {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn to_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.0).ok()
    }
}

impl fmt::Display for GitPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", String::from_utf8_lossy(&self.0))
    }
}

impl fmt::Debug for GitPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "\"{}\"", self.0.escape_ascii())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    pub path: GitPath,
    pub old_path: Option<GitPath>,
    pub status: FileStatus,
    pub old_mode: Option<u32>,
    pub new_mode: Option<u32>,
    pub old_oid: Option<String>,
    pub new_oid: Option<String>,
    pub similarity: Option<u8>,
    pub binary: bool,
    pub mode_only: bool,
    pub submodule: bool,
    pub hunks: Vec<Hunk>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    pub section: Option<Vec<u8>>,
    pub lines: Vec<Line>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub kind: LineKind,
    pub text: Vec<u8>,
    pub no_newline: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Added,
    Removed,
}

/// Parse the output of `Git::diff` for the fixed DiffSpec argv.
pub fn parse(output: &[u8]) -> Result<Vec<FileDiff>, GitError> {
    if output.is_empty() {
        return Ok(Vec::new());
    }
    if !output.ends_with(b"\n") {
        return Err(parse_error(
            output.iter().filter(|byte| **byte == b'\n').count() + 1,
            "diff output is missing its final newline",
        ));
    }
    let lines: Vec<&[u8]> = output[..output.len() - 1]
        .split(|byte| *byte == b'\n')
        .collect();
    let mut parser = Parser { lines, index: 0 };
    let mut files = Vec::new();
    while parser.index < parser.lines.len() {
        files.push(parser.file()?);
    }
    Ok(files)
}

struct Parser<'a> {
    lines: Vec<&'a [u8]>,
    index: usize,
}

type HeaderPaths = Option<(Vec<u8>, Vec<u8>)>;
type HunkHeader = (u32, u32, u32, u32, Option<Vec<u8>>);

impl<'a> Parser<'a> {
    fn line_number(&self) -> usize {
        self.index + 1
    }

    fn current(&self) -> Option<&'a [u8]> {
        self.lines.get(self.index).copied()
    }

    fn fail<T>(&self, message: impl Into<String>) -> Result<T, GitError> {
        Err(parse_error(self.line_number(), message))
    }

    fn file(&mut self) -> Result<FileDiff, GitError> {
        let header_line = self.line_number();
        let header = self
            .current()
            .ok_or_else(|| parse_error(header_line, "missing header"))?;
        let header_paths =
            parse_header(header).map_err(|message| parse_error(header_line, message))?;
        self.index += 1;

        let mut old_mode = None;
        let mut new_mode = None;
        let mut kind = HeaderKind::Plain;
        if let Some(line) = self.current() {
            if let Some(mode) = line.strip_prefix(b"new file mode ") {
                new_mode = Some(
                    parse_mode(mode).map_err(|message| parse_error(self.line_number(), message))?,
                );
                kind = HeaderKind::Added;
                self.index += 1;
            } else if let Some(mode) = line.strip_prefix(b"deleted file mode ") {
                old_mode = Some(
                    parse_mode(mode).map_err(|message| parse_error(self.line_number(), message))?,
                );
                kind = HeaderKind::Deleted;
                self.index += 1;
            } else if let Some(mode) = line.strip_prefix(b"old mode ") {
                old_mode = Some(
                    parse_mode(mode).map_err(|message| parse_error(self.line_number(), message))?,
                );
                self.index += 1;
                let line = self
                    .current()
                    .ok_or_else(|| parse_error(self.line_number(), "old mode needs new mode"))?;
                let Some(mode) = line.strip_prefix(b"new mode ") else {
                    return self.fail("old mode needs new mode");
                };
                new_mode = Some(
                    parse_mode(mode).map_err(|message| parse_error(self.line_number(), message))?,
                );
                self.index += 1;
            }
        }

        let mut similarity = None;
        let mut rename = None;
        if let Some(line) = self.current()
            && let Some(percent) = line.strip_prefix(b"similarity index ")
        {
            similarity = Some(
                parse_percent(percent)
                    .map_err(|message| parse_error(self.line_number(), message))?,
            );
            self.index += 1;
            let from_line = self.current().ok_or_else(|| {
                parse_error(self.line_number(), "similarity index needs rename from")
            })?;
            let Some(from) = from_line.strip_prefix(b"rename from ") else {
                return self.fail("similarity index needs rename from");
            };
            let old =
                parse_name(from).map_err(|message| parse_error(self.line_number(), message))?;
            self.index += 1;
            let to_line = self
                .current()
                .ok_or_else(|| parse_error(self.line_number(), "rename from needs rename to"))?;
            let Some(to) = to_line.strip_prefix(b"rename to ") else {
                return self.fail("rename from needs rename to");
            };
            let new = parse_name(to).map_err(|message| parse_error(self.line_number(), message))?;
            let expected = render_header(&old, &new);
            if header != expected {
                return self.fail("rename paths disagree with diff header");
            }
            rename = Some((old, new));
            self.index += 1;
        }

        let mut old_oid = None;
        let mut new_oid = None;
        if let Some(line) = self.current()
            && let Some(index) = line.strip_prefix(b"index ")
        {
            let (old, new, index_mode) =
                parse_index(index).map_err(|message| parse_error(self.line_number(), message))?;
            if let Some(mode) = index_mode {
                for known in [old_mode, new_mode].into_iter().flatten() {
                    if known != mode {
                        return self.fail("index mode disagrees with mode header");
                    }
                }
                if old_mode.is_none() && new_mode.is_none() {
                    old_mode = Some(mode);
                    new_mode = Some(mode);
                }
            }
            old_oid = Some(old);
            new_oid = Some(new);
            self.index += 1;
        }

        if let Some(line) = self.current()
            && is_extended(line)
        {
            return self.fail("extended header line is out of order or duplicated");
        }
        if rename.is_some() && !matches!(kind, HeaderKind::Plain) {
            return self.fail("rename cannot also be an added or deleted file");
        }
        if let Some((old, new)) = header_paths.as_ref() {
            if rename.is_none() && old != new {
                return Err(parse_error(
                    header_line,
                    "header paths differ without rename metadata",
                ));
            }
            if rename.is_none() && header != render_header(old, new) && !has_octal_escape(header) {
                return Err(parse_error(header_line, "header paths are not canonical"));
            }
        }
        let (old_path, new_path, status) = match (rename, kind) {
            (Some((old, new)), HeaderKind::Plain) => (old, new, FileStatus::Renamed),
            (None, HeaderKind::Added) => {
                let (_, new) =
                    header_paths.ok_or_else(|| parse_error(header_line, "invalid header paths"))?;
                (new.clone(), new, FileStatus::Added)
            }
            (None, HeaderKind::Deleted) => {
                let (old, _) =
                    header_paths.ok_or_else(|| parse_error(header_line, "invalid header paths"))?;
                (old.clone(), old, FileStatus::Deleted)
            }
            (None, HeaderKind::Plain) => {
                let (old, new) =
                    header_paths.ok_or_else(|| parse_error(header_line, "invalid header paths"))?;
                (old, new, FileStatus::Modified)
            }
            _ => return self.fail("invalid extended header"),
        };
        let submodule = old_mode == Some(0o160000) || new_mode == Some(0o160000);

        let mut binary = false;
        let mut hunks = Vec::new();
        if let Some(line) = self.current() {
            if line.starts_with(b"Binary files ") {
                if !line.ends_with(b" differ") {
                    return self.fail("invalid binary diff line");
                }
                binary = true;
                self.index += 1;
            } else if line.starts_with(b"--- ") {
                let expected = render_side(true, status, &old_path);
                if line != expected {
                    return self.fail("old path disagrees with diff header");
                }
                self.index += 1;
                let line = self
                    .current()
                    .ok_or_else(|| parse_error(self.line_number(), "old path needs new path"))?;
                let expected = render_side(false, status, &new_path);
                if line != expected {
                    return self.fail("new path disagrees with diff header");
                }
                self.index += 1;
                if !self.current().is_some_and(|line| line.starts_with(b"@@ ")) {
                    return self.fail("new path needs at least one hunk");
                }
                while self.current().is_some_and(|line| line.starts_with(b"@@ ")) {
                    hunks.push(self.hunk(submodule)?);
                }
            } else if line.starts_with(b"@@ ") {
                return self.fail("hunk is missing file path lines");
            }
        }
        if self
            .current()
            .is_some_and(|line| !line.starts_with(b"diff --git "))
        {
            return self.fail("unknown line");
        }

        let mode_only = old_mode.is_some()
            && new_mode.is_some()
            && old_mode != new_mode
            && hunks.is_empty()
            && !binary;
        if submodule {
            hunks.clear();
        }

        Ok(FileDiff {
            path: GitPath::new(new_path),
            old_path: (status == FileStatus::Renamed).then(|| GitPath::new(old_path)),
            status,
            old_mode,
            new_mode,
            old_oid,
            new_oid,
            similarity,
            binary,
            mode_only,
            submodule,
            hunks,
        })
    }

    fn hunk(&mut self, submodule: bool) -> Result<Hunk, GitError> {
        let header_line = self.line_number();
        let header = self
            .current()
            .ok_or_else(|| parse_error(header_line, "missing hunk header"))?;
        let (old_start, old_lines, new_start, new_lines, section) =
            parse_hunk_header(header).map_err(|message| parse_error(header_line, message))?;
        self.index += 1;
        let mut old_seen = 0;
        let mut new_seen = 0;
        let mut lines = Vec::new();
        while old_seen < old_lines || new_seen < new_lines {
            let line = self
                .current()
                .ok_or_else(|| parse_error(self.line_number(), "hunk ended before its counts"))?;
            let (kind, text) = match line.first() {
                Some(b' ') => (LineKind::Context, &line[1..]),
                Some(b'+') => (LineKind::Added, &line[1..]),
                Some(b'-') => (LineKind::Removed, &line[1..]),
                _ => return self.fail("invalid hunk line"),
            };
            let old_add = matches!(kind, LineKind::Context | LineKind::Removed);
            let new_add = matches!(kind, LineKind::Context | LineKind::Added);
            if (old_add && old_seen == old_lines) || (new_add && new_seen == new_lines) {
                return self.fail("hunk line exceeds its declared count");
            }
            if submodule && !valid_subproject_line(text) {
                return self.fail("invalid submodule hunk line");
            }
            old_seen += u32::from(old_add);
            new_seen += u32::from(new_add);
            lines.push(Line {
                kind,
                text: text.to_vec(),
                no_newline: false,
            });
            self.index += 1;
            if self
                .current()
                .is_some_and(|marker| marker.starts_with(b"\\ "))
            {
                let previous = lines
                    .last_mut()
                    .ok_or_else(|| parse_error(self.line_number(), "marker has no line"))?;
                previous.no_newline = true;
                self.index += 1;
            }
        }
        Ok(Hunk {
            old_start,
            old_lines,
            new_start,
            new_lines,
            section,
            lines,
        })
    }
}

#[derive(Clone, Copy)]
enum HeaderKind {
    Plain,
    Added,
    Deleted,
}

fn parse_error(line: usize, message: impl Into<String>) -> GitError {
    GitError::DiffParse {
        line,
        message: message.into(),
    }
}

fn is_extended(line: &[u8]) -> bool {
    [
        b"new file mode ".as_slice(),
        b"deleted file mode ",
        b"old mode ",
        b"new mode ",
        b"similarity index ",
        b"rename from ",
        b"rename to ",
        b"index ",
    ]
    .iter()
    .any(|prefix| line.starts_with(prefix))
}

fn parse_header(line: &[u8]) -> Result<HeaderPaths, String> {
    let Some(rest) = line.strip_prefix(b"diff --git ") else {
        return Err("expected diff header".into());
    };
    if rest.starts_with(b"\"") {
        let (old, consumed) = parse_quoted(rest)?;
        let remainder = &rest[consumed..];
        let Some(remainder) = remainder.strip_prefix(b" ") else {
            return Ok(None);
        };
        if !remainder.starts_with(b"\"") {
            return Ok(None);
        }
        let (new, used) = parse_quoted(remainder)?;
        if used != remainder.len() {
            return Ok(None);
        }
        return Ok(prefixed_paths(&old, &new));
    }
    if rest.contains(&b'"') {
        return Ok(None);
    }
    if rest.len() % 2 == 0 {
        return Ok(None);
    }
    let middle = rest.len() / 2;
    if rest[middle] != b' ' {
        return Ok(None);
    }
    Ok(prefixed_paths(&rest[..middle], &rest[middle + 1..]))
}

fn prefixed_paths(old: &[u8], new: &[u8]) -> HeaderPaths {
    Some((
        old.strip_prefix(b"a/")?.to_vec(),
        new.strip_prefix(b"b/")?.to_vec(),
    ))
}

fn has_octal_escape(value: &[u8]) -> bool {
    value
        .windows(2)
        .any(|escape| escape[0] == b'\\' && matches!(escape[1], b'0'..=b'3'))
}

fn parse_name(value: &[u8]) -> Result<Vec<u8>, String> {
    if value.starts_with(b"\"") {
        let (name, used) = parse_quoted(value)?;
        if used != value.len() {
            return Err("trailing bytes after quoted name".into());
        }
        Ok(name)
    } else if value.contains(&b'\"') {
        Err("mixed quoted and raw name".into())
    } else {
        Ok(value.to_vec())
    }
}

fn parse_quoted(value: &[u8]) -> Result<(Vec<u8>, usize), String> {
    if value.first() != Some(&b'\"') {
        return Err("expected quoted name".into());
    }
    let mut result = Vec::new();
    let mut index = 1;
    while let Some(byte) = value.get(index) {
        match byte {
            b'\"' => return Ok((result, index + 1)),
            b'\\' => {
                index += 1;
                let escaped = *value.get(index).ok_or("unfinished name escape")?;
                let decoded = match escaped {
                    b'a' => b'\x07',
                    b'b' => b'\x08',
                    b't' => b'\t',
                    b'n' => b'\n',
                    b'v' => b'\x0b',
                    b'f' => b'\x0c',
                    b'r' => b'\r',
                    b'\"' => b'\"',
                    b'\\' => b'\\',
                    b'0'..=b'3' => {
                        let two = *value.get(index + 1).ok_or("unfinished octal escape")?;
                        let three = *value.get(index + 2).ok_or("unfinished octal escape")?;
                        if !two.is_ascii_digit()
                            || !three.is_ascii_digit()
                            || two > b'7'
                            || three > b'7'
                        {
                            return Err("invalid octal escape".into());
                        }
                        index += 2;
                        (escaped - b'0') * 64 + (two - b'0') * 8 + (three - b'0')
                    }
                    _ => return Err("unknown name escape".into()),
                };
                result.push(decoded);
            }
            byte => result.push(*byte),
        }
        index += 1;
    }
    Err("missing closing quote".into())
}

fn quote_name(name: &[u8]) -> Vec<u8> {
    let needs_quotes = name
        .iter()
        .any(|byte| matches!(byte, 0..=31 | 127 | b'\"' | b'\\'));
    if !needs_quotes {
        return name.to_vec();
    }
    let mut result = vec![b'\"'];
    for &byte in name {
        match byte {
            b'\x07' => result.extend_from_slice(b"\\a"),
            b'\x08' => result.extend_from_slice(b"\\b"),
            b'\t' => result.extend_from_slice(b"\\t"),
            b'\n' => result.extend_from_slice(b"\\n"),
            b'\x0b' => result.extend_from_slice(b"\\v"),
            b'\x0c' => result.extend_from_slice(b"\\f"),
            b'\r' => result.extend_from_slice(b"\\r"),
            b'\"' => result.extend_from_slice(b"\\\""),
            b'\\' => result.extend_from_slice(b"\\\\"),
            0..=31 | 127 => result.extend(format!("\\{byte:03o}").bytes()),
            _ => result.push(byte),
        }
    }
    result.push(b'\"');
    result
}

fn render_header(old: &[u8], new: &[u8]) -> Vec<u8> {
    let mut line = b"diff --git ".to_vec();
    let mut old_name = b"a/".to_vec();
    old_name.extend_from_slice(old);
    let mut new_name = b"b/".to_vec();
    new_name.extend_from_slice(new);
    line.extend(quote_name(&old_name));
    line.push(b' ');
    line.extend(quote_name(&new_name));
    line
}

fn render_side(old: bool, status: FileStatus, path: &[u8]) -> Vec<u8> {
    let null = (old && status == FileStatus::Added) || (!old && status == FileStatus::Deleted);
    let mut line = if old {
        b"--- ".to_vec()
    } else {
        b"+++ ".to_vec()
    };
    if null {
        line.extend_from_slice(b"/dev/null");
        return line;
    }
    let mut name = if old { b"a/".to_vec() } else { b"b/".to_vec() };
    name.extend_from_slice(path);
    let rendered = quote_name(&name);
    let space = rendered.contains(&b' ');
    line.extend(rendered);
    if space {
        line.push(b'\t');
    }
    line
}

fn parse_mode(value: &[u8]) -> Result<u32, String> {
    if value.len() != 6 || value.iter().any(|byte| !matches!(byte, b'0'..=b'7')) {
        return Err("mode must be six octal digits".into());
    }
    u32::from_str_radix(std::str::from_utf8(value).map_err(|_| "invalid mode")?, 8)
        .map_err(|_| "invalid mode".into())
}

fn parse_percent(value: &[u8]) -> Result<u8, String> {
    let Some(value) = value.strip_suffix(b"%") else {
        return Err("similarity needs percent sign".into());
    };
    if value.is_empty() || value.iter().any(|byte| !byte.is_ascii_digit()) {
        return Err("invalid similarity".into());
    }
    let text = std::str::from_utf8(value).map_err(|_| "invalid similarity")?;
    let percent: u8 = text.parse().map_err(|_| "invalid similarity")?;
    if percent > 100 {
        return Err("similarity is over 100".into());
    }
    Ok(percent)
}

fn parse_index(value: &[u8]) -> Result<(String, String, Option<u32>), String> {
    let mut words = value.split(|byte| *byte == b' ');
    let pair = words.next().ok_or("missing index object IDs")?;
    let mode = match words.next() {
        Some(mode) => Some(parse_mode(mode)?),
        None => None,
    };
    if words.next().is_some() {
        return Err("too many index fields".into());
    }
    let Some(separator) = pair.windows(2).position(|window| window == b"..") else {
        return Err("index IDs need .. separator".into());
    };
    let (old, new) = (&pair[..separator], &pair[separator + 2..]);
    if old.len() != new.len()
        || !matches!(old.len(), 40 | 64)
        || !is_lower_hex(old)
        || !is_lower_hex(new)
    {
        return Err("invalid index object IDs".into());
    }
    Ok((
        String::from_utf8_lossy(old).into(),
        String::from_utf8_lossy(new).into(),
        mode,
    ))
}

fn is_lower_hex(value: &[u8]) -> bool {
    value
        .iter()
        .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn parse_hunk_header(value: &[u8]) -> Result<HunkHeader, String> {
    let Some(rest) = value.strip_prefix(b"@@ -") else {
        return Err("invalid hunk header".into());
    };
    let (old, rest) = parse_range(rest)?;
    let Some(rest) = rest.strip_prefix(b" +") else {
        return Err("hunk lacks new range".into());
    };
    let (new, rest) = parse_range(rest)?;
    let Some(rest) = rest.strip_prefix(b" @@") else {
        return Err("hunk lacks closing @@".into());
    };
    let section = match rest {
        b"" => None,
        [b' ', text @ ..] if !text.is_empty() => Some(text.to_vec()),
        _ => return Err("invalid hunk section".into()),
    };
    if old.1 == 0 && new.1 == 0 {
        return Err("hunk cannot have two empty sides".into());
    }
    Ok((old.0, old.1, new.0, new.1, section))
}

fn parse_range(value: &[u8]) -> Result<((u32, u32), &[u8]), String> {
    let digits = value
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if digits == 0 {
        return Err("range needs a number".into());
    }
    let start = parse_u32(&value[..digits])?;
    let mut rest = &value[digits..];
    let mut count = 1;
    if let Some(after_comma) = rest.strip_prefix(b",") {
        let digits = after_comma
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        if digits == 0 {
            return Err("range count needs a number".into());
        }
        count = parse_u32(&after_comma[..digits])?;
        rest = &after_comma[digits..];
    }
    Ok(((start, count), rest))
}

fn parse_u32(value: &[u8]) -> Result<u32, String> {
    std::str::from_utf8(value)
        .map_err(|_| "invalid number")?
        .parse()
        .map_err(|_| "number exceeds u32".into())
}

fn valid_subproject_line(text: &[u8]) -> bool {
    text.strip_prefix(b"Subproject commit ")
        .is_some_and(|oid| matches!(oid.len(), 40 | 64) && is_lower_hex(oid))
}
