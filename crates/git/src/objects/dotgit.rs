//! The names a checkout must never write, as git's fsck finds them in a tree (A-590): `.git` and the special dotfiles
//! (`.gitmodules`, `.gitattributes`, `.gitignore`, `.mailmap`) under every spelling that HFS+ or NTFS resolves to the
//! same file.
//!
//! - HFS+ (utf8.c's `is_hfs_dot_generic`): ASCII case folded, and the zero-width and directional code points HFS+
//!   ignores skipped. A name that is not well-formed UTF-8 reads as ended where it stops being so, so `.git` and then a
//!   malformed byte is `.git`.
//! - NTFS (path.c's `is_ntfs_dotgit` and `is_ntfs_dot_generic`): ASCII case folded; trailing spaces and dots; an
//!   alternate data stream after a `:`; the 8.3 short names (`git~1`, `gitmod~1` to `~4`, and the fall-back short name
//!   that begins with a hash's first characters, such as `gi7eba~1` for `.gitmodules`); for `.git`, a backslash or a
//!   slash after it.
//!
//! git reads each name as a C string; here a byte past the name reads as the NUL that would end it.

/// The byte at `at` in `name`, or the NUL that ends a C string.
fn at(name: &[u8], at: usize) -> u8 {
  name.get(at).copied().unwrap_or(b'\0')
}

/// Format: the longest UTF-8 encoding of one character, in bytes (RFC 3629 §3).
const UTF8_LONGEST: usize = 4;

/// Format: the code points git's reader refuses (`pick_one_utf8_char`, "U+FFFE or U+FFFF?") though UTF-8 itself, and
/// so Rust's reader, accepts them.
const REFUSED_NONCHARACTERS: [char; 2] = ['\u{fffe}', '\u{ffff}'];

/// The character at `from` as git's `pick_one_utf8_char` reads it: its code point and where the next begins. At the
/// name's end, the NUL a C string ends with (code point 0). None when the bytes there are not one well-formed
/// character: overlong forms, surrogates, past U+10FFFF and a cut sequence, which Rust's reader refuses as git's does,
/// and U+FFFE and U+FFFF, which only git's refuses.
fn character_at(name: &[u8], from: usize) -> Option<(u32, usize)> {
  let rest = name.get(from..).unwrap_or_default();
  if rest.is_empty() {
    return Some((0, from.saturating_add(1)));
  }
  let window = rest.get(..UTF8_LONGEST).unwrap_or(rest);
  let valid = match core::str::from_utf8(window) {
    Ok(text) => text,
    Err(error) => core::str::from_utf8(window.get(..error.valid_up_to()).unwrap_or_default())
      .unwrap_or_default(),
  };
  let character = valid.chars().next()?;
  if REFUSED_NONCHARACTERS.contains(&character) {
    return None;
  }
  Some((
    u32::from(character),
    from.saturating_add(character.len_utf8()),
  ))
}

/// Whether HFS+ ignores `code` in a name (utf8.c's `next_hfs_char`).
fn hfs_ignores(code: u32) -> bool {
  // Format: the zero-width and directional formatting code points, and the zero-width no-break space (utf8.c).
  matches!(code, 0x200c..=0x200f | 0x202a..=0x202e | 0x206a..=0x206f | 0xfeff)
}

/// `next_hfs_char`: the next code point HFS+ does not ignore, from `from`, and where the one after it begins: 0 at the
/// name's end, and 0 from there on once the name stops being well-formed UTF-8 (`from` none).
fn next_hfs(name: &[u8], from: Option<usize>) -> (u32, Option<usize>) {
  let mut from = from;
  while let Some(here) = from {
    match character_at(name, here) {
      None => return (0, None),
      Some((code, next)) if hfs_ignores(code) => from = Some(next),
      Some((code, next)) => return (code, Some(next)),
    }
  }
  (0, None)
}

/// `is_hfs_dot_generic`: whether `name` is `.` and then `needle` (lower-case ASCII) as HFS+ reads it, ending there or
/// at a `/` (`is_dir_sep` in git for Linux and macOS).
fn is_hfs_dot(name: &[u8], needle: &[u8]) -> bool {
  let (first, mut from) = next_hfs(name, Some(0));
  if first != u32::from(b'.') {
    return false;
  }
  for want in needle {
    let (code, next) = next_hfs(name, from);
    from = next;
    let folded = u8::try_from(code).map(|byte| byte.to_ascii_lowercase());
    if folded != Ok(*want) {
      return false;
    }
  }
  let (last, _) = next_hfs(name, from);
  last == 0 || last == u32::from(b'/')
}

/// Whether `name` begins with `prefix`, ASCII case folded (`strncasecmp`); the rest of `name` when it does.
fn strip_prefix_folded<'a>(name: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
  let head = name.get(..prefix.len())?;
  head
    .eq_ignore_ascii_case(prefix)
    .then(|| name.get(prefix.len()..).unwrap_or_default())
}

/// Whether `rest` holds nothing but spaces and dots up to its end or a byte of `ends` (path.c's
/// `only_spaces_and_periods`, and `is_ntfs_dotgit`'s own loop).
fn only_spaces_and_dots(rest: &[u8], ends: &[u8]) -> bool {
  rest
    .iter()
    .find(|byte| **byte != b' ' && **byte != b'.')
    .is_none_or(|byte| ends.contains(byte))
}

/// Format: what may end `.git` or `git~1` in `is_ntfs_dotgit`: a separator of either platform, or a stream's `:`.
const DOTGIT_ENDS: &[u8] = b"/\\:";

/// Format: what may end a special dotfile's name in `is_ntfs_dot_generic`: a stream's `:`.
const STREAM: &[u8] = b":";

/// `is_ntfs_dotgit`: `.git` or `git~1`, either case, then only spaces and dots up to the end, a separator of either
/// platform or a `:`.
fn is_ntfs_git(name: &[u8]) -> bool {
  strip_prefix_folded(name, b".git")
    .or_else(|| strip_prefix_folded(name, b"git~1"))
    .is_some_and(|rest| only_spaces_and_dots(rest, DOTGIT_ENDS))
}

/// Format: the characters of an NTFS short name before its `~` ("shortened to 6 characters", path.c).
const SHORT_NAME_PREFIX: usize = 6;

/// Format: the characters of an 8.3 short name's base (path.c's fall-back loop, `i < 8`).
const SHORT_NAME_BASE: usize = 8;

/// Format: the last number of a regular short name, `~1` to `~4` (path.c).
const REGULAR_SHORT_NAME_LAST: u8 = b'4';

/// `is_ntfs_dot_generic`: `name` is `.` and `dotgit_name` in either case, its regular short name (its first six
/// characters, `~` and 1 to 4), or a fall-back short name (`prefix`'s characters, `~`, and a number from 1 that does
/// not begin with 0); then only spaces and dots up to the end or a `:`.
fn is_ntfs_dot(name: &[u8], dotgit_name: &[u8], prefix: &[u8; SHORT_NAME_PREFIX]) -> bool {
  if let Some(rest) = name
    .strip_prefix(b".")
    .and_then(|rest| strip_prefix_folded(rest, dotgit_name))
  {
    return only_spaces_and_dots(rest, STREAM);
  }
  let short = dotgit_name.get(..SHORT_NAME_PREFIX).unwrap_or(dotgit_name);
  if strip_prefix_folded(name, short).is_some()
    && at(name, SHORT_NAME_PREFIX) == b'~'
    && (b'1'..=REGULAR_SHORT_NAME_LAST).contains(&at(name, SHORT_NAME_PREFIX.saturating_add(1)))
  {
    let rest = name
      .get(SHORT_NAME_PREFIX.saturating_add(2)..)
      .unwrap_or_default();
    return only_spaces_and_dots(rest, STREAM);
  }
  fall_back_short_name(name, prefix)
    .is_some_and(|end| only_spaces_and_dots(name.get(end..).unwrap_or_default(), STREAM))
}

/// path.c's fall-back short-name loop over the first eight characters: up to six of `prefix` (either case, never a
/// byte past ASCII), then `~`, a digit from 1 and digits to the eighth. Where the name goes on when it matches.
fn fall_back_short_name(name: &[u8], prefix: &[u8; SHORT_NAME_PREFIX]) -> Option<usize> {
  let mut saw_tilde = false;
  let mut index = 0;
  while index < SHORT_NAME_BASE {
    let byte = at(name, index);
    let fits = if byte == b'\0' {
      false
    } else if saw_tilde {
      byte.is_ascii_digit()
    } else if byte == b'~' {
      // git reads the digit after the `~` here and steps past it: `name[++i]`.
      index = index.saturating_add(1);
      saw_tilde = true;
      at(name, index).is_ascii_digit() && at(name, index) != b'0'
    } else {
      index < SHORT_NAME_PREFIX
        && byte.is_ascii()
        && prefix.get(index) == Some(&byte.to_ascii_lowercase())
    };
    if !fits {
      return None;
    }
    index = index.saturating_add(1);
  }
  Some(index)
}

/// `.git` as HFS+ or NTFS reads it (`is_hfs_dotgit || is_ntfs_dotgit`).
pub(crate) fn dotgit(name: &[u8]) -> bool {
  is_hfs_dot(name, b"git") || is_ntfs_git(name)
}

/// `.git` as NTFS alone reads it, for the part of a name after a backslash.
pub(crate) fn ntfs_dotgit(name: &[u8]) -> bool {
  is_ntfs_git(name)
}

/// `.gitmodules` as HFS+ or NTFS reads it.
pub(crate) fn dotgitmodules(name: &[u8]) -> bool {
  is_hfs_dot(name, b"gitmodules") || ntfs_dotgitmodules(name)
}

/// `.gitmodules` as NTFS alone reads it, for the part of a name after a backslash.
pub(crate) fn ntfs_dotgitmodules(name: &[u8]) -> bool {
  is_ntfs_dot(name, b"gitmodules", b"gi7eba")
}

/// `.gitattributes` as HFS+ or NTFS reads it.
pub(crate) fn dotgitattributes(name: &[u8]) -> bool {
  is_hfs_dot(name, b"gitattributes") || is_ntfs_dot(name, b"gitattributes", b"gi7d29")
}

/// `.gitignore` as HFS+ or NTFS reads it.
pub(crate) fn dotgitignore(name: &[u8]) -> bool {
  is_hfs_dot(name, b"gitignore") || is_ntfs_dot(name, b"gitignore", b"gi250a")
}

/// `.mailmap` as HFS+ or NTFS reads it.
pub(crate) fn dotmailmap(name: &[u8]) -> bool {
  is_hfs_dot(name, b"mailmap") || is_ntfs_dot(name, b"mailmap", b"maba30")
}
