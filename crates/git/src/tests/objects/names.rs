//! git's name rules against git's own code: the `.git` check, read through a tree's check, held to a line-for-line
//! transliteration of utf8.c's `pick_one_utf8_char`, `next_hfs_char` and `is_hfs_dot_generic` and path.c's
//! `is_ntfs_dotgit`, over every byte sequence that can follow or sit inside `.git`.

use super::super::tree::{self, Entry, Mode};
use super::super::{FsckId, Strictness};
use super::ample;
use crate::object_name::ObjectFormat;

/// git's `pick_one_utf8_char` (utf8.c) over a C string `s` (its NUL at the end): the code point and how many bytes it
/// takes, or none for malformed UTF-8. Transliterated, not reimplemented.
fn git_pick_one_utf8_char(s: &[u8]) -> Option<(u32, usize)> {
  let at = |i: usize| u32::from(s.get(i).copied().unwrap_or(0));
  let (s0, s1, s2, s3) = (at(0), at(1), at(2), at(3));
  if s0 < 0x80 {
    Some((s0, 1))
  } else if (s0 & 0xe0) == 0xc0 {
    if (s1 & 0xc0) != 0x80 || (s0 & 0xfe) == 0xc0 {
      return None;
    }
    Some((((s0 & 0x1f) << 6) | (s1 & 0x3f), 2))
  } else if (s0 & 0xf0) == 0xe0 {
    if (s1 & 0xc0) != 0x80
      || (s2 & 0xc0) != 0x80
      || (s0 == 0xe0 && (s1 & 0xe0) == 0x80)
      || (s0 == 0xed && (s1 & 0xe0) == 0xa0)
      || (s0 == 0xef && s1 == 0xbf && (s2 & 0xfe) == 0xbe)
    {
      return None;
    }
    Some((((s0 & 0x0f) << 12) | ((s1 & 0x3f) << 6) | (s2 & 0x3f), 3))
  } else if (s0 & 0xf8) == 0xf0 {
    if (s1 & 0xc0) != 0x80
      || (s2 & 0xc0) != 0x80
      || (s3 & 0xc0) != 0x80
      || (s0 == 0xf0 && (s1 & 0xf0) == 0x80)
      || (s0 == 0xf4 && s1 > 0x8f)
      || s0 > 0xf4
    {
      return None;
    }
    Some((
      ((s0 & 0x07) << 18) | ((s1 & 0x3f) << 12) | ((s2 & 0x3f) << 6) | (s3 & 0x3f),
      4,
    ))
  } else {
    None
  }
}

/// git's `next_hfs_char`: the next code point HFS+ does not ignore, and where the rest begins; 0 and the end of the
/// walk on malformed UTF-8 (`*in = NULL`).
fn git_next_hfs_char(s: &[u8]) -> (u32, Option<&[u8]>) {
  let mut rest = s;
  loop {
    let Some((out, taken)) = git_pick_one_utf8_char(rest) else {
      return (0, None);
    };
    rest = rest.get(taken..).unwrap_or_default();
    match out {
      0x200c | 0x200d | 0x200e | 0x200f | 0x202a | 0x202b | 0x202c | 0x202d | 0x202e | 0x206a
      | 0x206b | 0x206c | 0x206d | 0x206e | 0x206f | 0xfeff => continue,
      _ => return (out, Some(rest)),
    }
  }
}

/// git's `is_hfs_dot_generic(path, needle)` on Linux and macOS (`is_dir_sep` is `/`).
fn git_is_hfs_dot(path: &[u8], needle: &[u8]) -> bool {
  let (c, mut rest) = git_next_hfs_char(path);
  if c != u32::from(b'.') {
    return false;
  }
  for want in needle {
    let (c, next) = match rest {
      Some(at) => git_next_hfs_char(at),
      None => return false,
    };
    rest = next;
    if c > 127 || u8::try_from(c).unwrap().to_ascii_lowercase() != *want {
      return false;
    }
  }
  let c = match rest {
    Some(at) => git_next_hfs_char(at).0,
    None => return true,
  };
  c == 0 || c == u32::from(b'/')
}

/// git's `is_ntfs_dotgit(name)` (path.c).
fn git_is_ntfs_dotgit(name: &[u8]) -> bool {
  let at = |i: usize| name.get(i).copied().unwrap_or(0);
  let lower = |i: usize| at(i).to_ascii_lowercase();
  let mut i = if at(0) == b'.' {
    if lower(1) != b'g' || lower(2) != b'i' || lower(3) != b't' {
      return false;
    }
    4
  } else if lower(0) == b'g' {
    if lower(1) != b'i' || lower(2) != b't' || at(3) != b'~' || at(4) != b'1' {
      return false;
    }
    5
  } else {
    return false;
  };
  loop {
    let c = at(i);
    i += 1;
    if c == 0 || c == b'/' || c == b'\\' || c == b':' {
      return true;
    }
    if c != b'.' && c != b' ' {
      return false;
    }
  }
}

/// git's verdict on a tree entry named `name`: `is_hfs_dotgit || is_ntfs_dotgit` on the name and `is_ntfs_dotgit` on
/// the rest after each backslash (fsck.c's `fsck_tree`).
fn git_has_dotgit(name: &[u8]) -> bool {
  let mut string = name.to_vec();
  string.push(0);
  let mut found = git_is_hfs_dot(&string, b"git") || git_is_ntfs_dotgit(&string);
  for (index, byte) in string.iter().enumerate() {
    if *byte == b'\\' {
      found |= git_is_ntfs_dotgit(&string[index + 1..]);
    }
  }
  found
}

/// slates' verdict on a tree whose one entry, a file, is named `name`: whether its check reports `hasDotgit`.
fn has_dotgit(name: &[u8]) -> bool {
  let id = [0x44; 20];
  let mut bytes = Vec::new();
  Entry::new(Mode::File, name, &id).write(&mut bytes);
  let checked = tree::fsck(
    &bytes,
    ObjectFormat::Sha1,
    Strictness::Default,
    &mut ample(),
  )
  .unwrap();
  checked.report.findings().contains(&FsckId::HasDotgit)
}

/// Every byte that may follow a lead byte in the sweep: each continuation byte, and one on either side of them.
fn second_bytes() -> impl Iterator<Item = u8> + Clone {
  (0x7f..=0xc0).chain([b'a', b'.', b' ', 0xff])
}

/// A-590 (utf8.c's `pick_one_utf8_char`, `next_hfs_char`, `is_hfs_dot_generic`; path.c's `is_ntfs_dotgit`). Do: name a
/// file `.git` followed by every lead byte from 0x80 and every byte that can follow it, two and three bytes deep, and
/// four for the four-byte leads; and `.`, the same bytes, then `git`. Expect: `hasDotgit` exactly where git's own code,
/// transliterated, finds `.git`: after `.git`, a malformed sequence (overlong, surrogate, past U+10FFFF, cut, or U+FFFE
/// and U+FFFF, which UTF-8 allows and git refuses) or a code point HFS+ ignores; before `git`, an ignored one alone.
#[test]
fn a_dotgit_name_is_judged_as_gits_utf8_reader_judges_it() {
  let mut names = 0_usize;
  let mut found = 0_usize;
  let mut judge = |sequence: &[u8]| {
    let suffixed = [&b".git"[..], sequence].concat();
    let infixed = [&b"."[..], sequence, &b"git"[..]].concat();
    for name in [suffixed, infixed] {
      let expected = git_has_dotgit(&name);
      assert_eq!(has_dotgit(&name), expected, "{name:x?}");
      names += 1;
      found += usize::from(expected);
    }
  };
  for lead in 0x80..=0xff_u8 {
    for second in second_bytes() {
      judge(&[lead, second]);
      // A third byte belongs to the first character only after a three- or four-byte lead (and the bytes past them).
      if lead < 0xe0 {
        continue;
      }
      for third in second_bytes() {
        judge(&[lead, second, third]);
        if (0xf0..=0xf7).contains(&lead) {
          for fourth in [0x7f, 0x80, 0xbf, 0xc0] {
            judge(&[lead, second, third, fourth]);
          }
        }
      }
    }
  }
  // 128 leads by 70 seconds; 32 leads by 70 by 70; 8 leads by 70 by 70 by 4; each placed after and inside `.git`.
  assert_eq!(names, 2 * (128 * 70 + 32 * 70 * 70 + 8 * 70 * 70 * 4));
  assert!(
    found > 0 && found < names,
    "both answers met: {found} of {names}"
  );
}

/// A-590 (path.c's `is_ntfs_dotgit`, after each backslash in fsck.c). Do: name a file with `.git` or `git~1` after
/// backslashes and other bytes, at every place in a short name. Expect: `hasDotgit` exactly where git's own code finds
/// it.
#[test]
fn a_dotgit_after_a_backslash_is_judged_as_git_judges_it() {
  let parts: [&[u8]; 8] = [b"\\", b".git", b"git~1", b"x", b" ", b".", b":", b"/"];
  let mut count = 0;
  for first in parts {
    for second in parts {
      for third in parts {
        let name = [first, second, third].concat();
        assert_eq!(
          has_dotgit(&name),
          git_has_dotgit(&name),
          "{:?}",
          String::from_utf8_lossy(&name)
        );
        count += 1;
      }
    }
  }
  assert_eq!(count, 512);
}
