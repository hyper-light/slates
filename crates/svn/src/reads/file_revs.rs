//! `get-file-revs` (§4.17.4 "4. Reads": blame is the file's history across copies, and for each revision its
//! properties, its property changes and its text's svndiff from the previous revision's text; the client computes the
//! blame; A-613 part 2): as `svnserve`'s `get_file_revs` and `svn_repos_get_file_revs2` answer it (`serve.c`,
//! `libsvn_repos/rev_hunt.c`, Subversion 1.15.0), held to stock `svnserve`'s transcripts (`crates/svn/fixtures/reads`,
//! sessions `get-file-revs`, `svn-blame` and `svn-blame-g`):
//! - The parameters are `c(?r)(?r)?B`: the path, the start and end revisions (each the youngest when missing or
//!   negative), and optionally whether merged revisions are included. Then the authentication request.
//! - Start not after end: the file's history from the end revision back, across copies, to the first revision at or
//!   before the start (`find_interesting_revisions`; the path must be a file in the end revision: "'P' is not a file in
//!   revision N", 160017); with merged revisions, each revision's merged mergeinfo (`get_merged_mergeinfo`) walked the
//!   same way over each merged range's file, until no new revision is found (`find_merged_revisions`); then all sent
//!   oldest first, the merged ones interleaved and marked. Start after end: the history walked newest first and sent as
//!   found (`get_file_revs_backwards`), merged revisions refused ("Trying to use an unsupported feature", 200007).
//! - Each revision is `( path rev ( rev-props ) ( prop-diffs ) merged )`, its properties changed from the previous
//!   revision's (`svn_prop_diffs`, each `( name ( value ) )`, a deletion's value list empty), then its text's delta
//!   from the previous revision's text (the first's from no text) as svndiff strings, then the empty string; or the
//!   empty string alone when the text is the previous one's (and, with merged revisions, the path the same). The same
//!   text is `svn_fs_contents_different`'s (`svn_fs_fs__file_text_rep_equal`): two empty texts, a file without one
//!   counted empty, or two of one MD5 and SHA-1.
//! - The delta's windows are Subversion's (A-652's deltifier, `svndiff::deltas`, which makes the windows FSFS stores
//!   and computes), svndiff 2 to a client that accepts it and 0 to any other (never 1, zlib: the report editor's rule),
//!   a string for each write `svn_txdelta_to_svndiff3` makes: the header at the first window, or alone when the target
//!   is empty and there is none; each window's integers, then its instruction and new-data sections when not empty (the
//!   report editor's `window_sections`). In svndiff 0 a window of new data alone is `send_simple_insertion_window`'s:
//!   the header if it is not out, the integers and the instruction in one string, then the new data. A section an LZ4
//!   block shortens is `lz4_flex`'s block where `svnserve`'s is liblz4's, so it may differ in bytes while it decodes to
//!   the same window; svndiff 0 is `svnserve`'s byte for byte.
//! - Then `done`, then `( success ( ) )` or the refusal.
//!
//! Merged revisions of one number from more than one file (a revision that changed two merged sources) have no order to
//! match: `svnserve` gathers them in its hash's seeded order and sorts them with `qsort`, which keeps no order among
//! equals; slates sends them as they are found, by source path and range.
//!
//! The revisions to send are found before the first is sent, as Subversion finds them, and sent a revision a step. A
//! revision's text and the previous one's are held whole while its delta is made: the deltifier reads a source and a
//! target piece of a window at a time, but takes them as whole texts, so reading the green's pieces a window at a time
//! is owed (§4.17.4 item 11, GAPS).

use super::history::skip_ancestor;
use super::stream::{Progress, Stream};
use super::{
  DONE, Failure, Outcome, Reads, Repository, SVN_ERR_FS_NOT_FOUND, boolean, optional_revnum,
  path_and_revision, proplist,
};
use crate::handshake::ClientCapabilities;
use crate::index::{Node, NodeKind, PropertyList, Revision, Text};
use crate::mergeinfo::{Compare, Mergeinfo, MergeinfoInheritance, mergeinfo_at};
use crate::ra_svn::{ItemError, Value, write};
use crate::report::editor::{simple_insertion, window_sections};
use crate::svndiff::{Compression, Encoder, Version, WINDOW_LIMIT, deltas};
use std::collections::{BTreeSet, VecDeque};
use std::num::NonZeroUsize;

/// `SVN_ERR_FS_NOT_FILE`, the refusal of a path that is no file in the end revision.
/// Format: the filesystem category's start, 160,000, plus 17 (`svn_error_codes.h`); recorded (fixture `get-file-revs`).
const SVN_ERR_FS_NOT_FILE: u32 = 160_017;

/// `SVN_ERR_UNSUPPORTED_FEATURE`, the refusal of merged revisions walked newest first.
/// Format: the miscellaneous category's start, 120,000 + 16 × 5,000 = 200,000, plus 7 (`svn_error_codes.h`); recorded
/// (fixture `get-file-revs`).
const SVN_ERR_UNSUPPORTED_FEATURE: u32 = 200_007;

/// One revision of the file to send (`struct path_revision`).
#[derive(Clone, Debug, PartialEq, Eq)]
struct PathRev {
  /// The file's path in it.
  path: String,
  /// The revision.
  revision: Revision,
  /// Whether it came in by a merge.
  merged: bool,
  /// The mergeinfo it changed for the file (`merged_mergeinfo`), when merged revisions are included.
  merged_mergeinfo: Option<Mergeinfo>,
}

/// What the previous revision sent left (`struct send_baton`'s last root, path and properties).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Last {
  /// Its path.
  path: String,
  /// Its revision.
  revision: Revision,
  /// Its property list.
  list: PropertyList,
  /// Its properties.
  properties: Vec<(Vec<u8>, Vec<u8>)>,
  /// Its text.
  text: Option<Text>,
}

/// A parsed `get-file-revs` request.
#[derive(Clone, Debug, PartialEq, Eq)]
struct FileRevsRequest {
  /// The path.
  path: String,
  /// The start revision as given.
  start: Option<i64>,
  /// The end revision as given.
  end: Option<i64>,
  /// Whether merged revisions are included.
  merged: bool,
}

impl FileRevsRequest {
  /// `svn_ra_svn__parse_tuple(params, "c(?r)(?r)?B")`.
  fn parse(params: &[Value<'_>], base: &str) -> Option<Self> {
    let (path, start) = path_and_revision(params, base)?;
    let end = optional_revnum(params.get(2))?;
    let merged = match params.get(3) {
      None => false,
      some => boolean(some)?,
    };
    Some(Self {
      path,
      start,
      end,
      merged,
    })
  }
}

impl<R: Repository + ?Sized> Reads<'_, R> {
  /// `get-file-revs ( path [ start ] [ end ] ? merged )` (`serve.c` `get_file_revs`): the authentication request, then
  /// a [`Stream`] of the file's revisions.
  pub(super) fn file_revisions(
    &mut self,
    params: &[Value<'_>],
    capabilities: ClientCapabilities,
  ) -> Result<Outcome, ItemError> {
    let Some(request) = FileRevsRequest::parse(params, self.base) else {
      return Ok(Outcome::Close);
    };
    self.trivial_auth()?;
    let (version, compression) = if capabilities.accepts_svndiff2 {
      (Version::Two, Compression::Lz4)
    } else {
      (Version::Zero, Compression::Stored)
    };
    Ok(Outcome::Streaming(Stream::file_revisions(FileRevsStream {
      request,
      version,
      compression,
      pending: None,
      last: None,
      ended: false,
    })))
  }
}

/// A `get-file-revs` answer still to write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct FileRevsStream {
  /// The request.
  request: FileRevsRequest,
  /// The svndiff version the client takes.
  version: Version,
  /// How its sections are stored.
  compression: Compression,
  /// The revisions still to send, in order; `None` before they are found.
  pending: Option<VecDeque<PathRev>>,
  /// What the previous revision sent left.
  last: Option<Last>,
  /// Whether `done` and the answer are written.
  ended: bool,
}

impl FileRevsStream {
  /// Sends revisions until the bytes written and the revisions sent reach `room`, at least one step, or until the
  /// answer is whole.
  pub(super) fn resume<R: Repository + ?Sized>(
    &mut self,
    reads: &mut Reads<'_, R>,
    room: usize,
  ) -> Result<Progress, ItemError> {
    let start = reads.out.len();
    let mut steps = 0_usize;
    while !self.ended {
      match self.step(reads) {
        Ok(()) => {}
        Err(Stop::Refused(failure)) => self.finish(reads, Some(&failure))?,
        Err(Stop::Unwritten(error)) => return Err(error),
      }
      steps = steps.saturating_add(1);
      let spent = reads.out.len().saturating_sub(start).saturating_add(steps);
      if !self.ended && spent >= room {
        return Ok(Progress::More);
      }
    }
    Ok(Progress::Done)
  }

  /// One step: the revisions found, or the next sent, or the end once none is left.
  fn step<R: Repository + ?Sized>(&mut self, reads: &mut Reads<'_, R>) -> Result<(), Stop> {
    let Some(pending) = self.pending.as_mut() else {
      self.pending = Some(reads.file_revisions_to_send(&self.request)?);
      return Ok(());
    };
    let Some(next) = pending.pop_front() else {
      return Ok(self.finish(reads, None)?);
    };
    self.send(reads, &next)
  }

  /// `send_path_revision`: the revision's tuple, then its delta from the previous revision's text, or the empty string
  /// alone when its text is unchanged.
  fn send<R: Repository + ?Sized>(
    &mut self,
    reads: &mut Reads<'_, R>,
    revision: &PathRev,
  ) -> Result<(), Stop> {
    let node = reads
      .node(&revision.path, revision.revision)?
      .ok_or_else(|| Failure::not_found(&revision.path, revision.revision))?;
    let properties = match &self.last {
      Some(last) if last.list == node.properties() => last.properties.clone(),
      _ => reads.owned_properties(&node),
    };
    let previous: &[(Vec<u8>, Vec<u8>)] = self
      .last
      .as_ref()
      .map_or(&[][..], |last| last.properties.as_slice());
    let diffs = property_diffs(&properties, previous);
    let changed = match &self.last {
      None => true,
      Some(last) if self.request.merged && last.path != revision.path => true,
      Some(last) => !same_text(last.text, node.text()),
    };
    let revision_properties = reads
      .repository
      .revision_properties(revision.revision)
      .unwrap_or_default();
    let item = Value::List(vec![
      Value::String(revision.path.as_bytes()),
      Value::Number(revision.revision),
      proplist(revision_properties),
      Value::List(diffs.iter().map(diff_value).collect()),
      Value::Word(super::bool_word(revision.merged)),
    ]);
    write(&item, reads.out)?;
    if changed {
      self.delta(reads, &node)?;
    }
    write(&Value::String(b""), reads.out)?;
    self.last = Some(Last {
      path: revision.path.clone(),
      revision: revision.revision,
      list: node.properties(),
      properties,
      text: node.text(),
    });
    Ok(())
  }

  /// The delta from the previous revision's text (none for the first) to `node`'s, a string for each write
  /// `svn_txdelta_to_svndiff3` makes (`svndiff.c` `window_handler`): the header at the first window, or alone when
  /// there is none; each window's integers, then its instruction and new-data sections when they are not empty. In
  /// svndiff 0 a window of new data alone is written as `send_simple_insertion_window` writes it: the header if it is
  /// not out, the integers and the instruction in one string, then the new data.
  fn delta<R: Repository + ?Sized>(
    &self,
    reads: &mut Reads<'_, R>,
    node: &Node<'_>,
  ) -> Result<(), Stop> {
    let source = match &self.last {
      Some(last) => reads.whole_text(last.text)?,
      None => Vec::new(),
    };
    let target = reads.whole_text(node.text())?;
    let room = NonZeroUsize::new(usize::try_from(WINDOW_LIMIT).map_err(|_| Failure::corrupt())?)
      .ok_or_else(Failure::corrupt)?;
    let mut encoder = Encoder::new(self.version);
    let header = encoder.header();
    let mut header_out = false;
    let mut encoded = Vec::new();
    for window in deltas(&source, &target, room).map_err(|_| Failure::corrupt())? {
      encoded.clear();
      encoder
        .window(&window, self.compression, &mut encoded)
        .map_err(|_| Failure::corrupt())?;
      let [integers, instructions, new_data] =
        window_sections(&encoded).map_err(|_| Failure::corrupt())?;
      let unwritten_header: &[u8] = if header_out { &[] } else { &header };
      if simple_insertion(self.version, &window) {
        let joined = [unwritten_header, integers, instructions].concat();
        write_sections(&[joined.as_slice(), new_data], reads.out)?;
      } else {
        write_sections(
          &[unwritten_header, integers, instructions, new_data],
          reads.out,
        )?;
      }
      header_out = true;
    }
    if !header_out {
      write_sections(&[header.as_slice()], reads.out)?;
    }
    Ok(())
  }

  /// `done`, then `( success ( ) )` or the refusal.
  fn finish<R: Repository + ?Sized>(
    &mut self,
    reads: &mut Reads<'_, R>,
    failure: Option<&Failure>,
  ) -> Result<(), ItemError> {
    self.ended = true;
    write(&Value::Word(DONE), reads.out)?;
    match failure {
      None => reads.success(Vec::new())?,
      Some(failure) => reads.failure(failure)?,
    };
    Ok(())
  }
}

impl<R: Repository + ?Sized> Reads<'_, R> {
  /// `svn_repos_get_file_revs2`'s revisions, in the order they are sent.
  fn file_revisions_to_send(
    &self,
    request: &FileRevsRequest,
  ) -> Result<VecDeque<PathRev>, Failure> {
    let youngest = i64::try_from(self.index.youngest()).map_err(|_| Failure::corrupt())?;
    let valid = |revision: Option<i64>| {
      revision
        .filter(|&revision| revision >= 0)
        .unwrap_or(youngest)
    };
    let (start, end) = (valid(request.start), valid(request.end));
    if end < start {
      if request.merged {
        let message = String::from("Trying to use an unsupported feature");
        return Err(Failure::new(SVN_ERR_UNSUPPORTED_FEATURE, message));
      }
      let (older, newer) = (unsigned(end)?, unsigned(start)?);
      let mut found = BTreeSet::new();
      let backwards = self.interesting(&request.path, older, newer, Walk::default(), &mut found)?;
      return Ok(backwards.into());
    }
    let (start, end) = (unsigned(start)?, unsigned(end)?);
    let mut found = BTreeSet::new();
    let walk = Walk {
      merged: request.merged,
      mark: false,
    };
    let mainline = self.interesting(&request.path, start, end, walk, &mut found)?;
    let merged = if request.merged {
      self.merged_revisions(start, &mainline, &mut found)?
    } else {
      Vec::new()
    };
    Ok(interleaved(mainline, merged))
  }

  /// `find_interesting_revisions`: the file's history from `end` back across copies, each location kept until one at
  /// or before `start`, or one already found when merged revisions are included; refused when the path is no file in
  /// `end`.
  fn interesting(
    &self,
    path: &str,
    start: Revision,
    end: Revision,
    walk: Walk,
    found: &mut BTreeSet<(String, Revision)>,
  ) -> Result<Vec<PathRev>, Failure> {
    self.existing(signed(end)?)?;
    if self
      .node(path, end)?
      .is_none_or(|node| node.kind() != NodeKind::File)
    {
      let message = format!("'{path}' is not a file in revision {end}");
      return Err(Failure::new(SVN_ERR_FS_NOT_FILE, message));
    }
    let mut history = self.node_history(path, end)?;
    let mut revisions = Vec::new();
    while let Some(location) = self.history_prev(&history, true)? {
      let key = (location.path.clone(), location.revision);
      if walk.merged && found.contains(&key) {
        break;
      }
      let merged_mergeinfo = if walk.merged {
        self.merged_mergeinfo(&location.path, location.revision)?
      } else {
        None
      };
      revisions.push(PathRev {
        path: location.path.clone(),
        revision: location.revision,
        merged: walk.mark,
        merged_mergeinfo,
      });
      found.insert(key);
      if location.revision <= start {
        break;
      }
      history = location;
    }
    Ok(revisions)
  }

  /// `get_merged_mergeinfo`: the mergeinfo `revision` changed for the file at `path`, added and deleted together, when
  /// a changed path at or above it changed `svn:mergeinfo`; `None` without a change, or when the path did not exist the
  /// revision before.
  fn merged_mergeinfo(&self, path: &str, revision: Revision) -> Result<Option<Mergeinfo>, Failure> {
    let changes = self
      .index
      .changes(revision)
      .map_err(|refusal| Failure::of_index(&refusal))?;
    let touched = changes.iter().any(|change| {
      change.properties.is_some()
        && change.mergeinfo_modified
        && skip_ancestor(&change.path, path).is_some()
    });
    if !touched {
      return Ok(None);
    }
    let inherited = |revision: Revision| {
      mergeinfo_at(
        self.index,
        self.repository,
        path,
        revision,
        MergeinfoInheritance::Inherited,
      )
      .map_err(|refusal| Failure::of_query(&refusal, revision))
    };
    let after = inherited(revision)?;
    let before = match inherited(revision.saturating_sub(1)) {
      Ok(before) => before,
      Err(failure) if failure.code == SVN_ERR_FS_NOT_FOUND => return Ok(None),
      Err(failure) => return Err(failure),
    };
    let delta = Mergeinfo::diff(before.as_ref(), after.as_ref(), Compare::WithoutInheritance);
    let changed = delta.added.merge(&delta.deleted);
    Ok((!changed.is_empty()).then_some(changed))
  }

  /// `find_merged_revisions`: each merged range's file walked as `find_interesting_revisions` walks it, the revisions
  /// it finds walked for their own merges in turn, until none is new; all of them, newest first.
  fn merged_revisions(
    &self,
    start: Revision,
    mainline: &[PathRev],
    found: &mut BTreeSet<(String, Revision)>,
  ) -> Result<Vec<PathRev>, Failure> {
    let mut merged = Vec::new();
    let mut previous: Vec<PathRev> = mainline.to_vec();
    let walk = Walk {
      merged: true,
      mark: true,
    };
    while !previous.is_empty() {
      let mut next = Vec::new();
      for mergeinfo in previous
        .iter()
        .filter_map(|revision| revision.merged_mergeinfo.as_ref())
      {
        for (source, ranges) in mergeinfo.iter() {
          let source =
            String::from_utf8(source.as_bytes().to_vec()).map_err(|_| Failure::corrupt())?;
          for range in ranges.ranges() {
            if range.end() < start {
              continue;
            }
            self.existing(signed(range.end())?)?;
            if self
              .node(&source, range.end())?
              .is_some_and(|node| node.kind() == NodeKind::File)
            {
              next.extend(self.interesting(&source, range.start(), range.end(), walk, found)?);
            }
          }
        }
      }
      merged.extend(next.iter().cloned());
      previous = next;
    }
    merged.sort_by(|first, second| second.revision.cmp(&first.revision));
    Ok(merged)
  }

  /// A node's properties, owned.
  fn owned_properties(&self, node: &Node<'_>) -> Vec<(Vec<u8>, Vec<u8>)> {
    match node.properties() {
      PropertyList::Empty => Vec::new(),
      PropertyList::Held { identity, .. } => self
        .repository
        .properties(&identity)
        .unwrap_or_default()
        .into_iter()
        .map(|(name, value)| (name.to_vec(), value.to_vec()))
        .collect(),
    }
  }

  /// A text whole, gathered from the green's pieces; refused as corrupt when the green does not hold it.
  fn whole_text(&self, text: Option<Text>) -> Result<Vec<u8>, Failure> {
    let Some(text) = text else {
      return Ok(Vec::new());
    };
    let size = usize::try_from(text.size).map_err(|_| Failure::corrupt())?;
    let mut whole = Vec::with_capacity(size);
    while whole.len() < size {
      let offset = u64::try_from(whole.len()).map_err(|_| Failure::corrupt())?;
      let piece = self
        .repository
        .text(&text.identity, offset)
        .filter(|piece| !piece.is_empty())
        .ok_or_else(Failure::corrupt)?;
      let take = piece.len().min(size.saturating_sub(whole.len()));
      whole.extend_from_slice(piece.get(..take).ok_or_else(Failure::corrupt)?);
    }
    Ok(whole)
  }
}

/// How `find_interesting_revisions` walks: whether merged revisions are included, and whether what it finds is marked
/// merged.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Walk {
  /// `include_merged_revisions`.
  merged: bool,
  /// `mark_as_merged`.
  mark: bool,
}

/// `svn_repos_get_file_revs2`'s order: the mainline and the merged revisions, each newest first, sent oldest first,
/// interleaved, a mainline revision before a merged one of the same number.
fn interleaved(mut mainline: Vec<PathRev>, mut merged: Vec<PathRev>) -> VecDeque<PathRev> {
  let mut order = VecDeque::with_capacity(mainline.len().saturating_add(merged.len()));
  loop {
    let take_mainline = match (mainline.last(), merged.last()) {
      (Some(main), Some(merge)) => main.revision <= merge.revision,
      (Some(_), None) => true,
      (None, Some(_)) => false,
      (None, None) => break,
    };
    let next = if take_mainline {
      mainline.pop()
    } else {
      merged.pop()
    };
    order.extend(next);
  }
  order
}

/// Whether two texts are the same as `svn_fs_contents_different` judges them (`svn_fs_fs__file_text_rep_equal`,
/// `fs_fs.c`): two empty texts are, a file without a text counted empty; an empty and a non-empty text are not; two
/// others are when their MD5 and SHA-1 digests are, which FSFS format 4 and later keep for every text.
fn same_text(first: Option<Text>, second: Option<Text>) -> bool {
  let digests = |text: Option<Text>| {
    text
      .filter(|text| text.size != 0)
      .map(|text| (text.md5, text.sha1))
  };
  digests(first) == digests(second)
}

/// Each section that is not empty as a string, in order, as each stream write sends one (`serve.c`
/// `svndiff_handler`).
fn write_sections(sections: &[&[u8]], out: &mut Vec<u8>) -> Result<(), ItemError> {
  for section in sections
    .iter()
    .copied()
    .filter(|section| !section.is_empty())
  {
    write(&Value::String(section), out)?;
  }
  Ok(())
}

/// `svn_prop_diffs`: the changes from `previous` to `properties`, deletions with no value; in byte order of the names,
/// where `svnserve`'s is its hash order.
fn property_diffs<'value>(
  properties: &'value [(Vec<u8>, Vec<u8>)],
  previous: &'value [(Vec<u8>, Vec<u8>)],
) -> Vec<(&'value [u8], Option<&'value [u8]>)> {
  let find = |list: &'value [(Vec<u8>, Vec<u8>)], name: &[u8]| {
    list
      .iter()
      .find(|(held, _)| held.as_slice() == name)
      .map(|(_, value)| value.as_slice())
  };
  let mut diffs: Vec<(&[u8], Option<&[u8]>)> = Vec::new();
  for (name, value) in previous {
    match find(properties, name) {
      None => diffs.push((name, None)),
      Some(now) if now != value.as_slice() => diffs.push((name, Some(now))),
      Some(_) => {}
    }
  }
  for (name, value) in properties {
    if find(previous, name).is_none() {
      diffs.push((name, Some(value)));
    }
  }
  diffs.sort_unstable();
  diffs
}

/// One property change as `write_prop_diffs` writes it: `( name ( value ) )`, the value list empty for a deletion.
fn diff_value<'value>((name, value): &(&'value [u8], Option<&'value [u8]>)) -> Value<'value> {
  Value::List(vec![
    Value::String(name),
    Value::List(value.map(Value::String).into_iter().collect()),
  ])
}

/// A revision as a C `svn_revnum_t`.
fn signed(revision: Revision) -> Result<i64, Failure> {
  i64::try_from(revision).map_err(|_| Failure::corrupt())
}

/// An `svn_revnum_t` known not to be negative, as a revision.
fn unsigned(revision: i64) -> Result<Revision, Failure> {
  Revision::try_from(revision).map_err(|_| Failure::corrupt())
}

/// Why a step stopped: a refusal the answer carries after `done`, or an item that could not be written.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Stop {
  /// A refusal: the answer ends with `done` and it.
  Refused(Failure),
  /// An item the writer refused.
  Unwritten(ItemError),
}

impl From<Failure> for Stop {
  fn from(failure: Failure) -> Self {
    Self::Refused(failure)
  }
}

impl From<ItemError> for Stop {
  fn from(error: ItemError) -> Self {
    Self::Unwritten(error)
  }
}
