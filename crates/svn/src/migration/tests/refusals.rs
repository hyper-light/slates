//! Every refusal an import or an export makes, each by kind and by the number of the Subversion error `svnadmin load`
//! or `svnadmin dump` answers the same refusal with.

use super::support::{
  ADMISSION, CREATED_UUID, Dump, Node, ROOMY, VECTOR_UUID, created, created_within, digests,
  export, history, import, import_whole, import_with, properties, revision_properties,
};
use crate::dump::{CopyFrom, DeltaBase, Digest, DumpError, Format, NodeAction, NodeHeader, Uuid};
use crate::index::{IndexRefusal, NodeKind};
use crate::migration::{Exporter, Importer, MigrationRefusal, Properties, Repository, Room};
use crate::svndiff::{Instruction, SvndiffError, Window};

/// The text the first revision gives `f`.
const TEXT: &[u8] = b"a text\n";

/// A dump's revision 0 and a first revision adding the directory `d` and the file `f`, in format 2.
fn first_revision() -> Dump {
  first_revision_in(Format::Two)
}

/// A dump's revision 0 and a first revision adding the directory `d` and the file `f`, in `format`.
fn first_revision_in(format: Format) -> Dump {
  let mut dump = Dump::new(format, Some(VECTOR_UUID));
  dump
    .revision(
      0,
      &properties(&[("svn:date", "2005-04-07T22:13:00.000000Z")]),
    )
    .revision(1, &revision_properties(1))
    .node(&Node::add("d", NodeKind::Directory).properties(Properties::new()))
    .node(
      &Node::add("f", NodeKind::File)
        .properties(Properties::new())
        .text(TEXT),
    );
  dump
}

/// `dump`'s first revision, then a second whose records are `nodes`.
fn second_revision(nodes: &[Node]) -> Vec<u8> {
  let mut dump = first_revision();
  dump.revision(2, &revision_properties(2));
  for node in nodes {
    dump.node(node);
  }
  dump.bytes()
}

/// The refusal importing `dump` into a created repository meets, with the repository it leaves.
fn refusal(dump: &[u8]) -> (MigrationRefusal, Repository) {
  let mut repository = created();
  let refusal = import_whole(&mut repository, dump).expect_err("a refusal");
  (refusal, repository)
}

/// Holds a refusal to its expected kind and Subversion number.
fn refused_as(dump: &[u8], expected: &MigrationRefusal, code: u32) {
  let (refusal, _) = refusal(dump);
  assert_eq!(&refusal, expected);
  assert_eq!(refusal.subversion_code(), code, "{refusal:?}");
}

/// A-628 (refusals): do import a stream whose first revision is 2 into a created repository, and a stream of revision
/// 0 into a repository holding revision 1. Expect `OutOfSequence` (`E160006`) both times, and nothing imported.
#[test]
fn a_revision_that_does_not_continue_the_history_is_refused() {
  let mut skipping = Dump::new(Format::Two, None);
  skipping.revision(2, &revision_properties(2));
  let expected = MigrationRefusal::OutOfSequence {
    expected: 1,
    found: 2,
  };
  refused_as(&skipping.bytes(), &expected, 160_006);
  let mut repository = created();
  import_whole(&mut repository, &first_revision().bytes()).unwrap();
  let before = history(&repository);
  let refusal = import_whole(&mut repository, &first_revision().bytes()).unwrap_err();
  assert_eq!(
    refusal,
    MigrationRefusal::OutOfSequence {
      expected: 2,
      found: 0
    }
  );
  assert_eq!(history(&repository), before);
}

/// A-628 (refusals): do import a stream naming another UUID into a repository that holds history. Expect
/// `ForeignRepository` (`E125008`) with both UUIDs, and nothing imported.
#[test]
fn a_stream_of_another_repository_is_refused_once_history_is_held() {
  let mut repository = created();
  import_whole(&mut repository, &first_revision().bytes()).unwrap();
  let mut foreign = Dump::new(Format::Two, Some(CREATED_UUID));
  foreign
    .revision(2, &revision_properties(2))
    .node(&Node::change("f").text(b"changed"));
  let refusal = import_whole(&mut repository, &foreign.bytes()).unwrap_err();
  let expected = MigrationRefusal::ForeignRepository {
    repository: Uuid::parse(VECTOR_UUID).unwrap(),
    stream: Uuid::parse(CREATED_UUID).unwrap(),
  };
  assert_eq!(refusal, expected);
  assert_eq!(refusal.subversion_code(), 125_008);
  assert_eq!(repository.youngest(), 1);
}

/// A-628 (refusals): do import records whose paths hold a `..` name, or bytes that are not UTF-8. Expect
/// `PathSyntax` (`E160005`) naming the path as the stream gave it.
#[test]
fn a_path_fsfs_would_not_store_is_refused() {
  for path in [b"d/../f".as_slice(), b"d/\xff"] {
    let dump = second_revision(&[Node::add("x", NodeKind::Directory).raw_path(path)]);
    let expected = MigrationRefusal::PathSyntax {
      path: path.to_vec(),
    };
    refused_as(&dump, &expected, 160_005);
  }
}

/// A-628 (refusals): do import a record adding the root, one deleting it and one replacing it. Expect `RootChange`
/// (`E160021`) each time.
#[test]
fn the_root_is_never_added_deleted_or_replaced() {
  for node in [
    Node::add("", NodeKind::Directory),
    Node::delete(""),
    Node::replace("", NodeKind::Directory),
  ] {
    refused_as(
      &second_revision(&[node]),
      &MigrationRefusal::RootChange,
      160_021,
    );
  }
}

/// A-628 (refusals): do import a change of an absent path, a deletion of one, an addition beneath an absent directory
/// and a copy from a path absent in its revision. Expect `NotFound` (`E160013`) naming the path and, for the copy,
/// the revision looked in.
#[test]
fn a_path_absent_where_it_must_stand_is_refused() {
  let cases = [
    (Node::change("absent"), None, "/absent"),
    (Node::delete("absent"), None, "/absent"),
    (
      Node::add("absent/x", NodeKind::File).text(b"x"),
      None,
      "/absent",
    ),
    (
      Node::add("x", NodeKind::File).copied(1, "absent"),
      Some(1),
      "/absent",
    ),
  ];
  for (node, revision, path) in cases {
    let expected = MigrationRefusal::NotFound {
      revision,
      path: path.to_owned(),
    };
    refused_as(&second_revision(&[node]), &expected, 160_013);
  }
}

/// A-628 (refusals): do import an addition at a path that stands, and one beneath a file. Expect `AlreadyExists`
/// (`E160020`) and `NotDirectory` (`E160016`).
#[test]
fn an_addition_over_a_node_or_beneath_a_file_is_refused() {
  let over = second_revision(&[Node::add("f", NodeKind::File).text(b"x")]);
  let expected = MigrationRefusal::AlreadyExists {
    path: "/f".to_owned(),
  };
  refused_as(&over, &expected, 160_020);
  let beneath = second_revision(&[Node::add("f/x", NodeKind::File).text(b"x")]);
  let expected = MigrationRefusal::NotDirectory {
    path: "/f".to_owned(),
  };
  refused_as(&beneath, &expected, 160_016);
}

/// A-628 (refusals): do import a copy of a file stated as a directory, a change of a file stated as a directory, and a
/// text given to a directory by a change that states no kind. Expect `KindMismatch` (`E145001`) each time.
#[test]
fn a_record_whose_kind_is_not_its_nodes_is_refused() {
  let cases = [
    Node::add("x", NodeKind::Directory).copied(1, "f"),
    Node::change("f").kind(NodeKind::Directory),
    Node::change("d").text(b"a text on a directory"),
  ];
  for (node, path) in cases.into_iter().zip(["/x", "/f", "/d"]) {
    let expected = MigrationRefusal::KindMismatch {
      path: path.to_owned(),
    };
    refused_as(&second_revision(&[node]), &expected, 145_001);
  }
}

/// A-628 (refusals): do import a copy from revision 5 in revision 2. Expect `CopyFromFuture` (`E160006`).
#[test]
fn a_copy_from_a_revision_not_yet_made_is_refused() {
  let dump = second_revision(&[Node::add("x", NodeKind::File).copied(5, "f")]);
  refused_as(
    &dump,
    &MigrationRefusal::CopyFromFuture { revision: 5 },
    160_006,
  );
}

/// A-628 (refusals): do import a copy whose `Text-copy-source-md5` is not the source's, a delta whose base MD5 is not
/// the base's, and a delta whose result MD5 is not the result's. Expect `CopySourceChecksumMismatch`,
/// `BaseChecksumMismatch` and `ChecksumMismatch`, each `E200014` with the digest named and the one found.
#[test]
fn every_checksum_the_stream_carries_is_checked() {
  let wrong = digests(b"not the text");
  let (md5, sha1) = digests(TEXT);
  let mut copy = first_revision();
  copy
    .revision(2, &revision_properties(2))
    .header(NodeHeader {
      path: b"x".to_vec(),
      kind: Some(NodeKind::File),
      action: NodeAction::Add,
      copy_from: Some(CopyFrom {
        revision: 1,
        path: b"f".to_vec(),
        md5: Some(wrong.0),
        sha1: None,
      }),
      properties: None,
      text: None,
    });
  let (refusal, _) = refusal(&copy.bytes());
  let expected = MigrationRefusal::CopySourceChecksumMismatch {
    path: "/x".to_owned(),
    expected: Digest::Md5(wrong.0),
    actual: Digest::Md5(md5),
  };
  assert_eq!(refusal, expected);
  assert_eq!(refusal.subversion_code(), 200_014);
  delta_checksums_are_checked(wrong, (md5, sha1));
}

/// The delta halves of [`every_checksum_the_stream_carries_is_checked`].
fn delta_checksums_are_checked(wrong: ([u8; 16], [u8; 20]), base: ([u8; 16], [u8; 20])) {
  let window = whole_copy_window(TEXT.len() as u64);
  let named_base = DeltaBase {
    md5: Some(base.0),
    sha1: Some(base.1),
  };
  let wrong_base = DeltaBase {
    md5: Some(wrong.0),
    sha1: None,
  };
  let mut dump = first_revision_in(Format::Three);
  dump.revision(2, &revision_properties(2)).delta(
    "f",
    wrong_base,
    std::slice::from_ref(&window),
    base,
  );
  let expected = MigrationRefusal::BaseChecksumMismatch {
    path: "/f".to_owned(),
    expected: Digest::Md5(wrong.0),
    actual: Digest::Md5(base.0),
  };
  refused_as(&dump.bytes(), &expected, 200_014);
  let mut dump = first_revision_in(Format::Three);
  dump
    .revision(2, &revision_properties(2))
    .delta("f", named_base, &[window], wrong);
  let expected = MigrationRefusal::ChecksumMismatch {
    path: "/f".to_owned(),
    expected: Digest::Md5(wrong.0),
    actual: Digest::Md5(base.0),
  };
  refused_as(&dump.bytes(), &expected, 200_014);
}

/// A window copying the first `length` bytes of its base.
fn whole_copy_window(length: u64) -> Window {
  Window {
    source_offset: 0,
    source_len: length,
    target_len: length,
    instructions: vec![Instruction::Source { offset: 0, length }],
    new_data: Vec::new(),
  }
}

/// A-628 (refusals): do import a delta whose window reads past its base. Expect `Delta(SourceShort)`, svndiff's own
/// refusal and number.
#[test]
fn a_delta_that_reads_past_its_base_is_refused() {
  let past = whole_copy_window(TEXT.len() as u64 + 1);
  let mut dump = first_revision_in(Format::Three);
  dump.revision(2, &revision_properties(2)).delta(
    "f",
    DeltaBase::default(),
    &[past],
    digests(b"anything"),
  );
  let expected = MigrationRefusal::Delta(SvndiffError::SourceShort);
  refused_as(
    &dump.bytes(),
    &expected,
    SvndiffError::SourceShort.subversion_code(),
  );
}

/// A-628 (refusals): do import the first revision into a repository whose room holds revision 0 and the first
/// revision's properties but not its text, and into one whose index room holds fewer entries than its nodes. Expect
/// `Room` (`E160000`) at the text and the index's own `Room`, the repository unchanged both times.
#[test]
fn an_import_past_the_room_is_refused() {
  let dump = first_revision().bytes();
  let mut zero = created();
  let mut only_zero = Dump::new(Format::Two, None);
  only_zero.revision(
    0,
    &properties(&[("svn:date", "2005-04-07T22:13:00.000000Z")]),
  );
  import_whole(&mut zero, &only_zero.bytes()).unwrap();
  let first_properties: u64 = revision_properties(1)
    .iter()
    .map(|(name, value)| (name.len() + value.len()) as u64)
    .sum();
  let bytes = zero.held() + first_properties;
  let mut small = created_within(Room {
    entries: ROOMY.entries,
    bytes,
  });
  let refusal = import_whole(&mut small, &dump).unwrap_err();
  let expected = MigrationRefusal::Room {
    needed: bytes + TEXT.len() as u64,
    room: bytes,
  };
  assert_eq!(refusal, expected);
  assert_eq!(refusal.subversion_code(), 160_000);
  assert_eq!(small.youngest(), 0);
  let mut few = created_within(Room {
    entries: 2,
    bytes: ROOMY.bytes,
  });
  let refusal = import_whole(&mut few, &dump).unwrap_err();
  assert!(
    matches!(refusal, MigrationRefusal::Index(IndexRefusal::Room { .. })),
    "{refusal:?}"
  );
  assert_eq!(few.youngest(), 0);
}

/// A-628 (refusals): do import a stream cut inside its second revision, a byte at a time, then call the import again.
/// Expect the codec's `Truncated` (`E200003`), the first revision kept and nothing of the second, and the same refusal
/// on the next call.
#[test]
fn a_refused_import_keeps_the_revisions_before_it_and_answers_again_the_same() {
  let whole = second_revision(&[Node::change("f").text(b"changed")]);
  let cut = &whole[..whole.len() - 3];
  let mut repository = created();
  let mut importer = Importer::new(ADMISSION);
  let refusal = import_with(&mut importer, &mut repository, cut, 1).unwrap_err();
  assert_eq!(refusal, MigrationRefusal::Stream(DumpError::Truncated));
  assert_eq!(refusal.subversion_code(), 200_003);
  assert_eq!(repository.youngest(), 1);
  let again = importer.import(&mut repository, &whole, true).unwrap_err();
  assert_eq!(again, refusal);
  let mut fresh = created();
  import(&mut fresh, &whole, 1).unwrap();
  assert_eq!(fresh.youngest(), 2);
}

/// A-628 (refusals): do export a range whose first revision is past its last, and one past the youngest revision in
/// each format. Expect `BadRange` (`E165004`) each time.
#[test]
fn an_export_of_a_range_the_repository_cannot_give_is_refused() {
  let mut repository = created();
  import_whole(&mut repository, &first_revision().bytes()).unwrap();
  let backward = export(&repository, 1, 0).unwrap_err();
  assert_eq!(
    backward,
    MigrationRefusal::BadRange {
      first: 1,
      last: 0,
      youngest: 1
    }
  );
  assert_eq!(backward.subversion_code(), 165_004);
  let past = export(&repository, 0, 2).unwrap_err();
  assert_eq!(
    past,
    MigrationRefusal::BadRange {
      first: 0,
      last: 2,
      youngest: 1
    }
  );
  let deltas = Exporter::new(&repository, 0, 2, Format::Three).unwrap_err();
  assert_eq!(deltas, past);
}
