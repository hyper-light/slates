//! An import's history written as the changegroup a push of it would bring (A-767 C1 step 3: the import landed on the
//! node's changeset store as changesets): every revision the walk reads, in the walk's order, which is a changegroup's
//! (the changelog's group, the manifest log's, version 03's empty tree list, then each file log's group, the list's
//! end), each revision's chunk the delta its revlog stores against the revision its revlog names as the delta's parent
//! (`revlog::Outline::delta_parent`), never a delta made here, and a full snapshot as its text against the null base.
//! Nothing is rebuilt to write it: the walk rebuilds each text to hold it to its node, and the chunk is its revlog's.
//!
//! **Segments.** The stream is cut into segments of at least `bound` bytes, only between chunks, so each revision's
//! delta lies whole in one segment, and each segment names its revisions' texts as a push's `Texts` section does
//! ([`KeptText`]: the text's identity and length, its delta parent's text identity, and where the delta lies in the
//! segment). The segments concatenated in order are the changegroup. Each carries its record (main's ruling of
//! 2026-11-21 01:16): the index rows of the revisions whose deltas it holds, by identity
//! (`crate::import::record::Writer::begin_segment`), so a recovery restores them with no text rebuilt; the import
//! writes it once those rows are appended and cuts the segment then ([`Emitter::cut`]). A caller lands each as one
//! changeset as it is cut ([`Emitter::take`]), so what is held is one segment and the chunk that closes it.
//!
//! **Bounds.** Besides the segment, the emitter keeps, for the log being read, each revision's node and text identity
//! (52 bytes a revision, so a delta's parent is named without reading its entry again), charged to the walk's room as
//! the log is read and let go when the next log opens.

use std::collections::VecDeque;

use crate::changegroup::{self, Revision, Version};
use crate::index::Log;
use crate::push::intake::{CHANGELOG_PATH, KeptText, MANIFEST_PATH};
use crate::read::to_u64;
use crate::{Admission, NULL_NODE, Node, delta};

/// Format: the changegroup version an import is written as: 03, which carries each delta's base and revision flags.
pub const VERSION: Version = Version::V03;

/// One cut of the stream (module docs): its bytes and the texts its deltas give.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Segment {
  /// The changegroup's bytes in this segment.
  pub payload: Vec<u8>,
  /// Each revision whose delta lies in it, places within `payload`.
  pub texts: Vec<KeptText>,
  /// Its record: the index rows of those revisions, by identity, and the markers in the last segment.
  pub record: Vec<u8>,
  /// Its file revisions as the content store keeps them, each by its path and node (A-767 C1 step 2b): what its owner
  /// lets go of the content store once the segment is durable on the node's changeset store, which then serves them,
  /// and notes for a home's read once the import lands (goal 93 item 2b(b)).
  pub contents: Vec<crate::filerev::FileContents>,
}

/// Why a revision could not be written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EmitRefused {
  /// Past the walk's room.
  PastAdmission(crate::PastAdmission),
  /// A chunk the changegroup cannot carry (longer than a chunk's signed 32-bit length).
  Unencodable(changegroup::EncodeRefusal),
  /// A segment's record could not be written.
  Record(crate::import::record::WriteRefused),
  /// A delta whose parent the log has not read: a revlog naming a later revision.
  Parent {
    /// The revision.
    rev: u32,
    /// Its delta's parent.
    parent: u32,
  },
}

impl From<crate::PastAdmission> for EmitRefused {
  fn from(past: crate::PastAdmission) -> Self {
    EmitRefused::PastAdmission(past)
  }
}

/// One revision as the walk read it, for the stream: its fields, the chunk its revlog stores and the text rebuilt.
pub(crate) struct Read<'a> {
  /// The log it belongs to.
  pub log: Log<'a>,
  /// Its revision number in that log.
  pub rev: u32,
  /// Its node.
  pub node: Node,
  /// Its parents' nodes.
  pub parents: (Node, Node),
  /// The changeset that brought it.
  pub link: Node,
  /// Its revision flags.
  pub flags: u16,
  /// Its delta's parent in the log; none for a full snapshot.
  pub delta_parent: Option<u32>,
  /// The chunk its revlog stores: the delta against its parent's text, or the full text.
  pub chunk: &'a [u8],
  /// Its text, rebuilt and held to its node.
  pub text: &'a [u8],
}

/// The stream being written (module docs).
#[derive(Debug)]
pub struct Emitter {
  /// The least bytes a segment is cut at.
  bound: usize,
  /// The segment being written.
  segment: Vec<u8>,
  /// Its texts.
  texts: Vec<KeptText>,
  /// The segments cut and not yet taken.
  ready: VecDeque<Segment>,
  /// The log being read: each revision's node and text identity, by revision number.
  read: Vec<(Node, [u8; 32])>,
  /// Whether the file log being read has written its group's name.
  named: bool,
  /// The revisions whose deltas the segment being written holds: each log's, from its first to the one past its last.
  ranges: Vec<(Logged, u32, u32)>,
  /// The bytes of the segments cut so far.
  cut: u64,
  /// The file revisions the segment being written holds, as the content store keeps them ([`Segment::contents`]).
  contents: Vec<crate::filerev::FileContents>,
}

impl Emitter {
  /// An emitter cutting segments of at least `bound` bytes.
  pub fn new(bound: usize) -> Self {
    Emitter {
      bound: bound.max(1),
      segment: Vec::new(),
      texts: Vec::new(),
      ready: VecDeque::new(),
      read: Vec::new(),
      named: false,
      ranges: Vec::new(),
      cut: 0,
      contents: Vec::new(),
    }
  }

  /// Notes a file revision of the segment being written as the content store keeps it ([`Segment::contents`]).
  pub(crate) fn note_contents(&mut self, contents: crate::filerev::FileContents) {
    self.contents.push(contents);
  }

  /// A log opened: what the last one kept let go.
  pub(crate) fn log_opened(&mut self) {
    self.read = Vec::new();
    self.named = false;
  }

  /// Writes one revision (module docs), charging what it keeps to `room`.
  pub(crate) fn revision(
    &mut self,
    read: &Read<'_>,
    room: &mut Admission,
  ) -> Result<(), EmitRefused> {
    let identity = *blake3::hash(read.text).as_bytes();
    let (base, base_identity) = match read.delta_parent {
      None => (NULL_NODE, None),
      Some(parent) => {
        let &(node, identity) = self
          .read
          .get(usize::try_from(parent).unwrap_or(usize::MAX))
          .ok_or(EmitRefused::Parent {
            rev: read.rev,
            parent,
          })?;
        (node, Some(identity))
      }
    };
    let path = match read.log {
      Log::Changelog => CHANGELOG_PATH,
      Log::Manifest => MANIFEST_PATH,
      Log::File(path) => {
        if !self.named {
          changegroup::put_name(path, &mut self.segment).map_err(EmitRefused::Unencodable)?;
          self.named = true;
        }
        path
      }
    };
    room.charge_items::<(Node, [u8; 32])>(1)?;
    room.charge_items::<KeptText>(1)?;
    room.charge(to_u64(path.len()))?;
    let revision = Revision {
      node: read.node,
      p1: read.parents.0,
      p2: read.parents.1,
      base,
      link: read.link,
      flags: read.flags,
      info: None,
      protocol_flags: 0,
      data: read.chunk,
      sidedata: None,
    };
    let start = to_u64(self.segment.len());
    // A whole is kept as its raw text (the store's contract, A-767 C1): its place past the hunk it is written in.
    let hunk = match read.delta_parent {
      None => delta::HUNK_HEADER_LEN,
      Some(_) => 0,
    };
    let header = to_u64(
      changegroup::LENGTH_LEN
        .saturating_add(VERSION.header_len())
        .saturating_add(hunk),
    );
    match read.delta_parent {
      None => changegroup::put_revision_whole(&revision, VERSION, &mut self.segment),
      Some(_) => changegroup::put_revision(&revision, VERSION, &mut self.segment),
    }
    .map_err(EmitRefused::Unencodable)?;
    let end = to_u64(self.segment.len());
    self.texts.push(KeptText {
      path: path.to_vec(),
      node: read.node,
      identity,
      len: to_u64(read.text.len()),
      base: base_identity,
      place: start.saturating_add(header)..end,
    });
    self.read.push((read.node, identity));
    let logged = Logged::of(read.log);
    match self.ranges.last_mut() {
      Some((last, _, end)) if *last == logged => *end = read.rev.saturating_add(1),
      _ => {
        room.charge_items::<(Logged, u32, u32)>(1)?;
        self
          .ranges
          .push((logged, read.rev, read.rev.saturating_add(1)));
      }
    }
    Ok(())
  }

  /// Whether the segment being written has reached its bound: the import writes its record and cuts it
  /// ([`Emitter::cut`]).
  pub(crate) fn due(&self) -> bool {
    self.segment.len() >= self.bound
  }

  /// The revisions whose deltas the segment being written holds, each log's range, in the import's order.
  pub(crate) fn ranges(&self) -> Vec<(Log<'_>, u32, u32)> {
    self
      .ranges
      .iter()
      .map(|(logged, first, end)| (logged.log(), *first, *end))
      .collect()
  }

  /// A group ended: the changelog's, the manifest log's (with version 03's empty tree list after it), or a file log's
  /// where it wrote a revision.
  pub(crate) fn group_ended(&mut self, log: Log<'_>) {
    match log {
      Log::Changelog => self.end(),
      Log::Manifest => {
        self.end();
        // Version 03's tree list: an import's manifest is flat, so the list is empty.
        self.end();
      }
      Log::File(_) => {
        if self.named {
          self.end();
        }
      }
    }
  }

  /// The file list ended, and with it the stream: the last segment's record and cut follow ([`Emitter::cut`]).
  pub(crate) fn finished(&mut self) {
    self.end();
  }

  /// The next segment cut, oldest first.
  pub fn take(&mut self) -> Option<Segment> {
    self.ready.pop_front()
  }

  /// The bytes it holds: the segment being written and those cut and not yet taken.
  pub fn holding(&self) -> u64 {
    self
      .ready
      .iter()
      .map(|segment| to_u64(segment.payload.len()))
      .fold(to_u64(self.segment.len()), u64::saturating_add)
  }

  /// The bytes it has written in all.
  pub fn written(&self) -> u64 {
    self.cut.saturating_add(to_u64(self.segment.len()))
  }

  /// Appends the empty chunk.
  fn end(&mut self) {
    changegroup::put_end(&mut self.segment);
  }

  /// Cuts the segment being written with its `record` (module docs).
  pub(crate) fn cut(&mut self, record: Vec<u8>) {
    self.cut = self.cut.saturating_add(to_u64(self.segment.len()));
    self.ranges.clear();
    self.ready.push_back(Segment {
      payload: core::mem::take(&mut self.segment),
      texts: core::mem::take(&mut self.texts),
      record,
      contents: core::mem::take(&mut self.contents),
    });
  }
}

/// A log a segment's range names, owned.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Logged {
  /// The changelog.
  Changelog,
  /// The manifest log.
  Manifest,
  /// A file log, by its path.
  File(Vec<u8>),
}

impl Logged {
  /// `log`, owned.
  fn of(log: Log<'_>) -> Self {
    match log {
      Log::Changelog => Logged::Changelog,
      Log::Manifest => Logged::Manifest,
      Log::File(path) => Logged::File(path.to_vec()),
    }
  }

  /// The log it names.
  fn log(&self) -> Log<'_> {
    match self {
      Logged::Changelog => Log::Changelog,
      Logged::Manifest => Log::Manifest,
      Logged::File(path) => Log::File(path),
    }
  }
}
