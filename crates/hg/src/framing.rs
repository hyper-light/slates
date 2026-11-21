//! A changegroup's framing read a chunk at a time from bytes that may not all be at hand (A-767 C1: the piece-reading
//! intake; R2, a replay holding no payload whole): the one walk of a changegroup's chunks, for a whole changegroup in
//! memory ([`crate::changegroup::Changegroup::decode`], a push's bundle) and for one read back from the node's changeset
//! store a piece at a time (a recovery's replay).
//!
//! **A source** ([`Source`]) answers a range of the changegroup's bytes, or says it does not hold them ([`Wanted`]): a
//! slice holds every byte, so it never wants; a [`Window`] holds a run of the bytes its driver put in, and wants any
//! other. A walk that meets a want stops before it changes anything, so its driver fills the window (an awaited read of
//! the store's pieces) and steps again, and the step is taken as if the bytes had been there.
//!
//! **The walk** is 7.2's order (`changegroup.py`: `cg1unpacker`'s `deltachunk`s, `getchunks`), as
//! [`crate::changegroup`] states it: the changelog's group, the manifest log's, from version 03 the tree list, then the
//! file list, each group its delta chunks to an empty chunk, each list its named groups to an empty chunk, and nothing
//! after. A step takes one chunk, reading its length and, for a delta chunk, its header; the delta is placed (where it
//! lies), not read, unless the walk checks it ([`Framing::new`]'s `check`), as a whole changegroup's decoding does. A
//! chunk's length is held to the bytes left before anything is placed, so a framing never names bytes past the end.
//!
//! **What it gives** ([`Sink`]): each revision as it is placed, and each group as it ends, to a sink the caller
//! chooses, so the walk holds nothing of its own: a whole decoding counts in one walk and builds in a second, charging
//! the exact total between (nothing built before it is charged), and an intake collects the places ([`Collect`]),
//! charging each as it is found. A source's bytes are its own.

use core::ops::Range;

use crate::changegroup::{
  EMPTY, FLAG_SIDEDATA, LENGTH_LEN, Refusal, Revision, Version, check_data, parse_header,
};
use crate::read::to_u64;
use crate::{Admission, Node, Truncated};

/// Bytes a source does not hold (module docs): the range a driver puts in before the walk steps again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Wanted {
  /// The range, within the source's length.
  pub range: Range<u64>,
}

/// A changegroup's bytes, as far as a walk can read them (module docs).
pub trait Source {
  /// The changegroup's whole length.
  fn len(&self) -> u64;

  /// The bytes of `range`, clipped to the source's length (empty past it), or the range the source does not hold.
  fn bytes(&self, range: Range<u64>) -> Result<&[u8], Wanted>;
}

impl Source for [u8] {
  fn len(&self) -> u64 {
    to_u64(<[u8]>::len(self))
  }

  fn bytes(&self, range: Range<u64>) -> Result<&[u8], Wanted> {
    let start = usize::try_from(range.start).unwrap_or(usize::MAX);
    let end = usize::try_from(range.end).unwrap_or(usize::MAX);
    let end = end.min(<[u8]>::len(self));
    Ok(self.get(start.min(end)..end).unwrap_or_default())
  }
}

impl Source for Vec<u8> {
  fn len(&self) -> u64 {
    Source::len(self.as_slice())
  }

  fn bytes(&self, range: Range<u64>) -> Result<&[u8], Wanted> {
    Source::bytes(self.as_slice(), range)
  }
}

/// A run of a source's bytes a driver fills (module docs): the bytes from `start`, of a changegroup `len` long, its
/// earlier bytes let go as the walk moves past them.
#[derive(Clone, Debug, Default)]
pub struct Window {
  /// The changegroup's whole length.
  len: u64,
  /// Where the held bytes begin.
  start: u64,
  /// The held bytes.
  held: Vec<u8>,
}

impl Window {
  /// An empty window over a changegroup `len` long.
  pub fn new(len: u64) -> Self {
    Window {
      len,
      start: 0,
      held: Vec::new(),
    }
  }

  /// Where the held bytes end: the next byte a driver puts in, when it reads forward.
  pub fn end(&self) -> u64 {
    self.start.saturating_add(to_u64(self.held.len()))
  }

  /// Lets go of every byte before `at`, and of all of them where `at` is outside the held run (past it, or before it),
  /// so a driver putting in a want begins at its start ([`Window::put`] takes only bytes that follow the held run).
  pub fn keep_from(&mut self, at: u64) {
    if at >= self.end() || at < self.start {
      self.held.clear();
      self.start = at.min(self.len);
      return;
    }
    let drop = usize::try_from(at.saturating_sub(self.start)).unwrap_or(usize::MAX);
    if drop > 0 {
      self.held.drain(..drop.min(self.held.len()));
      self.start = at;
    }
  }

  /// Puts in `bytes`, the source's bytes from [`Window::end`] on, clipped to the changegroup's length; charged to
  /// `admission` before they are held.
  pub fn put(
    &mut self,
    bytes: &[u8],
    admission: &mut Admission,
  ) -> Result<(), crate::PastAdmission> {
    let room = usize::try_from(self.len.saturating_sub(self.end())).unwrap_or(usize::MAX);
    let taken = bytes.get(..room.min(bytes.len())).unwrap_or_default();
    admission.charge(to_u64(taken.len()))?;
    self
      .held
      .try_reserve(taken.len())
      .map_err(|_| crate::PastAdmission {
        asked: to_u64(taken.len()),
        left: admission.left(),
      })?;
    self.held.extend_from_slice(taken);
    Ok(())
  }

  /// How many bytes the window holds.
  pub fn held(&self) -> u64 {
    to_u64(self.held.len())
  }
}

impl Source for Window {
  fn len(&self) -> u64 {
    self.len
  }

  fn bytes(&self, range: Range<u64>) -> Result<&[u8], Wanted> {
    let end = range.end.min(self.len);
    let start = range.start.min(end);
    if start >= end {
      return Ok(&[]);
    }
    if start < self.start || end > self.end() {
      return Err(Wanted { range: start..end });
    }
    let from = usize::try_from(start.saturating_sub(self.start)).unwrap_or(usize::MAX);
    let to = usize::try_from(end.saturating_sub(self.start)).unwrap_or(usize::MAX);
    Ok(self.held.get(from..to).unwrap_or_default())
  }
}

/// Why a walk stopped short of its step: bytes its source wants, or the changegroup's refusal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Halt {
  /// Bytes the source does not hold: put them in and step again (module docs).
  Wanted(Wanted),
  /// The changegroup is refused.
  Refused(Refusal),
}

impl From<Refusal> for Halt {
  fn from(refusal: Refusal) -> Self {
    Halt::Refused(refusal)
  }
}

impl From<Truncated> for Halt {
  fn from(truncated: Truncated) -> Self {
    Halt::Refused(Refusal::Truncated(truncated))
  }
}

impl From<crate::PastAdmission> for Halt {
  fn from(past: crate::PastAdmission) -> Self {
    Halt::Refused(Refusal::PastAdmission(past))
  }
}

impl From<Wanted> for Halt {
  fn from(wanted: Wanted) -> Self {
    Halt::Wanted(wanted)
  }
}

/// One revision as its framing places it: its header's fields, and where its delta (and version 05's sidedata) lie in
/// the changegroup's bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placed {
  /// The node.
  pub node: Node,
  /// The first parent.
  pub p1: Node,
  /// The second parent.
  pub p2: Node,
  /// Its delta's base.
  pub base: Node,
  /// The changeset it belongs to.
  pub link: Node,
  /// Its revision flags.
  pub flags: u16,
  /// Version 04's delta information.
  pub info: Option<crate::changegroup::DeltaInfo>,
  /// The protocol flags.
  pub protocol_flags: u8,
  /// Where its delta lies.
  pub data: Range<u64>,
  /// Where version 05's sidedata chunk's data lies, where its flag says it has one.
  pub sidedata: Option<Range<u64>>,
}

impl Placed {
  /// The revision with its delta and sidedata read from `source`; none where `source` lacks them.
  pub fn revision<'s, S: Source + ?Sized>(&self, source: &'s S) -> Result<Revision<'s>, Wanted> {
    let data = source.bytes(self.data.clone())?;
    let sidedata = match &self.sidedata {
      Some(range) => Some(source.bytes(range.clone())?),
      None => None,
    };
    Ok(Revision {
      node: self.node,
      p1: self.p1,
      p2: self.p2,
      base: self.base,
      link: self.link,
      flags: self.flags,
      info: self.info,
      protocol_flags: self.protocol_flags,
      data,
      sidedata,
    })
  }
}

/// A changegroup's framing once walked: every revision placed, in stream order, and its groups.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Framed {
  /// Every revision, in stream order.
  pub revisions: Vec<Placed>,
  /// The changelog's revisions.
  pub changelog: Range<usize>,
  /// The manifest log's.
  pub manifests: Range<usize>,
  /// Each tree-manifest group (version 03 on): where its name lies, and its revisions.
  pub trees: Vec<(Range<u64>, Range<usize>)>,
  /// Each file group: where its path lies, and its revisions.
  pub files: Vec<(Range<u64>, Range<usize>)>,
}

/// Which group a walk ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupKind {
  /// The changelog's group.
  Changelog,
  /// The manifest log's group.
  Manifests,
  /// A tree manifest's group.
  Tree,
  /// A file's group.
  File,
}

/// Where a walk puts what it finds (module docs, "What it gives"): called only once a step has read all it needs, so a
/// want never leaves a sink half-told.
pub trait Sink {
  /// A revision, placed.
  fn revision(&mut self, placed: Placed) -> Result<(), Refusal>;

  /// The end of a group of `kind`, named where `name` lies (empty for the changelog's and the manifest log's), its
  /// revisions those the walk placed in `revisions`, numbered from the changegroup's first.
  fn group(
    &mut self,
    kind: GroupKind,
    name: Range<u64>,
    revisions: Range<usize>,
  ) -> Result<(), Refusal>;
}

/// A sink collecting a framing ([`Framed`]), each revision and group charged to `admission` as it is found.
pub struct Collect<'a> {
  /// What it has collected.
  pub framed: &'a mut Framed,
  /// What it charges.
  pub admission: &'a mut Admission,
}

impl Sink for Collect<'_> {
  fn revision(&mut self, placed: Placed) -> Result<(), Refusal> {
    self.admission.charge_items::<Placed>(1)?;
    self.framed.revisions.push(placed);
    Ok(())
  }

  fn group(
    &mut self,
    kind: GroupKind,
    name: Range<u64>,
    revisions: Range<usize>,
  ) -> Result<(), Refusal> {
    match kind {
      GroupKind::Changelog => self.framed.changelog = revisions,
      GroupKind::Manifests => self.framed.manifests = revisions,
      GroupKind::Tree => {
        self
          .admission
          .charge_items::<(Range<u64>, Range<usize>)>(1)?;
        self.framed.trees.push((name, revisions));
      }
      GroupKind::File => {
        self
          .admission
          .charge_items::<(Range<u64>, Range<usize>)>(1)?;
        self.framed.files.push((name, revisions));
      }
    }
    Ok(())
  }
}

/// The part of the changegroup a walk reads next.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Part {
  /// The changelog's group.
  Changelog,
  /// The manifest log's group.
  Manifests,
  /// The tree list's next name, or its end.
  TreeName,
  /// A tree group, named where the range lies.
  Tree(Range<u64>),
  /// The file list's next name, or its end.
  FileName,
  /// A file group, named where the range lies.
  File(Range<u64>),
  /// Past the file list: nothing may follow.
  Ended,
  /// Walked whole.
  Done,
}

/// A changegroup's framing walked a chunk at a time (module docs).
#[derive(Clone, Debug)]
pub struct Framing {
  /// The changegroup's version.
  version: Version,
  /// Whether each plain delta is checked as it is placed, its bytes read (a whole changegroup's decoding).
  check: bool,
  /// Where the next chunk begins.
  at: u64,
  /// What it reads next.
  part: Part,
  /// The node of the group's revision before, which version 01's deltas are against.
  previous: Option<Node>,
  /// Where the group being read began among the revisions.
  start: usize,
  /// How many revisions it has placed.
  placed: usize,
}

impl Framing {
  /// A walk of a changegroup of `version` from its first byte; `check` reads and checks each plain delta as it is placed.
  pub fn new(version: Version, check: bool) -> Self {
    Framing {
      version,
      check,
      at: 0,
      part: Part::Changelog,
      previous: None,
      start: 0,
      placed: 0,
    }
  }

  /// Where the next chunk begins: what a driver reading forward wants from next.
  pub fn at(&self) -> u64 {
    self.at
  }

  /// Takes the next chunk of `source` (module docs), telling `sink` what it found; whether more follow. A want changes
  /// nothing, and spans every byte the step reads, from the chunk's start.
  pub fn step<S: Source + ?Sized>(
    &mut self,
    source: &S,
    sink: &mut impl Sink,
  ) -> Result<bool, Halt> {
    let from = self.at;
    let stepped = match self.part.clone() {
      Part::Changelog | Part::Manifests | Part::Tree(_) | Part::File(_) => self.delta(source, sink),
      Part::TreeName | Part::FileName => self.name(source),
      Part::Ended => {
        let left = source.len().saturating_sub(self.at);
        if left > 0 {
          return Err(Refusal::TrailingBytes { bytes: left }.into());
        }
        self.part = Part::Done;
        Ok(false)
      }
      Part::Done => Ok(false),
    };
    // A step reads from where it began: a want spans from there, so a driver that lets go of everything before a want
    // keeps every byte the step reads again.
    stepped.map_err(|halt| match halt {
      Halt::Wanted(wanted) => Halt::Wanted(Wanted {
        range: from.min(wanted.range.start)..wanted.range.end,
      }),
      refused => refused,
    })
  }

  /// Whether the walk is whole: every chunk taken and nothing after them.
  pub fn done(&self) -> bool {
    self.part == Part::Done
  }

  /// The chunk at `at`'s length past its own four bytes, or `None` for the empty chunk; refused as 7.2 refuses one
  /// (negative, or 1 to 4), or past the bytes left.
  fn length<S: Source + ?Sized>(
    &self,
    source: &S,
    at: u64,
    what: &'static str,
  ) -> Result<Option<u64>, Halt> {
    let len = to_u64(LENGTH_LEN);
    let left = source.len().saturating_sub(at);
    if left < len {
      return Err(
        Truncated {
          what,
          at,
          needed: len,
          left,
        }
        .into(),
      );
    }
    let bytes: [u8; LENGTH_LEN] = source
      .bytes(at..at.saturating_add(len))?
      .try_into()
      .map_err(|_| Wanted {
        range: at..at.saturating_add(len),
      })?;
    let value = i32::from_be_bytes(bytes);
    if value == EMPTY {
      return Ok(None);
    }
    let body = u64::try_from(value)
      .ok()
      .and_then(|chunk| chunk.checked_sub(len))
      .filter(|body| *body > 0)
      .ok_or(Refusal::ChunkLength { value })?;
    let after = left.saturating_sub(len);
    if body > after {
      return Err(
        Truncated {
          what,
          at: at.saturating_add(len),
          needed: body,
          left: after,
        }
        .into(),
      );
    }
    Ok(Some(body))
  }

  /// A group's next delta chunk (with version 05's sidedata chunk), or its end.
  fn delta<S: Source + ?Sized>(&mut self, source: &S, sink: &mut impl Sink) -> Result<bool, Halt> {
    let len = to_u64(LENGTH_LEN);
    let Some(body) = self.length(source, self.at, "delta chunk")? else {
      self.end_group(sink)?;
      self.at = self.at.saturating_add(len);
      return Ok(true);
    };
    let header_len = to_u64(self.version.header_len());
    if body < header_len {
      return Err(
        Refusal::ShortChunk {
          len: body,
          header: header_len,
        }
        .into(),
      );
    }
    let header_at = self.at.saturating_add(len);
    let data = header_at.saturating_add(header_len)..header_at.saturating_add(body);
    let mut revision = parse_header(
      source.bytes(header_at..data.start)?,
      self.version,
      self.previous,
    )?;
    let mut next = data.end;
    let sidedata = if self.version == Version::V05 && revision.protocol_flags & FLAG_SIDEDATA != 0 {
      let range = match self.length(source, next, "sidedata chunk")? {
        Some(body) => next.saturating_add(len)..next.saturating_add(len).saturating_add(body),
        None => next.saturating_add(len)..next.saturating_add(len),
      };
      next = range.end;
      Some(range)
    } else {
      None
    };
    if self.check {
      revision.data = source.bytes(data.clone())?;
      check_data(&revision)?;
    }
    sink.revision(Placed {
      node: revision.node,
      p1: revision.p1,
      p2: revision.p2,
      base: revision.base,
      link: revision.link,
      flags: revision.flags,
      info: revision.info,
      protocol_flags: revision.protocol_flags,
      data,
      sidedata,
    })?;
    self.placed = self.placed.saturating_add(1);
    self.previous = Some(revision.node);
    self.at = next;
    Ok(true)
  }

  /// A list's next group name, or its end.
  fn name<S: Source + ?Sized>(&mut self, source: &S) -> Result<bool, Halt> {
    let len = to_u64(LENGTH_LEN);
    let named = self.length(source, self.at, "group name")?;
    let after = self.at.saturating_add(len);
    self.part = match (&self.part, named) {
      (Part::TreeName, Some(body)) => Part::Tree(after..after.saturating_add(body)),
      (Part::TreeName, None) => Part::FileName,
      (_, Some(body)) => Part::File(after..after.saturating_add(body)),
      (_, None) => Part::Ended,
    };
    self.at = match &self.part {
      Part::Tree(name) | Part::File(name) => name.end,
      _ => after,
    };
    self.start = self.placed;
    self.previous = None;
    Ok(true)
  }

  /// Ends the group being read: told to `sink`, and the part after it next.
  fn end_group(&mut self, sink: &mut impl Sink) -> Result<(), Refusal> {
    let revisions = self.start..self.placed;
    let (kind, name, next) = match &self.part {
      Part::Changelog => (GroupKind::Changelog, 0..0, Part::Manifests),
      Part::Manifests if self.version.has_tree_list() => {
        (GroupKind::Manifests, 0..0, Part::TreeName)
      }
      Part::Manifests => (GroupKind::Manifests, 0..0, Part::FileName),
      Part::Tree(name) => (GroupKind::Tree, name.clone(), Part::TreeName),
      Part::File(name) => (GroupKind::File, name.clone(), Part::FileName),
      _ => return Ok(()),
    };
    sink.group(kind, name, revisions)?;
    self.start = self.placed;
    self.previous = None;
    self.part = next;
    Ok(())
  }
}

#[cfg(test)]
#[path = "framing_tests.rs"]
mod tests;
