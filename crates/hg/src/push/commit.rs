//! A verified push's revisions kept in the repository's index (A-657; §4.17.3 "Push" step 6: "The changesets become the
//! versions' objects"): every changeset, manifest and file revision the push brought, appended in 7.2's order (the
//! changelog, then the manifest log, then each file log in the order the push sent it, as `changegroup.apply` adds them
//! \[C\]), each text kept once as content, a file revision's metadata block apart from its bytes and its parents in the
//! order 7.2 stores them (`FileRevision::parents_as_stored`, the issue6528 rule).
//!
//! **All or none.** The revisions go into a fork of the index ([`fork_with`]), which shares the index's tables and adds
//! only its own rows. The caller keeps the fork in the index's place once the push's register entry commits, or
//! releases it when the entry is refused; the index it was forked from is never changed, so a refusal at any point
//! leaves the repository's objects exactly as they were. Each text's hold is given back once the rows hold it, as the
//! import gives its holds back (A-630).

use slates_vcs::{ContentId, ContentRefused, ContentStore, IndexRefused};

use super::verify::{Arrived, Verified};
use crate::index::{self, HgIndex, Log, NewChangeset, NewRevision};
use crate::read::to_u64;
use crate::{Admission, Node, PastAdmission, changeset, filerev};

/// Why a push's revisions could not be kept; nothing of them is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unindexed {
  /// The index refused a group (its budget, a row that differs, a log full).
  Index(index::Refused),
  /// The index's fork or release refused (its budget).
  Fork(IndexRefused),
  /// The content store refused a text (its budget).
  Content(ContentRefused),
  /// A changeset's text the reader refuses: never met for a verified push.
  Changeset {
    /// The changeset.
    node: Node,
    /// What the reader found.
    fault: changeset::Refusal,
  },
  /// A file revision's text the reader refuses: never met for a verified push.
  FileRevision {
    /// The revision.
    node: Node,
    /// What the reader found.
    fault: filerev::Refusal,
  },
  /// More than the request's admission had left.
  PastAdmission(PastAdmission),
  /// A refusal, and the release of what it held refused beside it.
  Unreleased {
    /// The first refusal.
    cause: Box<Unindexed>,
    /// The release's.
    release: IndexRefused,
  },
}

impl From<PastAdmission> for Unindexed {
  fn from(past: PastAdmission) -> Self {
    Self::PastAdmission(past)
  }
}

impl From<ContentRefused> for Unindexed {
  fn from(refused: ContentRefused) -> Self {
    Self::Content(refused)
  }
}

impl From<index::Refused> for Unindexed {
  fn from(refused: index::Refused) -> Self {
    Self::Index(refused)
  }
}

/// The holds the texts' puts took, given back once the rows hold the contents or the push is refused.
#[derive(Default)]
struct Puts(Vec<ContentId>);

impl Puts {
  /// Keeps `bytes` as content and records the hold.
  fn put(&mut self, store: &mut ContentStore, bytes: &[u8]) -> Result<ContentId, Unindexed> {
    let identity = store.put_object(bytes)?;
    self.0.push(identity);
    Ok(identity)
  }

  /// Gives back every hold: the first refusal, if any.
  fn give_back(self, store: &mut ContentStore) -> Result<(), ContentRefused> {
    let mut first = Ok(());
    for identity in &self.0 {
      first = first.and(store.release(identity).map(drop));
    }
    first
  }
}

/// A fork of `index` holding every revision `verified` brought (module docs); refused, the fork is released and every
/// hold given back, and `index` is as it was.
pub fn fork_with(
  index: &mut HgIndex,
  store: &mut ContentStore,
  verified: &Verified,
  admission: &mut Admission,
) -> Result<HgIndex, Unindexed> {
  let mut fork = index.fork(store).map_err(Unindexed::Fork)?;
  let mut puts = Puts::default();
  let appended = append(&mut fork, store, verified, &mut puts, admission);
  let given = puts.give_back(store).map_err(IndexRefused::Content);
  let refused = match (appended, given) {
    (Ok(()), Ok(())) => return Ok(fork),
    (Ok(()), Err(release)) => Unindexed::Fork(release),
    (Err(cause), Ok(())) => cause,
    (Err(cause), Err(release)) => Unindexed::Unreleased {
      cause: Box::new(cause),
      release,
    },
  };
  match fork.release(store) {
    Ok(()) => Err(refused),
    Err(release) => Err(Unindexed::Unreleased {
      cause: Box::new(refused),
      release,
    }),
  }
}

/// Appends the changelog's, the manifest log's and each file log's new revisions to `fork`.
fn append(
  fork: &mut HgIndex,
  store: &mut ContentStore,
  verified: &Verified,
  puts: &mut Puts,
  admission: &mut Admission,
) -> Result<(), Unindexed> {
  changesets(fork, store, &verified.changesets, puts, admission)?;
  let manifests = revisions(store, &verified.manifests, puts, admission, false)?;
  fork.append_revisions(store, Log::Manifest, &manifests)?;
  for (path, arrived) in &verified.files {
    let group = revisions(store, arrived, puts, admission, true)?;
    fork.append_revisions(store, Log::File(path), &group)?;
  }
  Ok(())
}

/// What the changelog's row keeps of one changeset's text: its manifest's node, its branch and whether it closes it.
type ChangesetFields = (Node, Vec<u8>, bool);

/// The changelog's group appended: each text kept, its manifest, branch and close read as 7.2 reads them.
fn changesets(
  fork: &mut HgIndex,
  store: &mut ContentStore,
  arrived: &[Arrived],
  puts: &mut Puts,
  admission: &mut Admission,
) -> Result<(), Unindexed> {
  admission.charge_items::<(ChangesetFields, ContentId)>(arrived.len())?;
  let mut fields: Vec<(ChangesetFields, ContentId)> = Vec::with_capacity(arrived.len());
  for changeset in arrived {
    let fault = |fault| Unindexed::Changeset {
      node: changeset.node,
      fault,
    };
    let read = changeset::decode(&changeset.text).map_err(fault)?;
    let manifest = read.manifest_node().map_err(fault)?;
    let (branch, closed) = crate::import::branch_of(&read, admission).map_err(fault)?;
    let text = puts.put(store, &changeset.text)?;
    fields.push(((manifest, branch, closed), text));
  }
  let group: Vec<NewChangeset<'_>> = arrived
    .iter()
    .zip(&fields)
    .map(
      |(changeset, ((manifest, branch, closed), text))| NewChangeset {
        node: changeset.node,
        p1: changeset.p1,
        p2: changeset.p2,
        flags: 0,
        len: to_u64(changeset.text.len()),
        text: *text,
        manifest: *manifest,
        closed: *closed,
        branch,
      },
    )
    .collect();
  fork.append_changesets(store, &group)?;
  Ok(())
}

/// A manifest log's or a file log's group, each text kept: a file revision's block apart from its bytes and its
/// parents in the order 7.2 stores them.
fn revisions(
  store: &mut ContentStore,
  arrived: &[Arrived],
  puts: &mut Puts,
  admission: &mut Admission,
  files: bool,
) -> Result<Vec<NewRevision>, Unindexed> {
  admission.charge_items::<NewRevision>(arrived.len())?;
  let mut group = Vec::with_capacity(arrived.len());
  for revision in arrived {
    let (p1, p2, bytes, block) = if files {
      let split = filerev::split(&revision.text).map_err(|fault| Unindexed::FileRevision {
        node: revision.node,
        fault,
      })?;
      let (p1, p2) = split.parents_as_stored(revision.p1, revision.p2);
      let block = split
        .metadata
        .map(|block| puts.put(store, block))
        .transpose()?;
      (p1, p2, puts.put(store, split.bytes)?, block)
    } else {
      let text = puts.put(store, &revision.text)?;
      (revision.p1, revision.p2, text, None)
    };
    group.push(NewRevision {
      node: revision.node,
      p1,
      p2,
      link: revision.link,
      flags: 0,
      len: to_u64(revision.text.len()),
      bytes,
      block,
    });
  }
  Ok(group)
}

#[cfg(test)]
#[path = "commit_tests.rs"]
mod tests;
