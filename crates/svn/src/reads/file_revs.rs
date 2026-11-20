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
//!   report editor's `DeltaWrites`, the one writer of these writes, whose `textdelta-chunk`s are the same strings, as
//!   `svnserve`'s are: A-616 continued (7)). In svndiff 0 a window of new data alone is
//!   `send_simple_insertion_window`'s: the header if it is not out, the integers and the instruction in one string,
//!   then the new data. A section an LZ4 block shortens is `lz4_flex`'s block where `svnserve`'s is liblz4's, so it may
//!   differ in bytes while it decodes to the same window; svndiff 0 is `svnserve`'s byte for byte.
//! - Then `done`, then `( success ( ) )` or the refusal.
//!
//! Merged revisions of one number from more than one file (a revision that changed two merged sources) have no order to
//! match: `svnserve` gathers them in its hash's seeded order and sorts them with `qsort`, which keeps no order among
//! equals; slates sends them as they are found, by source path and range.
//!
//! The revisions to send are found before the first is sent, as Subversion finds them. Each is sent over many steps
//! (A-751): its tuple or its element's start, then its delta a unit a step through the update drive's window step
//! (`report::drive::window_unit`: a window's pieces read from the green, the window made by
//! `svndiff::WindowMaking`, its writes written a piece at a time), then its end. A slice stops at the first step past its
//! budget's quantum, so no step holds a shard past one window's unit, and no text is held whole: a window's two pieces
//! at most.

use super::history::skip_ancestor;
use super::log::dav::DavLogProgress;
use super::stream::{Progress, Stream};
use super::{Authentication, ROOT, full_path};
use super::{
  DONE, Failure, Outcome, Reads, Repository, SVN_ERR_FS_NOT_FOUND, boolean, optional_revnum,
  path_and_revision, proplist,
};
use crate::authz::PrincipalRules;
use crate::dav::listing::{
  FILE_REV_END, FILE_REVS_END, FILE_REVS_START, FileRev, FileRevsReport, TXDELTA_END,
  TXDELTA_START, file_rev_head,
};
use crate::dav::xml::Base64Stream;
use crate::handshake::ClientCapabilities;
use crate::index::{Node, NodeKind, PropertyList, Revision, Text};
use crate::mergeinfo::{Compare, Mergeinfo, MergeinfoInheritance, mergeinfo_at};
use crate::ra_svn::{ItemError, Value, write};
use crate::report::drive::{Delta, WindowWork, window_unit};
use crate::report::exchange::{Budget, Slice};
use crate::svndiff::{Compression, Encoder, Version};
use std::collections::{BTreeSet, VecDeque};

/// `SVN_ERR_FS_NOT_FILE`, the refusal of a path that is no file in the end revision.
/// Format: the filesystem category's start, 160,000, plus 17 (`svn_error_codes.h`); recorded (fixture `get-file-revs`).
const SVN_ERR_FS_NOT_FILE: u32 = 160_017;

/// `SVN_ERR_UNSUPPORTED_FEATURE`, the refusal of merged revisions walked newest first.
/// Format: the miscellaneous category's start, 120,000 + 16 × 5,000 = 200,000, plus 7 (`svn_error_codes.h`); recorded
/// (fixture `get-file-revs`).
const SVN_ERR_UNSUPPORTED_FEATURE: u32 = 200_007;

/// The svndiff version a `file-revs-report`'s deltas are written in: 0, as the update-report's are (`dav::update`), which
/// every client decodes. `mod_dav_svn` writes the version the request's `Accept-Encoding` prefers (`repos.c`
/// `negotiate_encoding_prefs`); a client decodes whichever it is sent, so blame's answer is the same (A-726).
const DAV_SVNDIFF: Version = Version::Zero;

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
    // Format: `"c(?r)(?r)?B"`'s fourth item, include-merged-revisions (`serve.c` `get_file_revs`).
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
  /// `get-file-revs ( path [ start ] [ end ] ? merged )` (`serve.c` `get_file_revs`): a [`Stream`] of the file's
  /// revisions, the authentication request written at its first step, once they are found.
  pub(super) fn file_revisions(
    &mut self,
    params: &[Value<'_>],
    capabilities: ClientCapabilities,
  ) -> Result<Outcome, ItemError> {
    let Some(request) = FileRevsRequest::parse(params, self.base) else {
      return Ok(Outcome::Close);
    };
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
      writer: RevsWriter::Items,
      sending: None,
    })))
  }
}

/// Where a file's revisions are written: as `svnserve`'s `file_rev_handler` writes them on an ra_svn connection, or as
/// `mod_dav_svn`'s writes a `file-revs-report` (`reports/file-revs.c`; A-726).
#[derive(Clone, Debug, PartialEq, Eq)]
enum RevsWriter {
  /// `serve.c`'s items.
  Items,
  /// `file-revs.c`'s elements.
  Dav {
    /// Whether the header is still to be written (`needs_header`).
    needs_header: bool,
    /// The refusal that ended the walk, for the caller to answer.
    refused: Option<Failure>,
    /// Whether the walk aborted where `svn_repos_get_file_revs2`'s assertion fails: httpd's child ends, and with it the
    /// connection, whatever was written.
    aborted: bool,
  },
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
  /// Where the revisions are written.
  writer: RevsWriter,
  /// The revision whose delta is being sent, a step at a time (A-751).
  sending: Option<Box<RevDelta>>,
}

/// A revision's delta being sent a step at a time (A-751): its source and target texts, how far its windows are sent,
/// whether its svndiff header is out, the window in the making ([`window_unit`]), and for DAV the base64 so far.
#[derive(Clone, Debug)]
struct RevDelta {
  /// The previous revision's text; none for the first revision.
  source: Option<Text>,
  /// This revision's text; none for a file without one.
  target: Option<Text>,
  /// The target bytes the windows have sent.
  sent: u64,
  /// Whether the delta's svndiff header is written.
  header_sent: bool,
  /// The window in the making, between its steps.
  window: Option<Box<WindowWork>>,
  /// The DAV answer's base64 of the svndiff stream, less a group not yet whole.
  base64: Base64Stream,
}

impl PartialEq for RevDelta {
  /// The same delta: its texts and how far it is sent (a window in the making is that place's).
  fn eq(&self, other: &Self) -> bool {
    (
      self.source,
      self.target,
      self.sent,
      self.header_sent,
      &self.base64,
    ) == (
      other.source,
      other.target,
      other.sent,
      other.header_sent,
      &other.base64,
    )
  }
}

impl Eq for RevDelta {}

impl FileRevsStream {
  /// Sends revisions until the bytes written and the revisions sent reach `room`, at least one step, or until the
  /// answer is whole; [`Progress::Close`] where `svnserve`'s process aborts (an empty mainline, `Stop::Aborted`).
  pub(super) fn resume<R: Repository + ?Sized>(
    &mut self,
    reads: &mut Reads<'_, R>,
    budget: Budget,
  ) -> Result<Progress, ItemError> {
    let room = budget.room;
    let slice = Slice::begin(budget.quantum);
    let start = reads.out.len();
    let mut steps = 0_usize;
    while !self.ended {
      match self.step(reads) {
        Ok(()) => {}
        Err(Stop::Refused(failure)) => self.finish(reads, Some(&failure))?,
        Err(Stop::Aborted) => {
          self.ended = true;
          if let RevsWriter::Dav { aborted, .. } = &mut self.writer {
            *aborted = true;
          }
          return Ok(Progress::Close);
        }
        Err(Stop::Unwritten(error)) => return Err(error),
      }
      steps = steps.saturating_add(1);
      let spent = reads.out.len().saturating_sub(start).saturating_add(steps);
      // A-751: a timed slice ends at the first step past its quantum, at least one step.
      if !self.ended && (spent >= room || slice.spent()) {
        return Ok(Progress::More);
      }
    }
    Ok(Progress::Done)
  }

  /// One step: the revisions found, or the next sent, or the end once none is left. The first writes the
  /// authentication request once the revisions are found, unless their walk aborted: `svnserve` writes the request into
  /// its connection's buffer before it walks, and the process aborts before the buffer is flushed, so its client reads
  /// nothing for the command (the `locked-file-revs-abort` recording).
  fn step<R: Repository + ?Sized>(&mut self, reads: &mut Reads<'_, R>) -> Result<(), Stop> {
    if let Some(delta) = self.sending.take() {
      return self.delta_unit(reads, delta);
    }
    let Some(pending) = self.pending.as_mut() else {
      let found = reads.file_revisions_to_send(&self.request);
      if !matches!(found, Err(Stop::Aborted)) && self.writer == RevsWriter::Items {
        reads.trivial_auth()?;
      }
      self.pending = Some(found?);
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
    if self.writer == RevsWriter::Items {
      let item = Value::List(vec![
        Value::String(revision.path.as_bytes()),
        Value::Number(revision.revision),
        proplist(revision_properties),
        Value::List(diffs.iter().map(diff_value).collect()),
        Value::Word(super::bool_word(revision.merged)),
      ]);
      write(&item, reads.out)?;
      if !changed {
        write(&Value::String(b""), reads.out)?;
      }
    } else {
      let element = file_rev_head(&FileRev {
        path: revision.path.clone(),
        revision: revision.revision,
        rev_props: owned_names(&revision_properties),
        props: diffs
          .iter()
          .map(|(name, value)| (lossy(name), value.map(<[u8]>::to_vec)))
          .collect(),
        merged: revision.merged,
        delta: None,
      });
      self.dav_header(reads.out);
      reads.out.extend_from_slice(element.as_bytes());
      let tail: &[u8] = if changed { TXDELTA_START } else { FILE_REV_END };
      reads.out.extend_from_slice(tail);
    }
    if changed {
      self.sending = Some(Box::new(RevDelta {
        source: self.last.as_ref().and_then(|last| last.text),
        target: node.text(),
        sent: 0,
        header_sent: false,
        window: None,
        base64: Base64Stream::new(),
      }));
    }
    self.last = Some(Last {
      path: revision.path.clone(),
      revision: revision.revision,
      list: node.properties(),
      properties,
      text: node.text(),
    });
    Ok(())
  }

  /// One step of a revision's delta from the previous revision's text (none for the first) to its own (A-751): a unit
  /// of its next window ([`window_unit`], Subversion's windows, the update drive's), each piece of a write
  /// `svn_txdelta_to_svndiff3` makes (`serve.c` `svndiff_handler`; the report editor's `DeltaWrites`' writes, A-616
  /// continued (7)) written as it comes: a string of its own for `svnserve`'s writer, base64 for DAV's. At the text's
  /// end, the header alone where no window wrote it, then the end: the empty string, or the DAV element's close.
  fn delta_unit<R: Repository + ?Sized>(
    &mut self,
    reads: &mut Reads<'_, R>,
    mut delta: Box<RevDelta>,
  ) -> Result<(), Stop> {
    let items = self.writer == RevsWriter::Items;
    let RevDelta {
      source,
      target,
      sent,
      header_sent,
      window,
      base64,
    } = &mut *delta;
    let out = &mut *reads.out;
    let mut sink = |write: usize, at: usize, piece: Vec<u8>| {
      if !items {
        out.extend_from_slice(base64.push(&piece).as_bytes());
        return;
      }
      if at == 0 {
        crate::ra_svn::write_string_head(u64::try_from(write).unwrap_or(u64::MAX), out);
      }
      out.extend_from_slice(&piece);
      if at.saturating_add(piece.len()) >= write {
        out.push(crate::ra_svn::ITEM_END);
      }
    };
    let Some(text) = target.filter(|text| *sent < text.size) else {
      if !*header_sent {
        let header = Encoder::new(self.version).header();
        sink(header.len(), 0, header.to_vec());
      }
      if items {
        write(&Value::String(b""), reads.out)?;
      } else {
        let rest = std::mem::take(base64).finish();
        reads.out.extend_from_slice(rest.as_bytes());
        reads.out.extend_from_slice(TXDELTA_END);
      }
      return Ok(());
    };
    let unit = Delta {
      text,
      source: *source,
      sent,
      header_sent,
    };
    window_unit(
      window,
      unit,
      ((self.version, self.compression), reads.repository),
      &mut sink,
    )
    .map_err(|_| Failure::corrupt())?;
    self.sending = Some(delta);
    Ok(())
  }

  /// `maybe_send_header`: the DAV answer's start, once, before its first revision or its end.
  fn dav_header(&mut self, out: &mut Vec<u8>) {
    if let RevsWriter::Dav { needs_header, .. } = &mut self.writer
      && std::mem::take(needs_header)
    {
      out.extend_from_slice(FILE_REVS_START.as_bytes());
    }
  }

  /// `done`, then `( success ( ) )` or the refusal; for DAV, the report's end, or the refusal kept for the caller.
  fn finish<R: Repository + ?Sized>(
    &mut self,
    reads: &mut Reads<'_, R>,
    failure: Option<&Failure>,
  ) -> Result<(), ItemError> {
    self.ended = true;
    if let RevsWriter::Dav { refused, .. } = &mut self.writer {
      match failure {
        Some(failure) => *refused = Some(failure.clone()),
        None => {
          self.dav_header(reads.out);
          reads.out.extend_from_slice(FILE_REVS_END.as_bytes());
        }
      }
      return Ok(());
    }
    write(&Value::Word(DONE), reads.out)?;
    match failure {
      None => reads.success(Vec::new())?,
      Some(failure) => reads.failure(failure)?,
    };
    Ok(())
  }
}

impl<R: Repository + ?Sized> Reads<'_, R> {
  /// `svn_repos_get_file_revs2`'s revisions, in the order they are sent; [`Stop::Aborted`] where its assertion that the
  /// mainline holds a revision fails, the file's first location being one the session may not read.
  fn file_revisions_to_send(&self, request: &FileRevsRequest) -> Result<VecDeque<PathRev>, Stop> {
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
        return Err(Failure::new(SVN_ERR_UNSUPPORTED_FEATURE, message).into());
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
    if mainline.is_empty() {
      return Err(Stop::Aborted);
    }
    let merged = if request.merged {
      self.merged_revisions(start, &mainline, &mut found)?
    } else {
      Vec::new()
    };
    Ok(interleaved(mainline, merged))
  }

  /// `find_interesting_revisions`: the file's history from `end` back across copies, each location kept until one at
  /// or before `start`, or one already found when merged revisions are included, or one the session may not read;
  /// refused when the path is no file in `end`.
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
      if (walk.merged && found.contains(&key)) || !self.readable(&location.path) {
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
    merged.sort_by_key(|location| core::cmp::Reverse(location.revision));
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

/// Why a step stopped: a refusal the answer carries after `done`, the connection's end where `svnserve` aborts, or an
/// item that could not be written.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Stop {
  /// A refusal: the answer ends with `done` and it.
  Refused(Failure),
  /// `SVN_ERR_ASSERT(mainline_path_revisions->nelts > 0)` failing: `svnserve` sets no malfunction handler, so
  /// Subversion's default aborts its process and the connection ends with nothing written for the command, its
  /// authentication request lost in the unflushed buffer; here it ends the same way.
  Aborted,
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

/// Revision properties with their names as text, as `send_prop` writes them.
fn owned_names(properties: &[(&[u8], &[u8])]) -> Vec<(String, Vec<u8>)> {
  properties
    .iter()
    .map(|(name, value)| (lossy(name), value.to_vec()))
    .collect()
}

/// A property name as text.
fn lossy(name: &[u8]) -> String {
  String::from_utf8_lossy(name).into_owned()
}

/// A `file-revs-report`'s answer still to write (`reports/file-revs.c`, A-726): `svn_repos_get_file_revs2`'s walk above,
/// with `mod_dav_svn`'s handler in place of `svnserve`'s. The request's path is joined to the repository path of the
/// resource the REPORT names (`svn_fspath__join`); its revisions are read as the svn:// command reads them. The answer
/// is written a revision a step: the header before the first revision (`maybe_send_header`), each revision's
/// `S:file-rev` with its revision properties, its property changes, `S:merged-revision` where merged, and its text's
/// delta in base64 where it changed, then the report's end. A refusal before anything is written is the caller's to
/// answer as `mod_dav` answers a report's error; one after, or the walk's abort, ends the connection ([`Self::begun`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DavFileRevs {
  /// The walk, its revisions going to `mod_dav_svn`'s writer.
  stream: FileRevsStream,
}

impl DavFileRevs {
  /// The walk `report` asks for, its path beneath `target`, the repository path of the resource the REPORT names.
  pub fn new(report: &FileRevsReport, target: &str) -> Self {
    let signed = |revision: Revision| i64::try_from(revision).unwrap_or(i64::MAX);
    let request = FileRevsRequest {
      path: full_path(target, report.path.as_bytes()),
      start: report.start.map(signed),
      end: report.end.map(signed),
      merged: report.include_merged_revisions,
    };
    Self {
      stream: FileRevsStream {
        request,
        version: DAV_SVNDIFF,
        compression: Compression::Stored,
        pending: None,
        last: None,
        ended: false,
        writer: RevsWriter::Dav {
          needs_header: true,
          refused: None,
          aborted: false,
        },
        sending: None,
      },
    }
  }

  /// Writes the answer's next part into `out`, read from `index` and `repository` with each path judged by `rules`
  /// (none: every path readable), until what this slice wrote and the steps it took reach `room`, at least one step,
  /// or the walk ends.
  pub fn resume<R: Repository + ?Sized>(
    &mut self,
    index: &crate::index::Index,
    repository: &R,
    rules: Option<&PrincipalRules>,
    out: &mut Vec<u8>,
    budget: Budget,
  ) -> DavLogProgress {
    let mut reads = Reads {
      index,
      repository,
      base: ROOT,
      uuid: "",
      authz: rules,
      authentication: Authentication::Settled,
      out,
    };
    match self.stream.resume(&mut reads, budget) {
      Ok(Progress::More) => DavLogProgress::More,
      Ok(Progress::Done | Progress::Close) => match &self.stream.writer {
        RevsWriter::Dav {
          refused: Some(failure),
          ..
        } => DavLogProgress::Refused {
          code: failure.code,
          message: failure.message.clone(),
        },
        RevsWriter::Dav { aborted: true, .. } => {
          let corrupt = Failure::corrupt();
          DavLogProgress::Refused {
            code: corrupt.code,
            message: corrupt.message,
          }
        }
        _ => DavLogProgress::Done,
      },
      // An item that cannot be written is `svnserve`'s writer's alone; this writer writes no item.
      Err(_) => {
        let corrupt = Failure::corrupt();
        DavLogProgress::Refused {
          code: corrupt.code,
          message: corrupt.message,
        }
      }
    }
  }

  /// Whether any of the answer is written, or the walk aborted: either way a refusal can only end the connection.
  pub fn begun(&self) -> bool {
    match &self.stream.writer {
      RevsWriter::Dav {
        needs_header,
        aborted,
        ..
      } => !needs_header || *aborted,
      RevsWriter::Items => true,
    }
  }
}
