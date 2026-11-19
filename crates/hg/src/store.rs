//! A Mercurial store's file names and its file list (A-630; §4.17.3 piece 3, the import; `store.py` \[C\]): how an
//! existing repository's `.hg/store` names the revlog of each tracked path, and the `fncache` that lists them.
//!
//! **The encodings.** A store path (`data/` and the tracked path, then `.i` or `.d`) is first renamed by `encodedir` (a
//! directory named `x.i`, `y.d` or `z.hg` gains `.hg`, so it cannot meet a revlog's name); then every byte is encoded
//! (`_encodefname`: a capital as `_` and its lower case, `_` as `__`, a control byte, a byte from 126 up and each of
//! Windows' reserved `\:*?"<>|` as `~` and two lower-case hexadecimal digits); then each segment (`_auxencode`)
//! escapes the third letter of a name Windows reserves (`aux`, `con`, `prn`, `nul`, `com1` to `com9`, `lpt1` to `lpt9`,
//! alone or before a period) and a last period or space, and, in a store with `dotencode`, a first period or space
//! instead. A path still longer than 120 bytes is hashed (`_hashencode`): `dh/`, the first eight bytes of each
//! directory (a last period or space made `_`) while they fit in 68, as much of the file's name as fits, the SHA-1 of the
//! whole renamed path in hexadecimal, and the name's extension. A `dotencode` store also hashes a path whose own length
//! is past 120 before encoding it (`_pathencode`); one without (`_hybridencode`) hashes only what encoding made too long.
//! The SHA-1 is 7.2's `hashutil.sha1`, with collision detection ([`crate::node`]).
//!
//! **The fncache** lists every data revlog of an `fncache` store, one path a line after `decodedir`, the file ending in a
//! newline and holding no empty line (7.2's `fncache._load` and `_checkentries` abort on either).
//!
//! **Requirements** (`.hg/store/requires`, or `.hg/requires` without `share-safe`): which of these a store uses.
//! A store without `fncache`, and version 2's file index (`fileindex-v1`), are read by later parts.

use crate::Admission;
use crate::PastAdmission;
use crate::lines::lines;
use crate::node::{self, CollisionAttack};

/// Format: the longest store path 7.2 keeps unhashed (`_maxstorepathlen`).
const MAX_STORE_PATH: usize = 120;

/// Format: the bytes of each directory a hashed path keeps (`_dirprefixlen`).
const DIR_PREFIX: usize = 8;

/// Format: the longest run of directory prefixes a hashed path keeps (`_maxshortdirslen`, 8 * (8 + 1) - 4).
const MAX_SHORT_DIRS: usize = 68;

/// Format: the bytes of the prefix a hashed path drops (`data/` or `meta/`).
const PREFIX_LEN: usize = 5;

/// Format: a hashed path's directory.
const HASHED: &[u8] = b"dh/";

/// Format: `encodedir`'s three renamings, in 7.2's order.
const ENCODE_DIR: [(&[u8], &[u8]); 3] = [
  (b".hg/", b".hg.hg/"),
  (b".i/", b".i.hg/"),
  (b".d/", b".d.hg/"),
];

/// Format: `decodedir`'s three renamings, in 7.2's order.
const DECODE_DIR: [(&[u8], &[u8]); 3] = [
  (b".d.hg/", b".d/"),
  (b".i.hg/", b".i/"),
  (b".hg.hg/", b".hg/"),
];

/// Format: what `decodedir` looks for before renaming anything.
const DIR_MARK: &[u8] = b".hg/";

/// Format: Windows' reserved characters, escaped beside the controls and the bytes from 126 (`_reserved`).
const WINDOWS_RESERVED: &[u8] = b"\\:*?\"<>|";

/// Format: the first byte past the controls (0 to 31 are escaped).
const CONTROLS_END: u8 = 32;

/// Format: the first high byte escaped (`~`, 126, up to 255).
const FIRST_HIGH: u8 = 126;

/// Format: Windows' reserved names of three letters (`_winres3`).
const RESERVED_3: [&[u8]; 4] = [b"aux", b"con", b"prn", b"nul"];

/// Format: Windows' reserved names of three letters before a digit 1 to 9 (`_winres4`).
const RESERVED_4: [&[u8]; 2] = [b"com", b"lpt"];

/// Format: the lengths of those names, and where the digit or the escaped letter stands.
const SHORT_NAME: usize = 3;

/// Format: the letter of a reserved name 7.2 escapes (the third, index 2).
const ESCAPED_LETTER: usize = 2;

/// Format: the escape's mark.
const TILDE: u8 = b'~';

/// Format: the capital's mark, doubled for itself.
const UNDERSCORE: u8 = b'_';

/// Format: a path's separator, an extension's, and the space that is escaped like a period.
const SLASH: u8 = b'/';

/// Format: an extension's separator.
const PERIOD: u8 = b'.';

/// Format: the space escaped like a period.
const SPACE: u8 = b' ';

/// Format: lower-case hexadecimal's digits (`%02x`).
const HEX: &[u8; 16] = b"0123456789abcdef";

/// Format: a hexadecimal digit's bits.
const NIBBLE: u32 = 4;

/// Format: the low hexadecimal digit's bits of a byte.
const LOW_NIBBLE: u8 = 0x0f;

/// Format: the line end of the fncache.
const NEWLINE: u8 = b'\n';

/// Every occurrence of `from` in `haystack` replaced by `to`, left to right, as Python's `bytes.replace`.
fn replace_all(haystack: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
  let mut out = Vec::with_capacity(haystack.len());
  let mut rest = haystack;
  while !rest.is_empty() {
    if rest.starts_with(from) {
      out.extend_from_slice(to);
      rest = rest.get(from.len()..).unwrap_or_default();
    } else if let Some((&byte, after)) = rest.split_first() {
      out.push(byte);
      rest = after;
    }
  }
  out
}

/// A store path as 7.2's `encodedir` renames it.
pub fn encode_dir(path: &[u8]) -> Vec<u8> {
  ENCODE_DIR
    .iter()
    .fold(path.to_vec(), |renamed, (from, to)| replace_all(&renamed, from, to))
}

/// A store path as 7.2's `decodedir` renames it back.
pub fn decode_dir(path: &[u8]) -> Vec<u8> {
  if !path.windows(DIR_MARK.len()).any(|window| window == DIR_MARK) {
    return path.to_vec();
  }
  DECODE_DIR
    .iter()
    .fold(path.to_vec(), |renamed, (from, to)| replace_all(&renamed, from, to))
}

/// Whether 7.2 escapes `byte` as `~xx` (`_reserved`).
fn reserved(byte: u8) -> bool {
  byte < CONTROLS_END || byte >= FIRST_HIGH || WINDOWS_RESERVED.contains(&byte)
}

/// Appends `byte` as `~` and two lower-case hexadecimal digits.
fn escape(byte: u8, out: &mut Vec<u8>) {
  out.push(TILDE);
  for digit in [byte.checked_shr(NIBBLE).unwrap_or_default(), byte & LOW_NIBBLE] {
    out.push(HEX.get(usize::from(digit)).copied().unwrap_or(b'0'));
  }
}

/// `path` as 7.2's `_encodefname` encodes it: capitals as `_x`, `_` as `__`, reserved bytes as `~xx`.
fn encode_filename(path: &[u8]) -> Vec<u8> {
  let mut out = Vec::with_capacity(path.len());
  for &byte in path {
    if byte.is_ascii_uppercase() {
      out.extend_from_slice(&[UNDERSCORE, byte.to_ascii_lowercase()]);
    } else if byte == UNDERSCORE {
      out.extend_from_slice(&[UNDERSCORE, UNDERSCORE]);
    } else if reserved(byte) {
      escape(byte, &mut out);
    } else {
      out.push(byte);
    }
  }
  out
}

/// `path` as 7.2's `lowerencode` encodes it: capitals lowered, reserved bytes as `~xx`.
fn lower_encode(path: &[u8]) -> Vec<u8> {
  let mut out = Vec::with_capacity(path.len());
  for &byte in path {
    if reserved(byte) {
      escape(byte, &mut out);
    } else {
      out.push(byte.to_ascii_lowercase());
    }
  }
  out
}

/// Whether a segment is a name Windows reserves, alone or before a period (`_auxencode`).
fn windows_reserved(segment: &[u8]) -> bool {
  let name = segment
    .iter()
    .position(|byte| *byte == PERIOD)
    .unwrap_or(segment.len());
  let head = segment.get(..SHORT_NAME).unwrap_or_default();
  let digit = segment.get(SHORT_NAME).copied().unwrap_or_default();
  (name == SHORT_NAME && RESERVED_3.contains(&head))
    || (name == SHORT_NAME.saturating_add(1)
      && (b'1'..=b'9').contains(&digit)
      && RESERVED_4.contains(&head))
}

/// The segments of a path as 7.2's `_auxencode` encodes them.
fn aux_encode(segments: &mut [Vec<u8>], dotencode: bool) {
  for segment in segments.iter_mut() {
    let Some(&first) = segment.first() else {
      continue;
    };
    if dotencode && (first == PERIOD || first == SPACE) {
      let mut out = Vec::new();
      escape(first, &mut out);
      out.extend_from_slice(segment.get(1..).unwrap_or_default());
      *segment = out;
    } else if windows_reserved(segment) {
      let letter = segment.get(ESCAPED_LETTER).copied().unwrap_or_default();
      let mut out = segment.get(..ESCAPED_LETTER).unwrap_or_default().to_vec();
      escape(letter, &mut out);
      out.extend_from_slice(segment.get(SHORT_NAME..).unwrap_or_default());
      *segment = out;
    }
    if let Some(&last) = segment.last()
      && (last == PERIOD || last == SPACE)
    {
      segment.pop();
      escape(last, segment);
    }
  }
}

/// A path's segments, split at `/`.
fn segments(path: &[u8]) -> Vec<Vec<u8>> {
  path.split(|byte| *byte == SLASH).map(<[u8]>::to_vec).collect()
}

/// A file name's extension as Python's `os.path.splitext` finds it: from its last period, if a byte other than a period
/// comes before it.
fn extension(name: &[u8]) -> &[u8] {
  match name.iter().rposition(|byte| *byte == PERIOD) {
    Some(dot) if name.get(..dot).is_some_and(|before| before.iter().any(|byte| *byte != PERIOD)) => {
      name.get(dot..).unwrap_or_default()
    }
    _ => &[],
  }
}

/// A renamed path hashed as 7.2's `_hashencode` hashes it.
fn hash_encode(path: &[u8], dotencode: bool) -> Result<Vec<u8>, CollisionAttack> {
  let mut digest = Vec::new();
  node::write_hex(&node::sha1(path)?, &mut digest);
  let mut parts = segments(&lower_encode(path.get(PREFIX_LEN..).unwrap_or_default()));
  aux_encode(&mut parts, dotencode);
  let (basename, directories) = parts.split_last().unwrap_or((&Vec::new(), &[]));
  let mut dirs: Vec<u8> = Vec::new();
  for directory in directories {
    let mut prefix = directory.get(..DIR_PREFIX).unwrap_or(directory).to_vec();
    if let Some(last) = prefix.last_mut()
      && (*last == PERIOD || *last == SPACE)
    {
      *last = UNDERSCORE;
    }
    let joined = if dirs.is_empty() {
      prefix.len()
    } else {
      dirs.len().saturating_add(1).saturating_add(prefix.len())
    };
    if !dirs.is_empty() && joined > MAX_SHORT_DIRS {
      break;
    }
    if !dirs.is_empty() {
      dirs.push(SLASH);
    }
    dirs.extend_from_slice(&prefix);
  }
  if !dirs.is_empty() {
    dirs.push(SLASH);
  }
  let ext = extension(basename);
  let bare = HASHED
    .len()
    .saturating_add(dirs.len())
    .saturating_add(digest.len())
    .saturating_add(ext.len());
  let room = MAX_STORE_PATH.saturating_sub(bare);
  let filler = basename.get(..room).unwrap_or(basename);
  Ok([HASHED, &dirs, filler, &digest, ext].concat())
}

/// The store path a revlog's path (`data/` and a tracked path, then `.i` or `.d`) has in a store, as 7.2's
/// `_pathencode` (with `dotencode`) or `_hybridencode` (without) gives it.
pub fn encode_path(path: &[u8], dotencode: bool) -> Result<Vec<u8>, CollisionAttack> {
  let renamed = encode_dir(path);
  if dotencode && path.len() > MAX_STORE_PATH {
    return hash_encode(&renamed, true);
  }
  let mut parts = segments(&encode_filename(&renamed));
  aux_encode(&mut parts, dotencode);
  let encoded = parts.join(&SLASH);
  if encoded.len() > MAX_STORE_PATH {
    return hash_encode(&renamed, dotencode);
  }
  Ok(encoded)
}

/// Why an fncache was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FncacheRefused {
  /// A file not ending in a newline (7.2: "fncache does not ends with a newline").
  NoFinalNewline,
  /// An empty line (7.2: "invalid entry in fncache, line %d").
  EmptyEntry {
    /// The line's number, from 1.
    line: u64,
  },
  /// More than the caller's admission allows.
  PastAdmission(PastAdmission),
}

impl From<PastAdmission> for FncacheRefused {
  fn from(past: PastAdmission) -> Self {
    Self::PastAdmission(past)
  }
}

/// The revlog paths an fncache lists, each after `decodedir`, sorted and each once (7.2 reads them into a set). Their
/// bytes and entries are charged to `admission` first.
pub fn read_fncache(bytes: &[u8], admission: &mut Admission) -> Result<Vec<Vec<u8>>, FncacheRefused> {
  if bytes.is_empty() {
    return Ok(Vec::new());
  }
  if bytes.last() != Some(&NEWLINE) {
    return Err(FncacheRefused::NoFinalNewline);
  }
  let count = lines(bytes).count();
  let mut kept = *admission;
  kept.charge(crate::read::to_u64(bytes.len()))?;
  kept.charge_items::<Vec<u8>>(count)?;
  let decoded = decode_dir(bytes);
  let mut entries = Vec::with_capacity(count);
  for (index, line) in lines(&decoded).enumerate() {
    if line.is_empty() {
      return Err(FncacheRefused::EmptyEntry {
        line: crate::read::to_u64(index.saturating_add(1)),
      });
    }
    entries.push(line.to_vec());
  }
  entries.sort_unstable();
  entries.dedup();
  *admission = kept;
  Ok(entries)
}

/// The store features a repository's requirements name, as far as reading its store needs them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Layout {
  /// `store`: the revlogs under `.hg/store`, their names encoded.
  pub store: bool,
  /// `fncache`: the data revlogs listed in `fncache`.
  pub fncache: bool,
  /// `dotencode`: a first period or space escaped, a long raw path hashed.
  pub dotencode: bool,
  /// `exp-revlogv2.2`: revlogs behind dockets.
  pub revlog_v2: bool,
  /// `fileindex-v1`: the data revlogs listed by version 2's file index.
  pub file_index: bool,
}

/// The layout a `requires` file names, one requirement a line.
pub fn read_layout(requires: &[u8]) -> Layout {
  let mut layout = Layout::default();
  for line in lines(requires) {
    match line {
      b"store" => layout.store = true,
      b"fncache" => layout.fncache = true,
      b"dotencode" => layout.dotencode = true,
      b"exp-revlogv2.2" => layout.revlog_v2 = true,
      b"fileindex-v1" => layout.file_index = true,
      _ => {}
    }
  }
  layout
}

#[cfg(test)]
mod tests {
  use super::{
    FncacheRefused, aux_encode, decode_dir, encode_dir, encode_filename, encode_path, read_fncache, segments,
  };
  use crate::Admission;

  /// A-630 (7.2's doctests of `_encodedir` and `decodedir`): do rename store paths with `.i`, `.d` and `.hg`
  /// directories, and back. Expect 7.2's renamings, and each read back.
  #[test]
  fn directories_are_renamed_as_7_2_renames_them() {
    for (path, renamed) in [
      (&b"data/foo.i"[..], &b"data/foo.i"[..]),
      (b"data/foo.i/bla.i", b"data/foo.i.hg/bla.i"),
      (b"data/foo.i.hg/bla.i", b"data/foo.i.hg.hg/bla.i"),
      (b"data/foo.d/bar.hg/x.i", b"data/foo.d.hg/bar.hg.hg/x.i"),
    ] {
      assert_eq!(encode_dir(path), renamed);
      assert_eq!(decode_dir(renamed), path);
    }
  }

  /// A-630 (7.2's doctests of `_buildencodefun` and `encodefilename`): do encode names with capitals, reserved bytes
  /// and control and high bytes. Expect 7.2's encodings.
  #[test]
  fn names_are_encoded_as_7_2_encodes_them() {
    for (name, encoded) in [
      (&b"nothing/special.txt"[..], &b"nothing/special.txt"[..]),
      (b"HELLO", b"_h_e_l_l_o"),
      (b"hello:world?", b"hello~3aworld~3f"),
      (b"the\x07quick\xadshot", b"the~07quick~adshot"),
      (b"under_score", b"under__score"),
    ] {
      assert_eq!(encode_filename(name), encoded);
    }
    let path = encode_dir(b"foo.i/bar.d/bla.hg/hi:world?/HELLO");
    assert_eq!(encode_filename(&path), b"foo.i.hg/bar.d.hg/bla.hg.hg/hi~3aworld~3f/_h_e_l_l_o");
  }

  /// A-630 (7.2's doctests of `_auxencode`): do encode the segments of 7.2's two examples, with and without
  /// `dotencode`, and a name ending in a period and a space. Expect 7.2's segments.
  #[test]
  fn reserved_names_are_encoded_as_7_2_encodes_them() {
    let cases: [(&[u8], bool, &[&[u8]]); 3] = [
      (
        b".foo/aux.txt/txt.aux/con/prn/nul/foo.",
        true,
        &[b"~2efoo", b"au~78.txt", b"txt.aux", b"co~6e", b"pr~6e", b"nu~6c", b"foo~2e"],
      ),
      (
        b".com1com2/lpt9.lpt4.lpt1/conprn/com0/lpt0/foo.",
        false,
        &[b".com1com2", b"lp~749.lpt4.lpt1", b"conprn", b"com0", b"lpt0", b"foo~2e"],
      ),
      (b"foo. ", true, &[b"foo.~20"]),
    ];
    for (path, dotencode, encoded) in cases {
      let mut parts = segments(path);
      aux_encode(&mut parts, dotencode);
      assert_eq!(parts, encoded);
    }
  }

  /// A-630: do encode a path short raw but long once encoded, and one long raw, with and without `dotencode`. Expect
  /// each hashed under `dh/`, as long as 7.2's longest unhashed path at most, ending in the revlog's extension.
  #[test]
  fn long_paths_are_hashed() {
    let encoded_long = [b"data/UPPER_".as_slice(), &[b'Z'; 50], b".txt.i"].concat();
    let raw_long = [b"data/".as_slice(), &[b'a'; 130], b".txt.i"].concat();
    for dotencode in [true, false] {
      for path in [&encoded_long, &raw_long] {
        let hashed = encode_path(path, dotencode).unwrap();
        assert!(hashed.starts_with(b"dh/") && hashed.ends_with(b".i") && hashed.len() <= 120);
      }
    }
  }

  /// AC-HG.9, A-630: do read an fncache without its last newline, one with an empty line, and one under too small an
  /// admission. Expect each refused by its kind, the admission untouched.
  #[test]
  fn damaged_fncaches_are_refused() {
    assert_eq!(read_fncache(b"data/a.i", &mut Admission::new(1 << 20)), Err(FncacheRefused::NoFinalNewline));
    assert_eq!(
      read_fncache(b"data/a.i\n\ndata/b.i\n", &mut Admission::new(1 << 20)),
      Err(FncacheRefused::EmptyEntry { line: 2 })
    );
    let mut small = Admission::new(4);
    assert!(matches!(read_fncache(b"data/a.i\n", &mut small), Err(FncacheRefused::PastAdmission(_))));
    assert_eq!(small.left(), 4);
  }
}
