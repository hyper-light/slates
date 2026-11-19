//! A-610: the pointer file, held to git-lfs v3.8.1's own pointer tests (`lfs/pointer_test.go` at a662a6dbf, its
//! examples transcribed here), its specification's examples (`docs/spec.md`, read from the fixture), and the rules
//! git-lfs's reader follows that its tests leave out.

use proptest::prelude::*;

use super::{
  CUTOFF, Extension, Key, MAX_EXTENSIONS, Pointer, Refusal, Version, WriteRefusal, empty, read,
  write,
};
use crate::lfs::Oid;
use crate::lfs::tests::SPEC_MD;
use crate::object_name::{Hasher, ObjectFormat};

/// The oid of pointer_test.go's examples.
const OID: &str = "4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393";
/// Its extensions' oids.
const FFF: &str = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
/// See [`FFF`].
const BBB: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
/// See [`FFF`].
const AAA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// An oid from its digits.
fn oid(hex: &str) -> Oid {
  Oid::from_hex(hex.as_bytes()).unwrap()
}

/// pointer_test.go's pointer of size 12345 with the extensions `foo`, `bar` and `baz` at priorities 0 to 2.
fn with_extensions() -> Pointer<'static> {
  let mut extensions = [None; MAX_EXTENSIONS];
  extensions[0] = Some(Extension {
    name: "foo",
    oid: oid(FFF),
  });
  extensions[1] = Some(Extension {
    name: "bar",
    oid: oid(BBB),
  });
  extensions[2] = Some(Extension {
    name: "baz",
    oid: oid(AAA),
  });
  Pointer {
    oid: oid(OID),
    size: 12_345,
    extensions,
  }
}

/// pointer_test.go's plain pointer of size 12345.
fn plain() -> Pointer<'static> {
  Pointer {
    oid: oid(OID),
    size: 12_345,
    extensions: [None; MAX_EXTENSIONS],
  }
}

/// pointer_test.go's `TestDecodeCanonical` examples that are canonical: the standard pointer, the one with extensions,
/// and the empty file.
fn canonical_examples() -> [(String, Pointer<'static>); 3] {
  let standard =
    format!("version https://git-lfs.github.com/spec/v1\noid sha256:{OID}\nsize 12345\n");
  let extended = format!(
    "version https://git-lfs.github.com/spec/v1\next-0-foo sha256:{FFF}\next-1-bar sha256:{BBB}\next-2-baz sha256:{AAA}\noid sha256:{OID}\nsize 12345\n"
  );
  [
    (standard, plain()),
    (extended, with_extensions()),
    (String::new(), empty()),
  ]
}

/// A-610, golden: do read pointer_test.go's canonical examples, then write what was read. Expect each read as its
/// pointer, at version 1, canonical, and written back to its bytes.
#[test]
fn git_lfs_own_canonical_examples_read_and_write_back() {
  for (text, pointer) in canonical_examples() {
    let read = read(text.as_bytes()).unwrap();
    assert_eq!(
      (read.pointer, read.version, read.canonical),
      (pointer, Version::V1, true),
      "{text:?}"
    );
    let mut out = Vec::new();
    write(&read.pointer, &mut out).unwrap();
    assert_eq!(out, text.as_bytes());
  }
}

/// A-610, golden: do read pointer_test.go's non-canonical examples: no last line feed, carriage returns, trailing
/// spaces, extensions out of order; and its `TestDecodeExtensionsSort` and `TestDecodePreRelease`. Expect each read as
/// the same pointer, and not canonical.
#[test]
fn git_lfs_own_non_canonical_examples_read_as_the_same_pointer() {
  let examples = [
    (
      format!("version https://git-lfs.github.com/spec/v1\noid sha256:{OID}\nsize 12345"),
      plain(),
    ),
    (
      format!("version https://git-lfs.github.com/spec/v1\r\noid sha256:{OID}\r\nsize 12345\r\n"),
      plain(),
    ),
    (
      format!("version https://git-lfs.github.com/spec/v1\noid sha256:{OID}\nsize 12345   \n"),
      plain(),
    ),
    (
      format!(
        "version https://git-lfs.github.com/spec/v1\next-2-baz sha256:{AAA}\next-0-foo sha256:{FFF}\next-1-bar sha256:{BBB}\noid sha256:{OID}\nsize 12345\n"
      ),
      with_extensions(),
    ),
    (
      format!("version https://hawser.github.com/spec/v1\noid sha256:{OID}\nsize 12345"),
      plain(),
    ),
  ];
  for (text, pointer) in examples {
    let read = read(text.as_bytes()).unwrap();
    assert_eq!((read.pointer, read.canonical), (pointer, false), "{text:?}");
  }
}

/// pointer_test.go's `TestDecodeInvalid` examples and `TestDecodeTinyFile`'s, each with the refusal this module gives
/// it and whether git-lfs takes it for no pointer at all.
fn invalid_examples() -> Vec<(String, Refusal, bool)> {
  let v1 = "version https://git-lfs.github.com/spec/v1";
  let after_version = v1.len() as u64 + 1;
  let ext_line =
    |key: &str, value: &str| format!("{v1}\n{key} {value}\noid sha256:{OID}\nsize 12345");
  let unexpected = |at, expected| Refusal::UnexpectedKey { at, expected };
  vec![
    (
      "this is not a git-lfs file!".into(),
      unexpected(0, Key::Version),
      true,
    ),
    ("invalid stuff".into(), Refusal::NoMarker, true),
    ("# git-media".into(), unexpected(0, Key::Version), true),
    (
      format!("{v1}\noid sha256:boom\nsize 12345"),
      Refusal::Oid {
        at: after_version + 4,
      },
      false,
    ),
    (
      format!("{v1}\noid shazam:{OID}\nsize 12345"),
      Refusal::Oid {
        at: after_version + 4,
      },
      false,
    ),
    (
      format!("{v1}\nsize 12345"),
      unexpected(after_version, Key::Oid),
      false,
    ),
    (
      format!("version http://git-media.io/v/whatever\noid sha256:{OID}\nsize 12345"),
      Refusal::Version { at: 8 },
      false,
    ),
    (
      format!("oid sha256:{OID}\nsize 12345"),
      unexpected(0, Key::Version),
      true,
    ),
    (
      format!("{v1}\noid sha256:{OID}\nsize fif"),
      Refusal::Size {
        at: after_version + 81,
      },
      false,
    ),
    (
      format!("{v1}\noid sha256:{OID}"),
      Refusal::MissingSize,
      false,
    ),
    (
      format!("version=https://git-lfs.github.com/spec/v1\noid=sha256:{OID}\nsize=fif"),
      Refusal::NoValue { at: 0 },
      true,
    ),
    (
      format!("version=http://wat.io/v/2\noid=sha256:{OID}\nsize=fif"),
      Refusal::NoMarker,
      true,
    ),
    (
      format!("{v1}\noid sha256:{OID}\nsize 12345\nwat wat"),
      Refusal::ExtraLine {
        at: after_version + 87,
      },
      true,
    ),
    (
      format!("{v1}\nsize 12345\noid sha256:{OID}"),
      unexpected(after_version, Key::Oid),
      false,
    ),
    (
      ext_line("ext-0-$$$$", &format!("sha256:{FFF}")),
      unexpected(after_version, Key::Oid),
      false,
    ),
    (
      ext_line("ext-#-foo", &format!("sha256:{FFF}")),
      unexpected(after_version, Key::Oid),
      false,
    ),
    (
      format!("{v1}\next-0-foo sha256:{FFF}\next-0-bar sha256:{BBB}\noid sha256:{OID}\nsize 12345"),
      Refusal::DuplicatePriority { priority: 0 },
      false,
    ),
    (
      ext_line("ext-10-foo", &format!("sha256:{FFF}")),
      unexpected(after_version, Key::Oid),
      false,
    ),
    (
      ext_line("ext-0-foo", "sha256:boom"),
      Refusal::Extension { at: after_version },
      false,
    ),
    (
      ext_line("ext-0-foo", &format!("boom:{FFF}")),
      Refusal::Extension { at: after_version },
      false,
    ),
    (
      format!("{v1}\noid sha256:{OID}&\nsize 177735"),
      Refusal::Oid {
        at: after_version + 4,
      },
      false,
    ),
  ]
}

/// A-610, golden: do read every invalid example of pointer_test.go. Expect each refused by its kind at its byte, and
/// taken for no pointer at all exactly when git-lfs's `NotAPointerError` takes it so.
#[test]
fn git_lfs_own_invalid_examples_are_refused_by_kind() {
  let examples = invalid_examples();
  assert_eq!(examples.len(), 21);
  for (text, refusal, not_a_pointer) in examples {
    assert_eq!(read(text.as_bytes()), Err(refusal), "{text:?}");
    assert_eq!(refusal.is_not_a_pointer(), not_a_pointer, "{text:?}");
  }
}

/// The pointers spec.md shows, each fenced block that begins with `version`, its `(ending \n)` line read as the line
/// feed it names.
fn spec_examples() -> Vec<String> {
  let mut examples = Vec::new();
  let mut current: Option<String> = None;
  for line in SPEC_MD.lines() {
    match current.as_mut() {
      None if line.starts_with("version ") => current = Some(format!("{line}\n")),
      None => {}
      Some(text) if line == "(ending \\n)" => {
        examples.push(text.clone());
        current = None;
      }
      Some(text) => {
        text.push_str(line);
        text.push('\n');
      }
    }
  }
  examples
}

/// A-610, golden: do read the two pointers spec.md shows, the current version's and the pre-release tool's. Expect both
/// the pointer of size 12345, the first canonical and the second not, at the pre-release version.
#[test]
fn the_specifications_examples_are_read() {
  let examples = spec_examples();
  assert_eq!(examples.len(), 2);
  let first = read(examples[0].as_bytes()).unwrap();
  assert_eq!(
    (first.pointer, first.version, first.canonical),
    (plain(), Version::V1, true)
  );
  let second = read(examples[1].as_bytes()).unwrap();
  assert_eq!(
    (second.pointer, second.version, second.canonical),
    (plain(), Version::PreRelease, false)
  );
}

/// A-610: do read the empty file, and hash no bytes. Expect the empty pointer, canonical, its oid SHA-256 of nothing,
/// written back as nothing.
#[test]
fn the_empty_file_is_the_empty_pointer() {
  let read = read(b"").unwrap();
  assert_eq!((read.pointer, read.canonical), (empty(), true));
  let hashed = Hasher::raw(ObjectFormat::Sha256).finish().unwrap();
  assert_eq!(&read.pointer.oid.as_bytes()[..], hashed.as_bytes());
  let mut out = Vec::new();
  write(&empty(), &mut out).unwrap();
  assert!(out.is_empty());
}

/// A-610, hostile input: do read a pointer padded with spaces to 1023 bytes, and to 1024. Expect the first read, not
/// canonical, and the second refused as no pointer, git-lfs's cutoff.
#[test]
fn a_blob_of_1024_bytes_or_more_is_no_pointer() {
  let text = format!("version https://git-lfs.github.com/spec/v1\noid sha256:{OID}\nsize 12345\n");
  let padded = |len: usize| format!("{text}{}", " ".repeat(len - text.len()));
  let below = padded(CUTOFF - 1);
  let read_padded = read(below.as_bytes()).unwrap();
  assert_eq!(
    (read_padded.pointer, read_padded.canonical),
    (plain(), false)
  );
  assert_eq!(read(padded(CUTOFF).as_bytes()), Err(Refusal::TooLarge));
}

/// A-610: do read pointers wrapped in Unicode white space (an ideographic space before, a no-break space after) and
/// one led by a character Unicode does not call space (U+001C). Expect the first read, not canonical, and the second
/// refused where `version` was expected, as Go's `bytes.TrimSpace` leaves it.
#[test]
fn unicode_white_space_is_trimmed_as_go_trims_it() {
  let text = format!("version https://git-lfs.github.com/spec/v1\noid sha256:{OID}\nsize 12345\n");
  let wrapped = format!("\u{3000}{text}\u{a0}");
  let read_wrapped = read(wrapped.as_bytes()).unwrap();
  assert_eq!(
    (read_wrapped.pointer, read_wrapped.canonical),
    (plain(), false)
  );
  let led = format!("\u{1c}{text}");
  assert_eq!(
    read(led.as_bytes()),
    Err(Refusal::UnexpectedKey {
      at: 0,
      expected: Key::Version
    })
  );
}

/// A-610: do read sizes as Go's `strconv.ParseInt` reads them: a plus sign, leading zeros, negative zero, the largest
/// `int64`, one past it, and a negative size. Expect the first four read (zero for negative zero, every one not
/// canonical but the largest), the last two refused.
#[test]
fn sizes_are_read_as_go_reads_them() {
  let with = |size: &str| {
    format!("version https://git-lfs.github.com/spec/v1\noid sha256:{OID}\nsize {size}\n")
  };
  for (size, value, canonical) in [
    ("+12345", 12_345, false),
    ("012345", 12_345, false),
    ("-0", 0, false),
    ("9223372036854775807", i64::MAX.unsigned_abs(), true),
  ] {
    let text = with(size);
    let read = read(text.as_bytes()).unwrap();
    assert_eq!(
      (read.pointer.size, read.canonical),
      (value, canonical),
      "{size}"
    );
  }
  for size in ["9223372036854775808", "-1"] {
    assert!(
      matches!(read(with(size).as_bytes()), Err(Refusal::Size { .. })),
      "{size}"
    );
  }
}

/// A-610: do read a pointer giving one extension's key twice with two oids. Expect the last read, as Go's map keeps it,
/// and the pointer not canonical.
#[test]
fn an_extension_given_twice_keeps_its_last_value() {
  let text = format!(
    "version https://git-lfs.github.com/spec/v1\next-0-foo sha256:{AAA}\next-0-foo sha256:{FFF}\noid sha256:{OID}\nsize 12345\n"
  );
  let read = read(text.as_bytes()).unwrap();
  assert_eq!(
    read.pointer.extensions[0],
    Some(Extension {
      name: "foo",
      oid: oid(FFF)
    })
  );
  assert!(!read.canonical);
}

/// A-610: do write pointers a client could not read back: a size past Go's `int64`, and extension names with upper
/// case, a leading underscore, a space or nothing at all. Expect each refused by kind with nothing written.
#[test]
fn pointers_a_client_could_not_read_are_refused() {
  let mut out = Vec::new();
  let huge = Pointer {
    size: i64::MAX.unsigned_abs() + 1,
    ..plain()
  };
  assert_eq!(write(&huge, &mut out), Err(WriteRefusal::Size));
  for name in ["Foo", "_x", "a b", ""] {
    let mut pointer = plain();
    pointer.extensions[3] = Some(Extension {
      name,
      oid: oid(FFF),
    });
    assert_eq!(
      write(&pointer, &mut out),
      Err(WriteRefusal::ExtensionName { priority: 3 }),
      "{name:?}"
    );
  }
  assert!(out.is_empty());
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

  /// A-610, model: do write any pointer (any oid, any size up to Go's `int64`, extensions at any priorities with names
  /// the specification allows) and read it back. Expect the same pointer, at version 1, canonical.
  #[test]
  fn any_pointer_written_reads_back_canonical(
    raw in any::<[u8; 32]>(),
    size in 1..=i64::MAX.unsigned_abs(),
    extensions in proptest::collection::vec(proptest::option::of(("[a-z0-9][a-z0-9.-]{0,8}", any::<[u8; 32]>())), MAX_EXTENSIONS),
  ) {
    let mut pointer = Pointer { oid: Oid::from_bytes(raw), size, extensions: [None; MAX_EXTENSIONS] };
    for (slot, extension) in pointer.extensions.iter_mut().zip(&extensions) {
      *slot = extension.as_ref().map(|(name, raw)| Extension { name: name.as_str(), oid: Oid::from_bytes(*raw) });
    }
    let mut out = Vec::new();
    write(&pointer, &mut out).unwrap();
    let read = read(&out).unwrap();
    prop_assert_eq!((read.pointer, read.version, read.canonical), (pointer, Version::V1, true));
  }
}
