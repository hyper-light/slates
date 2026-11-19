use proptest::prelude::*;

use super::{is_valid, tag_name_is_valid};

/// refs.c's `refname_disposition` (v2.50.1), byte by byte for ASCII; every byte from 0x80 up is 0. 0: allowed; 1: ends
/// the component (`/`, and the NUL ending a C string); 2: `.`, refused after `.`; 3: `{`, refused after `@`; 4:
/// refused; 5: `*`, refused unless the name is a refspec pattern.
const REFNAME_DISPOSITION: [u8; 128] = [
  1, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, //
  4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, //
  4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 5, 0, 0, 0, 2, 1, //
  0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 0, 4, //
  0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
  0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 4, 4, 0, 4, 0, //
  0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
  0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 4, 4, //
];

/// git's `check_refname_component` with no flags, transcribed: the component's length, 0 for an empty one, or `None`
/// where git returns -1.
fn git_check_refname_component(refname: &[u8]) -> Option<usize> {
  let mut last = 0u8;
  let mut at = 0usize;
  loop {
    let ch = refname.get(at).copied().unwrap_or(0);
    match REFNAME_DISPOSITION
      .get(usize::from(ch))
      .copied()
      .unwrap_or(0)
    {
      1 => break,
      2 if last == b'.' => return None,
      3 if last == b'@' => return None,
      4 | 5 => return None,
      _ => {}
    }
    last = ch;
    at += 1;
  }
  if at == 0 {
    return Some(0);
  }
  if refname[0] == b'.' {
    return None;
  }
  if at >= b".lock".len() && &refname[at - b".lock".len()..at] == b".lock" {
    return None;
  }
  Some(at)
}

/// git's `check_or_sanitize_refname` with no flags and nothing to sanitize, transcribed: whether `refname` passes.
/// git reads a C string, so a name holding a NUL is none git can hold: refused here.
fn git_check_refname_format(refname: &[u8]) -> bool {
  if refname.contains(&0) || refname == b"@" {
    return false;
  }
  let mut rest = refname;
  let mut components = 0;
  let last_len = loop {
    match git_check_refname_component(rest) {
      None | Some(0) => return false,
      Some(len) => {
        components += 1;
        if len == rest.len() {
          break len;
        }
        rest = &rest[len + 1..];
      }
    }
  };
  rest[last_len - 1] != b'.' && components >= 2
}

/// A-582 part 2, kept by A-615 (gitprotocol-common's list, and the two rules git's `check_refname_format` adds): do
/// check whole names that pass and one breaking each rule. Expect: `HEAD` and names under `refs/` pass, bytes past
/// ASCII too; every broken rule refused.
#[test]
fn a_ref_name_is_head_or_under_refs_by_gits_own_rules() {
  for name in [
    &b"HEAD"[..],
    b"refs/heads/main",
    b"refs/tags/v1.0",
    b"refs/heads/a.b/c-d_e",
    b"refs/for/main",
    "refs/heads/caf\u{e9}".as_bytes(),
    b"refs/heads/x.lockx",
    b"refs/heads/@",
  ] {
    assert!(is_valid(name), "{name:?}");
  }
  for name in [
    &b""[..],
    b"head",
    b"main",
    b"refs",
    b"refs/",
    b"refs//x",
    b"refs/x/",
    b"refs/.x",
    b"refs/x/.y",
    b"refs/x.",
    b"refs/a..b",
    b"refs/x.lock",
    b"refs/x.lock/y",
    b"refs/a@{b",
    b"refs/a b",
    b"refs/a:b",
    b"refs/a?b",
    b"refs/a[b",
    b"refs/a\\b",
    b"refs/a^b",
    b"refs/a~b",
    b"refs/a*b",
    b"refs/a\tb",
    b"refs/a\x7fb",
    b"refs/a\0b",
  ] {
    assert!(!is_valid(name), "{name:?}");
  }
}

/// A-590's question, asked of the shared rule (A-615; `fsck.c`'s `badTagName`): do check tag names as a tag object's
/// `tag` line holds them, without `refs/tags/`. Expect: plain, dotted, nested and non-ASCII names and `@` alone pass,
/// since git checks `refs/tags/@`; each broken rule refused, a NUL among them.
#[test]
fn a_tag_name_is_checked_as_git_checks_refs_tags_and_the_name() {
  for name in [
    &b"v1.0"[..],
    b"release/2026-11",
    b"@",
    b"a@b",
    "caf\u{e9}".as_bytes(),
    b"x.lockx",
  ] {
    assert!(tag_name_is_valid(name), "{name:?}");
  }
  for name in [
    &b""[..],
    b"/v1",
    b"v1/",
    b"a//b",
    b".v1",
    b"v1.",
    b"a..b",
    b"v1.lock",
    b"a@{b",
    b"v 1",
    b"v~1",
    b"v^1",
    b"v:1",
    b"v?1",
    b"v[1",
    b"v\\1",
    b"v*1",
    b"v\x011",
    b"v\x7f1",
    b"v1\0",
  ] {
    assert!(!tag_name_is_valid(name), "{name:?}");
  }
}

/// A byte that reaches every disposition of `refname_disposition`, and plain ones.
fn any_name_byte() -> impl Strategy<Value = u8> {
  prop_oneof![
    Just(b'a'),
    Just(b'l'),
    Just(b'o'),
    Just(b'c'),
    Just(b'k'),
    Just(b'.'),
    Just(b'/'),
    Just(b'@'),
    Just(b'{'),
    Just(b'*'),
    Just(b' '),
    Just(b'~'),
    Just(0u8),
    Just(0x7fu8),
    Just(0xc3u8),
    any::<u8>(),
  ]
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

  /// A-615: do check generated names, bare and under `refs/`, `refs/heads/` and `HEAD`, through both questions and
  /// through refs.c's `check_refname_format` transcribed byte for byte. Expect: a whole name passes exactly when it is
  /// `HEAD` or begins `refs/` and git passes it; a tag name exactly when git passes `refs/tags/` and the name.
  #[test]
  fn both_questions_answer_as_gits_own_check_answers(
    tail in proptest::collection::vec(any_name_byte(), 0..14),
    prefix in 0usize..4,
  ) {
    let prefix: &[u8] = [&b""[..], b"refs/", b"refs/heads/", b"HEAD"][prefix];
    let name = [prefix, &tail[..]].concat();
    let expected = name == b"HEAD" || (name.starts_with(b"refs/") && git_check_refname_format(&name));
    prop_assert_eq!(is_valid(&name), expected, "{name:?}");
    let under_tags = [&b"refs/tags/"[..], &tail[..]].concat();
    prop_assert_eq!(tag_name_is_valid(&tail), git_check_refname_format(&under_tags), "{tail:?}");
  }
}
