//! The revisions the server makes of the versions works submit to a Subversion repository's green (A-664's Subversion
//! objects; §4.17.1 "A push is a work" items 3 and 4; §4.17.4 item 1: version N of the repository's one green is its
//! revision N).
//!
//! **Why.** A work submitted to a Subversion repository's green moves the green a version, as any submit does, and
//! nothing else made the revision of it: the green passed its records' youngest revision, so every later commit, whose
//! step lands only while the green is at the youngest revision, was refused, and no client saw the version.
//!
//! **What.** As the version commits on the owner ([`make`], from `verbs::judge_submission`, after git's made commit),
//! the revision after the records' youngest, made of the version's change: each path the change index names between the
//! two versions (`Green::changed_between`), read before from the records at the youngest revision and after from the
//! green's head, becomes one changed-path row as FSFS records it: `add` where nothing stood, `delete` where nothing
//! stands now, `replace` where the node's kind changed, `modify` where its text or its property list did; a file's text
//! with its digests (`slates_svn::commit::drive::text_of`), its bytes brought with the revision; a node's property list
//! the identity its reserved attribute names (`svn_commit::PROPS`), which the records must hold. A directory a row adds
//! beneath that did not stand before is added too, and a deletion beneath a deleted directory is the directory's. Its
//! revision properties: `svn:log`, the submit's message, `svn:date`, the owner's instant, and `svn:author`, the
//! principal's own name its session carries (`crate::identities`), as the svn+ssh edge names a commit's author.
//!
//! **Recorded as an import's is** (`svn_commit::land_made`): checked by the records, its record appended in the submit's
//! own transaction in an entry of the register holding it alone, after the version's own record, so the version and its
//! revision are one durable atom, and a recovery replays it with every other record, in log order.
//!
//! **Not made, and counted** ([`UNMADE`]), the version standing: a version not the one after the youngest revision, a
//! copy in the change (A-586: the index names a copy at its destination alone), a special file or a symbolic link
//! with properties of its own (no Subversion node the step makes yet), a property list the records do not hold, the
//! records' or the budget's refusal.
//!
//! **Symbolic links** become files as Subversion keeps them: the text `link ` and the target, the property list
//! `svn:special` set to `*` alone, brought with the revision.
//!
//! **Copies** (A-626 part 4's `WorkOp::Copy`): the submit hands the version's copies over (`copies_of`); a copy's
//! destination is a row with its copy source, `add`, or `replace` where something stood there or it replaces, its text
//! and property list set where the head differs from the source's; a path beneath it is judged against the source's
//! node at the copy-from revision, and a directory above a node added beneath it stood where it stood in the source.
//!
//! **Owed** (GAPS, A-664): a symbolic link with properties of its own (refused, counted).

use std::borrow::Cow;
use std::collections::BTreeMap;

use slates_db::catalog::{RepositoryId, VcsTool, VolumeId as DbVolumeId};
use slates_ipc::protocol::Authorship;
use slates_merge::engine::Green;
use slates_svn::commit::revision::Revision;
use slates_svn::index::{
  Action, ChangedPath, ContentIdentity, CopySource, Index, NodeKind, PropertyList, RevisionRecord,
  Text,
};
use slates_svn::lock::AprTime;
use slates_svn::migration::Repository as SvnRecords;
use slates_vfs::clock::Clock;

use crate::repository::svn_commit::{PROPS, Uncommitted, land_made};
use crate::state::ShardState;

/// Counter: a revision the server made of a submitted version. Format: a counter name in the daemon's status report.
pub(crate) const MADE: &str = "vcs.svn.revision_made";

/// Counter: a submitted version of which no revision was made (module docs). Format: a counter name in the daemon's
/// status report.
pub(crate) const UNMADE: &str = "vcs.svn.revision_unmade";

/// Format: the property `svn:log`.
const LOG: &[u8] = b"svn:log";
/// Format: the property `svn:author`.
const AUTHOR: &[u8] = b"svn:author";
/// Format: the property `svn:date`.
const DATE: &[u8] = b"svn:date";
/// Format: the property `svn:mergeinfo`, which a property list's row says it holds.
const MERGEINFO: &[u8] = b"svn:mergeinfo";
/// Format: nanoseconds in a microsecond, `AprTime`'s unit.
const NANOS_PER_MICRO: i64 = 1_000;

/// Why no revision was made of a version (module docs).
enum Unmade {
  /// The version is not the one after the records' youngest revision, or not the green's head.
  NotNext,
  /// The change holds a copy.
  Copy,
  /// A path holds a special file, or a symbolic link with properties of its own.
  Special,
  /// A property list the records do not hold, or an index that refused a path.
  Unreadable,
  /// A text's digests could not be made.
  Digest,
  /// The step refused it.
  Step(Uncommitted),
}

/// Written out rather than derived, as a derived `Debug` is all that reads the step's refusal and dead-code analysis
/// ignores derived impls; the output is the derived one's.
impl std::fmt::Debug for Unmade {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Unmade::NotNext => formatter.write_str("NotNext"),
      Unmade::Copy => formatter.write_str("Copy"),
      Unmade::Special => formatter.write_str("Special"),
      Unmade::Unreadable => formatter.write_str("Unreadable"),
      Unmade::Digest => formatter.write_str("Digest"),
      Unmade::Step(refused) => formatter.debug_tuple("Step").field(refused).finish(),
    }
  }
}

/// Makes the revision of `green`'s `version`, where `green` is a Subversion repository's (module docs); counted.
pub(crate) fn make(
  state: &mut ShardState,
  green: DbVolumeId,
  version: u64,
  authorship: &Authorship,
  copies: &[MadeCopy],
) {
  match made(state, green, version, (authorship, copies)) {
    Ok(true) => state.count(MADE, 1),
    Ok(false) => {}
    Err(why) => {
      slates_machine::log_line!(
        "slates-server: partition {}: no revision was made of a submitted version: {why:?}",
        state.partition
      );
      state.count(UNMADE, 1);
    }
  }
}

/// [`make`]'s steps: whether a revision was made.
fn made(
  state: &mut ShardState,
  green: DbVolumeId,
  version: u64,
  (authorship, copies): (&Authorship, &[MadeCopy]),
) -> Result<bool, Unmade> {
  let Some(repository) = subversion_repository_of(state, green) else {
    return Ok(false);
  };
  let youngest = state
    .vcs_stores
    .subversion_records(repository)
    .map_or(u64::MAX, SvnRecords::youngest);
  // A revision a client's commit landed moved the green itself: made already.
  if youngest >= version {
    return Ok(false);
  }
  if youngest.checked_add(1) != Some(version) {
    return Err(Unmade::NotNext);
  }
  let now = AprTime(state.clock.wall_ns().div_euclid(NANOS_PER_MICRO));
  let revision = revision_of(
    state,
    (repository, green),
    (youngest, version),
    (authorship, now, copies),
  )?;
  land_made(state, repository, revision).map_err(Unmade::Step)?;
  Ok(true)
}

/// The Subversion repository `green` is the green of: a member of a repository whose tool is Subversion's.
fn subversion_repository_of(state: &ShardState, green: DbVolumeId) -> Option<RepositoryId> {
  let repositories = state.db.partition().repositories();
  let repository = repositories.member(green)?.repository;
  let record = repositories.repository(repository)?;
  matches!(record.tool, VcsTool::Subversion).then_some(repository)
}

/// The revision of `green`'s `version`, made after the records' `youngest` (module docs).
fn revision_of(
  state: &ShardState,
  (repository, green): (RepositoryId, DbVolumeId),
  (youngest, version): (u64, u64),
  (authorship, now, copies): (&Authorship, AprTime, &[MadeCopy]),
) -> Result<Revision, Unmade> {
  let engine = state.greens.get(&green).ok_or(Unmade::NotNext)?;
  let records = state
    .vcs_stores
    .subversion_records(repository)
    .ok_or(Unmade::NotNext)?;
  if engine.head() != version {
    return Err(Unmade::NotNext);
  }
  let mut rows = BTreeMap::new();
  let mut contents = BTreeMap::new();
  let index = records.index();
  for path in engine.changed_between(youngest, version) {
    let at = svn_path(&path);
    if let Some(copy) = copies.iter().find(|copy| copy.to == at) {
      let after = after(&engine, records, &path)?;
      let row = copy_row(index, youngest, copy, after, &mut contents)?;
      rows.insert(at, row);
      continue;
    }
    let beneath = copy_beneath(copies, &at);
    if beneath.is_none() && engine.copied_after(&path, youngest) {
      return Err(Unmade::Copy);
    }
    let before = match &beneath {
      Some((source, revision)) => before(index, source, *revision)?,
      None => before(index, &at, youngest)?,
    };
    let after = after(&engine, records, &path)?;
    if engine.symlink(&path).is_some() {
      // A symbolic link's `svn:special` list, brought with the revision as any list a row names.
      contents.insert(special().1, SPECIAL_LIST.to_vec());
    }
    if let Some(row) = row_of(&at, before, after, &mut contents)? {
      rows.insert(at, row);
    }
  }
  add_parents(index, (youngest, copies), &mut rows)?;
  keep_outermost_deletions(&mut rows);
  let date = slates_svn::dav::response::svn_time(now).map_err(|_| Unmade::Unreadable)?;
  let mut properties = BTreeMap::from([(DATE.to_vec(), date.into_bytes())]);
  if let Some(message) = &authorship.message {
    properties.insert(LOG.to_vec(), message.as_bytes().to_vec());
  }
  if let Some(author) = &authorship.identities.subversion {
    properties.insert(AUTHOR.to_vec(), author.as_bytes().to_vec());
  }
  Ok(Revision {
    head: youngest,
    record: RevisionRecord {
      transaction: format!("{youngest}-{version}"),
      changes: rows.into_values().collect(),
    },
    properties,
    contents,
    lock_entries: Vec::new(),
  })
}

/// A path of the green as Subversion's canonical path: rooted, without a trailing separator.
fn svn_path(path: &str) -> String {
  format!("/{}", path.trim_matches('/'))
}

/// What stood at `path` in the records' revision `youngest`: its kind, text and property list.
type Before = Option<(NodeKind, Option<Text>, PropertyList)>;

/// What stands at `path` in the green's head: its kind, a file's bytes, and its property list.
type After<'g> = Option<(NodeKind, Option<Cow<'g, [u8]>>, PropertyList)>;

/// [`Before`] at `path`.
fn before(index: &Index, path: &str, youngest: u64) -> Result<Before, Unmade> {
  let node = index.node(path, youngest).map_err(|_| Unmade::Unreadable)?;
  Ok(node.map(|node| (node.kind(), node.text(), node.properties())))
}

/// [`After`] at `path`: a directory, a file (a hard link read as its primary), or a symbolic link as Subversion keeps
/// one, a file of `link ` and its target whose property list is `svn:special` alone ([`special`]); a special file, or a
/// symbolic link with properties of its own, refused.
fn after<'g>(engine: &'g Green, records: &SvnRecords, path: &str) -> Result<After<'g>, Unmade> {
  if engine.special(path).is_some() {
    return Err(Unmade::Special);
  }
  if let Some(target) = engine.symlink(path) {
    if engine.xattr(path, PROPS).is_some() {
      return Err(Unmade::Special);
    }
    let text = [LINK, target.as_bytes()].concat();
    return Ok(Some((NodeKind::File, Some(Cow::Owned(text)), special().0)));
  }
  let properties = list_of(records, engine.xattr(path, PROPS))?;
  if engine.is_dir(path) {
    return Ok(Some((NodeKind::Directory, None, properties)));
  }
  let primary = engine.hardlink(path).unwrap_or(path);
  Ok(
    engine
      .content(primary)
      .map(|bytes| (NodeKind::File, Some(Cow::Borrowed(bytes)), properties)),
  )
}

/// Format: the text Subversion keeps a symbolic link as, before its target (`subr/subst.c` `create_special_file_from_stream`
/// reads `link `; `libsvn_wc` writes it for a link it commits).
const LINK: &[u8] = b"link ";

/// Format: the property list a symbolic link carries alone, `svn:special` set to `*`, in FSFS's form (`hash.c`
/// `svn_hash_write2`: each name and value as `K`/`V` and its length, then `END`).
const SPECIAL_LIST: &[u8] = b"K 11\nsvn:special\nV 1\n*\nEND\n";

/// A symbolic link's property list ([`SPECIAL_LIST`]) as a row names it, and its identity, by which the revision
/// brings its bytes.
fn special() -> (PropertyList, ContentIdentity) {
  let identity = *blake3::hash(SPECIAL_LIST).as_bytes();
  let list = PropertyList::Held {
    identity,
    has_mergeinfo: false,
  };
  (list, identity)
}

/// The property list the reserved attribute names, which the records must hold; none without one.
fn list_of(records: &SvnRecords, named: Option<&[u8]>) -> Result<PropertyList, Unmade> {
  let Some(named) = named else {
    return Ok(PropertyList::Empty);
  };
  let identity = ContentIdentity::try_from(named).map_err(|_| Unmade::Unreadable)?;
  let unmarked = PropertyList::Held {
    identity,
    has_mergeinfo: false,
  };
  let properties = records
    .properties(unmarked)
    .map_err(|_| Unmade::Unreadable)?;
  Ok(PropertyList::Held {
    identity,
    has_mergeinfo: properties.contains_key(MERGEINFO),
  })
}

/// The row for `path` from what stood there and what stands now; none where nothing changed. A text a row sets is
/// brought in `contents`.
fn row_of(
  path: &str,
  before: Before,
  after: After<'_>,
  contents: &mut BTreeMap<ContentIdentity, Vec<u8>>,
) -> Result<Option<ChangedPath>, Unmade> {
  let row = |action, kind| ChangedPath {
    path: path.to_owned(),
    action,
    kind,
    copy_from: None,
    text: None,
    properties: None,
    mergeinfo_modified: false,
  };
  match (before, after) {
    (None, None) => Ok(None),
    (Some((kind, _, _)), None) => Ok(Some(row(Action::Delete, kind))),
    (None, Some((kind, bytes, properties))) => {
      let made = row(Action::Add, kind);
      new_node(made, bytes.as_deref(), properties, contents).map(Some)
    }
    (Some((was, _, _)), Some((kind, bytes, properties))) if was != kind => {
      let made = row(Action::Replace, kind);
      new_node(made, bytes.as_deref(), properties, contents).map(Some)
    }
    (Some((_, text, listed)), Some((kind, bytes, properties))) => {
      let mut changed = row(Action::Modify, kind);
      if let Some(bytes) = bytes {
        let made = text_of(&bytes, contents)?;
        if text.map(|text| text.identity) != Some(made.identity) {
          changed.text = Some(made);
        }
      }
      if listed != properties {
        changed.mergeinfo_modified = listed.has_mergeinfo() || properties.has_mergeinfo();
        changed.properties = Some(properties);
      }
      Ok((changed.text.is_some() || changed.properties.is_some()).then_some(changed))
    }
  }
}

/// A node `made` adds or replaces: its text, where it is a file, and its property list, where it has one.
fn new_node(
  mut made: ChangedPath,
  bytes: Option<&[u8]>,
  properties: PropertyList,
  contents: &mut BTreeMap<ContentIdentity, Vec<u8>>,
) -> Result<ChangedPath, Unmade> {
  made.text = bytes.map(|bytes| text_of(bytes, contents)).transpose()?;
  if properties != PropertyList::Empty {
    made.mergeinfo_modified = properties.has_mergeinfo();
    made.properties = Some(properties);
  }
  Ok(made)
}

/// `bytes` as a row's text, brought in `contents`.
fn text_of(
  bytes: &[u8],
  contents: &mut BTreeMap<ContentIdentity, Vec<u8>>,
) -> Result<Text, Unmade> {
  let text = slates_svn::commit::drive::text_of(bytes).map_err(|_| Unmade::Digest)?;
  contents.insert(text.identity, bytes.to_vec());
  Ok(text)
}

/// Each directory above a node a row adds or replaces that did not stand in `youngest` and no row names, added.
fn add_parents(
  index: &Index,
  (youngest, copies): (u64, &[MadeCopy]),
  rows: &mut BTreeMap<String, ChangedPath>,
) -> Result<(), Unmade> {
  let made: Vec<String> = rows
    .values()
    .filter(|row| matches!(row.action, Action::Add | Action::Replace))
    .map(|row| row.path.clone())
    .collect();
  for path in made {
    let mut parent = path.as_str();
    while let Some((above, _)) = parent.rsplit_once('/') {
      if above.is_empty() || rows.contains_key(above) {
        break;
      }
      let stood = match copy_beneath(copies, above) {
        Some((source, revision)) => before(index, &source, revision)?,
        None => before(index, above, youngest)?,
      };
      if stood.is_none() {
        let added = ChangedPath {
          path: above.to_owned(),
          action: Action::Add,
          kind: NodeKind::Directory,
          copy_from: None,
          text: None,
          properties: None,
          mergeinfo_modified: false,
        };
        rows.insert(above.to_owned(), added);
      }
      parent = above;
    }
  }
  Ok(())
}

/// A copy a submitted version made (A-626 part 4's `WorkOp::Copy`), as its revision's row names it: its destination and
/// its source, Subversion's canonical paths, the source's version (version N of the green is revision N, so it is the
/// copy-from revision), and whether it replaces what its destination held.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MadeCopy {
  /// The destination.
  to: String,
  /// The source.
  from: String,
  /// The source's version: the copy-from revision.
  version: u64,
  /// Whether it replaces what the destination held.
  replace: bool,
}

/// The copies `inc` declares ([`MadeCopy`]), in its document's order.
pub(crate) fn copies_of(inc: &slates_merge::engine::Increment) -> Vec<MadeCopy> {
  inc
    .doc
    .ops
    .iter()
    .filter(|op| op.kind == slates_merge::ops_doc::OpKind::Copy)
    .filter_map(|op| {
      Some(MadeCopy {
        to: svn_path(inc.doc.paths.path(op.path)?),
        from: svn_path(inc.doc.paths.path(op.src)?),
        version: op.at,
        replace: op.replaces(),
      })
    })
    .collect()
}

/// The copy-from row of `copy` (§4.17.4 item 3.5): `add`, or `replace` where its destination stood or it replaces,
/// the source's kind at its revision and the copy source; its text and property list where the head differs from the
/// source's.
fn copy_row(
  index: &Index,
  youngest: u64,
  copy: &MadeCopy,
  after: After<'_>,
  contents: &mut BTreeMap<ContentIdentity, Vec<u8>>,
) -> Result<ChangedPath, Unmade> {
  let (kind, text, listed) = before(index, &copy.from, copy.version)?.ok_or(Unmade::Unreadable)?;
  let stood = before(index, &copy.to, youngest)?.is_some();
  let mut row = ChangedPath {
    path: copy.to.clone(),
    action: if copy.replace || stood {
      Action::Replace
    } else {
      Action::Add
    },
    kind,
    copy_from: Some(CopySource {
      path: copy.from.clone(),
      revision: copy.version,
    }),
    text: None,
    properties: None,
    mergeinfo_modified: false,
  };
  if let Some((_, bytes, properties)) = after {
    if let Some(bytes) = bytes {
      let made = text_of(&bytes, contents)?;
      if text.map(|text| text.identity) != Some(made.identity) {
        row.text = Some(made);
      }
    }
    if listed != properties {
      row.mergeinfo_modified = listed.has_mergeinfo() || properties.has_mergeinfo();
      row.properties = Some(properties);
    }
  }
  Ok(row)
}

/// Where a path beneath one of `copies`' destinations stood in its source, at the copy-from revision: what a row there
/// is judged against, since the copy brought it.
fn copy_beneath(copies: &[MadeCopy], path: &str) -> Option<(String, u64)> {
  copies.iter().find_map(|copy| {
    let rest = path.strip_prefix(copy.to.as_str())?;
    let rel = rest.strip_prefix('/').filter(|rel| !rel.is_empty())?;
    Some((
      format!("{}/{rel}", copy.from.trim_end_matches('/')),
      copy.version,
    ))
  })
}

/// Drops each deletion beneath a directory another row deletes: FSFS records the directory's alone.
fn keep_outermost_deletions(rows: &mut BTreeMap<String, ChangedPath>) {
  let deleted: Vec<String> = rows
    .values()
    .filter(|row| row.action == Action::Delete)
    .map(|row| format!("{}/", row.path))
    .collect();
  rows.retain(|path, row| {
    row.action != Action::Delete || !deleted.iter().any(|above| path.starts_with(above.as_str()))
  });
}
