//! Golden vectors: dumps stock `svnadmin` 1.15.0 (r1937767) wrote of one repository (`fixtures/dump`, made by
//! `fixtures/dump/make-fixtures.sh`, whose header names the command, the machine and the date), each checked against
//! its recorded SHA-256 before it is read, with `svnlook changed`'s account of every revision beside them.

use std::collections::BTreeMap;

use super::support::{Owned, ROOMY, Reading, hex, read, read_whole, write};
use crate::dump::{
  Admission, Digests, DumpError, Format, NodeAction, NodeHeader, RevisionHeader, check_text,
};
use crate::index::NodeKind;
use crate::svndiff::{WINDOW_LIMIT, Window, apply_all, deltas};
use std::num::NonZeroUsize;

/// Format 2, every revision whole (`svnadmin dump`).
const FULL: &[u8] = include_bytes!("../../../fixtures/dump/full.dump");
/// Format 3, every revision (`svnadmin dump --deltas`).
const DELTAS: &[u8] = include_bytes!("../../../fixtures/dump/deltas.dump");
/// Format 2, revisions 0 to 3 (`svnadmin dump -r 0:3`).
const PART1: &[u8] = include_bytes!("../../../fixtures/dump/part1.dump");
/// Format 3, revisions 4 on, as changes (`svnadmin dump --deltas --incremental -r 4:13`).
const PART2: &[u8] = include_bytes!("../../../fixtures/dump/part2.dump");
/// `svnlook changed --copy-info` for every revision.
const CHANGED: &[u8] = include_bytes!("../../../fixtures/dump/changed.txt");
/// The first line of `svnadmin --version`.
const VERSION: &[u8] = include_bytes!("../../../fixtures/dump/svn-version.txt");
/// The SHA-256 of every file the script wrote.
const SUMS: &str = include_str!("../../../fixtures/dump/SHA256SUMS");

/// The vector named `name`, refused unless its SHA-256 is the one recorded when `svnadmin` wrote it.
pub(super) fn vector(name: &str) -> &'static [u8] {
  let bytes = match name {
    "full.dump" => FULL,
    "deltas.dump" => DELTAS,
    "part1.dump" => PART1,
    "part2.dump" => PART2,
    "changed.txt" => CHANGED,
    "svn-version.txt" => VERSION,
    other => panic!("no vector {other}"),
  };
  let recorded = SUMS
    .lines()
    .find_map(|line| {
      line
        .split_once("  ")
        .filter(|(_, file)| *file == name)
        .map(|(sum, _)| sum)
    })
    .unwrap_or_else(|| panic!("{name} is not in SHA256SUMS"));
  let sum = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, bytes);
  assert_eq!(
    hex(sum.as_ref()),
    recorded,
    "{name} is not the vector svnadmin wrote"
  );
  bytes
}

/// Every dump vector, and the stream two of them make one after the other.
fn dumps() -> Vec<(&'static str, Vec<u8>)> {
  let mut dumps: Vec<(&'static str, Vec<u8>)> =
    ["full.dump", "deltas.dump", "part1.dump", "part2.dump"]
      .into_iter()
      .map(|name| (name, vector(name).to_vec()))
      .collect();
  dumps.push((
    "part1.dump then part2.dump",
    [vector("part1.dump"), vector("part2.dump")].concat(),
  ));
  dumps
}

/// A-593 (golden): do read every dump stock `svnadmin` wrote, whole, a byte at a time and in pieces of 7 and 4,096
/// bytes, and write the events again. Expect the same events however the bytes arrive, and the writer's bytes to be
/// `svnadmin`'s byte for byte: every record shape it writes (both formats, a replacement by a copy, the root's empty
/// path, a binary text, an empty one, a delta of two windows, deleted properties) and the two version records of
/// dumps written one after the other.
#[test]
fn every_dump_svnadmin_wrote_is_read_and_written_again_byte_for_byte() {
  let version = String::from_utf8_lossy(vector("svn-version.txt"));
  for (name, dump) in dumps() {
    let events =
      read_whole(&dump).unwrap_or_else(|refusal| panic!("{name} ({version}): {refusal:?}"));
    for piece in [1, 7, 4096] {
      let pieces = read(&dump, piece, ROOMY)
        .unwrap_or_else(|refusal| panic!("{name} in {piece}s: {refusal:?}"));
      assert_eq!(pieces.events, events, "{name} read in pieces of {piece}");
    }
    let written = write(&events).unwrap_or_else(|refusal| panic!("{name} written: {refusal:?}"));
    assert!(
      written == dump,
      "{name} ({version}) written again differs from svnadmin's {}",
      first_difference(&written, &dump)
    );
  }
}

/// A-593 (golden, admission): do read each dump a byte at a time within an admission no dump comes near, which finds
/// the most bytes it ever needs held at once, then within exactly that many and within one fewer, both whole and a
/// byte at a time. Expect the first to read, and the second refused as past the admission at the same length however
/// the bytes arrive: every length is checked against the admission when it is read, before its bytes are waited for.
#[test]
fn the_tightest_admission_reads_a_dump_and_one_byte_less_is_refused() {
  for (name, dump) in dumps() {
    let most = read(&dump, 1, ROOMY).unwrap().most_needed;
    assert!(most > 1, "{name}: the admission was never tested");
    let tight = Admission { buffer_bytes: most };
    let short = Admission {
      buffer_bytes: most - 1,
    };
    for piece in [1, usize::MAX] {
      assert!(read(&dump, piece, tight).is_ok(), "{name} within {most}");
      assert_eq!(
        read(&dump, piece, short).unwrap_err(),
        DumpError::PastAdmission {
          needed: most,
          admitted: most - 1
        },
        "{name} within {} in pieces of {piece}",
        most - 1
      );
    }
  }
}

/// A node of the oracle's naive filesystem.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Entry {
  directory: bool,
  text: Vec<u8>,
  properties: BTreeMap<Vec<u8>, Vec<u8>>,
}

/// A whole tree, path to entry; the root is the empty path.
type Tree = BTreeMap<Vec<u8>, Entry>;

/// Whether `path` is `ancestor` or beneath it.
fn at_or_beneath(path: &[u8], ancestor: &[u8]) -> bool {
  ancestor.is_empty()
    || path == ancestor
    || (path.len() > ancestor.len() && path.starts_with(ancestor) && path[ancestor.len()] == b'/')
}

/// What the oracle counted, so a vector that stopped holding a case cannot pass unnoticed.
#[derive(Clone, Debug, Default)]
struct Counts {
  text_deltas: usize,
  most_windows: usize,
  property_deltas: usize,
  deleted_properties: usize,
  copies: usize,
  replacements_by_copy: usize,
  root_records: usize,
  /// Text deltas whose windows the deltifier made again from their base and their text (A-652), and the first that it
  /// did not.
  remade_deltas: usize,
  first_unremade: Option<String>,
}

/// A naive filesystem that applies one dump's events revision by revision, the oracle's other leg: format 2's whole
/// texts and lists set, format 3's deltas applied to their bases with svndiff's reference applier and its property
/// changes applied to the base's list. It keeps every revision's tree, since a copy reads its source's revision.
struct Replay {
  trees: Vec<Tree>,
  tree: Tree,
  node: Option<NodeHeader>,
  delta_base: Vec<u8>,
  windows: Vec<Window>,
  counts: Counts,
}

impl Replay {
  fn new() -> Self {
    let mut root = Tree::new();
    root.insert(
      Vec::new(),
      Entry {
        directory: true,
        ..Entry::default()
      },
    );
    Self {
      trees: Vec::new(),
      tree: root,
      node: None,
      delta_base: Vec::new(),
      windows: Vec::new(),
      counts: Counts::default(),
    }
  }

  /// Applies one event.
  fn apply(&mut self, event: &Owned) {
    match event {
      Owned::Format(_) | Owned::Uuid(_) | Owned::PropertiesEnd => {}
      Owned::Revision(header) => self.revision(header),
      Owned::Node(header) => self.node(header),
      Owned::Set { .. } | Owned::Delete { .. } if self.node.is_none() => {}
      Owned::Set { name, value } => {
        self
          .current()
          .properties
          .insert(name.clone(), value.clone());
      }
      Owned::Delete { name } => {
        self.counts.deleted_properties += 1;
        assert!(
          self.current().properties.remove(name).is_some(),
          "deleted {name:?}, which the base lacks"
        );
      }
      Owned::Text(bytes) => self.current().text = bytes.clone(),
      Owned::Delta(_) => self.windows.clear(),
      Owned::Window(window) => self.windows.push(window.clone()),
      Owned::TextEnd => self.text_end(),
    }
  }

  /// A revision record: the tree so far is the previous revision's, kept; the revision's own properties, which follow,
  /// are no node's.
  fn revision(&mut self, header: &RevisionHeader) {
    self.node = None;
    if header.number > 0 {
      assert_eq!(
        self.trees.len() as u64 + 1,
        header.number,
        "revisions out of order"
      );
      self.trees.push(self.tree.clone());
    }
  }

  /// The revision the oracle is building: the trees kept so far.
  fn building(&self) -> usize {
    self.trees.len()
  }

  /// A node record: its action applied to the tree; its sections follow.
  fn node(&mut self, header: &NodeHeader) {
    let path = header.path.clone();
    if path.is_empty() {
      self.counts.root_records += 1;
    }
    if matches!(header.action, NodeAction::Delete | NodeAction::Replace) {
      assert!(self.tree.contains_key(&path), "{path:?} deleted absent");
      self
        .tree
        .retain(|existing, _| !at_or_beneath(existing, &path));
    }
    if matches!(header.action, NodeAction::Add | NodeAction::Replace) {
      self.add(header);
    }
    self.node = Some(header.clone());
    if header.properties.is_some_and(|section| !section.delta) {
      self.current().properties.clear();
    }
    if header.properties.is_some_and(|section| section.delta) {
      self.counts.property_deltas += 1;
    }
    self.delta_base = self
      .tree
      .get(&path)
      .map(|entry| entry.text.clone())
      .unwrap_or_default();
    if header.text.is_some_and(|section| section.delta.is_none()) {
      self.current().text.clear();
    }
  }

  /// An addition or a replacement's addition: a copy of its source's subtree in the source's revision, or a new node.
  fn add(&mut self, header: &NodeHeader) {
    let path = header.path.clone();
    assert!(!self.tree.contains_key(&path), "{path:?} added present");
    let Some(copy) = &header.copy_from else {
      let directory = header.kind == Some(NodeKind::Directory);
      self.tree.insert(
        path,
        Entry {
          directory,
          ..Entry::default()
        },
      );
      return;
    };
    self.counts.copies += 1;
    if header.action == NodeAction::Replace {
      self.counts.replacements_by_copy += 1;
    }
    assert!(
      (copy.revision as usize) < self.building(),
      "a copy from its own revision or later"
    );
    let source = &self.trees[copy.revision as usize];
    let copied: Vec<(Vec<u8>, Entry)> = source
      .iter()
      .filter(|(existing, _)| at_or_beneath(existing, &copy.path))
      .map(|(existing, entry)| {
        let mut moved = path.clone();
        moved.extend_from_slice(&existing[copy.path.len()..]);
        (moved, entry.clone())
      })
      .collect();
    assert!(
      !copied.is_empty(),
      "copy of {:?}@{}, absent",
      copy.path,
      copy.revision
    );
    if let (Some(md5), Some(entry)) = (copy.md5, source.get(&copy.path)) {
      assert_eq!(
        Some(md5),
        digests(&entry.text).0.into(),
        "Text-copy-source-md5 of {:?}",
        copy.path
      );
    }
    self.tree.extend(copied);
  }

  /// The node the current record names.
  fn current(&mut self) -> &mut Entry {
    let path = self
      .node
      .as_ref()
      .expect("a section outside a node")
      .path
      .clone();
    self
      .tree
      .get_mut(&path)
      .expect("a section of a node the tree lacks")
  }

  /// The end of a text: a delta's windows applied to the node's base, both checked against their headers' digests.
  fn text_end(&mut self) {
    let node = self.node.clone().expect("a text outside a node");
    let text = node.text.expect("a text without a section");
    if let Some(base) = text.delta {
      self.counts.text_deltas += 1;
      self.counts.most_windows = self.counts.most_windows.max(self.windows.len());
      check_text(base.md5, base.sha1, digests(&self.delta_base)).expect("the base the delta names");
      let made = apply_all(&self.delta_base, &self.windows).expect("the delta applies to its base");
      self.remake(&node.path, &made);
      self.current().text = made;
    }
    let made = self.current().text.clone();
    check_text(text.md5, text.sha1, digests(&made)).expect("the text its header names");
  }

  /// Gives the deltifier the delta's base and the text it made, and counts whether its windows are the recorded ones.
  fn remake(&mut self, path: &[u8], made: &[u8]) {
    let room = NonZeroUsize::new(usize::try_from(WINDOW_LIMIT).unwrap()).unwrap();
    let remade: Vec<Window> = deltas(&self.delta_base, made, room).unwrap().collect();
    if remade == self.windows {
      self.counts.remade_deltas += 1;
    } else if self.counts.first_unremade.is_none() {
      self.counts.first_unremade = Some(format!(
        "{} in revision {}: remade {remade:?}, recorded {:?}",
        String::from_utf8_lossy(path),
        self.building(),
        self.windows
      ));
    }
  }

  /// Ends the stream: the last revision's tree kept.
  fn finish(mut self) -> (Vec<Tree>, Counts) {
    self.trees.push(self.tree.clone());
    (self.trees, self.counts)
  }
}

/// The MD5 and SHA-1 of `text`.
fn digests(text: &[u8]) -> ([u8; 16], [u8; 20]) {
  let mut digests = Digests::new().unwrap();
  digests.update(text).unwrap();
  digests.finish().unwrap()
}

/// Replays `events` into every revision's tree.
fn replay(events: &[Owned]) -> (Vec<Tree>, Counts) {
  let mut replay = Replay::new();
  for event in events {
    replay.apply(event);
  }
  replay.finish()
}

/// A-593 (golden, oracle): do replay format 2's dump, whole texts and whole lists, and format 3's, every text a delta
/// applied to its base with svndiff's reference applier and every property section a change to its base's list, into
/// a naive filesystem each. Expect the same tree in every revision, every base and result to have the digests their
/// headers name, and the vectors to hold what they were made to hold: deltas of two windows, deleted properties,
/// copies, replacements by a copy, records of the root.
#[test]
fn format_three_s_deltas_rebuild_format_two_s_trees_revision_by_revision() {
  let (whole_trees, whole_counts) = replay(&read_whole(vector("full.dump")).unwrap());
  let (delta_trees, counts) = replay(&read_whole(vector("deltas.dump")).unwrap());
  assert_eq!(whole_trees.len(), delta_trees.len(), "revisions");
  for (revision, (whole, deltas)) in whole_trees.iter().zip(&delta_trees).enumerate() {
    assert!(whole == deltas, "revision {revision}'s trees differ");
  }
  assert_eq!(whole_counts.text_deltas, 0, "format 2 holds no deltas");
  assert!(counts.text_deltas >= 10, "{counts:?}");
  assert!(counts.most_windows >= 2, "{counts:?}");
  assert!(
    counts.property_deltas >= 4 && counts.deleted_properties >= 2,
    "{counts:?}"
  );
  assert!(
    counts.copies >= 8 && counts.replacements_by_copy >= 4,
    "{counts:?}"
  );
  assert!(counts.root_records >= 2, "{counts:?}");
}

/// A-652 (golden): do replay `deltas.dump`, and `part1.dump` then `part2.dump`, and give Subversion's deltifier, as
/// ported, each text delta's base and the text it makes. Expect the windows stock `svnadmin` 1.15.0 wrote, every one:
/// its source and target views, its instructions and its new data (two-window deltas, copies changed, texts added
/// against the empty text among them).
#[test]
fn the_deltifier_makes_every_window_svnadmin_dump_deltas_wrote() {
  let parts = [vector("part1.dump"), vector("part2.dump")].concat();
  for (name, dump) in [
    ("deltas.dump", vector("deltas.dump")),
    ("part1.dump, part2.dump", &parts[..]),
  ] {
    let (_, counts) = replay(&read_whole(dump).unwrap());
    assert_eq!(counts.first_unremade, None, "{name}");
    assert_eq!(counts.remade_deltas, counts.text_deltas, "{name}");
    assert!(
      counts.remade_deltas > 0 && counts.most_windows >= 2,
      "{name}: {counts:?}"
    );
  }
}

/// One changed path as `svnlook changed --copy-info` lists it: its path without a directory's `/`, its action (a
/// replacement as a deletion and an addition, as `svnlook` 1.15 lists it), whether its text and its properties
/// changed (for a change), and its copy source.
type Change = (Vec<u8>, char, bool, bool, Option<(Vec<u8>, u64)>);

/// The changes `changed.txt` lists for each revision, in order.
fn svnlook_changes() -> BTreeMap<u64, Vec<Change>> {
  let mut revisions: BTreeMap<u64, Vec<Change>> = BTreeMap::new();
  let mut revision = 0;
  for line in vector("changed.txt")
    .split(|&byte| byte == b'\n')
    .filter(|line| !line.is_empty())
  {
    if let Some(number) = line.strip_prefix(b"r") {
      revision = std::str::from_utf8(number).unwrap().parse().unwrap();
      continue;
    }
    if let Some(from) = line.strip_prefix(b"    (from ") {
      let from = from.strip_suffix(b")").unwrap();
      let colon = from.iter().rposition(|&byte| byte == b':').unwrap();
      let source = trim_directory(&from[..colon]);
      let number = std::str::from_utf8(&from[colon + 2..])
        .unwrap()
        .parse()
        .unwrap();
      revisions.get_mut(&revision).unwrap().last_mut().unwrap().4 = Some((source, number));
      continue;
    }
    let action = char::from(line[0]);
    let change = (
      trim_directory(&line[4..]),
      action,
      action == 'U',
      line[1] == b'U',
      None,
    );
    revisions.entry(revision).or_default().push(change);
  }
  revisions
}

/// A path as the dump writes it: a directory's `/` and the root's own `/` dropped.
fn trim_directory(path: &[u8]) -> Vec<u8> {
  path.strip_suffix(b"/").unwrap_or(path).to_vec()
}

/// The changes a dump's events make in each revision, listed as `svnlook` lists them.
fn dump_changes(events: &[Owned]) -> BTreeMap<u64, Vec<Change>> {
  let mut revisions: BTreeMap<u64, Vec<Change>> = BTreeMap::new();
  let mut revision = 0;
  for event in events {
    match event {
      Owned::Revision(header) => revision = header.number,
      Owned::Node(header) => {
        let rows = revisions.entry(revision).or_default();
        let copy = header
          .copy_from
          .as_ref()
          .map(|copy| (copy.path.clone(), copy.revision));
        let path = header.path.clone();
        match header.action {
          NodeAction::Change => {
            rows.push((
              path,
              'U',
              header.text.is_some(),
              header.properties.is_some(),
              None,
            ));
            if header.text.is_none() {
              rows.last_mut().unwrap().1 = '_';
            }
          }
          NodeAction::Add => rows.push((path, 'A', false, false, copy)),
          NodeAction::Delete => rows.push((path, 'D', false, false, None)),
          NodeAction::Replace => {
            rows.push((path.clone(), 'D', false, false, None));
            rows.push((path, 'A', false, false, copy));
          }
        }
      }
      _ => {}
    }
  }
  revisions
}

/// Sorts each revision's changes, `svnlook`'s order and the dump's being their own.
fn sorted(mut revisions: BTreeMap<u64, Vec<Change>>) -> BTreeMap<u64, Vec<Change>> {
  for rows in revisions.values_mut() {
    rows.sort();
  }
  revisions
}

/// A-593 (golden, oracle): do list the changes each revision's records make in both dumps, as `svnlook changed
/// --copy-info` lists them. Expect `svnlook`'s own listing of the repository, revision by revision: every action and
/// copy source, which text and property changes each change carries, and each replacement by a copy, which `svnadmin`
/// writes as two records and the reader reads as the one replacement FSFS records.
#[test]
fn every_revision_s_records_are_what_svnlook_says_it_changed() {
  let expected = sorted(svnlook_changes());
  assert_eq!(
    expected.len(),
    13,
    "svnlook's listing covers every revision"
  );
  for name in ["full.dump", "deltas.dump"] {
    let mut changes = sorted(dump_changes(&read_whole(vector(name)).unwrap()));
    for rows in changes.values_mut() {
      for row in rows.iter_mut().filter(|row| row.1 == 'A' || row.1 == 'D') {
        (row.2, row.3) = (false, false);
      }
    }
    changes.remove(&0);
    assert_eq!(changes, expected, "{name}");
  }
}

/// A-593 (golden): do read the stream two dumps make one after the other, revisions 0 to 3 in format 2 and 4 on in
/// format 3. Expect two version records and two UUID records in it, every revision once and in order, deltas only after
/// the second version record, and revisions 4 on the same events as the whole format-3 dump's.
#[test]
fn dumps_written_one_after_the_other_read_as_one_stream_whose_format_changes() {
  let events = read_whole(&[vector("part1.dump"), vector("part2.dump")].concat()).unwrap();
  let formats: Vec<Format> = events
    .iter()
    .filter_map(|event| {
      if let Owned::Format(format) = event {
        Some(*format)
      } else {
        None
      }
    })
    .collect();
  assert_eq!(formats, [Format::Two, Format::Three]);
  assert_eq!(
    events
      .iter()
      .filter(|event| matches!(event, Owned::Uuid(_)))
      .count(),
    2
  );
  let numbers: Vec<u64> = events
    .iter()
    .filter_map(|event| {
      if let Owned::Revision(header) = event {
        Some(header.number)
      } else {
        None
      }
    })
    .collect();
  assert_eq!(numbers, (0..=13).collect::<Vec<u64>>());
  let second_format = events
    .iter()
    .rposition(|event| matches!(event, Owned::Format(_)))
    .unwrap();
  assert!(
    events[..second_format]
      .iter()
      .all(|event| !matches!(event, Owned::Window(_)))
  );
  let from_four = |events: &[Owned]| -> Vec<Owned> {
    let start = events
      .iter()
      .position(|event| matches!(event, Owned::Revision(header) if header.number == 4))
      .unwrap();
    events[start..].to_vec()
  };
  assert_eq!(
    from_four(&events),
    from_four(&read_whole(vector("deltas.dump")).unwrap())
  );
}

/// A-593 (golden): do read revision 6 of the format-2 dump, where `svnadmin` wrote each of four replacements by a copy
/// as a deletion record ended by its header block alone and an addition record with the copy source. Expect four
/// replacement records, each with its copy source and no deletion record beside it.
#[test]
fn a_replacement_by_a_copy_is_read_as_the_one_record_fsfs_keeps() {
  let events = read_whole(vector("full.dump")).unwrap();
  let start = events
    .iter()
    .position(|event| matches!(event, Owned::Revision(header) if header.number == 6))
    .unwrap();
  let nodes: Vec<&NodeHeader> = events[start + 1..]
    .iter()
    .take_while(|event| !matches!(event, Owned::Revision(_)))
    .filter_map(|event| {
      if let Owned::Node(header) = event {
        Some(header)
      } else {
        None
      }
    })
    .collect();
  let rows: Vec<(&[u8], NodeAction, &[u8], u64)> = nodes
    .iter()
    .map(|header| {
      let copy = header.copy_from.as_ref().expect("a copy source");
      (
        header.path.as_slice(),
        header.action,
        copy.path.as_slice(),
        copy.revision,
      )
    })
    .collect();
  let expected: [(&[u8], NodeAction, &[u8], u64); 4] = [
    (b"branches/b1", NodeAction::Replace, b"trunk", 5),
    (
      b"branches/b1/bin.dat",
      NodeAction::Replace,
      b"trunk/a.txt",
      5,
    ),
    (b"trunk/a: b c.txt", NodeAction::Replace, b"trunk/a.txt", 5),
    (b"trunk/bin.dat", NodeAction::Replace, b"trunk/a.txt", 5),
  ];
  assert_eq!(rows, expected);
}

/// The reading of a vector handed over whole: its events and where its whole texts lie.
pub(super) fn reading(name: &str) -> Reading {
  read(vector(name), usize::MAX, ROOMY).unwrap()
}

/// Where two byte strings first differ, with a little of each from there, for a failure's message.
fn first_difference(written: &[u8], expected: &[u8]) -> String {
  let at = written
    .iter()
    .zip(expected)
    .position(|(left, right)| left != right)
    .unwrap_or(written.len().min(expected.len()));
  let around = |bytes: &[u8]| {
    String::from_utf8_lossy(&bytes[at.saturating_sub(40)..(at + 40).min(bytes.len())]).into_owned()
  };
  format!(
    "at byte {at} of {} written and {} expected: {:?} against {:?}",
    written.len(),
    expected.len(),
    around(written),
    around(expected)
  )
}
