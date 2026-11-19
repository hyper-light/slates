//! `log-report` (A-681, part 1's second slice; §4.17.4 item 5 "HTTP" and piece 13): the log's walk above, A-613's
//! `svn_repos_get_logs5`, with `mod_dav_svn`'s receivers in place of `svnserve`'s, as `reports/log.c` writes them
//! (Subversion 1.15.0) and `dav_method_report` delivers them (httpd 2.4.66 `mod_dav.c`):
//! - The request, as `dav_svn__log_report` reads it ([`DavLog::new`]): each `S:path` joined to the path of the
//!   resource the REPORT names (none: the root's shortcut over every revision, as for no path); a start or an end
//!   missing or negative is the youngest; `S:limit` 0 is none; no revision-property element asks a client before 1.5's
//!   author, date and log, `S:no-revprops` none, `S:all-revprops` every one, `S:revprop` those named;
//!   `S:discover-changed-paths`, `S:strict-node-history`, `S:include-merged-revisions` and `S:encode-binary-props` by
//!   their presence. Each path the walk meets is judged by the principal's read rules, as `dav_svn__authz_read_func`
//!   judges it for `svn_repos_get_logs5`: the same censoring of changed paths, copy sources and revision properties.
//! - The answer: the XML declaration and `S:log-report` written before the first changed path or entry
//!   (`maybe_send_header`), so a walk refused before either writes nothing; each entry's `S:log-item` opened before its
//!   first changed path (`maybe_start_log_item`); each changed path as `log_change_receiver` writes it; then
//!   `log_revision_receiver`'s `D:version-name` (`-1` for the entry closing a run of merged revisions), the revision
//!   properties, `S:has-children` and `S:subtractive-merge`, and the item's end; at the end `</S:log-report>`.
//! - A refusal ([`DavLogProgress::Refused`]): while nothing is written, the caller answers it as `mod_dav` answers a
//!   report's error before its first byte (`dav_svn__convert_err`'s status, the error in `D:error`); once the header is
//!   written, `dav_method_report` can only abort the connection ([`DavLog::begun`]).
//!
//! Departures, each stated: the revision properties are written in the order the green holds them, where
//! `mod_dav_svn` walks an APR hash, whose order its seed varies; a negative `S:limit`, which `svn_repos_get_logs5`
//! reads as no limit in its shortcut and as one entry in its walk (`limit && ++send_count >= limit`), is refused as
//! malformed ([`DavLog::new`] gives none); `mod_dav_svn`'s forced flushes after the 4th, 16th, 64th and 256th item are
//! the caller's slices.

use super::super::{Authentication, Failure, ROOT, Reads, Repository, full_path, kind_word};
use super::{EntryFlags, LogRequest, LogStream, Revprops, Writer, compat_revprops};
use crate::authz::PrincipalRules;
use crate::dav::request::{LogReport, Revprops as AskedRevprops};
use crate::dav::response::{
  self, Change, ChangedPath as WrittenChange, LOG_ITEM_START, LOG_REPORT_END, LOG_REPORT_START,
};
use crate::index::{Action, ChangedPath, Index, Revision};
use crate::reads::Progress;

/// `reports/log.c`'s receiver baton (`struct log_receiver_baton`), less its flush counters, and the refusal that ended
/// the walk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::reads) struct DavWriter {
  /// Whether the header is still to be written (`needs_header`).
  needs_header: bool,
  /// Whether the next item's `S:log-item` is still to be opened (`needs_log_item`).
  needs_log_item: bool,
  /// Whether a revision property XML cannot carry goes in base64 (`encode_binary_props`).
  encode_binary_props: bool,
  /// The refusal that ended the walk, for the caller to answer.
  refused: Option<Failure>,
}

impl DavWriter {
  /// A writer with nothing written.
  fn new(encode_binary_props: bool) -> Self {
    Self {
      needs_header: true,
      needs_log_item: true,
      encode_binary_props,
      refused: None,
    }
  }

  /// `maybe_send_header`.
  fn header(&mut self, out: &mut Vec<u8>) {
    if self.needs_header {
      for part in LOG_REPORT_START {
        out.extend_from_slice(part.as_bytes());
      }
      self.needs_header = false;
    }
  }

  /// `maybe_send_header`, then `maybe_start_log_item`.
  fn start_item(&mut self, out: &mut Vec<u8>) {
    self.header(out);
    if self.needs_log_item {
      out.extend_from_slice(LOG_ITEM_START.as_bytes());
      self.needs_log_item = false;
    }
  }

  /// `log_change_receiver`: one changed path, its copy source written when the principal may read it.
  pub(super) fn changed_path(
    &mut self,
    out: &mut Vec<u8>,
    change: &ChangedPath,
    source_readable: bool,
  ) {
    self.start_item(out);
    let written = WrittenChange {
      path: change.path.clone(),
      change: match change.action {
        Action::Add => Change::Added,
        Action::Replace => Change::Replaced,
        Action::Delete => Change::Deleted,
        Action::Modify => Change::Modified,
      },
      kind: kind_word(change.kind),
      text_mods: change.text.is_some(),
      prop_mods: change.properties.is_some(),
      copy_from: change
        .copy_from
        .as_ref()
        .filter(|_| source_readable)
        .map(|source| (source.path.clone(), source.revision)),
    };
    out.extend_from_slice(response::changed_path(&written).as_bytes());
  }

  /// `log_revision_receiver`: the item opened when no changed path opened it, then the rest of it; the next item's
  /// start owed again.
  pub(super) fn entry(
    &mut self,
    out: &mut Vec<u8>,
    revision: Revision,
    flags: EntryFlags,
    properties: &[(&[u8], &[u8])],
  ) {
    self.start_item(out);
    self.needs_log_item = true;
    let names: Vec<String> = properties
      .iter()
      .map(|(name, _)| String::from_utf8_lossy(name).into_owned())
      .collect();
    let revprops: Vec<(&str, &[u8])> = names
      .iter()
      .zip(properties)
      .map(|(name, (_, value))| (name.as_str(), *value))
      .collect();
    let revision = (!flags.invalid).then_some(revision);
    let written = response::log_item_end(
      revision,
      &revprops,
      (flags.has_children, flags.subtractive),
      self.encode_binary_props,
    );
    out.extend_from_slice(written.as_bytes());
  }

  /// The walk's end: `</S:log-report>` after the header when the walk was whole; its refusal kept for the caller
  /// otherwise.
  pub(super) fn finish(&mut self, out: &mut Vec<u8>, failure: Option<&Failure>) {
    match failure {
      None => {
        self.header(out);
        out.extend_from_slice(LOG_REPORT_END.as_bytes());
      }
      Some(failure) => self.refused = Some(failure.clone()),
    }
  }
}

/// A `log-report`'s answer still to write (the module doc).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DavLog {
  /// The walk, its entries going to `mod_dav_svn`'s writer.
  stream: LogStream,
}

/// How far a `log-report`'s answer is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DavLogProgress {
  /// More follows: resume it again.
  More,
  /// The answer is whole.
  Done,
  /// The walk was refused with Subversion's error number and message; nothing more is written.
  Refused {
    /// The error number.
    code: u32,
    /// The message.
    message: String,
  },
}

impl DavLog {
  /// The walk `report` asks for, its paths beneath `target`, the repository path of the resource the REPORT names (the
  /// module doc); none for a negative `S:limit`.
  pub fn new(report: &LogReport, target: &str) -> Option<Self> {
    let limit = u64::try_from(report.limit).ok()?;
    let signed = |revision: Revision| i64::try_from(revision).unwrap_or(i64::MAX);
    let revprops = match &report.revprops {
      AskedRevprops::Default => Revprops::Named(compat_revprops()),
      AskedRevprops::All => Revprops::All,
      AskedRevprops::Named(names) => {
        Revprops::Named(names.iter().map(|name| name.as_bytes().to_vec()).collect())
      }
    };
    let request = LogRequest {
      paths: report
        .paths
        .iter()
        .map(|path| full_path(target, path.as_bytes()))
        .collect(),
      start: report.start.map(signed),
      end: report.end.map(signed),
      changed_paths: report.discover_changed_paths,
      strict: report.strict_node_history,
      limit,
      merged: report.include_merged_revisions,
      revprops,
    };
    let writer = Writer::Dav(DavWriter::new(report.encode_binary_props));
    Some(Self {
      stream: LogStream::new(request, writer),
    })
  }

  /// Writes the answer's next part into `out`, read from `index` and `repository` with each path judged by `rules`
  /// (none: every path readable), until what this slice wrote and the steps it took reach `room`, at least one step,
  /// or the walk ends.
  pub fn resume<R: Repository + ?Sized>(
    &mut self,
    index: &Index,
    repository: &R,
    rules: Option<&PrincipalRules>,
    out: &mut Vec<u8>,
    room: usize,
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
    match self.stream.resume(&mut reads, room) {
      Ok(Progress::More) => DavLogProgress::More,
      Ok(Progress::Done | Progress::Close) => match self.refused() {
        Some(failure) => DavLogProgress::Refused {
          code: failure.code,
          message: failure.message.clone(),
        },
        None => DavLogProgress::Done,
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

  /// Whether any of the answer is written: once the header is, a refusal can only end the connection.
  pub fn begun(&self) -> bool {
    match &self.stream.receiver.writer {
      Writer::Dav(writer) => !writer.needs_header,
      Writer::Items { .. } => true,
    }
  }

  /// The refusal that ended the walk, if one did.
  fn refused(&self) -> Option<&Failure> {
    match &self.stream.receiver.writer {
      Writer::Dav(writer) => writer.refused.as_ref(),
      Writer::Items { .. } => None,
    }
  }
}
