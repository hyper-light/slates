//! The paths of authz rules (A-606): how `parse_rule_path` (`authz_parse.c`, Subversion's `1.14.x` branch at
//! `d434dc14`) splits a section's path into segments and classifies each one of a `[:glob:...]` section, and APR's
//! `apr_fnmatch` and `apr_fnmatch_test` (APR 1.7.5, `strings/apr_fnmatch.c`) as Subversion calls them: with no flags,
//! so a backslash escapes, `/` and a leading `.` are ordinary bytes, and case counts.
//!
//! A segment of a plain section is a literal. A segment of a glob section is a literal if `apr_fnmatch_test` finds no
//! wildcard in it (its escapes then removed), `*` or `**`, a prefix (`abc*`) or a suffix (`*abc`) whose fixed part
//! holds no wildcard, and otherwise a pattern handed to `apr_fnmatch`. Runs of `*` and `**` are normalized as
//! Subversion normalizes them: `**/**` is one `**`, and `**/*` is `*/**`, so `/*/**/*`, `/**/*/*` and `/*/*/**` are one
//! rule (1.10 release notes).
//!
//! `apr_fnmatch` compares bytes as C `char`, which is signed on x86_64 and on Apple's arm64 and unsigned on Linux's;
//! the one place it shows is a bracket range over bytes past 0x7f, and this port takes the unsigned order, the one that
//! keeps UTF-8's.

use super::AuthzRefusal;

/// The path separator.
/// Format: `memchr(++start, '/', path_len - 1)` (`authz_parse.c` 494).
pub(super) const SLASH: u8 = b'/';

/// The wildcard for any run of bytes, and as a whole segment for one segment.
/// Format: `apr_fnmatch.c` 203 and `authz_parse.c` 566.
const STAR: u8 = b'*';

/// The wildcard for one byte.
/// Format: `apr_fnmatch.c` 203.
const QUESTION: u8 = b'?';

/// The escape: the byte after it is matched as itself.
/// Format: `apr_fnmatch.c` 83 and `unescape_in_place` (`authz_parse.c` 400-440).
const BACKSLASH: u8 = b'\\';

/// The byte that opens a bracket expression.
/// Format: `apr_fnmatch.c` 63.
const BRACKET_OPEN: u8 = b'[';

/// The byte that closes a bracket expression.
/// Format: `apr_fnmatch.c` 74.
const BRACKET_CLOSE: u8 = b']';

/// The bytes that negate a bracket expression right after its `[`.
/// Format: `negate = ((**pattern == '!') || (**pattern == '^'))` (`apr_fnmatch.c` 68).
const NEGATIONS: [u8; 2] = [b'!', b'^'];

/// The byte between a range's ends in a bracket expression.
/// Format: `((*pattern)[1] == '-')` (`apr_fnmatch.c` 98).
const RANGE: u8 = b'-';

/// The byte a dummy match counts a bracket expression against.
/// Format: `static const char dummystring[2] = {' ', 0}` (`apr_fnmatch.c` 190).
const DUMMY: u8 = b' ';

/// The segment `.`, refused in a rule's path.
/// Format: `authz_parse.c` 523-532.
const DOT: &[u8] = b".";

/// The segment `..`, refused in a rule's path.
/// Format: as [`DOT`].
const DOT_DOT: &[u8] = b"..";

/// The segment `*`: one segment.
/// Format: `authz_parse.c` 566-569.
const ANY_ONE: &[u8] = b"*";

/// The segment `**`: any number of segments.
/// Format: as [`ANY_ONE`].
const ANY_MANY: &[u8] = b"**";

/// One segment of a rule's path, as `parse_rule_path` classifies it (`authz.h` `authz_rule_segment_t`).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Segment {
  /// Matches a segment of exactly these bytes.
  Literal(Box<[u8]>),
  /// Matches a segment that starts with these bytes (`abc*`).
  Prefix(Box<[u8]>),
  /// Matches a segment that ends with these bytes (`*abc`); Subversion stores them reversed, which changes nothing
  /// here.
  Suffix(Box<[u8]>),
  /// `*`: any one segment.
  AnyOne,
  /// `**`: any number of segments, none included.
  AnyMany,
  /// Any other pattern, matched by `apr_fnmatch`.
  Pattern(Box<[u8]>),
}

/// The segments of a section's path `path` (which starts with `/`), classified as a glob section's when `glob`; the
/// root has none.
///
/// # Errors
/// [`AuthzRefusal::RulePathEmptySegment`] for two slashes in a row or a trailing slash (the module documentation of
/// `authz`, item 3: two slashes at the start too), [`AuthzRefusal::RulePathDot`] and [`AuthzRefusal::RulePathDotDot`].
pub(super) fn rule_path(path: &[u8], glob: bool) -> Result<Vec<Segment>, AuthzRefusal> {
  let after_root = path.get(1..).unwrap_or_default();
  let mut segments = Vec::new();
  if after_root.is_empty() {
    return Ok(segments);
  }
  for name in after_root.split(|byte| *byte == SLASH) {
    check_canonical(name)?;
    if glob {
      push_normalized(&mut segments, classify(name));
    } else {
      segments.push(Segment::Literal(name.into()));
    }
  }
  Ok(segments)
}

/// Refuses an empty segment, `.` and `..` (`authz_parse.c` 505-532).
fn check_canonical(name: &[u8]) -> Result<(), AuthzRefusal> {
  if name.is_empty() {
    Err(AuthzRefusal::RulePathEmptySegment)
  } else if name == DOT {
    Err(AuthzRefusal::RulePathDot)
  } else if name == DOT_DOT {
    Err(AuthzRefusal::RulePathDotDot)
  } else {
    Ok(())
  }
}

/// A glob section's segment `name`, classified (`authz_parse.c` 544-632).
fn classify(name: &[u8]) -> Segment {
  if !has_wildcards(name) {
    return Segment::Literal(unescape(name));
  }
  if name == ANY_ONE {
    return Segment::AnyOne;
  }
  if name == ANY_MANY {
    return Segment::AnyMany;
  }
  if let Some(tail) = name.strip_prefix(&[STAR])
    && !has_wildcards(tail)
  {
    return Segment::Suffix(unescape(tail));
  }
  match prefix_of(name) {
    Some(head) => Segment::Prefix(unescape(head)),
    None => Segment::Pattern(name.into()),
  }
}

/// The fixed part of `name` when it is a prefix pattern: it ends in a `*` not preceded by a backslash, and the rest
/// holds no wildcard (`authz_parse.c` 609-628).
fn prefix_of(name: &[u8]) -> Option<&[u8]> {
  let head = name.strip_suffix(&[STAR])?;
  if head.last() == Some(&BACKSLASH) || has_wildcards(head) {
    return None;
  }
  Some(head)
}

/// Appends `segment`, normalizing runs of `*` and `**` as `parse_rule_path` does: a `*` after a `**` swaps places with
/// it, and a `**` after a `**` is dropped (`authz_parse.c` 566-602).
fn push_normalized(segments: &mut Vec<Segment>, segment: Segment) {
  let after_many = segments.last() == Some(&Segment::AnyMany);
  match segment {
    Segment::AnyOne if after_many => {
      if let Some(last) = segments.last_mut() {
        *last = Segment::AnyOne;
      }
      segments.push(Segment::AnyMany);
    }
    Segment::AnyMany if after_many => {}
    other => segments.push(other),
  }
}

/// `name` without its escapes: each backslash takes the byte after it as itself, and a backslash at the end is kept
/// (`unescape_in_place`).
fn unescape(name: &[u8]) -> Box<[u8]> {
  let mut out = Vec::with_capacity(name.len());
  let mut escaped = false;
  for &byte in name {
    if escaped {
      out.push(byte);
      escaped = false;
    } else if byte == BACKSLASH {
      escaped = true;
    } else {
      out.push(byte);
    }
  }
  if escaped {
    out.push(BACKSLASH);
  }
  out.into_boxed_slice()
}

/// `apr_fnmatch_test`: whether `pattern` holds a wildcard, a `*` or `?`, or a `]` after a `[`, outside escapes.
pub(super) fn has_wildcards(pattern: &[u8]) -> bool {
  let mut bracket_opened = false;
  let mut at = 0usize;
  while let Some(&byte) = pattern.get(at) {
    match byte {
      STAR | QUESTION => return true,
      BACKSLASH => {
        at = at.saturating_add(1);
        if pattern.get(at).is_none() {
          return false;
        }
      }
      BRACKET_OPEN => bracket_opened = true,
      BRACKET_CLOSE if bracket_opened => return true,
      _ => {}
    }
    at = at.saturating_add(1);
  }
  false
}

/// `apr_fnmatch(pattern, string, 0) == 0`: whether `string` matches `pattern` whole.
pub(super) fn fnmatch(pattern: &[u8], string: &[u8]) -> bool {
  if pattern.first() != Some(&STAR) && (pattern.is_empty() || string.is_empty()) {
    return pattern.is_empty() && string.is_empty();
  }
  let mut matcher = Matcher {
    pattern,
    string,
    p: 0,
    s: 0,
    start: 0,
    resume: 0,
    fixed: 0,
  };
  matcher.run() && matcher.s >= string.len() && matcher.p >= pattern.len()
}

/// `apr_fnmatch`'s state over one pattern and one string (flags 0: one segment, the whole string).
struct Matcher<'a> {
  pattern: &'a [u8],
  string: &'a [u8],
  /// The place in the pattern.
  p: usize,
  /// The place in the string.
  s: usize,
  /// Where the current wildcard's attempt started in the string (`strstartseg`).
  start: usize,
  /// The pattern after the current wildcard (`mismatch`).
  resume: usize,
  /// The bytes the pattern's fixed run after the wildcard takes (`matchlen`).
  fixed: usize,
}

impl Matcher<'_> {
  /// The pattern's loop; false where `apr_fnmatch` answers `APR_FNM_NOMATCH` before its end checks.
  fn run(&mut self) -> bool {
    let end = self.string.len();
    while self.p < self.pattern.len() {
      if self.s >= end && self.pattern.get(self.p) != Some(&STAR) {
        break;
      }
      let Some(mut wild) = self.wildcards() else {
        return false;
      };
      if wild {
        match self.anchor() {
          Some(retry) => wild = retry,
          None => return false,
        }
      }
      if !self.fixed_run(wild) {
        return false;
      }
    }
    true
  }

  /// Consumes a run of `*` and `?`, each `?` taking one byte; `Some(true)` when a `*` was among them, `None` when a
  /// `?` found no byte.
  fn wildcards(&mut self) -> Option<bool> {
    let mut wild = false;
    while let Some(&byte) = self.pattern.get(self.p) {
      if byte == STAR {
        wild = true;
      } else if byte == QUESTION {
        if self.s >= self.string.len() {
          return None;
        }
        self.s = self.s.saturating_add(1);
      } else {
        break;
      }
      self.p = self.p.saturating_add(1);
    }
    Some(wild)
  }

  /// After a `*`: notes where to retry from; when no `*` follows, moves to the string's tail, which the rest of the
  /// pattern must match with no retry (`wild = 0`). `None` when too few bytes remain, else whether a mismatch may
  /// retry one byte further on.
  fn anchor(&mut self) -> Option<bool> {
    self.start = self.s;
    self.resume = self.p;
    let (fixed, last) = fixed_length(self.pattern, self.p);
    self.fixed = fixed;
    let end = self.string.len();
    if self.s.saturating_add(fixed) > end {
      return None;
    }
    if last {
      self.s = end.saturating_sub(fixed);
      return Some(false);
    }
    Some(true)
  }

  /// Matches the pattern item by item up to its next `*`; after a mismatch under a `*`, retries one byte further on.
  fn fixed_run(&mut self, wild: bool) -> bool {
    let end = self.string.len();
    while self.p < self.pattern.len() && self.s < end {
      if self.pattern.get(self.p) == Some(&STAR) {
        break;
      }
      if let Some((p, s)) = item(self.pattern, self.p, self.string, self.s) {
        self.p = p;
        self.s = s;
        continue;
      }
      if !wild {
        return false;
      }
      self.start = self.start.saturating_add(1);
      self.s = self.start;
      if self.s.saturating_add(self.fixed) > end {
        return false;
      }
      self.p = self.resume;
    }
    true
  }
}

/// How many string bytes the pattern from `from` takes up to its next `*` or its end, and whether the end came first
/// (`apr_fnmatch.c` 212-252).
fn fixed_length(pattern: &[u8], from: usize) -> (usize, bool) {
  let mut at = from;
  let mut length = 0usize;
  loop {
    match pattern.get(at) {
      None => return (length, true),
      Some(&STAR) => return (length, false),
      Some(&BACKSLASH) if pattern.get(at.saturating_add(1)).is_some() => {
        at = at.saturating_add(2);
      }
      Some(&BRACKET_OPEN) => {
        at = bracket(pattern, at, DUMMY).map_or(at.saturating_add(1), |(next, _)| next);
      }
      Some(_) => at = at.saturating_add(1),
    }
    length = length.saturating_add(1);
  }
}

/// `fnmatch_ch`: the pattern's item at `p` against the string's byte at `s`; the places after both when it matches.
fn item(pattern: &[u8], p: usize, string: &[u8], s: usize) -> Option<(usize, usize)> {
  let byte = *string.get(s)?;
  let next_s = s.saturating_add(1);
  match *pattern.get(p)? {
    BRACKET_OPEN => match bracket(pattern, p, byte) {
      Some((next_p, true)) => Some((next_p, next_s)),
      Some((_, false)) => None,
      None => (byte == BRACKET_OPEN).then_some((p.saturating_add(1), next_s)),
    },
    QUESTION => Some((p.saturating_add(1), next_s)),
    BACKSLASH => match pattern.get(p.saturating_add(1)) {
      Some(&escaped) => (escaped == byte).then_some((p.saturating_add(2), next_s)),
      None => (byte == BACKSLASH).then_some((p.saturating_add(1), next_s)),
    },
    literal => (literal == byte).then_some((p.saturating_add(1), next_s)),
  }
}

/// A bracket expression at `open` (a `[`) against `byte`: the place after its `]` and whether it matched, or `None`
/// when the expression is not closed, which makes the `[` a literal (`apr_fnmatch.c` 63-140).
fn bracket(pattern: &[u8], open: usize, byte: u8) -> Option<(usize, bool)> {
  let at = |index: usize| pattern.get(index).copied();
  let mut p = open.saturating_add(1);
  let negate = at(p).is_some_and(|first| NEGATIONS.contains(&first));
  if negate {
    p = p.saturating_add(1);
  }
  let mut matched = false;
  let mut first = true;
  loop {
    let mut member = at(p)?;
    let leading_close = first && member == BRACKET_CLOSE;
    first = false;
    if member == BRACKET_CLOSE && !leading_close {
      return Some((p.saturating_add(1), matched != negate));
    }
    if member == BACKSLASH && !leading_close {
      p = p.saturating_add(1);
      member = at(p)?;
    }
    let (next, hit) = bracket_member(pattern, p, member, byte)?;
    matched |= hit;
    p = next;
  }
}

/// One member of a bracket expression at `p` (whose byte is `member`): a range `a-z` unless its end is `]`, else the
/// byte itself; the place after it and whether `byte` falls in it. `None` when a range runs off the pattern's end.
fn bracket_member(pattern: &[u8], p: usize, member: u8, byte: u8) -> Option<(usize, bool)> {
  let at = |index: usize| pattern.get(index).copied();
  let dash = at(p.saturating_add(1)) == Some(RANGE);
  let after_dash = at(p.saturating_add(2));
  if dash && after_dash != Some(BRACKET_CLOSE) {
    let skip = if after_dash == Some(BACKSLASH) { 3 } else { 2 };
    let end_at = p.saturating_add(skip);
    let last = at(end_at)?;
    return Some((end_at.saturating_add(1), member <= byte && byte <= last));
  }
  Some((p.saturating_add(1), member == byte))
}
