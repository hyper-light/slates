//! A sparse filter's patterns (A-640 part 4; §4.17.2 "A fetch" item 3, "Filters"): the list a `sparse:oid=<blob-ish>`
//! filter names, read from its blob and asked of each path as git reads and asks it [C: `dir.c`
//! `add_patterns_from_blob_to_list`, `add_patterns_from_buffer`, `trim_trailing_spaces`, `parse_path_pattern`,
//! `path_matches_pattern_list`, `last_matching_pattern_from_list`, `match_basename` and `match_pathname`;
//! `list-objects-filter.c` `filter_sparse_oid__init`; git v2.50.1].
//!
//! **Reading.** A blob past git's limit, its final newline counted ([`MOST_BYTES`]), is refused, as git refuses it
//! ("ignoring excessively large pattern blob", then "unable to parse sparse filter data"). An empty blob holds no
//! pattern. A UTF-8 byte-order mark at the start is skipped. Each line ends at a newline, the last at the blob's end
//! when it has none. A line that is empty or begins with `#` holds no pattern; any other loses a carriage return before
//! its newline, ends at its first NUL (git reads it as a C string), and loses its trailing spaces unless a backslash
//! escapes them. So a line of spaces, or of a carriage return alone, is an empty pattern, which matches the root tree
//! alone: its name is the one that is empty.
//!
//! **A pattern** (`parse_path_pattern`). `!` first makes it negative. A trailing `/` makes it match trees alone, and is
//! dropped. Then one holding no `/` matches a path's last segment, wherever it is (`match_basename`): equal to it when
//! the pattern holds no glob special, ending with what follows a first `*` when that is its one special, and by git's
//! glob otherwise. One holding a `/` matches the whole path from the root, a leading `/` dropped (`match_pathname`; a
//! filter's patterns have the root as their base): the bytes before its first glob special equal to the path's first
//! bytes, and the rest of the path matched by the rest of the pattern, globbed as a pattern of its own (so `ab**/c`
//! matches `abx/y/c`, its rest `**/c` standing at a pattern's start). The glob is `slates_vcs::wildmatch` (A-671), the
//! servers' one matcher, which has `WM_PATHNAME`; git matches a segment pattern without it, which changes a verdict only
//! where the text or the pattern holds a `/`, and neither does there.
//!
//! **A path's verdict** (`path_matches_pattern_list` outside cone mode, which a filter's list never enters): the last
//! pattern that matches decides, [`Verdict::Matched`], or [`Verdict::NotMatched`] when it is negative; when none matches,
//! the path is [`Verdict::Undecided`], and the filter takes the verdict of the tree it is in.
//!
//! **Bounds.** The patterns are counted before the list is made, and the list's slots charged to the request's
//! [`Admission`]; each pattern's own bytes and compiled glob are charged as it is made, before the list keeps it.

use slates_vcs::wildmatch::{Glob, is_glob_special};

use crate::{Admission, PastAdmission};

/// Format: the most bytes git reads of a pattern blob, its final newline counted (`dir.c`'s `PATTERN_MAX_FILE_SIZE`,
/// 100 MiB, git v2.50.1).
pub const MOST_BYTES: u64 = 100 * 1024 * 1024;

/// Format: the UTF-8 byte-order mark git skips at a blob's start (`utf8.c` `skip_utf8_bom`).
const BYTE_ORDER_MARK: &[u8] = b"\xef\xbb\xbf";

/// Format: the byte that ends a line.
const NEWLINE: u8 = b'\n';

/// Format: the byte a line loses before its newline.
const CARRIAGE_RETURN: u8 = b'\r';

/// Format: the byte a C string ends at.
const NUL: u8 = 0;

/// Format: the byte that makes a line a comment.
const COMMENT: u8 = b'#';

/// Format: the byte a line loses a trailing run of.
const SPACE: u8 = b' ';

/// Format: the byte that keeps the byte after it.
const BACKSLASH: u8 = b'\\';

/// Format: the byte that makes a pattern negative.
const NEGATIVE: u8 = b'!';

/// Format: the byte between a path's segments.
const SLASH: u8 = b'/';

/// Format: the byte a pattern matching a segment's end begins with.
const STAR: u8 = b'*';

/// A sparse filter's pattern list (the module doc).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Patterns {
  /// The patterns, in the blob's order: the last that matches a path decides it.
  patterns: Vec<Pattern>,
}

/// What a list says of a path (git's `pattern_match_result` outside cone mode).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
  /// No pattern matches it.
  Undecided,
  /// The last pattern that matches it is negative.
  NotMatched,
  /// The last pattern that matches it is not negative.
  Matched,
}

/// Why a blob's patterns are not read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
  /// The blob, its final newline counted, holds more than [`MOST_BYTES`].
  TooLarge {
    /// Its bytes, the final newline counted.
    bytes: u64,
  },
  /// What the list would keep is past the request's admission.
  PastAdmission(PastAdmission),
}

/// One pattern (git's `struct path_pattern`).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Pattern {
  /// `!`: a match says not matched.
  negative: bool,
  /// A trailing `/`: it matches trees alone (`PATTERN_FLAG_MUSTBEDIR`).
  trees_only: bool,
  /// How it matches.
  form: Form,
}

/// How a pattern matches a path (`PATTERN_FLAG_NODIR`, `PATTERN_FLAG_ENDSWITH` and `nowildcardlen`).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Form {
  /// No `/` and no glob special: the path's last segment equals these bytes.
  Segment(Vec<u8>),
  /// No `/`, a `*` first and no other special: the last segment ends with these bytes.
  SegmentEnd(Vec<u8>),
  /// No `/`: the last segment, by git's glob.
  SegmentGlob(Glob),
  /// A `/`: the whole path, its first bytes equal to `literal` and the rest matched by `rest`.
  Path {
    /// The pattern's bytes before its first glob special, a leading `/` dropped.
    literal: Vec<u8>,
    /// The rest of the pattern, globbed as a pattern of its own.
    rest: Glob,
  },
}

impl Patterns {
  /// The list `blob` holds, read as git reads a sparse filter's blob (the module doc), kept within `admission`.
  pub fn read(blob: &[u8], admission: &mut Admission) -> Result<Patterns, Refused> {
    let bytes = counted_bytes(blob);
    if bytes > MOST_BYTES {
      return Err(Refused::TooLarge { bytes });
    }
    let body = blob.strip_prefix(BYTE_ORDER_MARK).unwrap_or(blob);
    let count = body
      .split(|byte| *byte == NEWLINE)
      .filter_map(entry)
      .count();
    admission
      .charge_items::<Pattern>(count)
      .map_err(Refused::PastAdmission)?;
    let mut patterns = Vec::with_capacity(count);
    for line in body.split(|byte| *byte == NEWLINE).filter_map(entry) {
      let pattern = Pattern::parse(line);
      admission
        .charge(pattern.kept_bytes())
        .map_err(Refused::PastAdmission)?;
      patterns.push(pattern);
    }
    Ok(Patterns { patterns })
  }

  /// What the list says of the object at `path`, its segments from the root tree joined by `/` (the root tree's own
  /// path is empty), a tree when `tree`: the last pattern that matches it decides.
  pub fn decide(&self, path: &[u8], tree: bool) -> Verdict {
    let segment = path.rsplit(|byte| *byte == SLASH).next().unwrap_or(path);
    let last = self
      .patterns
      .iter()
      .rev()
      .find(|pattern| pattern.matches(path, segment, tree));
    match last {
      None => Verdict::Undecided,
      Some(pattern) if pattern.negative => Verdict::NotMatched,
      Some(_) => Verdict::Matched,
    }
  }
}

impl Pattern {
  /// The pattern `entry`, a line as read, writes (`parse_path_pattern`).
  fn parse(entry: &[u8]) -> Pattern {
    let (negative, body) = match entry.split_first() {
      Some((&NEGATIVE, rest)) => (true, rest),
      _ => (false, entry),
    };
    let (trees_only, text) = match body.split_last() {
      Some((&SLASH, rest)) => (true, rest),
      _ => (false, body),
    };
    let literal = simple_length(body).min(text.len());
    let form = if text.contains(&SLASH) {
      path_form(text, literal)
    } else {
      segment_form(body, text, literal)
    };
    Pattern {
      negative,
      trees_only,
      form,
    }
  }

  /// Whether the pattern matches the object at `path`, whose last segment is `segment`, a tree when `tree`
  /// (`last_matching_pattern_from_list`'s test of one pattern).
  fn matches(&self, path: &[u8], segment: &[u8], tree: bool) -> bool {
    if self.trees_only && !tree {
      return false;
    }
    match &self.form {
      Form::Segment(text) => segment == text.as_slice(),
      Form::SegmentEnd(end) => segment.ends_with(end),
      Form::SegmentGlob(glob) => glob.matches(segment),
      Form::Path { literal, rest } => {
        !path.is_empty()
          && path
            .strip_prefix(literal.as_slice())
            .is_some_and(|after| rest.matches(after))
      }
    }
  }

  /// The bytes the pattern keeps beyond its own size: its bytes' room and its glob's tokens.
  fn kept_bytes(&self) -> u64 {
    match &self.form {
      Form::Segment(bytes) | Form::SegmentEnd(bytes) => crate::to_u64(bytes.capacity()),
      Form::SegmentGlob(glob) => glob.kept_bytes(),
      Form::Path { literal, rest } => {
        crate::to_u64(literal.capacity()).saturating_add(rest.kept_bytes())
      }
    }
  }
}

/// The bytes git reads of `blob`: its own, and the newline it adds when the last is not one (`do_read_blob`).
fn counted_bytes(blob: &[u8]) -> u64 {
  let added = !blob.is_empty() && blob.last() != Some(&NEWLINE);
  crate::to_u64(blob.len()).saturating_add(u64::from(added))
}

/// The pattern a line holds, as `add_patterns_from_buffer` reads it: none for an empty line or a comment; otherwise the
/// line without a carriage return before its newline, cut at its first NUL, its trailing spaces trimmed.
fn entry(line: &[u8]) -> Option<&[u8]> {
  if line.first().is_none_or(|first| *first == COMMENT) {
    return None;
  }
  let line = line.strip_suffix(&[CARRIAGE_RETURN]).unwrap_or(line);
  let line = line.split(|byte| *byte == NUL).next().unwrap_or(line);
  Some(trim_trailing_spaces(line))
}

/// `line` without its trailing spaces, a space a backslash escapes kept, and nothing trimmed after a lone backslash at
/// its end (`trim_trailing_spaces`).
fn trim_trailing_spaces(line: &[u8]) -> &[u8] {
  let mut last_space = None;
  let mut at = 0usize;
  while let Some(&byte) = line.get(at) {
    match byte {
      SPACE => {
        last_space = last_space.or(Some(at));
      }
      BACKSLASH => {
        at = at.saturating_add(1);
        if line.get(at).is_none() {
          return line;
        }
        last_space = None;
      }
      _ => last_space = None,
    }
    at = at.saturating_add(1);
  }
  last_space.and_then(|cut| line.get(..cut)).unwrap_or(line)
}

/// How many of `bytes` come before its first glob special (`simple_length`).
fn simple_length(bytes: &[u8]) -> usize {
  bytes
    .iter()
    .position(|byte| is_glob_special(*byte))
    .unwrap_or(bytes.len())
}

/// How a pattern holding no `/` matches (`match_basename`): `body` is the pattern with a trailing `/`, `text` without,
/// and `literal` how many bytes of it come before its first glob special.
fn segment_form(body: &[u8], text: &[u8], literal: usize) -> Form {
  if literal == text.len() {
    return Form::Segment(text.to_vec());
  }
  match body.split_first() {
    Some((&STAR, after)) if simple_length(after) == after.len() => {
      Form::SegmentEnd(text.get(1..).unwrap_or_default().to_vec())
    }
    _ => Form::SegmentGlob(Glob::new(text)),
  }
}

/// How a pattern holding a `/` matches (`match_pathname`): a leading `/` dropped, its literal bytes one fewer.
fn path_form(text: &[u8], literal: usize) -> Form {
  let (text, literal) = match text.split_first() {
    Some((&SLASH, rest)) => (rest, literal.saturating_sub(1)),
    _ => (text, literal),
  };
  let (literal, rest) = text.split_at_checked(literal).unwrap_or((text, &[]));
  Form::Path {
    literal: literal.to_vec(),
    rest: Glob::new(rest),
  }
}

#[cfg(test)]
#[path = "tests/sparse.rs"]
mod tests;
