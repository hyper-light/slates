//! A-640 part 4: a sparse filter's patterns, read and asked as git's `dir.c` reads and asks them (git v2.50.1). Each
//! hand-written case was traced through the functions it names; the property holds every generated list and path to
//! those functions transcribed in their own shape.

use proptest::prelude::*;
use slates_vcs::wildmatch::{Glob, is_glob_special};

use super::{MOST_BYTES, Patterns, Refused, Verdict};
use crate::Admission;

/// Shape: room enough for every list these tests read, 2^30 bytes.
const ROOM: u64 = 1 << 30;

/// The bytes a generated line is made of: git's glob specials, `/`, `!`, `#`, a space, a carriage return and a NUL among
/// two letters and a dot.
const LINE_BYTES: &[u8] = b"ab.*?[]\\/!# \r\0";

/// The bytes a generated path segment is made of: never `/` or NUL, which no segment holds.
const SEGMENT_BYTES: &[u8] = b"ab.*?[\\!# ";

/// The list `blob` holds, read with room enough.
fn read(blob: &[u8]) -> Patterns {
  Patterns::read(blob, &mut Admission::new(ROOM)).unwrap()
}

/// Asks the list `blob` holds of each `(path, tree)`, expecting each verdict.
fn check(blob: &[u8], cases: &[(&[u8], bool, Verdict)]) {
  let patterns = read(blob);
  for (path, tree, expected) in cases {
    let found = patterns.decide(path, *tree);
    assert_eq!(found, *expected, "blob {blob:?} path {path:?} tree {tree}");
  }
}

/// A-640 part 4 (`git-sparse-checkout`'s non-cone example, `/*` then `!/*/`; `last_matching_pattern_from_list` reads
/// from the list's end). Do ask it of the root's files and trees and of a deeper file, then ask two lists of one
/// pattern and its negation in both orders. Expect: a root file matched, a root tree not (the later negative pattern
/// matches trees alone), a blob named like a tree matched, a deeper file and the root itself undecided (`*` holds no `/`
/// and a slashed pattern never matches the empty path); and the later of `a` and `!a` deciding.
#[test]
fn the_last_pattern_that_matches_decides() {
  use Verdict::{Matched, NotMatched, Undecided};
  check(
    b"/*\n!/*/\n",
    &[
      (b"README", false, Matched),
      (b"docs", true, NotMatched),
      (b"docs", false, Matched),
      (b"docs/a.md", false, Undecided),
      (b"", true, Undecided),
    ],
  );
  check(b"a\n!a\n", &[(b"x/a", false, NotMatched)]);
  check(b"!a\na\n", &[(b"x/a", false, Matched)]);
}

/// A-640 part 4 (`match_basename`: equal bytes, `*` then a literal end, or the glob). Do ask a literal name, an ending
/// and a glob of segments anywhere. Expect: each matches the last segment at any depth, trees as well as blobs, and
/// nothing else; `*.c` matches `.c` itself (its end is all of it).
#[test]
fn a_pattern_without_a_slash_matches_a_last_segment_anywhere() {
  use Verdict::{Matched, Undecided};
  check(
    b"Makefile\n*.c\nf?o\n",
    &[
      (b"Makefile", false, Matched),
      (b"src/lib/Makefile", false, Matched),
      (b"src/Makefile.am", false, Undecided),
      (b"src/main.c", false, Matched),
      (b"main.c", true, Matched),
      (b".c", false, Matched),
      (b"src/main.h", false, Undecided),
      (b"a/foo", false, Matched),
      (b"a/fo/o", false, Undecided),
    ],
  );
}

/// A-640 part 4 (`match_pathname` with the root as base: a leading `/` dropped, the literal bytes compared, the rest
/// globbed with `WM_PATHNAME`). Do ask literal, starred and doubly starred paths, at the root and below it. Expect: each
/// matches from the root alone; `*` stops at a `/`; `**/` matches no directory or several; a literal longer than the
/// path matches nothing.
#[test]
fn a_pattern_with_a_slash_matches_the_whole_path_from_the_root() {
  use Verdict::{Matched, Undecided};
  check(
    b"docs/api\n/src/*.rs\nlib/**/mod.rs\n/ab*\n",
    &[
      (b"docs/api", true, Matched),
      (b"x/docs/api", true, Undecided),
      (b"src/main.rs", false, Matched),
      (b"src/net/main.rs", false, Undecided),
      (b"lib/mod.rs", false, Matched),
      (b"lib/a/b/mod.rs", false, Matched),
      (b"abc", false, Matched),
      (b"x/abc", false, Undecided),
      (b"a", false, Undecided),
    ],
  );
}

/// A-640 part 4 (`match_pathname` globs what follows the literal bytes as a pattern of its own). Do ask `ab**/c` of
/// paths below `abx`. Expect: matched, as git matches them: the rest `**/c` stands at a pattern's start, where `**/` is
/// any directories, though in the whole pattern the `**` follows a `b` and would hold no `/`.
#[test]
fn a_slashed_patterns_rest_is_globbed_on_its_own_as_git_globs_it() {
  use Verdict::Matched;
  check(
    b"ab**/c\n",
    &[(b"abx/y/c", false, Matched), (b"abx/c", false, Matched)],
  );
}

/// A-640 part 4 (`PATTERN_FLAG_MUSTBEDIR`). Do ask a name and an ending with a trailing `/` of trees and blobs, then
/// `/` alone of the root and a tree. Expect: trees matched, blobs undecided; `/` alone, an empty pattern that must be a
/// tree, matches the root tree, whose name is empty, and nothing else.
#[test]
fn a_trailing_slash_matches_trees_alone() {
  use Verdict::{Matched, Undecided};
  check(
    b"build/\n*.d/\n",
    &[
      (b"a/build", true, Matched),
      (b"a/build", false, Undecided),
      (b"x.d", true, Matched),
      (b"x.d", false, Undecided),
    ],
  );
  check(b"/\n", &[(b"", true, Matched), (b"a", true, Undecided)]);
}

/// A-640 part 4 (`add_patterns_from_buffer`, `trim_trailing_spaces`, `do_read_blob`). Do read a blob with a byte-order
/// mark, a CRLF line, a comment, an empty line, trailing spaces, an escaped space, a NUL inside a line and a last line
/// without a newline; then an escaped `#`, a line of a carriage return alone and a line of spaces. Expect: each line's
/// pattern as git keeps it, the comment none; the escaped `#` a pattern; the last two the empty pattern, which matches
/// the root tree alone.
#[test]
fn a_blob_is_read_line_by_line_as_git_reads_it() {
  use Verdict::{Matched, Undecided};
  check(
    b"\xef\xbb\xbfa\r\n#b\n\nc  \nd\\ \ne\0f\ng",
    &[
      (b"a", false, Matched),
      (b"#b", false, Undecided),
      (b"b", false, Undecided),
      (b"c", false, Matched),
      (b"d ", false, Matched),
      (b"d", false, Undecided),
      (b"e", false, Matched),
      (b"g", false, Matched),
    ],
  );
  check(b"\\#b\n", &[(b"#b", false, Matched)]);
  check(b"\r\n", &[(b"", true, Matched), (b"a", true, Undecided)]);
  check(b"   \n", &[(b"", true, Matched), (b"a", false, Undecided)]);
}

/// A-640 part 4 (`last_matching_pattern_from_list` with no pattern answers undecided). Do read an empty blob, a
/// byte-order mark alone, a comment and empty lines. Expect: every path undecided, the root's too.
#[test]
fn a_list_without_patterns_leaves_every_path_undecided() {
  use Verdict::Undecided;
  for blob in [&b""[..], b"\xef\xbb\xbf", b"# only a comment\n", b"\n\n"] {
    check(
      blob,
      &[
        (b"", true, Undecided),
        (b"a", false, Undecided),
        (b"a/b", true, Undecided),
      ],
    );
  }
}

/// A-640 part 4 (`add_patterns_from_blob_to_list` refuses past `PATTERN_MAX_FILE_SIZE`, the newline `do_read_blob`
/// adds counted). Do read a blob of exactly the limit, one a byte past it, and one of the limit whose last byte is no
/// newline. Expect: the first read; the others refused, each counted one past the limit.
#[test]
fn a_blob_past_gits_limit_is_refused() {
  let limit = usize::try_from(MOST_BYTES).unwrap();
  let mut blob = Vec::with_capacity(limit + 1);
  blob.resize(limit, b'\n');
  assert!(Patterns::read(&blob, &mut Admission::new(ROOM)).is_ok());
  let past = Err(Refused::TooLarge {
    bytes: MOST_BYTES + 1,
  });
  blob.push(b'\n');
  assert_eq!(Patterns::read(&blob, &mut Admission::new(ROOM)), past);
  blob.truncate(limit);
  *blob.last_mut().unwrap() = b'#';
  assert_eq!(Patterns::read(&blob, &mut Admission::new(ROOM)), past);
}

/// A-640 part 4 (§4.2: what a request keeps is charged to its admission). Do read a list of three patterns with room
/// enough, then with one byte less than it took. Expect: the first takes some room; the second is refused past its
/// admission.
#[test]
fn a_list_past_its_admission_is_refused() {
  let blob = b"a\n*.c\nsrc/*.rs\n";
  let mut admission = Admission::new(ROOM);
  Patterns::read(blob, &mut admission).unwrap();
  let used = ROOM - admission.left();
  assert!(used > 0);
  let refused = Patterns::read(blob, &mut Admission::new(used - 1));
  assert!(
    matches!(refused, Err(Refused::PastAdmission(_))),
    "{refused:?}"
  );
}

/// git's `add_patterns_from_blob_to_list` and `add_patterns_from_buffer` (`dir.c` 1185-1246, v2.50.1), transcribed:
/// the entries git keeps, each a C string as `trim_trailing_spaces` leaves it.
fn git_entries(blob: &[u8]) -> Vec<Vec<u8>> {
  if blob.is_empty() {
    return Vec::new();
  }
  let mut buf = blob.to_vec();
  if buf[buf.len() - 1] != b'\n' {
    buf.push(b'\n');
  }
  let start = if buf.starts_with(b"\xef\xbb\xbf") {
    3
  } else {
    0
  };
  let mut entries = Vec::new();
  let mut entry = start;
  for i in start..buf.len() {
    if buf[i] == b'\n' {
      if entry != i && buf[entry] != b'#' {
        let end = i - usize::from(i > start && buf[i - 1] == b'\r');
        buf[end] = 0;
        let string: Vec<u8> = buf[entry..]
          .iter()
          .take_while(|byte| **byte != 0)
          .copied()
          .collect();
        entries.push(git_trim_trailing_spaces(string));
      }
      entry = i + 1;
    }
  }
  entries
}

/// git's `trim_trailing_spaces` (`dir.c` 988-1009), transcribed over a C string's bytes.
fn git_trim_trailing_spaces(mut string: Vec<u8>) -> Vec<u8> {
  let mut last_space = None;
  let mut p = 0;
  while p < string.len() {
    match string[p] {
      b' ' => {
        if last_space.is_none() {
          last_space = Some(p);
        }
      }
      b'\\' => {
        p += 1;
        if p == string.len() {
          return string;
        }
        last_space = None;
      }
      _ => last_space = None,
    }
    p += 1;
  }
  if let Some(cut) = last_space {
    string.truncate(cut);
  }
  string
}

/// git's `simple_length` (`dir.c` 639-649).
fn git_simple_length(string: &[u8]) -> usize {
  string
    .iter()
    .position(|byte| is_glob_special(*byte))
    .unwrap_or(string.len())
}

/// git's `match_basename` (`dir.c` 1287-1309), `fnmatch_icase_mem` being `wildmatch`.
fn git_match_basename(basename: &[u8], pattern: &[u8], prefix: usize, endswith: bool) -> bool {
  let patternlen = pattern.len();
  if prefix == patternlen {
    patternlen == basename.len() && pattern == basename
  } else if endswith {
    patternlen - 1 <= basename.len()
      && basename[basename.len() - (patternlen - 1)..] == pattern[1..]
  } else {
    Glob::new(pattern).matches(basename)
  }
}

/// git's `match_pathname` (`dir.c` 1311-1367) with an empty base, as a filter's patterns have.
fn git_match_pathname(pathname: &[u8], pattern: &[u8], prefix: usize) -> bool {
  let (mut pattern, mut prefix) = (pattern, prefix);
  if pattern.first() == Some(&b'/') {
    pattern = &pattern[1..];
    prefix -= 1;
  }
  if pathname.is_empty() {
    return false;
  }
  let mut name = pathname;
  if prefix > 0 {
    if prefix > name.len() || pattern[..prefix] != name[..prefix] {
      return false;
    }
    pattern = &pattern[prefix..];
    name = &name[prefix..];
    if pattern.is_empty() && name.is_empty() {
      return true;
    }
  }
  Glob::new(pattern).matches(name)
}

/// git's `last_matching_pattern_from_list` and `path_matches_pattern_list` outside cone mode (`dir.c` 1375-1502), with
/// `parse_path_pattern` (656-692) run on each entry as it is asked.
fn git_decide(entries: &[Vec<u8>], pathname: &[u8], tree: bool) -> Verdict {
  let basename = &pathname[pathname
    .iter()
    .rposition(|byte| *byte == b'/')
    .map_or(0, |at| at + 1)..];
  for entry in entries.iter().rev() {
    let negative = entry.first() == Some(&b'!');
    let p = &entry[usize::from(negative)..];
    let mustbedir = p.last() == Some(&b'/');
    let len = p.len() - usize::from(mustbedir);
    let nodir = !p[..len].contains(&b'/');
    let nowildcardlen = git_simple_length(p).min(len);
    let endswith = p.first() == Some(&b'*') && git_simple_length(&p[1..]) == p.len() - 1;
    if mustbedir && !tree {
      continue;
    }
    let matched = if nodir {
      git_match_basename(basename, &p[..len], nowildcardlen, endswith)
    } else {
      git_match_pathname(pathname, &p[..len], nowildcardlen)
    };
    if matched {
      return if negative {
        Verdict::NotMatched
      } else {
        Verdict::Matched
      };
    }
  }
  Verdict::Undecided
}

/// A blob of up to five lines of [`LINE_BYTES`], a byte-order mark first or not, a last newline or not.
fn any_blob() -> impl Strategy<Value = Vec<u8>> {
  (
    prop::collection::vec(prop::collection::vec(0..LINE_BYTES.len(), 0..6), 0..6),
    any::<bool>(),
    any::<bool>(),
  )
    .prop_map(|(lines, mark, newline)| {
      let lines: Vec<Vec<u8>> = lines
        .iter()
        .map(|picks| picks.iter().map(|at| LINE_BYTES[*at]).collect())
        .collect();
      let mut blob = if mark {
        b"\xef\xbb\xbf".to_vec()
      } else {
        Vec::new()
      };
      blob.extend(lines.join(&b'\n'));
      if newline {
        blob.push(b'\n');
      }
      blob
    })
}

/// A path of up to three segments of [`SEGMENT_BYTES`], and whether it is a tree (the root, the empty path, always is).
fn any_path() -> impl Strategy<Value = (Vec<u8>, bool)> {
  (
    prop::collection::vec(prop::collection::vec(0..SEGMENT_BYTES.len(), 1..4), 0..4),
    any::<bool>(),
  )
    .prop_map(|(segments, tree)| {
      let segments: Vec<Vec<u8>> = segments
        .iter()
        .map(|picks| picks.iter().map(|at| SEGMENT_BYTES[*at]).collect())
        .collect();
      let path = segments.join(&b'/');
      let tree = tree || path.is_empty();
      (path, tree)
    })
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

  /// A-640 part 4: do read generated blobs (every glob special, `/`, `!`, `#`, spaces, carriage returns, NULs, a
  /// byte-order mark, a last newline or none) and ask each list of generated paths, trees and blobs. Expect: every
  /// verdict that of git's `dir.c` transcribed above, reading the blob and parsing each pattern as git does.
  #[test]
  fn every_path_is_decided_as_gits_dir_c_decides_it(
    blob in any_blob(),
    paths in prop::collection::vec(any_path(), 1..8),
  ) {
    let patterns = Patterns::read(&blob, &mut Admission::new(ROOM))
      .map_err(|refused| TestCaseError::fail(format!("{refused:?}")))?;
    let entries = git_entries(&blob);
    for (path, tree) in &paths {
      prop_assert_eq!(
        patterns.decide(path, *tree),
        git_decide(&entries, path, *tree),
        "blob {:?} path {:?} tree {}",
        blob,
        path,
        tree
      );
    }
  }
}
