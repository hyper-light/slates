use super::{Attribute, Attributes, Ref, Target, decode_answer, encode_answer, write_ref};
use crate::fixtures::LS_REFS_RESPONSE;
use crate::object_name::{HexRefusal, ObjectFormat, ObjectId};
use crate::out::Measure;
use crate::pktline::Packet;
use crate::v2::tests::{room, sha1, wire};
use crate::v2::{Ending, Line, Refusal};
use crate::{Admission, PastAdmission, to_u64};

/// The id of `main` in the golden repository.
const MAIN: &str = "d0f175d3390380c935e9b56e0cebb715995d838d";

/// `attributes` held in order.
fn attributes<'a>(attributes: &[Attribute<'a>]) -> Attributes<'a> {
  attributes
    .iter()
    .fold(Attributes::default(), |held, attribute| {
      held.with(*attribute).unwrap()
    })
}

/// A-582 part 2, golden vectors: do decode the `ls-refs` answer stock git 2.50.1 wrote to `peel`, `symrefs` and
/// `unborn`, then encode it. Expect: `HEAD` with its target, the two branches, and the tag with its peeled id, every
/// line ending with an LF, and the stream's own bytes back.
#[test]
fn stock_gits_answer_decodes_to_its_refs_and_encodes_back() {
  let refs = decode_answer(LS_REFS_RESPONSE, ObjectFormat::Sha1, &mut room()).unwrap();
  let expected = [
    Ref {
      target: Target::Id(sha1(MAIN)),
      name: b"HEAD",
      attributes: attributes(&[Attribute::SymrefTarget(b"refs/heads/main")]),
    },
    Ref {
      target: Target::Id(sha1(MAIN)),
      name: b"refs/heads/main",
      attributes: Attributes::default(),
    },
    Ref {
      target: Target::Id(sha1("de204ecf4cbcbf9dec27d422e67f087c136709db")),
      name: b"refs/heads/side",
      attributes: Attributes::default(),
    },
    Ref {
      target: Target::Id(sha1("01b733a3e55963dbd3b64253d4ff5ff68d763b89")),
      name: b"refs/tags/v1",
      attributes: attributes(&[Attribute::Peeled(sha1(
        "46959f8a540a8b77be110b36029d84b840a9faff",
      ))]),
    },
  ];
  assert_eq!(
    refs.iter().map(|line| line.value).collect::<Vec<_>>(),
    expected
  );
  assert!(refs.iter().all(|line| line.ending == Ending::Lf));
  assert_eq!(
    refs[0].value.attributes.symref_target(),
    Some(&b"refs/heads/main"[..])
  );
  assert_eq!(
    refs[3].value.attributes.peeled(),
    Some(sha1("46959f8a540a8b77be110b36029d84b840a9faff"))
  );
  let mut out = Vec::new();
  encode_answer(&refs, ObjectFormat::Sha1, &mut out).unwrap();
  assert_eq!(out, LS_REFS_RESPONSE);
}

/// A-582 part 2 (gitprotocol-v2 "ls-refs"; `connect.c`: the attributes "don't have a particular order"): do encode
/// an answer holding an unborn `HEAD` with its target, a tag whose peeled id comes before a target, and a ref with no
/// LF, in SHA-256, and decode it. Expect: the same answer back, the attributes in the order written.
#[test]
fn an_unborn_head_and_attributes_in_either_order_are_written_back_as_they_came() {
  let id = ObjectId::from_hex(ObjectFormat::Sha256, "c".repeat(64).as_bytes()).unwrap();
  let peeled_first = attributes(&[
    Attribute::Peeled(id),
    Attribute::SymrefTarget(b"refs/tags/v2"),
  ]);
  let answer = vec![
    Line::lf(Ref {
      target: Target::Unborn,
      name: b"HEAD",
      attributes: attributes(&[Attribute::SymrefTarget(b"refs/heads/main")]),
    }),
    Line::lf(Ref {
      target: Target::Id(id),
      name: b"refs/tags/alias",
      attributes: peeled_first,
    }),
    Line::bare(Ref {
      target: Target::Id(id),
      name: b"refs/heads/caf\xc3\xa9",
      attributes: Attributes::default(),
    }),
  ];
  let mut out = Vec::new();
  encode_answer(&answer, ObjectFormat::Sha256, &mut out).unwrap();
  let text = String::from_utf8_lossy(&out);
  assert!(
    text.contains("unborn HEAD symref-target:refs/heads/main\n"),
    "{text}"
  );
  assert!(
    text.contains(&format!(
      "peeled:{} symref-target:refs/tags/v2",
      "c".repeat(64)
    )),
    "{text}"
  );
  assert_eq!(
    decode_answer(&out, ObjectFormat::Sha256, &mut room()),
    Ok(answer)
  );
}

/// A-624 (a reader takes an id in either case, as git's `get_oid_hex` does): do decode an answer line naming `main` in
/// upper case, then encode the answer. Expect: the ref read with `main`'s id, written back in lower case.
#[test]
fn an_upper_case_id_in_an_answer_is_read_and_written_back_in_lower_case() {
  let answer = |id: String| {
    let text = format!("{id} refs/heads/main\n");
    wire(&[Packet::Data(text.as_bytes()), Packet::Flush])
  };
  let upper = answer(MAIN.to_uppercase());
  let refs = decode_answer(&upper, ObjectFormat::Sha1, &mut room()).unwrap();
  let mut out = Vec::new();
  encode_answer(&refs, ObjectFormat::Sha1, &mut out).unwrap();
  assert_eq!(
    (refs[0].value.target, out),
    (Target::Id(sha1(MAIN)), answer(MAIN.to_owned()))
  );
}

/// A-582 part 2: do decode answer lines out of the grammar, one fault each. Expect: each refused by its kind at the
/// line: an attribute twice, an attribute git does not write, a line with no name, a name or target that is no ref
/// name, a peeled id too short, a delimiter, the bytes ending early or going on.
#[test]
fn an_answer_line_out_of_its_grammar_is_refused_by_kind() {
  let line = |text: String| wire(&[Packet::Data(text.as_bytes()), Packet::Flush]);
  let cases = [
    (
      line(format!(
        "{MAIN} HEAD symref-target:refs/heads/a symref-target:refs/heads/b\n"
      )),
      Refusal::Repeated { at: 0 },
    ),
    (
      line(format!("{MAIN} refs/tags/v1 peeled:{MAIN} peeled:{MAIN}\n")),
      Refusal::Repeated { at: 0 },
    ),
    (
      line(format!("{MAIN} refs/heads/main color:blue\n")),
      Refusal::Line { at: 0 },
    ),
    (
      line(format!("{MAIN} refs/heads/main \n")),
      Refusal::Line { at: 0 },
    ),
    (line(format!("{MAIN}\n")), Refusal::Line { at: 0 }),
    (line(format!("{MAIN} main\n")), Refusal::Refname { at: 0 }),
    (
      line(format!("{MAIN} refs/heads/a..b\n")),
      Refusal::Refname { at: 0 },
    ),
    (
      line(format!("{MAIN} HEAD symref-target:main\n")),
      Refusal::Refname { at: 0 },
    ),
    (
      line(format!(
        "{MAIN} refs/tags/v1 peeled:{}\n",
        MAIN.get(..20).unwrap()
      )),
      Refusal::ObjectId {
        at: 0,
        refusal: HexRefusal::Length { len: 20 },
      },
    ),
    (
      wire(&[Packet::Delim, Packet::Flush]),
      Refusal::Misplaced { at: 0 },
    ),
    (
      wire(&[Packet::Flush, Packet::Flush]),
      Refusal::Trailing { at: 4 },
    ),
    (wire(&[]), Refusal::Unfinished { at: 0 }),
  ];
  for (stream, refusal) in cases {
    assert_eq!(
      decode_answer(&stream, ObjectFormat::Sha1, &mut room()),
      Err(refusal),
      "{stream:?}"
    );
  }
  assert_eq!(
    decode_answer(b"0000", ObjectFormat::Sha1, &mut room()),
    Ok(vec![])
  );
}

/// A-582 part 2: do write stock git's answer a ref at a time, as a server streams one, then a flush; and write a ref
/// whose name is no ref name, and one whose id is of the other format. Expect: the answer's own bytes, each ref's
/// write saying the bytes it took; the bad refs refused at their place, nothing written.
#[test]
fn an_answer_streams_a_ref_at_a_time_and_a_bad_ref_writes_nothing() {
  let refs = decode_answer(LS_REFS_RESPONSE, ObjectFormat::Sha1, &mut room()).unwrap();
  let mut out = Vec::new();
  let mut at = 0u64;
  for line in &refs {
    at += write_ref(line, ObjectFormat::Sha1, at, &mut out).unwrap();
    assert_eq!(at, to_u64(out.len()));
  }
  crate::pktline::write(Packet::Flush, &mut out).unwrap();
  assert_eq!(out, LS_REFS_RESPONSE);

  let bad_name = Line::lf(Ref {
    target: Target::Id(sha1(MAIN)),
    name: b"refs/heads/no space",
    attributes: Attributes::default(),
  });
  let mut out = Vec::new();
  assert_eq!(
    write_ref(&bad_name, ObjectFormat::Sha1, 9, &mut out),
    Err(Refusal::Refname { at: 9 })
  );
  assert_eq!(
    encode_answer(&[bad_name], ObjectFormat::Sha1, &mut out),
    Err(Refusal::Refname { at: 0 })
  );
  let other_format = Line::lf(Ref {
    target: Target::Id(sha1(MAIN)),
    name: b"refs/heads/main",
    attributes: Attributes::default(),
  });
  assert_eq!(
    encode_answer(&[other_format], ObjectFormat::Sha256, &mut out),
    Err(Refusal::ObjectId {
      at: 0,
      refusal: HexRefusal::Length { len: 40 }
    })
  );
  assert!(out.is_empty(), "nothing written");
  assert_eq!(
    Attributes::default()
      .with(Attribute::Peeled(sha1(MAIN)))
      .and_then(|held| held.with(Attribute::Peeled(sha1(MAIN)))),
    None
  );
  let mut measure = Measure::default();
  encode_answer(&refs, ObjectFormat::Sha1, &mut measure).unwrap();
  assert_eq!(measure.0, to_u64(LS_REFS_RESPONSE.len()));
}

/// A-582 part 2 (§4.2): do decode stock git's answer with one byte less room than its four refs take, then with
/// exactly the room. Expect: `PastAdmission` with nothing charged, then the answer and the room spent.
#[test]
fn a_decode_past_its_admission_is_refused_before_its_list_is_made() {
  let needed = to_u64(4 * core::mem::size_of::<Line<Ref<'_>>>());
  let mut short = Admission::new(needed - 1);
  assert_eq!(
    decode_answer(LS_REFS_RESPONSE, ObjectFormat::Sha1, &mut short),
    Err(Refusal::PastAdmission(PastAdmission {
      asked: needed,
      left: needed - 1
    }))
  );
  assert_eq!(short.left(), needed - 1);
  let mut exact = Admission::new(needed);
  assert!(decode_answer(LS_REFS_RESPONSE, ObjectFormat::Sha1, &mut exact).is_ok());
  assert_eq!(exact.left(), 0);
}
