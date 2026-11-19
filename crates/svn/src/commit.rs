//! The Subversion commit rule (§4.17.4 items 3.2 to 3.4 and piece 5; A-592): whether a work, the transaction a
//! client's editor drive built on base revision B, commits on the head, or which refusal Subversion answers it with.
//!
//! A commit is a work on the repository's one green (§4.17.1), and a stock client must end where `svnserve` would have
//! left it, so the rule is Subversion's own, read in its code (Subversion's `1.14.x` branch at `d434dc14`; research
//! notes §6, `docs/wip/research/vcs-servers-subversion.md`, and each file's hash in A-592's entry), in the order
//! Subversion applies it:
//! 1. **During the drive, out of date** ([`check_out_of_date`]; `libsvn_repos/commit.c` `check_out_of_date` 179-198).
//!    `open-file`, `delete-entry` and a directory's first property change carry the revision R the client holds the
//!    node at. R older than the node's created revision in the transaction is out of date (`E160028`); R past the
//!    transaction's base is no such revision (`E160006`). The drive asks it as each command arrives (piece 7).
//! 2. **At `close-edit`, FSFS's merge** ([`verdict`]; `libsvn_fs_fs/tree.c` `merge()` 1790-2146). FSFS commits a
//!    transaction by merging the head into it, `merge(target = the transaction, source = the head, ancestor = its
//!    base)` from the root (`merge_changes` 2186, from `svn_fs_fs__commit_txn` 2284). Stated on the index, it reads the
//!    rows of the head's revisions since B and the nodes they and the work reach, never a whole tree:
//!    - At a directory the work changed (the root always, since a transaction's root is a new node-revision), nothing
//!      is to merge when the head left it as B had it or holds the work's own node-revision (1840). Otherwise a
//!      property list the head changed is a conflict (rule 3, 1946); and a property list the work changed is a
//!      conflict when the head changed the directory's entries' names or their node or copy identities (rule 4,
//!      `compare_dir_structure` 1724-1765, called at 1960). Property lists compare by content, as
//!      `svn_fs_fs__prop_rep_equal` compares them (`fs_fs.c` 1608-1665): the index names a list by its content and
//!      an empty list is [`PropertyList::Empty`], whatever emptied it.
//!    - Then each of B's entries the head changed, in name order (1978): one the work left takes the head's (1994);
//!      one both changed is a conflict when either side deleted it (2046), when any of its three sides is a file
//!      (2052: rule 1, even for the same change, and a node kind conflict when the three kinds differ), or when either
//!      side is no direct modification of B's (2061: rule 2, another node or copy identity, "even an identical
//!      replacement"); otherwise the merge goes down into it.
//!    - Then the head's additions, in name order (2096): one the work added too is a conflict (2112).
//!
//!    The first conflict in that order is the one answered, at its path: `E160024` "Conflict at '<path>'", which the
//!    client reports as out of date. The work's identities are FSFS's, made on B by the index's own builder
//!    ([`Index::trial`]), so a directory a work first changes inside a branch takes the branch's copy identity there
//!    (`get_copy_inheritance`), and two works that change different files beneath one directory of a branch that no
//!    revision changed since the copy conflict at that directory, as they do in FSFS.
//! 3. **Then the locks** (`libsvn_fs_fs/transaction.c` `verify_locks` 3527-3593, which `commit_body` runs after the
//!    merge, 3797). The verdict hands the work's changes to the caller's lock check, the lock table's own
//!    ([`crate::lock::LockTable::verdict`], A-602, item 3.4), and its refusal is the verdict's; what the check answers
//!    for an accepted commit (the lock entries the merge task appends in the same step) is the verdict's answer.
//!
//! Every refusal is typed ([`CommitRefusal`]) and names the Subversion error a client is answered with
//! ([`CommitRefusal::subversion_code`]); a work whose rows its base cannot hold is refused with the index's refusal.
//!
//! The oracle (module tests) transcribes `merge()`, `compare_dir_structure`, `verify_locks`, `verify_lock` and
//! `svn_path_compare_paths` over the naive FSFS of the index's tests, whose transactions FSFS's own operations build on
//! whole trees; the verdict, its lock check the lock table's, must answer every generated history as they do: the same
//! commit, the same conflict at the same path found by the same check, or the same lock in the way.
//!
//! The drive's commands are read in [`edit`] (A-617): the commit's request and every editor command, as `svnserve`
//! reads them.
//!
//! Not here: the drive's other checks (a copy's source, checksums, property values, authorization) and its early-error
//! rule (piece 7); hooks (item 9); the lock table (A-602).
//!
//! No I/O, no clock, no randomness, no panic.

use std::collections::BTreeSet;
use std::ops::Bound;

use crate::index::{
  ChangedPath, CopyId, Index, IndexRefusal, Node, NodeId, NodeKind, NodeRevisionId, PropertyList,
  Revision, RevisionRecord, Trial, TrialNode,
};
use crate::lock::LockRefusal;

pub mod edit;

/// `SVN_ERR_FS_NO_SUCH_REVISION`, "No such revision %ld": a client's revision past the transaction's base.
/// Format: the filesystem category's start, 120,000 + 8 × 5,000 = 160,000, plus 6 (`svn_error_codes.h` 121-122,
/// 605-606).
const SVN_ERR_FS_NO_SUCH_REVISION: u32 = 160_006;

/// `SVN_ERR_FS_CONFLICT`, "Conflict at '%s'": every conflict FSFS's merge finds.
/// Format: the filesystem category's start plus 24 (`svn_error_codes.h` 677-678).
const SVN_ERR_FS_CONFLICT: u32 = 160_024;

/// `SVN_ERR_FS_TXN_OUT_OF_DATE`, "File '%s' is out of date", "Directory '%s' is out of date" or "'%s' is out of date"
/// (`commit.c` `out_of_date` 165-175).
/// Format: the filesystem category's start plus 28 (`svn_error_codes.h` 693-694).
const SVN_ERR_FS_TXN_OUT_OF_DATE: u32 = 160_028;

/// The root directory's path, where FSFS's merge begins.
/// Format: Subversion's canonical absolute path of the root (`svn_fspath__is_canonical`).
const ROOT: &str = "/";

/// The character between a path's names.
/// Format: Subversion's path separator (`svn_fspath__join`).
const SEPARATOR: char = '/';

/// Which of `merge()`'s checks found a conflict, in the order it makes them. Every one is answered `E160024`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ConflictKind {
  /// The head changed the property list of a directory both sides changed (rule 3; `tree.c` 1946).
  PropertiesChanged,
  /// The work changed the property list of a directory whose entries the head added, removed, or gave another node or
  /// copy identity (rule 4; `compare_dir_structure`, 1952-1960).
  StructureChanged,
  /// Both sides deleted the entry: a double delete (rule 2; 2046).
  DeletedTwice,
  /// One side deleted the entry and the other changed it (rule 2; 2046).
  DeletedAgainstChange,
  /// Both sides changed an entry whose three sides are not all of one kind, a file among them: a node kind conflict
  /// (2052).
  KindChanged,
  /// Both sides changed a file, even alike (rule 1; 2052).
  FileChanged,
  /// Both sides changed a directory one of them holds as no direct modification of the base's: replaced, copied, or
  /// reached through another copy, "even an identical replacement" (rule 2; 2061).
  NotADirectModification,
  /// Both sides added the entry: "two additions are not auto-mergeable" (rule 2; 2112).
  AddedTwice,
}

/// Why a work does not commit, each answered with a Subversion error ([`CommitRefusal::subversion_code`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommitRefusal {
  /// The client holds the node at `path` at a revision older than the node's created revision in the transaction, or
  /// named a path the transaction holds against its command (`SVN_ERR_FS_TXN_OUT_OF_DATE`). `kind` chooses the words:
  /// a file's, a directory's, or none for a path the transaction does not hold.
  OutOfDate {
    /// The path.
    path: String,
    /// The node's kind, when the transaction holds one there.
    kind: Option<NodeKind>,
  },
  /// The client named a revision past the transaction's base (`SVN_ERR_FS_NO_SUCH_REVISION`).
  RevisionPastBase {
    /// The revision the client named.
    revision: Revision,
  },
  /// FSFS's merge declares a conflict at `path`, found by `kind` (`SVN_ERR_FS_CONFLICT`).
  Conflict {
    /// The path, as FSFS reports it.
    path: String,
    /// The check that found it.
    kind: ConflictKind,
  },
  /// A lock stands in the way (§4.17.4 item 3.4): the lock check's refusal (`E160034`, `E160037`, `E160039`).
  Locked(LockRefusal),
  /// The work, or the base it names, is not one FSFS builds a transaction from: the index's refusal, by its kind.
  Index(IndexRefusal),
}

impl CommitRefusal {
  /// The number of the Subversion error a client is answered with.
  pub fn subversion_code(&self) -> u32 {
    match self {
      Self::OutOfDate { .. } => SVN_ERR_FS_TXN_OUT_OF_DATE,
      Self::RevisionPastBase { .. } => SVN_ERR_FS_NO_SUCH_REVISION,
      Self::Conflict { .. } => SVN_ERR_FS_CONFLICT,
      Self::Locked(refusal) => refusal.subversion_code(),
      Self::Index(refusal) => refusal.subversion_code(),
    }
  }
}

impl From<IndexRefusal> for CommitRefusal {
  fn from(refusal: IndexRefusal) -> Self {
    Self::Index(refusal)
  }
}

/// The drive's out-of-date test for one editor command naming the node at `path`, which the client holds at revision
/// `client` (§4.17.4 item 3.2; `commit.c` `check_out_of_date` 179-198): `open-file` (627-634), `delete-entry` (504-509)
/// and a directory's first property change (710-726) carry it, each only when the client names a revision.
///
/// `created` is the node's created revision in the transaction when the command arrives, as `svn_fs_node_created_rev`
/// reads the transaction's root: for a node the transaction has not changed, its own (the index's
/// [`Node::created_revision`] at the base, or at a copy's source beneath a copy the transaction made); for the root,
/// the base while the transaction has changed nothing (`dag.c` `svn_fs_fs__dag_get_revision`, issue #2608); and `None`
/// for a node the transaction made or changed, whose revision FSFS leaves invalid until it commits. `kind` chooses the
/// refusal's words: a file's for `open-file`, a directory's for a property change, the node's for `delete-entry`.
///
/// Out of date when `client` is older than `created`; no such revision when `client` is past the transaction's base
/// `base` and differs from `created`; otherwise the command goes on.
pub fn check_out_of_date(
  path: &str,
  kind: NodeKind,
  client: Revision,
  created: Option<Revision>,
  base: Revision,
) -> Result<(), CommitRefusal> {
  match created {
    Some(created) if client < created => Err(CommitRefusal::OutOfDate {
      path: path.to_owned(),
      kind: Some(kind),
    }),
    Some(created) if client == created => Ok(()),
    _ if client > base => Err(CommitRefusal::RevisionPastBase { revision: client }),
    _ => Ok(()),
  }
}

/// Whether `work`, a transaction built on revision `base`, commits on the index's head, or the refusal FSFS's commit
/// answers it with: the first conflict `merge()` finds; else what the lock check `locks` answers for the work's changes
/// (module documentation, items 2 and 3). The merge task passes the lock table's check
/// ([`crate::lock::LockTable::verdict`] over each change's path and action, for the commit's principal and presented
/// tokens, at the owner's instant), whose accepted answer is the lock entries it appends with the version.
pub fn verdict<Accepted>(
  index: &Index,
  base: Revision,
  work: &RevisionRecord,
  locks: impl FnOnce(&[ChangedPath]) -> Result<Accepted, LockRefusal>,
) -> Result<Accepted, CommitRefusal> {
  let trial = index.trial(base, work)?;
  let head = index.youngest();
  let mut changed = BTreeSet::new();
  for revision in base.saturating_add(1)..=head {
    changed.extend(index.changes(revision)?.iter().map(|row| row.path.as_str()));
  }
  Judge {
    index,
    trial: &trial,
    base,
    head,
    changed: &changed,
  }
  .root()?;
  locks(&work.changes).map_err(CommitRefusal::Locked)
}

/// A node's facts the merge compares, on any of its three sides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Facts {
  /// Its node-revision.
  id: NodeRevisionId,
  /// Its kind.
  kind: NodeKind,
  /// Its node identity.
  node_id: NodeId,
  /// Its copy identity.
  copy_id: CopyId,
  /// Its property list.
  properties: PropertyList,
}

impl From<Node<'_>> for Facts {
  fn from(node: Node<'_>) -> Self {
    Self {
      id: node.id(),
      kind: node.kind(),
      node_id: node.node_id(),
      copy_id: node.copy_id(),
      properties: node.properties(),
    }
  }
}

impl From<TrialNode> for Facts {
  fn from(node: TrialNode) -> Self {
    Self {
      id: node.id,
      kind: node.kind,
      node_id: node.node_id,
      copy_id: node.copy_id,
      properties: node.properties,
    }
  }
}

/// Whether `entry` stands on `ancestor`'s line and copy: a direct modification of it, or itself (`tree.c` 2061-2069).
fn direct(entry: Facts, ancestor: Facts) -> bool {
  entry.node_id == ancestor.node_id && entry.copy_id == ancestor.copy_id
}

/// One place on the merge's three sides: the base's node (`ancestor`), the head's (`source`), the work's (`target`).
#[derive(Clone, Copy, Debug)]
struct Sides {
  /// The base's node.
  ancestor: Facts,
  /// The head's node.
  source: Facts,
  /// The work's node.
  target: Facts,
}

impl Sides {
  /// The conflict `merge()` declares at an entry both sides changed and neither deleted, before it goes down into it:
  /// a file among the three sides (2052), or a side that is no direct modification of the base's (2061).
  fn entry_conflict(self) -> Option<ConflictKind> {
    let kinds = [self.ancestor.kind, self.source.kind, self.target.kind];
    if kinds.contains(&NodeKind::File) {
      return Some(if kinds.iter().all(|kind| *kind == NodeKind::File) {
        ConflictKind::FileChanged
      } else {
        ConflictKind::KindChanged
      });
    }
    if !direct(self.source, self.ancestor) || !direct(self.target, self.ancestor) {
      return Some(ConflictKind::NotADirectModification);
    }
    None
  }
}

/// `merge()`'s three sides over the index and what the head changed since the base.
struct Judge<'judged, 'index> {
  /// The index, which holds the base and the head.
  index: &'index Index,
  /// The work's transaction on the base.
  trial: &'judged Trial<'index>,
  /// The base revision.
  base: Revision,
  /// The head revision.
  head: Revision,
  /// Every path a row of the head's revisions since the base names.
  changed: &'judged BTreeSet<&'index str>,
}

impl Judge<'_, '_> {
  /// The base's node at `path`.
  fn ancestor(&self, path: &str) -> Result<Option<Facts>, CommitRefusal> {
    Ok(self.index.node(path, self.base)?.map(Facts::from))
  }

  /// The head's node at `path`.
  fn source(&self, path: &str) -> Result<Option<Facts>, CommitRefusal> {
    Ok(self.index.node(path, self.head)?.map(Facts::from))
  }

  /// The work's node at `path`.
  fn target(&self, path: &str) -> Result<Option<Facts>, CommitRefusal> {
    Ok(self.trial.node(path)?.map(Facts::from))
  }

  /// `merge()` from the root, as `merge_changes` calls it (2186). Its first check, that all three sides are
  /// directories (1908-1913), holds at the root of every index (no row may add, delete or replace the root) and is
  /// never reached beneath it (an entry's kinds are judged first, 2052), so a root of another kind is a corrupt index.
  fn root(&self) -> Result<(), CommitRefusal> {
    let (Some(ancestor), Some(source), Some(target)) =
      (self.ancestor(ROOT)?, self.source(ROOT)?, self.target(ROOT)?)
    else {
      return Err(IndexRefusal::Corrupt.into());
    };
    let kinds = [ancestor.kind, source.kind, target.kind];
    if kinds.iter().any(|kind| *kind != NodeKind::Directory) {
      return Err(IndexRefusal::Corrupt.into());
    }
    self.directory(
      ROOT,
      Sides {
        ancestor,
        source,
        target,
      },
    )
  }

  /// `merge()` at the directory `path`, held as a directory on all three sides: the first conflict it finds.
  fn directory(&self, path: &str, sides: Sides) -> Result<(), CommitRefusal> {
    // "Either no change made in source, or same change as made in target. Both mean nothing to merge here." (1837-1843)
    if sides.ancestor.id == sides.source.id || sides.source.id == sides.target.id {
      return Ok(());
    }
    let names = self.names_beneath(path);
    self.properties(path, sides, &names)?;
    for name in &names {
      self.entry(&joined(path, name))?;
    }
    self.additions(path, &names)
  }

  /// The property checks at a directory both sides changed (1916-1964): the head's change of its list (rule 3), then
  /// the work's against a change of its entries' structure (rule 4). `names` are its entries the head's rows reach.
  fn properties(&self, path: &str, sides: Sides, names: &[&str]) -> Result<(), CommitRefusal> {
    if sides.source.properties != sides.ancestor.properties {
      return Err(conflict(path, ConflictKind::PropertiesChanged));
    }
    if sides.target.properties != sides.ancestor.properties
      && self.structure_changed(path, names)?
    {
      return Err(conflict(path, ConflictKind::StructureChanged));
    }
    Ok(())
  }

  /// `merge()`'s judgement of the base's entry at `path` (1978-2094): the conflict it finds first.
  fn entry(&self, path: &str) -> Result<(), CommitRefusal> {
    // A name the base does not hold is the head's addition, judged after every entry of the base (2096).
    let Some(ancestor) = self.ancestor(path)? else {
      return Ok(());
    };
    // "No changes were made to this entry while the transaction was in progress" (1989).
    let source = self.source(path)?;
    if source.is_some_and(|source| source.id == ancestor.id) {
      return Ok(());
    }
    // "A change was made to this entry while the transaction was in process, but the transaction did not touch this
    // entry": the head's entry is taken (1994).
    let target = self.target(path)?;
    if target.is_some_and(|target| target.id == ancestor.id) {
      return Ok(());
    }
    // A double delete, or a delete against a modification (2046).
    let (Some(source), Some(target)) = (source, target) else {
      let kind = if source.is_none() && target.is_none() {
        ConflictKind::DeletedTwice
      } else {
        ConflictKind::DeletedAgainstChange
      };
      return Err(conflict(path, kind));
    };
    let sides = Sides {
      ancestor,
      source,
      target,
    };
    match sides.entry_conflict() {
      Some(kind) => Err(conflict(path, kind)),
      // "Direct modifications were made to the directory ANCESTOR-ENTRY in both SOURCE and TARGET. Recursively merge
      // these modifications." (2075)
      None => self.directory(path, sides),
    }
  }

  /// The head's additions in the directory at `path`, in name order (2096-2133): one the work added too is a conflict.
  fn additions(&self, path: &str, names: &[&str]) -> Result<(), CommitRefusal> {
    for name in names {
      let entry = joined(path, name);
      if self.ancestor(&entry)?.is_none()
        && self.source(&entry)?.is_some()
        && self.target(&entry)?.is_some()
      {
        return Err(conflict(&entry, ConflictKind::AddedTwice));
      }
    }
    Ok(())
  }

  /// Whether the head changed the structure of the directory at `path` (`compare_dir_structure` 1724-1765): another
  /// count of entries, another name, or another node or copy identity at a name. `names` are its entries the head's
  /// rows reach; every other entry is the base's own node-revision on both sides.
  fn structure_changed(&self, path: &str, names: &[&str]) -> Result<bool, CommitRefusal> {
    for name in names {
      let entry = joined(path, name);
      let same = match (self.ancestor(&entry)?, self.source(&entry)?) {
        (None, None) => true,
        (Some(ancestor), Some(source)) => direct(source, ancestor),
        (None, Some(_)) | (Some(_), None) => false,
      };
      if !same {
        return Ok(true);
      }
    }
    Ok(false)
  }

  /// The names of the entries of the directory at `path` that a row of the head's revisions since the base reaches,
  /// at them or beneath, in name order (FSFS's sorted entries compare names byte by byte): the only entries the head
  /// can have changed, since a row makes a new node-revision of every directory above it (bubble-up).
  fn names_beneath(&self, path: &str) -> Vec<&str> {
    let prefix = if path == ROOT {
      ROOT.to_owned()
    } else {
      format!("{path}{SEPARATOR}")
    };
    let names: BTreeSet<&str> = self
      .changed
      .range::<str, _>((Bound::Included(prefix.as_str()), Bound::Unbounded))
      .take_while(|changed| changed.starts_with(prefix.as_str()))
      .filter_map(|changed| changed.get(prefix.len()..)?.split(SEPARATOR).next())
      .filter(|name| !name.is_empty())
      .collect();
    names.into_iter().collect()
  }
}

/// `name` in the directory `path` (`svn_fspath__join`).
fn joined(path: &str, name: &str) -> String {
  if path == ROOT {
    format!("{SEPARATOR}{name}")
  } else {
    format!("{path}{SEPARATOR}{name}")
  }
}

/// A conflict at `path`, found by `kind`.
fn conflict(path: &str, kind: ConflictKind) -> CommitRefusal {
  CommitRefusal::Conflict {
    path: path.to_owned(),
    kind,
  }
}

#[cfg(test)]
mod tests;
