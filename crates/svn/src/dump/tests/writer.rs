//! The writer's own refusals, each leaving its output as it was, and the bytes of the record shapes the vectors do not
//! hold, each as `dump.c` writes it.

use super::support::{Owned, V2, digests, digests_hex, read_whole, write};
use crate::dump::{
  Contradiction, CopyFrom, DeltaBase, Digest, DumpError, Event, Format, Header, NodeAction, NodeHeader, Property,
  PropertyFault, PropertySection, RevisionHeader, TextSection, Uuid, Writer, section_len,
};
use crate::index::NodeKind;
use crate::svndiff::{Instruction, Version, Window};

/// A writer that has written a format-`format` version record and revision 1 with no properties.
fn in_revision_one(format: Format) -> (Writer, Vec<u8>) {
  let mut writer = Writer::new();
  let mut out = Vec::new();
  writer.write(&Event::Format(format), &mut out).unwrap();
  writer
    .write(
      &Event::Revision(RevisionHeader {
        number: 1,
        properties: None,
      }),
      &mut out,
    )
    .unwrap();
  (writer, out)
}

/// A file change of `path` with the sections given.
fn change(path: &[u8], properties: Option<PropertySection>, text: Option<TextSection>) -> NodeHeader {
  NodeHeader {
    path: path.to_vec(),
    kind: Some(NodeKind::File),
    action: NodeAction::Change,
    copy_from: None,
    properties,
    text,
  }
}

/// A whole text of `length` bytes with no checksums.
fn whole(length: u64) -> Option<TextSection> {
  Some(TextSection {
    delta: None,
    md5: None,
    sha1: None,
    length,
  })
}

/// Writes `event` and expects `refusal`, with nothing appended to what `writer` wrote before.
fn refuses(writer: &mut Writer, out: &mut Vec<u8>, event: Event<'_>, refusal: DumpError) {
  let before = out.clone();
  assert_eq!(writer.write(&event, out), Err(refusal), "{event:?}");
  assert_eq!(*out, before, "{event:?} appended on its refusal");
}

/// A-593 (writer): do write events where the stream cannot take them: a record before the version record, a node
/// outside a revision, a property, a text or an end where no section is open, a text past its stated length, a window
/// in a whole text, a section ended before its length, and the stream ended inside a record. Expect each refused, the
/// output as it was.
#[test]
fn an_event_out_of_its_place_is_refused_and_appends_nothing() {
  let mut writer = Writer::new();
  let mut out = Vec::new();
  refuses(&mut writer, &mut out, Event::PropertiesEnd, DumpError::OutOfOrder);
  writer.write(&Event::Format(Format::Two), &mut out).unwrap();
  refuses(
    &mut writer,
    &mut out,
    Event::Node(change(b"a", None, None)),
    DumpError::Contradiction {
      fault: Contradiction::NodeOutsideRevision,
    },
  );
  let (mut writer, mut out) = in_revision_one(Format::Two);
  refuses(&mut writer, &mut out, Event::Property(Property::Delete { name: b"p" }), DumpError::OutOfOrder);
  refuses(&mut writer, &mut out, Event::Text(b"x"), DumpError::OutOfOrder);
  refuses(&mut writer, &mut out, Event::TextEnd, DumpError::OutOfOrder);
  writer.write(&Event::Node(change(b"a", None, whole(3))), &mut out).unwrap();
  refuses(&mut writer, &mut out, Event::Text(b"four"), DumpError::OutOfOrder);
  refuses(&mut writer, &mut out, Event::Delta(Version::Zero), DumpError::OutOfOrder);
  refuses(&mut writer, &mut out, Event::Window(Window {
    source_offset: 0,
    source_len: 0,
    target_len: 0,
    instructions: Vec::new(),
    new_data: Vec::new(),
  }), DumpError::OutOfOrder);
  writer.write(&Event::Text(b"ab"), &mut out).unwrap();
  refuses(&mut writer, &mut out, Event::TextEnd, DumpError::OutOfOrder);
  refuses(&mut writer, &mut out, Event::Format(Format::Two), DumpError::OutOfOrder);
  assert_eq!(writer.finish(), Err(DumpError::OutOfOrder), "ended inside a text");
  writer.write(&Event::Text(b"c"), &mut out).unwrap();
  writer.write(&Event::TextEnd, &mut out).unwrap();
  assert_eq!(writer.finish(), Ok(()));
}

/// A-593 (writer): do write a node in revision 0, a deletion entry in a section that is no delta and in a revision's
/// properties, a property name holding a NUL, a path holding a line feed or a NUL, a revision past 2^63 - 1, a delta in
/// a format-2 stream, an addition of no kind, a property section too short for `PROPS-END`, and an entry past its
/// section. Expect each refused by kind as the reader would refuse it, the output as it was.
#[test]
fn what_the_reader_would_refuse_the_writer_refuses() {
  let mut writer = Writer::new();
  let mut out = Vec::new();
  writer.write(&Event::Format(Format::Three), &mut out).unwrap();
  writer
    .write(&Event::Revision(RevisionHeader { number: 0, properties: Some(10) }), &mut out)
    .unwrap();
  refuses(
    &mut writer,
    &mut out,
    Event::Property(Property::Delete { name: b"p" }),
    DumpError::Property {
      fault: PropertyFault::DeletionOutOfPlace,
    },
  );
  refuses(
    &mut writer,
    &mut out,
    Event::Property(Property::Set { name: b"a", value: b"" }),
    DumpError::Property {
      fault: PropertyFault::PastSection,
    },
  );
  writer.write(&Event::PropertiesEnd, &mut out).unwrap();
  refuses(
    &mut writer,
    &mut out,
    Event::Node(change(b"a", None, None)),
    DumpError::Contradiction {
      fault: Contradiction::NodeInRevisionZero,
    },
  );
  refuses(
    &mut writer,
    &mut out,
    Event::Revision(RevisionHeader { number: 1 << 63, properties: None }),
    DumpError::MalformedValue {
      header: Header::RevisionNumber,
    },
  );
  refuses(
    &mut writer,
    &mut out,
    Event::Revision(RevisionHeader { number: 1, properties: Some(9) }),
    DumpError::Contradiction {
      fault: Contradiction::PropertySectionTooShort,
    },
  );
  writer.write(&Event::Revision(RevisionHeader { number: 1, properties: None }), &mut out).unwrap();
  for path in [b"a\nb".as_slice(), b"a\0b"] {
    refuses(
      &mut writer,
      &mut out,
      Event::Node(change(path, None, None)),
      DumpError::MalformedValue { header: Header::NodePath },
    );
  }
  let mut no_kind = change(b"a", None, None);
  (no_kind.kind, no_kind.action) = (None, NodeAction::Add);
  refuses(&mut writer, &mut out, Event::Node(no_kind), DumpError::MissingHeader { header: Header::NodeKind });
  let section = PropertySection { delta: false, length: 20 };
  writer.write(&Event::Node(change(b"a", Some(section), None)), &mut out).unwrap();
  refuses(
    &mut writer,
    &mut out,
    Event::Property(Property::Delete { name: b"p" }),
    DumpError::Property { fault: PropertyFault::DeletionOutOfPlace },
  );
  refuses(
    &mut writer,
    &mut out,
    Event::Property(Property::Set { name: b"a\0", value: b"" }),
    DumpError::Property { fault: PropertyFault::NameNul },
  );
  let (mut two, mut two_out) = in_revision_one(Format::Two);
  let delta = Some(TextSection { delta: Some(DeltaBase::default()), md5: None, sha1: None, length: 4 });
  refuses(
    &mut two,
    &mut two_out,
    Event::Node(change(b"a", None, delta)),
    DumpError::Contradiction { fault: Contradiction::DeltaInFormatTwo },
  );
}

/// A-593 (writer, checksums): do write a whole text whose bytes are not the ones its MD5 names, then one whose SHA-1
/// is another text's. Expect each refused as a checksum mismatch at the text's end, as FSFS's read of a text refuses
/// what `svnadmin dump` would have written.
#[test]
fn a_whole_text_whose_digests_are_another_s_is_refused_at_its_end() {
  let (right, other) = (digests(b"text"), digests(b"other"));
  for (expected_md5, expected_sha1, refused) in [(right.0, right.1, false), (other.0, right.1, true), (right.0, other.1, true)] {
    let (mut writer, mut out) = in_revision_one(Format::Two);
    let text = Some(TextSection { delta: None, md5: Some(expected_md5), sha1: Some(expected_sha1), length: 4 });
    writer.write(&Event::Node(change(b"a", None, text)), &mut out).unwrap();
    writer.write(&Event::Text(b"text"), &mut out).unwrap();
    let ended = writer.write(&Event::TextEnd, &mut out);
    assert_eq!(ended.is_err(), refused, "{ended:?}");
    if let Err(refusal) = ended {
      assert!(matches!(refusal, DumpError::ChecksumMismatch { expected: Digest::Md5(_) | Digest::Sha1(_), .. }));
    }
  }
}

/// A-593 (writer, format): do write a revision with no property section, a UUID record, a node with both sections
/// whose property section `section_len` measured, a contentless change, a replacement without a copy and one by a copy.
/// Expect `dump.c`'s bytes for each (`svn_repos__dump_revision_record`, `svn_repos__dump_uuid_header_record`,
/// `dump_node`, `dump_node_delete`), and the reader to read them back to the same events.
#[test]
fn each_record_shape_is_written_as_dump_c_writes_it() {
  let uuid = Uuid::parse(b"7bf7a5ef-cabf-0310-b7d4-93df341afa7e").unwrap();
  let properties = [Property::Set { name: b"svn:keywords", value: b"Id" }];
  let section_length = section_len(properties).unwrap();
  let (md5, sha1) = digests_hex(b"text");
  let text_digests = digests(b"text");
  let events = vec![
    Owned::Format(Format::Two),
    Owned::Uuid(uuid),
    Owned::Revision(RevisionHeader { number: 0, properties: None }),
    Owned::Revision(RevisionHeader { number: 1, properties: None }),
    Owned::Node(change(b"f", Some(PropertySection { delta: false, length: section_length }), Some(TextSection {
      delta: None,
      md5: Some(text_digests.0),
      sha1: Some(text_digests.1),
      length: 4,
    }))),
    Owned::Set { name: b"svn:keywords".to_vec(), value: b"Id".to_vec() },
    Owned::PropertiesEnd,
    Owned::Text(b"text".to_vec()),
    Owned::TextEnd,
    Owned::Node(change(b"g", None, None)),
    Owned::Node(NodeHeader {
      path: b"d".to_vec(),
      kind: Some(NodeKind::Directory),
      action: NodeAction::Replace,
      copy_from: None,
      properties: Some(PropertySection { delta: false, length: 10 }),
      text: None,
    }),
    Owned::PropertiesEnd,
    Owned::Node(NodeHeader {
      path: b"e".to_vec(),
      kind: Some(NodeKind::Directory),
      action: NodeAction::Replace,
      copy_from: Some(CopyFrom { revision: 0, path: Vec::new(), md5: None, sha1: None }),
      properties: None,
      text: None,
    }),
  ];
  let expected = format!(
    "SVN-fs-dump-format-version: 2\n\nUUID: 7bf7a5ef-cabf-0310-b7d4-93df341afa7e\n\n\
     Revision-number: 0\n\n\nRevision-number: 1\n\n\n\
     Node-path: f\nNode-kind: file\nNode-action: change\nText-content-md5: {md5}\nText-content-sha1: {sha1}\n\
     Prop-content-length: 35\nText-content-length: 4\nContent-length: 39\n\n\
     K 12\nsvn:keywords\nV 2\nId\nPROPS-END\ntext\n\n\
     Node-path: g\nNode-kind: file\nNode-action: change\n\n\n\
     Node-path: d\nNode-kind: dir\nNode-action: replace\nProp-content-length: 10\nContent-length: 10\n\nPROPS-END\n\n\n\
     Node-path: e\nNode-action: delete\n\n\
     Node-path: e\nNode-kind: dir\nNode-action: add\nNode-copyfrom-rev: 0\nNode-copyfrom-path: \n\n\n"
  );
  assert_eq!(section_length, 35);
  let written = write(&events).unwrap();
  assert_eq!(String::from_utf8(written.clone()).unwrap(), expected);
  assert_eq!(read_whole(&written).unwrap(), events);
  assert!(written.starts_with(V2));
}

/// A-593 (writer, format 3): do write a node whose text is a delta of two windows against a base its checksums name and
/// whose property section changes the base's list. Expect `dump.c`'s header order (`Prop-delta`, `Text-delta`, the
/// base's checksums, the result's, then the lengths), the delta svndiff's version-0 encoding, and the reader to read
/// it back to the same events.
#[test]
fn a_delta_node_is_written_with_dump_c_s_header_order_and_read_back() {
  let windows = [
    Window { source_offset: 0, source_len: 2, target_len: 3, instructions: vec![Instruction::Source { offset: 0, length: 2 }, Instruction::New { length: 1 }], new_data: b"c".to_vec() },
    Window { source_offset: 2, source_len: 0, target_len: 1, instructions: vec![Instruction::New { length: 1 }], new_data: b"d".to_vec() },
  ];
  let mut delta = Vec::new();
  let mut encoder = crate::svndiff::Encoder::new(Version::Zero);
  delta.extend_from_slice(&encoder.header());
  for window in &windows {
    encoder.window(window, crate::svndiff::Compression::Stored, &mut delta).unwrap();
  }
  let base = digests(b"ab");
  let result = digests(b"abcd");
  let changes = [Property::Set { name: b"p", value: b"v" }, Property::Delete { name: b"q" }];
  let header = NodeHeader {
    path: b"f".to_vec(),
    kind: Some(NodeKind::File),
    action: NodeAction::Change,
    copy_from: None,
    properties: Some(PropertySection { delta: true, length: section_len(changes).unwrap() }),
    text: Some(TextSection {
      delta: Some(DeltaBase { md5: Some(base.0), sha1: Some(base.1) }),
      md5: Some(result.0),
      sha1: Some(result.1),
      length: delta.len() as u64,
    }),
  };
  let mut events = vec![
    Owned::Format(Format::Three),
    Owned::Revision(RevisionHeader { number: 1, properties: None }),
    Owned::Node(header),
    Owned::Set { name: b"p".to_vec(), value: b"v".to_vec() },
    Owned::Delete { name: b"q".to_vec() },
    Owned::PropertiesEnd,
    Owned::Delta(Version::Zero),
  ];
  events.extend(windows.iter().cloned().map(Owned::Window));
  events.push(Owned::TextEnd);
  let written = write(&events).unwrap();
  let text = String::from_utf8_lossy(&written);
  let order = ["Prop-delta: true", "Text-delta: true", "Text-delta-base-md5", "Text-delta-base-sha1", "Text-content-md5", "Text-content-sha1", "Prop-content-length", "Text-content-length", "Content-length"];
  let places: Vec<usize> = order.iter().map(|name| text.find(name).unwrap_or_else(|| panic!("no {name}"))).collect();
  assert!(places.windows(2).all(|pair| pair[0] < pair[1]), "{text}");
  assert!(written.windows(delta.len()).any(|window| window == delta.as_slice()), "the delta's bytes");
  assert_eq!(read_whole(&written).unwrap(), events);
}
