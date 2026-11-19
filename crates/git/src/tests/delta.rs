use proptest::prelude::*;

use super::{
  CopyRefusal, INSERT_MAX, Instruction, Instructions, MIN_LEN, Refusal, Sizes, apply, apply_to,
  sizes, write_copy, write_insert, write_sizes,
};
use crate::object_name::{Kind, ObjectFormat, ObjectId, name_of};
use crate::pack_fixtures::{DELTAS, Listed, Pack, adler32, object_size, packs, text_at_commit};
use crate::{Admission, PastAdmission};

/// The name `hex` writes, in the fixtures' format.
fn named(hex: &str) -> ObjectId {
  ObjectId::from_hex(ObjectFormat::Sha1, hex.as_bytes()).unwrap()
}

/// The file `text` at each of the fixture repository's three commits, each with the name A-600 gives it.
fn files() -> Vec<(ObjectId, Vec<u8>)> {
  (1..=3)
    .map(|commit| {
      let text = text_at_commit(commit);
      (name_of(ObjectFormat::Sha1, Kind::Blob, &text).unwrap(), text)
    })
    .collect()
}

/// Each captured delta, named by its object, with its base's content.
fn golden() -> Vec<(&'static str, &'static [u8], Vec<u8>)> {
  let files = files();
  let [pack, _] = packs();
  DELTAS
    .iter()
    .map(|(hex, delta)| {
      let base = pack.entry(named(hex)).base.unwrap();
      let (_, source) = files.iter().find(|(name, _)| *name == base).unwrap();
      (*hex, *delta, source.clone())
    })
    .collect()
}

/// A delta from a source of `source` bytes to a target of `target` bytes, its instructions as given.
fn delta(source: u64, target: u64, instructions: &[u8]) -> Vec<u8> {
  let mut out = Vec::new();
  write_sizes(Sizes { source, target }, &mut out);
  out.extend_from_slice(instructions);
  out
}

/// Applies `delta` to `source` through `apply_to`, checking what it wrote: exactly the declared target when applied,
/// nothing when refused.
fn applied(source: &[u8], delta: &[u8]) -> Result<Vec<u8>, Refusal> {
  let mut out = Vec::new();
  match apply_to(source, delta, &mut out) {
    Ok(sizes) => {
      assert_eq!(u64::try_from(out.len()).unwrap(), sizes.target);
      assert_eq!(u64::try_from(source.len()).unwrap(), sizes.source);
      Ok(out)
    }
    Err(refusal) => {
      assert!(out.is_empty(), "{refusal:?}, yet {} bytes written", out.len());
      Err(refusal)
    }
  }
}

/// A-582 part 3 (golden vectors: the two deltas `fixtures/make-delta-fixtures.sh` inflated from `ofs.pack` and
/// `ref.pack`; RFC 1950 §2.2). Do: compute each captured delta's Adler-32 and read its two sizes. Expect: in both packs
/// the entry listed for its object ends its zlib stream with that Adler-32 and names the delta's own length as its
/// size, so the file is that entry's delta, inflated; and its sizes are its base's and its object's, as `objects.txt`
/// lists them.
#[test]
fn each_captured_delta_is_the_one_both_packs_carry_for_its_object() {
  for pack in packs() {
    for (hex, delta) in DELTAS {
      let entry = pack.entry(named(hex));
      assert_eq!(pack.zlib_trailer(entry), adler32(delta), "{} {hex}", pack.name);
      assert_eq!(entry.size, u64::try_from(delta.len()).unwrap(), "{} {hex}", pack.name);
      let expected = Sizes {
        source: object_size(entry.base.unwrap()),
        target: object_size(entry.id),
      };
      assert_eq!(sizes(delta).map(|(sizes, _)| sizes), Ok(expected), "{hex}");
    }
  }
}

/// The content of `entry`, one of the three files: a whole one checked against the Adler-32 its zlib stream ends with
/// in both packs; a delta applied to the content already made for its base, under an admission of exactly its size.
fn rebuilt(entry: &Listed, file: &[u8], made: &[(ObjectId, Vec<u8>)], packs: &[Pack]) -> Vec<u8> {
  let Some(base) = entry.base else {
    for pack in packs {
      let trailer = pack.zlib_trailer(pack.entry(entry.id));
      assert_eq!(trailer, adler32(file), "{}: the whole file", pack.name);
    }
    return file.to_vec();
  };
  let (_, source) = made.iter().find(|(name, _)| *name == base).unwrap();
  let (_, delta) = DELTAS.iter().find(|(hex, _)| named(hex) == entry.id).unwrap();
  let mut admission = Admission::new(object_size(entry.id));
  let target = apply(source, delta, &mut admission).unwrap();
  assert_eq!(admission.left(), 0, "{:?}: charged exactly its size", entry.id);
  target
}

/// A-582 part 3 (golden vectors; the object's name checked through A-600's `object_name`). Do: rebuild the file
/// `text` at each of the fixture repository's three commits by `make-fixtures.sh`'s recipe; then walk `ofs.pack`'s
/// listing in order, taking each file whole where git stored it whole and applying its captured delta to its base where
/// git stored a delta. Expect: the three files are three of the listed blobs; the whole one, the third commit's, ends
/// its zlib stream with its own Adler-32 in both packs; each delta makes the very file git named, its name
/// `name_of`'s; and the second delta's base is the first one's target, so the chain of two git chose is walked.
#[test]
fn each_delta_applied_to_its_base_rebuilds_the_object_git_named() {
  let files = files();
  let packs = packs();
  let [pack, _] = &packs;
  let mut made: Vec<(ObjectId, Vec<u8>)> = Vec::new();
  for entry in &pack.listing {
    let Some((_, file)) = files.iter().find(|(name, _)| *name == entry.id) else {
      continue;
    };
    let content = rebuilt(entry, file, &made, &packs);
    assert_eq!(name_of(ObjectFormat::Sha1, Kind::Blob, &content), Ok(entry.id));
    assert_eq!(&content, file);
    made.push((entry.id, content));
  }
  assert_eq!(made.len(), 3, "the three files, each listed");
  let deltas: Vec<&Listed> = pack.listing.iter().filter(|entry| entry.base.is_some()).collect();
  assert_eq!(deltas.len(), 2);
  let on_a_delta = deltas
    .iter()
    .filter(|entry| deltas.iter().any(|base| entry.base == Some(base.id)));
  assert_eq!(on_a_delta.count(), 1, "a delta whose base is a delta");
}

/// A-582 part 3 (`diff-delta.c` 344-358 and 434-466: git writes each size and each copy's operands in the fewest
/// bytes, and inserts of at most 127). Do: read each captured delta's sizes and instructions, and write them again
/// with this module's writers. Expect: the very bytes git wrote.
#[test]
fn gits_deltas_read_to_instructions_that_write_back_to_their_very_bytes() {
  for (hex, delta) in DELTAS {
    let (sizes, start) = sizes(delta).unwrap();
    let mut out = Vec::new();
    write_sizes(sizes, &mut out);
    assert_eq!(out.len(), start, "{hex}: the sizes");
    for item in Instructions::new(delta, start) {
      match item.unwrap() {
        (_, Instruction::Copy { offset, len }) => write_copy(offset, len, &mut out).unwrap(),
        (_, Instruction::Insert(bytes)) => write_insert(bytes, &mut out),
      }
    }
    assert_eq!(out, delta, "{hex}");
  }
}

/// A-582 part 3 (hostile input: every cut). Do: apply each delta stock git wrote, cut at each of its bytes from none to
/// all but its last, to its base. Expect: each cut refused with nothing written: `TooShort` under git's four bytes,
/// otherwise `Truncated` or `ShortOfTarget`.
#[test]
fn every_cut_of_a_delta_stock_git_wrote_is_refused_and_writes_nothing() {
  for (hex, delta, source) in golden() {
    for cut in 0..delta.len() {
      let refusal = applied(&source, &delta[..cut]).unwrap_err();
      let by_kind = match refusal {
        Refusal::TooShort { len } => cut < MIN_LEN && len == u64::try_from(cut).unwrap(),
        Refusal::Truncated { .. } | Refusal::ShortOfTarget { .. } => cut >= MIN_LEN,
        _ => false,
      };
      assert!(by_kind, "{hex} cut at {cut}: {refusal:?}");
    }
  }
}

/// The name of a refusal's kind, for a tally.
fn kind_of(refusal: &Refusal) -> &'static str {
  match refusal {
    Refusal::TooShort { .. } => "TooShort",
    Refusal::Truncated { .. } => "Truncated",
    Refusal::SizeTooLong { .. } => "SizeTooLong",
    Refusal::Reserved { .. } => "Reserved",
    Refusal::SourceSize { .. } => "SourceSize",
    Refusal::CopyOutsideSource { .. } => "CopyOutsideSource",
    Refusal::PastTarget { .. } => "PastTarget",
    Refusal::ShortOfTarget { .. } => "ShortOfTarget",
    Refusal::PastAdmission(_) => "PastAdmission",
    Refusal::AllocationRefused { .. } => "AllocationRefused",
  }
}

/// A-582 part 3 (hostile input: bit flips). Do: flip each bit of each delta stock git wrote, one at a time, and apply
/// it to its base. Expect: each refused by kind with nothing written, or applied whole, making exactly the target size
/// the flipped delta declares; and among them flips refused for the source's size, flips refused inside the
/// instructions, and flips that still apply (a changed insert byte), so no outcome is vacuous.
#[test]
fn every_bit_flip_of_a_delta_stock_git_wrote_is_refused_by_kind_or_makes_its_declared_size() {
  let mut seen = Vec::new();
  for (_, delta, source) in golden() {
    for bit in 0..delta.len() * 8 {
      let mut flipped = delta.to_vec();
      flipped[bit / 8] ^= 1 << (bit % 8);
      match applied(&source, &flipped) {
        Ok(_) => seen.push("applied"),
        Err(refusal) => seen.push(kind_of(&refusal)),
      }
    }
  }
  assert!(seen.contains(&"applied"), "{seen:?}");
  assert!(seen.contains(&"SourceSize"), "{seen:?}");
  let inside = ["CopyOutsideSource", "PastTarget", "ShortOfTarget", "Truncated", "Reserved"];
  assert!(seen.iter().any(|kind| inside.contains(kind)), "{seen:?}");
  assert!(!seen.contains(&"PastAdmission"), "apply_to charges nothing");
}

/// A-582 part 3 (`patch-delta.c` 23-90 and `delta.h` 89-102: every rule git holds a delta to). Do: apply deltas built
/// by hand, each breaking one rule: under git's four bytes; a size cut short; sizes past 64 bits, by a group that loses
/// bits and by a zero group past bit 63; the reserved byte zero; a source of another length; a copy past the source's
/// end, and one past both the source's end and the target's; a copy's operand and an insert's bytes cut short; an
/// insert, and a copy, past the target; and instructions short of it. Expect: each refused by its own kind, naming
/// where it stopped, with nothing written.
#[test]
fn each_rule_git_holds_a_delta_to_is_a_refusal_of_its_own_kind() {
  let hello: &[u8] = b"hello";
  let mut past_64 = vec![0xff; 9];
  past_64.extend([0x7f, 0x00]);
  let mut zero_past_63 = vec![0x80; 10];
  zero_past_63.push(0x00);
  let cases: Vec<(&[u8], Vec<u8>, Refusal)> = vec![
    (hello, vec![], Refusal::TooShort { len: 0 }),
    (hello, vec![0x05, 0x01, 0x01], Refusal::TooShort { len: 3 }),
    (hello, vec![0x05, 0x80, 0x80, 0x80], Refusal::Truncated { at: 1 }),
    (hello, past_64, Refusal::SizeTooLong { at: 0 }),
    (hello, zero_past_63, Refusal::SizeTooLong { at: 0 }),
    (b"x", delta(1, 1, &[0x00]), Refusal::Reserved { at: 2 }),
    (
      b"hell",
      delta(5, 1, &[0x01, b'a']),
      Refusal::SourceSize {
        declared: 5,
        source: 4,
      },
    ),
    (
      hello,
      delta(5, 3, &[0x91, 0x03, 0x03]),
      Refusal::CopyOutsideSource {
        at: 2,
        offset: 3,
        len: 3,
        source: 5,
      },
    ),
    (
      hello,
      delta(5, 2, &[0x91, 0x03, 0x05]),
      Refusal::CopyOutsideSource {
        at: 2,
        offset: 3,
        len: 5,
        source: 5,
      },
    ),
    (hello, delta(5, 3, &[0x91, 0x03]), Refusal::Truncated { at: 2 }),
    (hello, delta(5, 3, &[0x03, b'a']), Refusal::Truncated { at: 2 }),
    (
      hello,
      delta(5, 2, &[0x03, b'a', b'b', b'c']),
      Refusal::PastTarget { at: 2, target: 2 },
    ),
    (
      hello,
      delta(5, 2, &[0x01, b'h', 0x91, 0x00, 0x03]),
      Refusal::PastTarget { at: 4, target: 2 },
    ),
    (
      hello,
      delta(5, 4, &[0x03, b'a', b'b', b'c']),
      Refusal::ShortOfTarget { made: 3, target: 4 },
    ),
  ];
  for (source, delta, refusal) in cases {
    assert_eq!(applied(source, &delta), Err(refusal), "{delta:02x?}");
  }
}

/// A-582 part 3 (`delta.h` 89-102: a size is read seven bits a byte, as many bytes as its high bits say). Do: read
/// the largest sizes, 2^64 - 1 in ten bytes, and zero in non-minimal forms, and apply a delta of four bytes that names
/// an empty source and an empty target that way. Expect: each size read as git reads it, and the empty target made.
#[test]
fn sizes_are_read_to_64_bits_in_any_number_of_bytes_git_reads() {
  let mut largest = vec![0xff; 9];
  largest.extend([0x01, 0x00]);
  let expected = Sizes {
    source: u64::MAX,
    target: 0,
  };
  assert_eq!(sizes(&largest), Ok((expected, 11)));
  let zero_in_two_bytes_each = [0x80, 0x00, 0x80, 0x00];
  let empty = Sizes {
    source: 0,
    target: 0,
  };
  assert_eq!(sizes(&zero_in_two_bytes_each), Ok((empty, 4)));
  assert_eq!(applied(b"", &zero_in_two_bytes_each), Ok(Vec::new()));
}

/// A-582 part 3 (`gitformat-pack.adoc` 153-156: "size zero is automatically converted to 0x10000"; `patch-delta.c`
/// 57). Do: from a source of 0x10001 bytes, copy with no operand bytes at all, with one size byte of zero, and from
/// offset 1 with no size bytes; and write a copy of 0x10000 bytes from offset 0. Expect: each copies 0x10000 bytes from
/// its offset; the writer says it in the one byte 0x80, as git's `create_delta` does.
#[test]
fn a_copy_whose_size_is_zero_copies_0x10000_bytes() {
  let source: Vec<u8> = (0..=0x10000u32)
    .map(|n| u8::try_from(n % 251).unwrap())
    .collect();
  for (instruction, offset) in [
    (vec![0x80], 0usize),
    (vec![0x90, 0x00], 0),
    (vec![0x81, 0x01], 1),
  ] {
    let target = applied(&source, &delta(0x10001, 0x10000, &instruction)).unwrap();
    assert_eq!(target, source[offset..offset + 0x10000], "{instruction:02x?}");
  }
  let mut out = Vec::new();
  write_copy(0, 0x10000, &mut out).unwrap();
  assert_eq!(out, [0x80]);
}

/// A-582 part 3 (the caller's admission). Do: apply a delta making `hello world` from `hello` under admissions of 10
/// and of 11 bytes, and a delta refused for its own sake under 100. Expect: refused past the admission with nothing
/// charged; then made, the admission spent exactly; the refused delta charging nothing.
#[test]
fn a_target_past_its_admission_is_refused_before_it_is_made() {
  let delta = delta(5, 11, &[0x90, 0x05, 0x06, b' ', b'w', b'o', b'r', b'l', b'd']);
  let mut short = Admission::new(10);
  let past = PastAdmission {
    asked: 11,
    left: 10,
  };
  assert_eq!(apply(b"hello", &delta, &mut short), Err(Refusal::PastAdmission(past)));
  assert_eq!(short.left(), 10, "nothing charged");
  let mut exact = Admission::new(11);
  assert_eq!(apply(b"hello", &delta, &mut exact), Ok(b"hello world".to_vec()));
  assert_eq!(exact.left(), 0);
  let mut roomy = Admission::new(100);
  let refused = apply(b"hell", &delta, &mut roomy);
  let source_size = Refusal::SourceSize {
    declared: 5,
    source: 4,
  };
  assert_eq!(refused, Err(source_size));
  assert_eq!(roomy.left(), 100, "nothing charged");
}

/// A-582 part 3 (`gitformat-pack.adoc` 128-167; `diff-delta.c` 434-466). Do: write copies the format cannot say (no
/// bytes, 2^24 bytes, an offset past 2^32 - 1); copies at each operand byte's edge, each read back; and inserts of 0,
/// 1, 127, 128 and 300 bytes, each read back. Expect: the three refused with nothing written; each copy only its
/// nonzero operand bytes, each named by its bit, and read back the same; inserts in runs of at most 127, none for no
/// bytes, which would be the reserved instruction, joining to the bytes given.
#[test]
fn the_writers_say_each_instruction_in_the_fewest_bytes_and_refuse_what_the_format_cannot() {
  for (offset, len, refusal) in [
    (0, 0, CopyRefusal::Empty),
    (0, 1 << 24, CopyRefusal::TooLong { len: 1 << 24 }),
    (1 << 32, 1, CopyRefusal::OffsetTooFar { offset: 1 << 32 }),
  ] {
    let mut out = Vec::new();
    assert_eq!(write_copy(offset, len, &mut out), Err(refusal));
    assert!(out.is_empty(), "{refusal:?}: nothing written");
  }
  for (offset, len, bytes) in [
    (0, 1, vec![0x90, 0x01]),
    (0xff, 0x100, vec![0xa1, 0xff, 0x01]),
    (0x0100_0000, 0xff_ffff, vec![0xf8, 0x01, 0xff, 0xff, 0xff]),
    (0xffff_ffff, 0x1_0000, vec![0x8f, 0xff, 0xff, 0xff, 0xff]),
    (0x01_0203, 0x1_0001, vec![0xd7, 0x03, 0x02, 0x01, 0x01, 0x01]),
  ] {
    let mut out = Vec::new();
    write_copy(offset, len, &mut out).unwrap();
    assert_eq!(out, bytes, "copy {len:#x} from {offset:#x}");
    let read_back = Instructions::new(&out, 0).next();
    assert_eq!(read_back, Some(Ok((0, Instruction::Copy { offset, len }))));
  }
  for (len, runs) in [
    (0usize, vec![]),
    (1, vec![1]),
    (INSERT_MAX, vec![127]),
    (128, vec![127, 1]),
    (300, vec![127, 127, 46]),
  ] {
    let bytes: Vec<u8> = (0..len).map(|n| u8::try_from(n % 256).unwrap()).collect();
    let mut out = Vec::new();
    write_insert(&bytes, &mut out);
    let read_back: Vec<&[u8]> = Instructions::new(&out, 0)
      .map(|item| match item.unwrap() {
        (_, Instruction::Insert(run)) => run,
        other => panic!("{other:?}"),
      })
      .collect();
    let lens: Vec<usize> = read_back.iter().map(|run| run.len()).collect();
    assert_eq!(lens, runs, "an insert of {len}");
    assert_eq!(read_back.concat(), bytes);
  }
}

/// One instruction as the model holds it.
#[derive(Clone, Debug)]
enum Step {
  /// Copy `len` bytes of the source from `offset`.
  Copy {
    /// Where in the source.
    offset: u64,
    /// How many bytes.
    len: u64,
  },
  /// Insert these bytes.
  Insert(Vec<u8>),
}

/// A source of 1 to 699 bytes and up to ten steps over it: copies inside it, inserts of 1 to 299 bytes.
fn source_and_steps() -> impl Strategy<Value = (Vec<u8>, Vec<Step>)> {
  proptest::collection::vec(any::<u8>(), 1..700).prop_flat_map(|source| {
    let len = u64::try_from(source.len()).unwrap();
    let copy = (0..len)
      .prop_flat_map(move |offset| (Just(offset), 1..=len - offset))
      .prop_map(|(offset, len)| Step::Copy { offset, len });
    let insert = proptest::collection::vec(any::<u8>(), 1..300).prop_map(Step::Insert);
    let steps = proptest::collection::vec(prop_oneof![copy, insert], 0..10);
    (Just(source), steps)
  })
}

/// The model's target: each copy a slice of the source, each insert its bytes, in order.
fn model_target(source: &[u8], steps: &[Step]) -> Vec<u8> {
  let mut target = Vec::new();
  for step in steps {
    match step {
      Step::Copy { offset, len } => {
        let start = usize::try_from(*offset).unwrap();
        target.extend_from_slice(&source[start..start + usize::try_from(*len).unwrap()]);
      }
      Step::Insert(bytes) => target.extend_from_slice(bytes),
    }
  }
  target
}

/// The delta this module's writers make for the steps.
fn encoded(source: &[u8], steps: &[Step], target: &[u8]) -> Vec<u8> {
  let mut out = Vec::new();
  let sizes = Sizes {
    source: u64::try_from(source.len()).unwrap(),
    target: u64::try_from(target.len()).unwrap(),
  };
  write_sizes(sizes, &mut out);
  for step in steps {
    match step {
      Step::Copy { offset, len } => write_copy(*offset, *len, &mut out).unwrap(),
      Step::Insert(bytes) => write_insert(bytes, &mut out),
    }
  }
  out
}

/// The instructions a reader must find for the steps: each copy, and each insert in runs of at most 127 bytes.
fn model_instructions(steps: &[Step]) -> Vec<Instruction<'_>> {
  steps
    .iter()
    .flat_map(|step| match step {
      Step::Copy { offset, len } => vec![Instruction::Copy {
        offset: *offset,
        len: *len,
      }],
      Step::Insert(bytes) => bytes.chunks(INSERT_MAX).map(Instruction::Insert).collect(),
    })
    .collect()
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

  /// A-582 part 3 (a model oracle). Do: write any steps over any source as a delta with this module's writers, read it
  /// back and apply it under an admission of exactly the model's target. Expect: refused `TooShort` when the delta is
  /// under git's four bytes (no steps, both sizes a byte each), as git refuses it; otherwise the model's instructions
  /// read back, the model's target made, and the admission spent exactly.
  #[test]
  fn any_delta_the_writers_make_applies_as_the_model_says((source, steps) in source_and_steps()) {
    let target = model_target(&source, &steps);
    let delta = encoded(&source, &steps, &target);
    let mut admission = Admission::new(u64::try_from(target.len()).unwrap());
    let made = apply(&source, &delta, &mut admission);
    if delta.len() < MIN_LEN {
      prop_assert_eq!(made, Err(Refusal::TooShort { len: u64::try_from(delta.len()).unwrap() }));
    } else {
      prop_assert_eq!(made, Ok(target.clone()));
      prop_assert_eq!(admission.left(), 0);
      let (_, start) = sizes(&delta).unwrap();
      let read_back: Vec<Instruction<'_>> = Instructions::new(&delta, start).map(|item| item.unwrap().1).collect();
      prop_assert_eq!(read_back, model_instructions(&steps));
    }
  }

  /// A-582 part 3 (hostile input: arbitrary bytes). Do: apply, to any source of up to 40 bytes, a delta that names the
  /// source's size truly and then holds any bytes at all. Expect: refused by kind with nothing written, or applied,
  /// making exactly the target size those bytes declare.
  #[test]
  fn any_bytes_after_a_true_source_size_are_refused_by_kind_or_make_their_declared_target(
    source in proptest::collection::vec(any::<u8>(), 0..40),
    rest in proptest::collection::vec(any::<u8>(), 0..40),
  ) {
    let mut delta = Vec::new();
    write_sizes(Sizes { source: u64::try_from(source.len()).unwrap(), target: 0 }, &mut delta);
    delta.pop();
    delta.extend_from_slice(&rest);
    if let Err(refusal) = applied(&source, &delta) {
      let charged = matches!(refusal, Refusal::PastAdmission(_) | Refusal::AllocationRefused { .. });
      prop_assert!(!charged, "apply_to takes no admission and allocates nothing: {:?}", refusal);
    }
  }
}
