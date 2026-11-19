//! Generated streams: every record shape the format holds, in both formats, written by the writer, read back by the
//! reader in pieces of any size, and written again.

use proptest::prelude::*;

use super::support::{Owned, ROOMY, digests, read, read_whole, write};
use crate::dump::{
  CopyFrom, DeltaBase, Format, NodeAction, NodeHeader, Property, PropertySection, RevisionHeader, TextSection, Uuid,
  section_len,
};
use crate::index::NodeKind;
use crate::svndiff::{Compression, Encoder, Instruction, Version, Window};

/// One property entry: a name and a value to set, or a name to delete when the value is `None`.
type Entry = (Vec<u8>, Option<Vec<u8>>);

/// A node record before the format's rules shape it (`build_node`).
#[derive(Clone, Debug)]
struct NodePlan {
  path: Vec<u8>,
  shape: u8,
  copy: Option<(u64, Vec<u8>, bool)>,
  properties: Option<(bool, Vec<Entry>)>,
  text: Option<(bool, Vec<u8>, u8)>,
}

/// A stream before it is made into events.
#[derive(Clone, Debug)]
struct StreamPlan {
  three: bool,
  uuid: Option<[u8; 16]>,
  revisions: Vec<(Option<Vec<(Vec<u8>, Vec<u8>)>>, Vec<NodePlan>)>,
}

/// Bytes that may stand in a header's value: anything but a line feed and a NUL.
fn header_bytes(most: usize) -> impl Strategy<Value = Vec<u8>> {
  prop::collection::vec(prop_oneof![1u8..=9, 11u8..=255], 0..most)
}

/// A property name: anything but a NUL.
fn name() -> impl Strategy<Value = Vec<u8>> {
  prop::collection::vec(1u8..=255, 0..6)
}

/// Any bytes.
fn bytes(most: usize) -> impl Strategy<Value = Vec<u8>> {
  prop::collection::vec(any::<u8>(), 0..most)
}

fn node_plan() -> impl Strategy<Value = NodePlan> {
  (
    header_bytes(10),
    any::<u8>(),
    prop::option::of((0u64..1000, header_bytes(8), any::<bool>())),
    prop::option::of((any::<bool>(), prop::collection::vec((name(), prop::option::of(bytes(12))), 0..4))),
    prop::option::of((any::<bool>(), bytes(300), any::<u8>())),
  )
    .prop_map(|(path, shape, copy, properties, text)| NodePlan {
      path,
      shape,
      copy,
      properties,
      text,
    })
}

fn stream_plan() -> impl Strategy<Value = StreamPlan> {
  (
    any::<bool>(),
    prop::option::of(any::<[u8; 16]>()),
    prop::collection::vec(
      (
        prop::option::of(prop::collection::vec((name(), bytes(16)), 0..3)),
        prop::collection::vec(node_plan(), 0..5),
      ),
      0..4,
    ),
  )
    .prop_map(|(three, uuid, revisions)| StreamPlan {
      three,
      uuid,
      revisions,
    })
}

/// A UUID made of 16 bytes, in its record's form.
fn uuid_of(bytes: [u8; 16]) -> Uuid {
  let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
  let text = format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..]);
  Uuid::parse(text.as_bytes()).unwrap()
}

/// A property section of `entries`, a deletion kept only in a delta and a set otherwise, with its events.
fn property_events(delta: bool, entries: &[Entry]) -> (PropertySection, Vec<Owned>) {
  let owned: Vec<Owned> = entries
    .iter()
    .map(|(name, value)| match (value, delta) {
      (None, true) => Owned::Delete { name: name.clone() },
      (value, _) => Owned::Set {
        name: name.clone(),
        value: value.clone().unwrap_or_default(),
      },
    })
    .collect();
  let properties = owned.iter().map(|event| match event {
    Owned::Set { name, value } => Property::Set { name, value },
    Owned::Delete { name } => Property::Delete { name },
    other => panic!("{other:?}"),
  });
  let length = section_len(properties).unwrap();
  let mut events = owned;
  events.push(Owned::PropertiesEnd);
  (PropertySection { delta, length }, events)
}

/// A whole text of `bytes` whose checksums, by `mode`, are none, right, or all zeros (which name none), with its
/// events.
fn whole_text(bytes: &[u8], mode: u8) -> (TextSection, Vec<Owned>) {
  let (md5, sha1) = digests(bytes);
  let (md5, sha1) = match mode % 3 {
    0 => (None, None),
    1 => (Some(md5), Some(sha1)),
    _ => (Some([0; 16]), Some([0; 20])),
  };
  let section = TextSection {
    delta: None,
    md5,
    sha1,
    length: bytes.len() as u64,
  };
  let mut events = Vec::new();
  if !bytes.is_empty() {
    events.push(Owned::Text(bytes.to_vec()));
  }
  events.push(Owned::TextEnd);
  (section, events)
}

/// A delta that makes `bytes` from new data alone, in windows of at most `(mode % 4 + 1) * 16` bytes, its base named
/// by `mode`'s low bit, with its events.
fn delta_text(bytes: &[u8], mode: u8) -> (TextSection, Vec<Owned>) {
  let size = usize::from(mode % 4 + 1) * 16;
  let windows: Vec<Window> = bytes
    .chunks(size)
    .map(|chunk| Window {
      source_offset: 0,
      source_len: 0,
      target_len: chunk.len() as u64,
      instructions: vec![Instruction::New {
        length: chunk.len() as u64,
      }],
      new_data: chunk.to_vec(),
    })
    .collect();
  let mut encoder = Encoder::new(Version::Zero);
  let mut encoded = encoder.header().to_vec();
  for window in &windows {
    encoder.window(window, Compression::Stored, &mut encoded).unwrap();
  }
  let (md5, sha1) = digests(bytes);
  let base = if mode % 2 == 0 {
    DeltaBase::default()
  } else {
    DeltaBase {
      md5: Some(digests(b"base").0),
      sha1: Some(digests(b"base").1),
    }
  };
  let section = TextSection {
    delta: Some(base),
    md5: Some(md5),
    sha1: Some(sha1),
    length: encoded.len() as u64,
  };
  let mut events = vec![Owned::Delta(Version::Zero)];
  events.extend(windows.into_iter().map(Owned::Window));
  events.push(Owned::TextEnd);
  (section, events)
}

/// The events of a node record `plan` names, shaped to the format's rules: an action and a kind from `shape`, a kind
/// for every addition and replacement, nothing but a path on a deletion, no copy on a change, no text on a directory,
/// no delta in format 2, a deletion entry only in a delta.
fn build_node(plan: &NodePlan, format: Format) -> Vec<Owned> {
  let action = [NodeAction::Change, NodeAction::Add, NodeAction::Delete, NodeAction::Replace][usize::from(plan.shape % 4)];
  let mut kind = [None, Some(NodeKind::File), Some(NodeKind::Directory)][usize::from(plan.shape / 4 % 3)];
  if matches!(action, NodeAction::Add | NodeAction::Replace) && kind.is_none() {
    kind = Some(NodeKind::File);
  }
  let three = format == Format::Three;
  let mut header = NodeHeader {
    path: plan.path.clone(),
    kind,
    action,
    copy_from: None,
    properties: None,
    text: None,
  };
  let mut sections = Vec::new();
  if action == NodeAction::Delete {
    return vec![Owned::Node(header)];
  }
  if action != NodeAction::Change {
    header.copy_from = plan.copy.as_ref().map(|(revision, path, checksums)| CopyFrom {
      revision: *revision,
      path: path.clone(),
      md5: checksums.then(|| digests(path).0),
      sha1: checksums.then(|| digests(path).1),
    });
  }
  if let Some((delta, entries)) = &plan.properties {
    let (section, events) = property_events(*delta && three, entries);
    header.properties = Some(section);
    sections.extend(events);
  }
  if let Some((delta, bytes, mode)) = &plan.text
    && kind != Some(NodeKind::Directory)
  {
    let (section, events) = if *delta && three { delta_text(bytes, *mode) } else { whole_text(bytes, *mode) };
    header.text = Some(section);
    sections.extend(events);
  }
  let mut events = vec![Owned::Node(header)];
  events.extend(sections);
  events
}

/// The events of a stream `plan` names. An addition right after a deletion of its path is given another path: the
/// reader reads that pair as the one replacement, which is another stream.
fn build(plan: &StreamPlan) -> Vec<Owned> {
  let format = if plan.three { Format::Three } else { Format::Two };
  let mut events = vec![Owned::Format(format)];
  events.extend(plan.uuid.map(|bytes| Owned::Uuid(uuid_of(bytes))));
  let mut revisions = vec![(Some(Vec::new()), Vec::new())];
  revisions.extend(plan.revisions.iter().cloned());
  for (number, (properties, nodes)) in revisions.iter().enumerate() {
    let entries: Option<Vec<Entry>> =
      properties.as_ref().map(|list| list.iter().map(|(name, value)| (name.clone(), Some(value.clone()))).collect());
    let section = entries.as_ref().map(|entries| property_events(false, entries));
    events.push(Owned::Revision(RevisionHeader {
      number: number as u64,
      properties: section.as_ref().map(|(section, _)| section.length),
    }));
    events.extend(section.map(|(_, events)| events).unwrap_or_default());
    if number == 0 {
      continue;
    }
    let mut deleted: Option<Vec<u8>> = None;
    for plan in nodes {
      let mut node = build_node(plan, format);
      if let Some(Owned::Node(header)) = node.first_mut() {
        if header.action == NodeAction::Add && deleted.as_ref() == Some(&header.path) {
          header.path.push(b'+');
        }
        deleted = (header.action == NodeAction::Delete).then(|| header.path.clone());
      }
      events.extend(node);
    }
  }
  events
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::default()))]

  /// A-593 (generated): do write a generated stream (either format; revisions with and without properties; nodes of
  /// every action and kind, copies with and without checksums, property sections whole and as changes, whole texts with
  /// right, absent and all-zero checksums, deltas of several windows), then read it whole and in pieces of a random
  /// size. Expect the generated events back both ways, and the events read written again to the same bytes.
  #[test]
  fn a_generated_stream_is_read_back_to_its_events_and_written_again_to_its_bytes(
    plan in stream_plan(), piece in 1usize..64
  ) {
    let events = build(&plan);
    let bytes = write(&events).unwrap();
    let read_back = read_whole(&bytes).unwrap();
    prop_assert_eq!(&read_back, &events);
    prop_assert_eq!(&read(&bytes, piece, ROOMY).unwrap().events, &events);
    prop_assert_eq!(write(&read_back).unwrap(), bytes);
  }
}
