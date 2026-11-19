//! `log` (§4.17.4 "4. Reads": per path the history list gives revisions newest first, crossing copies unless
//! `strict-node` is set, the revision rows the changed paths, the revision-property table the properties; A-613 part
//! 2): the log entries of paths over a range of revisions, as `svnserve`'s `log_cmd` and `svn_repos_get_logs5` write
//! them (`serve.c`, `libsvn_repos/log.c`, Subversion 1.15.0), held to stock `svnserve`'s transcripts
//! (`crates/svn/fixtures/reads`, sessions `log`, `log-merged`, the three `hostile-log-*`, `svn-log`, `svn-log-g` and
//! the two `svn-mergeinfo-*`):
//! - The parameters are `l(?r)(?r)bb?n?Bwl`, read as `vparse_tuple` reads them: the paths (each a string), the start
//!   and end revisions, whether changed paths are sent, `strict-node`, then optionally the limit, then optionally
//!   whether merged revisions are included, the word `all-revprops` or `revprops` and a list of names (each a string).
//!   Five items are a client before 1.5's (the author, date and log asked for), six add the limit, nine or more are
//!   whole; seven or eight, another word, or an item of another kind end the connection. A limit past `INT_MAX` is
//!   none.
//! - After the authentication request: a missing or negative start or end is the youngest, either past it refused after
//!   `done` ("No such revision N"); the range walked newest first when the start is not older than the end, else
//!   oldest first (each revision found first, then sent in reverse).
//! - No paths, or the root alone, without merged revisions: every revision of the range (`svn_repos_get_logs5`'s
//!   shortcut). Otherwise `do_logs`: each path's node history (module `history`) from the range's youngest revision,
//!   the absent path refused ("File not found"), every path's history checked at each revision in turn, a revision
//!   sent when any path's history holds it, until every history is done or the limit is reached.
//! - An entry is `( ( changed-path ... ) rev ( author ) ( date ) ( message ) has-children invalid-revnum count
//!   ( props ) subtractive-merge )`: the changed paths only when asked and never for revision 0, in the revision's
//!   stored order (byte order of the paths), each `( path action ( copy-path copy-rev ) ( kind text-mod prop-mod ) )`;
//!   the revision properties as `fill_log_entry` gathers them (every one, or those named; when the names match the
//!   revision's own one for one, all of them) and `revision_receiver` splits them: `svn:author`, `svn:date` and
//!   `svn:log` in their own places, the rest counted and listed, unless the three are all there are.
//! - With `include-merged-revisions` (module `merged`): the log's own history as mergeinfo first; then at each revision
//!   sent, the mergeinfo it changed for the paths, and when it merged or reverse merged anything, the entry marked as
//!   having children, which follow it: each run of merged revisions walked by a nested `do_logs` (newest first, no
//!   limit, its histories opened leniently, its searches recorded so none is repeated), a merged revision sent only
//!   when one of its changed paths lies outside the log's own history and only once, reverse merges marked
//!   subtractive; then an entry of revision 0 marked `invalid-revnum` closing the children (`handle_merged_revisions`).
//!   A merged revision passed over has its changed paths written all the same, which the next entry carries, as
//!   `svnserve` writes them (`interesting_merge` calls the client's receiver first).
//! - Then `done`, then `( success ( ) )` or the refusal.
//!
//! The walk is a [`super::Stream`] whose state is a stack of frames, one revision a step. Its entries go to one of two
//! writers: `svnserve`'s receivers' items, or `mod_dav_svn`'s `log-report` (module [`dav`], A-681), the walk the same.

pub(super) mod dav;
mod merged;

use super::history::History;
use super::stream::{Progress, Stream};
use super::{
  AUTHOR_PROPERTY, DATE_PROPERTY, DONE, Failure, Outcome, ROOT, Reads, Repository, RevisionAccess,
  boolean, c_string_of, full_path, kind_word, optional_revnum,
};
use crate::index::{Action, ChangedPath, Revision};
use crate::mergeinfo::Mergeinfo;
use crate::ra_svn::{ItemError, Value, write};
use merged::{
  MergeinfoChanges, PathListRange, combine, outside_history, reduce_search, store_search,
};
use std::collections::BTreeSet;

/// The revision property holding a revision's log message.
/// Format: `SVN_PROP_REVISION_LOG` (`svn_props.h`).
const LOG_PROPERTY: &[u8] = b"svn:log";

/// The three revision properties an entry carries in their own places, never among the rest it counts and lists.
/// Format: `svn_compat_log_revprops_out_string` and `svn_compat_log_revprops_clear` (`compat.c`), called by
/// `revision_receiver` (`serve.c`), whose shortcut for an entry with these three alone (`apr_hash_count(...) == 3`)
/// gives the same answer as removing them: nothing left.
const STANDARD_REVPROPS: [&[u8]; 3] = [AUTHOR_PROPERTY, DATE_PROPERTY, LOG_PROPERTY];

/// The word asking for every revision property.
/// Format: `log_cmd` (`serve.c`).
const ALL_REVPROPS: &str = "all-revprops";

/// The word asking for the revision properties named after it.
/// Format: `log_cmd` (`serve.c`).
const NAMED_REVPROPS: &str = "revprops";

/// The largest limit `log_cmd` takes; above it the log has none.
/// Format: `limit > INT_MAX` (`serve.c` `log_cmd`), C's `int` of 32 bits on every platform `svnserve` is built for.
const LIMIT_CEILING: u64 = 2_147_483_647;

/// The histories `get_path_histories` keeps open; past them a path's history is opened again at each step.
/// Format: `MAX_OPEN_HISTORIES` (`log.c`).
const OPEN_HISTORIES: usize = 32;

/// The items of a log request from a client before 1.5: no limit, no merged revisions, no revision-property word.
/// Format: `"l(?r)(?r)bb?n?Bwl"`'s items up to its first `?` (`serve.c` `log_cmd`, `marshal.c` `vparse_tuple`).
const COMPAT_ITEMS: usize = 5;

/// The items of a log request that adds a limit alone.
/// Format: `"l(?r)(?r)bb?n?Bwl"`'s items up to its second `?`.
const LIMIT_ITEMS: usize = 6;

/// The items of a whole log request.
/// Format: `"l(?r)(?r)bb?n?Bwl"`'s nine items; seven or eight end the prototype at `w` or `l`, which is malformed.
const WHOLE_ITEMS: usize = 9;

/// The bytes that open a list item by item.
/// Format: `marshal.c` `svn_ra_svn__start_list`, `"( "`.
const LIST_OPEN: &[u8] = b"( ";

/// The bytes that close a list opened item by item.
/// Format: `marshal.c` `svn_ra_svn__end_list`, `") "`.
const LIST_CLOSE: &[u8] = b") ";

/// The revision properties a log names.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Revprops {
  /// Every one (`all-revprops`).
  All,
  /// Those named, in the order named (`revprops`, or a client before 1.5's three).
  Named(Vec<Vec<u8>>),
}

/// A parsed `log` request.
#[derive(Clone, Debug, PartialEq, Eq)]
struct LogRequest {
  /// The paths, each the session's base joined with the given path.
  paths: Vec<String>,
  /// The start revision as given.
  start: Option<i64>,
  /// The end revision as given.
  end: Option<i64>,
  /// Whether changed paths are sent.
  changed_paths: bool,
  /// Whether histories stop at copies.
  strict: bool,
  /// The most revisions sent at the top level; 0 for no limit.
  limit: u64,
  /// Whether merged revisions are included.
  merged: bool,
  /// The revision properties named.
  revprops: Revprops,
}

impl LogRequest {
  /// `svn_ra_svn__parse_tuple(params, "l(?r)(?r)bb?n?Bwl")`, then `log_cmd`'s reading of its word, names and paths.
  fn parse(params: &[Value<'_>], base: &str) -> Option<Self> {
    let Some(Value::List(paths)) = params.first() else {
      return None;
    };
    let paths = paths
      .iter()
      .map(|path| match path {
        Value::String(bytes) => Some(full_path(base, c_string_of(bytes))),
        _ => None,
      })
      .collect::<Option<Vec<String>>>()?;
    let (limit, merged, revprops) = match params.len() {
      COMPAT_ITEMS => (0, false, Revprops::Named(compat_revprops())),
      LIMIT_ITEMS => (
        revnum_count(params.get(5))?,
        false,
        Revprops::Named(compat_revprops()),
      ),
      count if count >= WHOLE_ITEMS => Self::whole_tail(params)?,
      _ => return None,
    };
    Some(Self {
      paths,
      start: optional_revnum(params.get(1))?,
      end: optional_revnum(params.get(2))?,
      changed_paths: boolean(params.get(3))?,
      strict: boolean(params.get(4))?,
      limit: if limit > LIMIT_CEILING { 0 } else { limit },
      merged,
      revprops,
    })
  }

  /// The items after `strict-node` in a whole request: the limit, whether merged revisions are included, the word and
  /// its names.
  fn whole_tail(params: &[Value<'_>]) -> Option<(u64, bool, Revprops)> {
    let limit = revnum_count(params.get(5))?;
    let merged = boolean(params.get(6))?;
    let Some(Value::Word(word)) = params.get(7) else {
      return None;
    };
    let Some(Value::List(names)) = params.get(8) else {
      return None;
    };
    let revprops = match *word {
      ALL_REVPROPS => Revprops::All,
      NAMED_REVPROPS => Revprops::Named(
        names
          .iter()
          .map(|name| match name {
            Value::String(bytes) => Some(c_string_of(bytes).to_vec()),
            _ => None,
          })
          .collect::<Option<Vec<Vec<u8>>>>()?,
      ),
      _ => return None,
    };
    Some((limit, merged, revprops))
  }
}

/// An `n` parameter: a number.
fn revnum_count(param: Option<&Value<'_>>) -> Option<u64> {
  match param {
    Some(Value::Number(number)) => Some(*number),
    _ => None,
  }
}

/// `svn_compat_log_revprops_in`: the three revision properties a client before 1.5 gets.
fn compat_revprops() -> Vec<Vec<u8>> {
  vec![
    AUTHOR_PROPERTY.to_vec(),
    DATE_PROPERTY.to_vec(),
    LOG_PROPERTY.to_vec(),
  ]
}

impl<R: Repository + ?Sized> Reads<'_, R> {
  /// `log ( ( path ... ) [ start ] [ end ] changed-paths strict ? limit ? merged revprops-word ( name ... ) )`
  /// (`serve.c` `log_cmd`): the authentication request, then a [`Stream`] of the log.
  pub(super) fn log(&mut self, params: &[Value<'_>]) -> Result<Outcome, ItemError> {
    let Some(request) = LogRequest::parse(params, self.base) else {
      return Ok(Outcome::Close);
    };
    self.trivial_auth()?;
    let writer = Writer::Items { started: false };
    Ok(Outcome::Streaming(Stream::log(LogStream::new(
      request, writer,
    ))))
  }
}

/// One path's history as `log.c`'s `struct path_info` follows it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PathInfo {
  /// The path at the last location reached.
  path: String,
  /// The revision of the last location reached.
  history_rev: Revision,
  /// Whether the history is done.
  done: bool,
  /// Whether the history, opened again at each step, has yet to take its first.
  first_time: bool,
  /// The history kept open (the first [`OPEN_HISTORIES`] paths); `None` for one opened again at each step, or ended.
  history: Option<History>,
  /// Whether the history is kept open.
  kept: bool,
}

/// One `do_logs` call: the histories of its paths walked over its range.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Walk {
  /// The histories.
  histories: Vec<PathInfo>,
  /// The revision the next step checks.
  current: Revision,
  /// The range's oldest revision.
  start: Revision,
  /// Whether revisions are sent as found (newest first) rather than gathered and sent oldest first.
  descending: bool,
  /// The revisions found, to send oldest first, each with the mergeinfo changes it made.
  found: Vec<Found>,
  /// The revisions sent so far.
  sent: u64,
  /// The most revisions sent; 0 for no limit.
  limit: u64,
  /// Whether merged revisions are included.
  merged: bool,
  /// Whether this walk is of merged revisions (`handling_merged_revisions`).
  handling: bool,
  /// Whether its revisions were reverse merged (`subtractive_merge`).
  subtractive: bool,
  /// Whether this walk made the shared record of merged revisions sent, which goes when it ends.
  owns: bool,
}

/// A revision found, with the mergeinfo changes it made.
type Found = (Revision, MergeinfoChanges);

/// What `send_log` is told of an entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Sending {
  /// Whether merged revisions follow it.
  has_children: bool,
  /// Whether it is reverse merged.
  subtractive: bool,
  /// Whether it is a merged revision.
  handling: bool,
}

/// A frame of the log's walk.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Frame {
  /// `svn_repos_get_logs5`'s shortcut: every revision from `next`, `remaining` of them, newest first or oldest first.
  Every {
    /// The next revision to send.
    next: Revision,
    /// The revisions still to send.
    remaining: u64,
    /// Whether newest first.
    descending: bool,
  },
  /// `do_logs`' walk of the histories.
  Walk(Walk),
  /// The revisions a walk found, sent oldest first: those not yet sent, the last first.
  Ascending {
    /// The revisions still to send, the next last.
    revisions: Vec<Found>,
    /// The revisions sent so far.
    sent: u64,
    /// The most revisions sent; 0 for no limit.
    limit: u64,
  },
  /// A revision's merged revisions (`handle_merged_revisions`): its runs, each walked in turn from the last, then the
  /// entry closing them.
  Children {
    /// The runs still to walk, the next last.
    runs: Vec<PathListRange>,
  },
  /// The end of the walk that made the shared record of merged revisions sent, after its last children.
  Release,
}

/// What the receivers keep between entries (`serve.c`'s `log_baton_t`, `reports/log.c`'s `log_receiver_baton`).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Receiver {
  /// How many entries with children are open.
  stack_depth: u64,
  /// Where the entries are written.
  writer: Writer,
}

/// Where a log's entries are written: as `svnserve`'s receivers write them on an ra_svn connection, or as
/// `mod_dav_svn`'s write a `log-report` ([`dav`], A-681).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Writer {
  /// `serve.c`'s `path_change_receiver` and `revision_receiver`.
  Items {
    /// Whether an entry's two lists are open: its changed paths have begun.
    started: bool,
  },
  /// `reports/log.c`'s `log_change_receiver` and `log_revision_receiver`.
  Dav(dav::DavWriter),
}

/// A log still to write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct LogStream {
  /// The request.
  request: LogRequest,
  /// The walk's frames, the innermost last; empty before it begins and after it ends.
  frames: Vec<Frame>,
  /// Whether the walk has begun.
  begun: bool,
  /// Whether `done` and the answer are written.
  ended: bool,
  /// What the receiver keeps between entries.
  receiver: Receiver,
  /// The log's own history as mergeinfo (`log_target_history_as_mergeinfo`); empty without merged revisions.
  history: Mergeinfo,
  /// The merged revisions sent (`nested_merges`), while a walk of merged revisions keeps it.
  nested: Option<BTreeSet<Revision>>,
  /// The sources and ranges nested walks have searched (`processed`), while the newest-first walk keeps it.
  processed: Option<Mergeinfo>,
}

impl LogStream {
  /// A log of a request, not begun, its entries going to `writer`.
  fn new(request: LogRequest, writer: Writer) -> Self {
    Self {
      request,
      frames: Vec::new(),
      begun: false,
      ended: false,
      receiver: Receiver {
        stack_depth: 0,
        writer,
      },
      history: Mergeinfo::default(),
      nested: None,
      processed: None,
    }
  }

  /// Steps until the bytes written and the steps taken reach `room`, at least one step, or until the log ends.
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

  /// One step: the walk begun, or one revision of the innermost frame, or the end once no frame is left.
  fn step<R: Repository + ?Sized>(&mut self, reads: &mut Reads<'_, R>) -> Result<(), Stop> {
    if !self.begun {
      self.begun = true;
      return self.begin(reads);
    }
    match self.frames.pop() {
      None => Ok(self.finish(reads, None)?),
      Some(Frame::Every {
        next,
        remaining,
        descending,
      }) => self.every(reads, next, remaining, descending),
      Some(Frame::Walk(walk)) => self.walk(reads, walk),
      Some(Frame::Ascending {
        revisions,
        sent,
        limit,
      }) => self.ascending(reads, revisions, sent, limit),
      Some(Frame::Children { runs }) => self.children(reads, runs),
      Some(Frame::Release) => {
        self.nested = None;
        self.processed = None;
        Ok(())
      }
    }
  }

  /// `svn_repos_get_logs5` before the walk: the range settled and checked, then the shortcut for no paths or the root
  /// alone (refused, "Item is not readable", where the session may not read the root), else `do_logs` over the paths'
  /// histories.
  fn begin<R: Repository + ?Sized>(&mut self, reads: &mut Reads<'_, R>) -> Result<(), Stop> {
    let head = reads.index.youngest();
    let settle = |revision: Option<i64>| -> Result<Revision, Failure> {
      match revision.filter(|&revision| revision >= 0) {
        None => Ok(head),
        Some(revision) => reads.existing(revision),
      }
    };
    let start = settle(self.request.start)?;
    let end = settle(self.request.end)?;
    let descending = start >= end;
    let (oldest, youngest) = (start.min(end), start.max(end));
    let paths = &self.request.paths;
    let root_alone =
      paths.is_empty() || (paths.len() == 1 && paths.first().is_some_and(|path| path == ROOT));
    if root_alone && !self.request.merged {
      if !reads.readable(ROOT) {
        return Err(Failure::unreadable_item().into());
      }
      let count = youngest.saturating_sub(oldest).saturating_add(1);
      let remaining = match self.request.limit {
        0 => count,
        limit => count.min(limit),
      };
      let next = if descending { youngest } else { oldest };
      self.frames.push(Frame::Every {
        next,
        remaining,
        descending,
      });
      return Ok(());
    }
    if self.request.merged {
      self.history = reads.history_as_mergeinfo(paths, oldest, youngest)?;
    }
    let histories = reads.path_histories(paths, oldest, youngest, self.request.strict, false)?;
    self.frames.push(Frame::Walk(Walk {
      histories,
      current: youngest,
      start: oldest,
      descending,
      found: Vec::new(),
      sent: 0,
      limit: self.request.limit,
      merged: self.request.merged,
      handling: false,
      subtractive: false,
      owns: false,
    }));
    Ok(())
  }

  /// The shortcut's next revision.
  fn every<R: Repository + ?Sized>(
    &mut self,
    reads: &mut Reads<'_, R>,
    next: Revision,
    remaining: u64,
    descending: bool,
  ) -> Result<(), Stop> {
    if remaining == 0 {
      return Ok(());
    }
    self.send(reads, next, Sending::top(false))?;
    let following = if descending {
      next.checked_sub(1)
    } else {
      next.checked_add(1)
    };
    if let Some(following) = following {
      self.frames.push(Frame::Every {
        next: following,
        remaining: remaining.saturating_sub(1),
        descending,
      });
    }
    Ok(())
  }

  /// One revision of `do_logs`' loop: every history checked at the current revision; when one holds it, its mergeinfo
  /// changes for the histories' next locations (with merged revisions), then the revision sent with its children
  /// (newest first) or gathered (oldest first); then the next revision any history holds.
  fn walk<R: Repository + ?Sized>(
    &mut self,
    reads: &mut Reads<'_, R>,
    mut walk: Walk,
  ) -> Result<(), Stop> {
    let mut changed = false;
    let mut any_left = false;
    for info in &mut walk.histories {
      changed |= reads.check_history(info, walk.current, self.request.strict, walk.start)?;
      any_left |= !info.done;
    }
    let mut finished = false;
    let mut children = None;
    if changed {
      let changes = if walk.merged {
        let paths: Vec<String> = walk
          .histories
          .iter()
          .map(|info| info.path.clone())
          .collect();
        reads.mergeinfo_changes(&paths, walk.current)?
      } else {
        MergeinfoChanges::default()
      };
      if walk.descending {
        let has_children = changes.any();
        self.send(reads, walk.current, walk.sending(has_children))?;
        if has_children {
          if self.nested.is_none() {
            self.nested = Some(BTreeSet::new());
            self.processed = Some(Mergeinfo::default());
            walk.owns = true;
          }
          children = Some(children_of(&changes));
        }
        walk.sent = walk.sent.saturating_add(1);
        finished = walk.limit != 0 && walk.sent >= walk.limit;
      } else {
        walk.found.push((walk.current, changes));
      }
    }
    self.walk_on(walk, finished || !any_left);
    self.frames.extend(children);
    Ok(())
  }

  /// The walk's frame after a revision: the walk again at the next revision a history holds, or, when it has ended,
  /// what follows it: the revisions it gathered, sent oldest first, and the end of the record it made.
  fn walk_on(&mut self, mut walk: Walk, ended: bool) {
    let next = next_history_rev(&walk.histories);
    let ended = ended || next.is_none_or(|next| next >= walk.current);
    if !ended && let Some(next) = next {
      walk.current = next;
      self.frames.push(Frame::Walk(walk));
      return;
    }
    if walk.owns {
      self.frames.push(Frame::Release);
    }
    if !walk.found.is_empty() {
      self.frames.push(Frame::Ascending {
        revisions: walk.found,
        sent: 0,
        limit: walk.limit,
      });
    }
  }

  /// The gathered revisions' next, oldest first, with its children, until the limit.
  fn ascending<R: Repository + ?Sized>(
    &mut self,
    reads: &mut Reads<'_, R>,
    mut revisions: Vec<Found>,
    sent: u64,
    limit: u64,
  ) -> Result<(), Stop> {
    let Some((revision, changes)) = revisions.pop() else {
      return Ok(());
    };
    let has_children = changes.any();
    self.send(reads, revision, Sending::top(has_children))?;
    let sent = sent.saturating_add(1);
    let limited = limit != 0 && sent >= limit;
    if !limited && !revisions.is_empty() {
      self.frames.push(Frame::Ascending {
        revisions,
        sent,
        limit,
      });
    }
    if has_children {
      self.nested.get_or_insert_with(BTreeSet::new);
      self.frames.push(children_of(&changes));
    }
    Ok(())
  }

  /// `handle_merged_revisions`' next run: a nested `do_logs` over its sources and revisions, what earlier walks
  /// searched taken off and this search recorded (`reduce_search`, `store_search`); when no run is left, the entry
  /// closing the children.
  fn children<R: Repository + ?Sized>(
    &mut self,
    reads: &mut Reads<'_, R>,
    mut runs: Vec<PathListRange>,
  ) -> Result<(), Stop> {
    let Some(run) = runs.pop() else {
      return Ok(self.end_marker(reads)?);
    };
    self.frames.push(Frame::Children { runs });
    let (mut paths, mut start, mut end) = (run.paths, run.start, run.end);
    if let Some(processed) = &self.processed {
      reduce_search(&mut paths, &mut start, &mut end, processed);
    }
    if paths.is_empty() {
      return Ok(());
    }
    if let Some(processed) = &mut self.processed {
      store_search(processed, &paths, start, end);
    }
    let histories = reads.path_histories(&paths, start, end, self.request.strict, true)?;
    self.frames.push(Frame::Walk(Walk {
      histories,
      current: end,
      start,
      descending: true,
      found: Vec::new(),
      sent: 0,
      limit: 0,
      merged: true,
      handling: true,
      subtractive: run.reverse,
      owns: false,
    }));
    Ok(())
  }

  /// `send_log` and `fill_log_entry`: the revision's changed paths when asked (never revision 0's), its revision
  /// properties as asked, then its entry; a merged revision only when a changed path lies outside the log's own history
  /// (`interesting_merge`) and it was not sent before (`nested_merges`), its changed paths written either way. With
  /// rules, `detect_changed`: a changed path the session may not read is left out, and a copy source it may not read
  /// dropped from its change; then the revision properties shown are as many as the session may read of the revision.
  fn send<R: Repository + ?Sized>(
    &mut self,
    reads: &mut Reads<'_, R>,
    revision: Revision,
    sending: Sending,
  ) -> Result<(), Stop> {
    let judged = sending.handling && !self.history.is_empty();
    let (outside, access) =
      if revision > 0 && (self.request.changed_paths || judged || reads.authz.is_some()) {
        self.detect_changed(reads, revision, judged)?
      } else {
        (false, RevisionAccess::Full)
      };
    let properties = self.entry_properties(reads, revision, access);
    if judged && !outside {
      return Ok(());
    }
    if sending.handling
      && let Some(nested) = &mut self.nested
      && !nested.insert(revision)
    {
      return Ok(());
    }
    let flags = EntryFlags {
      has_children: sending.has_children,
      invalid: false,
      subtractive: sending.subtractive,
    };
    self.entry(reads, revision, flags, properties)?;
    Ok(())
  }

  /// `detect_changed` with the receivers `send_log` gives it: each changed path the session may read written when
  /// asked (its copy source too when the session may read that), and, when `judged`, whether one of them lies outside
  /// the log's own history (`interesting_merge`). Answers that, and how much of the revision the session may read: all
  /// of a revision that changed nothing.
  fn detect_changed<R: Repository + ?Sized>(
    &mut self,
    reads: &mut Reads<'_, R>,
    revision: Revision,
    judged: bool,
  ) -> Result<(bool, RevisionAccess), Stop> {
    let changes = reads
      .index
      .changes(revision)
      .map_err(|refusal| Failure::of_index(&refusal))?;
    if changes.is_empty() {
      return Ok((false, RevisionAccess::Full));
    }
    let (mut found_readable, mut found_unreadable, mut outside) = (false, false, false);
    for change in changes {
      if !reads.readable(&change.path) {
        found_unreadable = true;
        continue;
      }
      found_readable = true;
      let source_readable = change
        .copy_from
        .as_ref()
        .is_none_or(|source| reads.readable(&source.path));
      found_unreadable |= !source_readable;
      if self.request.changed_paths {
        self.changed_path(reads, change, source_readable)?;
      }
      outside = outside || (judged && outside_history(&self.history, revision, &change.path));
    }
    Ok((
      outside,
      RevisionAccess::of(found_readable, found_unreadable),
    ))
  }

  /// `handle_merged_revisions`' last entry, revision 0 marked `invalid-revnum`, closing an entry's children; nothing
  /// when no entry with children is open (`revision_receiver`'s `stack_depth`).
  fn end_marker<R: Repository + ?Sized>(
    &mut self,
    reads: &mut Reads<'_, R>,
  ) -> Result<(), ItemError> {
    let Some(depth) = self.receiver.stack_depth.checked_sub(1) else {
      return Ok(());
    };
    self.receiver.stack_depth = depth;
    let flags = EntryFlags {
      has_children: false,
      invalid: true,
      subtractive: false,
    };
    self.entry(reads, 0, flags, None)
  }

  /// `path_change_receiver`: one changed path, the entry's two lists opened first; its copy source written when the
  /// session may read it. A `log-report`'s writer writes it as `log_change_receiver` does.
  fn changed_path<R: Repository + ?Sized>(
    &mut self,
    reads: &mut Reads<'_, R>,
    change: &ChangedPath,
    source_readable: bool,
  ) -> Result<(), ItemError> {
    let started = match &mut self.receiver.writer {
      Writer::Items { started } => started,
      Writer::Dav(writer) => {
        writer.changed_path(reads.out, change, source_readable);
        return Ok(());
      }
    };
    if !*started {
      reads.out.extend_from_slice(LIST_OPEN);
      reads.out.extend_from_slice(LIST_OPEN);
      *started = true;
    }
    let action = match change.action {
      Action::Modify => "M",
      Action::Add => "A",
      Action::Delete => "D",
      Action::Replace => "R",
    };
    let copy = change
      .copy_from
      .iter()
      .filter(|_| source_readable)
      .flat_map(|source| {
        [
          Value::String(source.path.as_bytes()),
          Value::Number(source.revision),
        ]
      })
      .collect();
    let flags = Value::List(vec![
      Value::String(kind_word(change.kind).as_bytes()),
      Value::Word(super::bool_word(change.text.is_some())),
      Value::Word(super::bool_word(change.properties.is_some())),
    ]);
    write(
      &Value::List(vec![
        Value::String(change.path.as_bytes()),
        Value::Word(action),
        Value::List(copy),
        flags,
      ]),
      reads.out,
    )
  }

  /// `fill_log_entry`'s revision properties: none when none are wanted or the session may read none of the revision;
  /// every one; or, when the names asked match the revision's own one for one, every one; else those named that the
  /// revision has. Where the session may read the revision in part, the author and the date alone of those, the names
  /// asked always matched one by one.
  fn entry_properties<'value, R: Repository + ?Sized>(
    &self,
    reads: &Reads<'value, R>,
    revision: Revision,
    access: RevisionAccess,
  ) -> Option<Vec<(&'value [u8], &'value [u8])>> {
    if access == RevisionAccess::Nothing {
      return None;
    }
    let wanted = match &self.request.revprops {
      Revprops::All => None,
      Revprops::Named(names) if names.is_empty() => return None,
      Revprops::Named(names) => Some(names),
    };
    let all = reads
      .repository
      .revision_properties(revision)
      .unwrap_or_default();
    let Some(names) = wanted else {
      return Some(
        all
          .into_iter()
          .filter(|(name, _)| access.shows(name))
          .collect(),
      );
    };
    let matched = access == RevisionAccess::Full
      && names.len() == all.len()
      && names
        .iter()
        .all(|name| all.iter().any(|(held, _)| *held == name.as_slice()));
    if matched {
      return Some(all);
    }
    let mut chosen: Vec<(&[u8], &[u8])> = Vec::new();
    for name in names.iter().filter(|name| access.shows(name)) {
      if let Some(&(held, value)) = all.iter().find(|(held, _)| *held == name.as_slice())
        && !chosen.iter().any(|(kept, _)| *kept == held)
      {
        chosen.push((held, value));
      }
    }
    Some(chosen)
  }

  /// `revision_receiver`, or a `log-report`'s writer's `log_revision_receiver`: the entry written, then one more entry
  /// with children open when it has them.
  fn entry<R: Repository + ?Sized>(
    &mut self,
    reads: &mut Reads<'_, R>,
    revision: Revision,
    flags: EntryFlags,
    properties: Option<Vec<(&[u8], &[u8])>>,
  ) -> Result<(), ItemError> {
    let properties = properties.unwrap_or_default();
    match &mut self.receiver.writer {
      Writer::Items { started } => items_entry(reads.out, started, revision, flags, &properties)?,
      Writer::Dav(writer) => writer.entry(reads.out, revision, flags, &properties),
    }
    if flags.has_children {
      self.receiver.stack_depth = self.receiver.stack_depth.saturating_add(1);
    }
    Ok(())
  }

  /// `done`, then `( success ( ) )` or the refusal; a `log-report`'s writer ends its answer, or keeps the refusal for
  /// its caller.
  fn finish<R: Repository + ?Sized>(
    &mut self,
    reads: &mut Reads<'_, R>,
    failure: Option<&Failure>,
  ) -> Result<(), ItemError> {
    self.ended = true;
    self.frames.clear();
    if let Writer::Dav(writer) = &mut self.receiver.writer {
      writer.finish(reads.out, failure);
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

/// `revision_receiver` (`serve.c`): the entry's changed-path list closed (opened first when it holds none, as `started`
/// says), then the revision, the author, date and message in their places, the flags, the other revision properties
/// counted and listed, and whether it is a reverse merge.
fn items_entry(
  out: &mut Vec<u8>,
  started: &mut bool,
  revision: Revision,
  flags: EntryFlags,
  properties: &[(&[u8], &[u8])],
) -> Result<(), ItemError> {
  let find = |name: &[u8]| {
    properties
      .iter()
      .find(|(held, _)| *held == name)
      .map(|(_, value)| *value)
  };
  let (author, date, message) = (
    find(AUTHOR_PROPERTY),
    find(DATE_PROPERTY),
    find(LOG_PROPERTY),
  );
  let rest: Vec<(&[u8], &[u8])> = properties
    .iter()
    .copied()
    .filter(|(name, _)| !STANDARD_REVPROPS.contains(name))
    .collect();
  if !*started {
    out.extend_from_slice(LIST_OPEN);
    out.extend_from_slice(LIST_OPEN);
  }
  out.extend_from_slice(LIST_CLOSE);
  *started = false;
  for item in [
    Value::Number(revision),
    optional_string(author),
    optional_string(date),
    optional_string(message),
    Value::Word(super::bool_word(flags.has_children)),
    Value::Word(super::bool_word(flags.invalid)),
    Value::Number(u64::try_from(rest.len()).unwrap_or(u64::MAX)),
    super::proplist(rest),
    Value::Word(super::bool_word(flags.subtractive)),
  ] {
    write(&item, out)?;
  }
  out.extend_from_slice(LIST_CLOSE);
  Ok(())
}

impl<R: Repository + ?Sized> Reads<'_, R> {
  /// `get_path_histories`: each path's history opened at the range's youngest revision and taken to its first location,
  /// the path refused when absent there, or with `lenient` (`ignore_missing_locations`, a nested walk's) passed over
  /// when it is absent, below a file or past the youngest; the youngest revision itself must be one. A path the session
  /// may not read is refused before its history is opened, "Item is not readable", lenient or not.
  fn path_histories(
    &self,
    paths: &[String],
    start: Revision,
    end: Revision,
    strict: bool,
    lenient: bool,
  ) -> Result<Vec<PathInfo>, Failure> {
    self.existing(i64::try_from(end).map_err(|_| Failure::corrupt())?)?;
    let mut histories = Vec::with_capacity(paths.len());
    for (position, path) in paths.iter().enumerate() {
      if !self.readable(path) {
        return Err(Failure::unreadable_item());
      }
      match self.path_history(path, position, start, end, strict) {
        Ok(info) => histories.push(info),
        Err(failure) if lenient && missing(&failure) => {}
        Err(failure) => return Err(failure),
      }
    }
    Ok(histories)
  }

  /// One path's history for `get_path_histories`: opened and taken to its first location.
  fn path_history(
    &self,
    path: &str,
    position: usize,
    start: Revision,
    end: Revision,
    strict: bool,
  ) -> Result<PathInfo, Failure> {
    let kept = position < OPEN_HISTORIES;
    let mut info = PathInfo {
      path: path.to_owned(),
      history_rev: end,
      done: false,
      first_time: true,
      history: None,
      kept,
    };
    if kept {
      info.history = Some(self.node_history(path, end)?);
    }
    self.next_history(&mut info, strict, start)?;
    Ok(info)
  }

  /// `check_history`: whether the path's history holds `current`, and if so its next location taken.
  fn check_history(
    &self,
    info: &mut PathInfo,
    current: Revision,
    strict: bool,
    start: Revision,
  ) -> Result<bool, Failure> {
    if info.done || info.history_rev < current {
      return Ok(false);
    }
    self.next_history(info, strict, start)?;
    Ok(true)
  }

  /// `get_history`: the path's next history location, from the history kept open or from one opened again at the last
  /// location (one step past it, two after the first time); done when the history ends, passes the range's oldest
  /// revision, or reaches a location the session may not read.
  fn next_history(
    &self,
    info: &mut PathInfo,
    strict: bool,
    start: Revision,
  ) -> Result<(), Failure> {
    let next = if info.kept {
      match &info.history {
        Some(history) => self.history_prev(history, !strict)?,
        None => None,
      }
    } else {
      let opened = self.node_history(&info.path, info.history_rev)?;
      let mut next = self.history_prev(&opened, !strict)?;
      if info.first_time {
        info.first_time = false;
      } else if let Some(location) = &next {
        next = self.history_prev(location, !strict)?;
      }
      next
    };
    let Some(location) = next else {
      info.done = true;
      info.history = None;
      return Ok(());
    };
    info.path.clone_from(&location.path);
    info.history_rev = location.revision;
    if location.revision < start || !self.readable(&info.path) {
      info.done = true;
    }
    if info.kept {
      info.history = Some(location);
    }
    Ok(())
  }
}

/// The flags an entry carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct EntryFlags {
  /// `has-children`.
  has_children: bool,
  /// `invalid-revnum`: the entry closing a revision's children.
  invalid: bool,
  /// `subtractive-merge`.
  subtractive: bool,
}

impl Sending {
  /// A top-level revision's entry.
  fn top(has_children: bool) -> Self {
    Self {
      has_children,
      subtractive: false,
      handling: false,
    }
  }
}

impl Walk {
  /// What its revisions' entries are told.
  fn sending(&self, has_children: bool) -> Sending {
    Sending {
      has_children,
      subtractive: self.subtractive,
      handling: self.handling,
    }
  }
}

/// `handle_merged_revisions`' runs of a revision's merged revisions: the added mergeinfo's, then the deleted's (reverse
/// merges), sorted by their first and last revisions, walked from the last.
fn children_of(changes: &MergeinfoChanges) -> Frame {
  let mut runs = Vec::new();
  if !changes.added.is_empty() {
    combine(&changes.added, false, &mut runs);
  }
  if !changes.deleted.is_empty() {
    combine(&changes.deleted, true, &mut runs);
  }
  runs.sort_by_key(|run| (run.start, run.end));
  Frame::Children { runs }
}

/// Whether a lenient walk passes over a refusal opening a path's history (`ignore_missing_locations`).
fn missing(failure: &Failure) -> bool {
  matches!(
    failure.code,
    super::SVN_ERR_FS_NOT_FOUND
      | super::SVN_ERR_FS_NOT_DIRECTORY
      | super::SVN_ERR_FS_NO_SUCH_REVISION
  )
}

/// A string in a list of its own, or an empty list (`write_tuple_string_opt_list`).
fn optional_string(value: Option<&[u8]>) -> Value<'_> {
  Value::List(value.map(Value::String).into_iter().collect())
}

/// `next_history_rev`: the youngest revision a history not yet done is at; `None` when every one is done.
fn next_history_rev(histories: &[PathInfo]) -> Option<Revision> {
  histories
    .iter()
    .filter(|info| !info.done)
    .map(|info| info.history_rev)
    .max()
}

/// Why a step of the log stopped: a refusal the answer carries after `done`, or an item that could not be written.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Stop {
  /// A refusal: the log ends with `done` and it.
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
