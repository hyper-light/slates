//! The changesets the server makes of the versions works submit to a Mercurial repository's green (A-731; A-664 item
//! 4's Mercurial half; §4.17.1 "A push is a work" items 3 and 4), the twin of git's made commits (`super::commits`) and
//! Subversion's made revisions (`super::svn_made`).
//!
//! **Why.** A slates-native work submitted to a Mercurial repository's green moves the green a version and carries no
//! changeset, so the repository's history had no changeset of it, and the next push's race checks met a line moved to no
//! head they knew.
//!
//! **What.** As the version commits on the owner ([`make`], from `verbs::judge_submission`, after git's and
//! Subversion's), the changeset of the version's change (`slates_hg::push::made`): its parent the line's head before the
//! version ([`head_of`] at the version before), each path the green changed between the two versions
//! (`Green::changed_between`) its bytes and flag at the head (a symbolic link's target with `l`, an executable `x`), or
//! removed; its user the principal's Mercurial identity, its message the submit's, its time the owner's instant read
//! once, its branch its parent's (a root takes its line's). The changeset, its manifest and its file revisions are kept
//! in a fork of the index (`commit::fork_with`) and recorded as a push's are, in the one Mercurial record kind with the
//! green's key and the version beside them (`import::record::write_made`, `Source::Made`), in an entry of the register
//! holding it alone (A-662), appended in the submit's own transaction after the version's record: judged by the
//! register's guard first, the fork kept only once the entry is appended, given back otherwise.
//!
//! **Its head** ([`head_of`]): a line's or a bookmark's changeset is the one the server made of its green's head version
//! where it made one (the store's `made` map, green by green, the newest only), else the one its head names
//! (`HEAD_ATTRIBUTE`, which a push sets): a push on top of a made changeset is the newer version and wins as the head.
//! A recovery replays the record and puts the made node back in the map from the record alone (`super::replay_hg`).
//!
//! **Not made, and counted** ([`UNMADE`]), the version standing: no Mercurial identity or message, a version that is not
//! the green's head, a change the changeset cannot hold, the index's, the store's or the register's refusal.
//!
//! **Owed** (GAPS, A-731): copies as file revisions naming their source; the holders' changesets (A-636's fleet half).

use super::svn_made::MadeCopy;
use slates_db::catalog::{RepositoryEntry, RepositoryId, VcsTool, VolumeId as DbVolumeId};
use slates_db::op::Op;
use slates_hg::Node;
use slates_hg::index::HgIndex;
use slates_hg::manifest::Flag;
use slates_hg::push::commit::{Unindexed, fork_with};
use slates_hg::push::held::IndexHeld;
use slates_hg::push::made::{Authored, Change, Unmade as MadeRefused, made as made_of};
use slates_hg::push::verify::Verified;
use slates_ipc::protocol::{Authorship, Refusal};
use slates_merge::engine::Green;
use slates_merge::push::{HEAD_ATTRIBUTE, ROOT};
use slates_vfs::clock::Clock;

use super::green_key;
use crate::error::refusal_of_db;
use crate::repository::hg_push::{LINE_TAG, line_of};
use crate::state::ShardState;

/// Counter: a changeset the server made of a submitted version. Format: a counter name in the daemon's status report.
pub(crate) const MADE: &str = "vcs.hg.changeset_made";

/// Counter: a submitted version of a Mercurial repository's green of which no changeset was made (module docs).
/// Format: a counter name in the daemon's status report.
pub(crate) const UNMADE: &str = "vcs.hg.changeset_unmade";

/// Format: nanoseconds in a second.
const NANOS_PER_SECOND: i64 = 1_000_000_000;
/// Format: the mode bits of a file anyone may execute.
const EXECUTABLE: u32 = 0o111;
/// Format: the branch a root changeset on no line takes.
const DEFAULT_BRANCH: &[u8] = b"default";

/// Why no changeset was made of a version (module docs).
enum Unmade {
  /// No Mercurial identity or message.
  Authorship,
  /// The version is not the green's head, or the repository's store is not held here.
  NotHead,
  /// The changeset could not be made of the change.
  Made(MadeRefused),
  /// The index refused the revisions.
  Index(Unindexed),
  /// The record could not be written.
  Record,
  /// The register refused the entry.
  Entry(Refusal),
}

/// Written out rather than derived, as a derived `Debug` is all that reads the refusals and dead-code analysis ignores
/// derived impls; the output is the derived one's.
impl std::fmt::Debug for Unmade {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Unmade::Authorship => formatter.write_str("Authorship"),
      Unmade::NotHead => formatter.write_str("NotHead"),
      Unmade::Made(refused) => formatter.debug_tuple("Made").field(refused).finish(),
      Unmade::Index(refused) => formatter.debug_tuple("Index").field(refused).finish(),
      Unmade::Record => formatter.write_str("Record"),
      Unmade::Entry(refused) => formatter.debug_tuple("Entry").field(refused).finish(),
    }
  }
}

/// Makes the changeset of `green`'s `version`, where `green` is a Mercurial repository's (module docs); counted.
pub(crate) fn make(
  state: &mut ShardState,
  green: DbVolumeId,
  version: u64,
  authorship: &Authorship,
  copies: &[MadeCopy],
) {
  match made(state, green, version, authorship, copies) {
    Ok(true) => state.count(MADE, 1),
    Ok(false) => {}
    Err(why) => {
      slates_machine::log_line!(
        "slates-server: partition {}: no changeset was made of a submitted version: {why:?}",
        state.partition
      );
      state.count(UNMADE, 1);
    }
  }
}

/// The changeset that is `green`'s head (module docs): the one the server made of its head version, else the one its
/// head names.
pub(crate) fn head_of(state: &ShardState, green: DbVolumeId) -> Option<Node> {
  let version = state.db.partition().green_head(green);
  made_at(state, green, version).or_else(|| named_head(state, green))
}

/// The changeset the server made of `green`'s `version`, if it made one.
fn made_at(state: &ShardState, green: DbVolumeId, version: u64) -> Option<Node> {
  let repository = state
    .db
    .partition()
    .repositories()
    .member(green)?
    .repository;
  let (_, store) = state.vcs_stores.mercurial_ref(repository)?;
  store
    .made
    .get(&green_key(green))
    .filter(|(made, _)| *made == version)
    .map(|(_, node)| *node)
}

/// The changeset `green`'s head attribute names (a push's).
fn named_head(state: &ShardState, green: DbVolumeId) -> Option<Node> {
  state.greens.get(&green).and_then(|engine| {
    engine
      .xattr(ROOT, HEAD_ATTRIBUTE)
      .and_then(|bytes| Node::try_from(bytes).ok())
  })
}

/// [`make`]'s steps: whether a changeset was made.
fn made(
  state: &mut ShardState,
  green: DbVolumeId,
  version: u64,
  authorship: &Authorship,
  copies: &[MadeCopy],
) -> Result<bool, Unmade> {
  let Some(repository) = mercurial_repository_of(state, green) else {
    return Ok(false);
  };
  // A store being made again from its records is lent to nothing (A-732): no changeset is made over half its history.
  if state.vcs_stores.rebuilding(repository) {
    return Err(Unmade::NotHead);
  }
  let user = authorship
    .identities
    .mercurial
    .as_deref()
    .ok_or(Unmade::Authorship)?;
  let message = authorship.message.as_deref().ok_or(Unmade::Authorship)?;
  // The changeset's time is the owner's instant, read once; its text carries it, so a replay reads no clock.
  let seconds = u64::try_from(state.clock.wall_ns().div_euclid(NANOS_PER_SECOND)).unwrap_or(0);
  let before = version.checked_sub(1).ok_or(Unmade::NotHead)?;
  let parent = made_at(state, green, before)
    .or_else(|| named_head(state, green))
    .unwrap_or(slates_hg::NULL_NODE);
  let room = crate::vcs_share::room(state, repository).bytes;
  let verified = super::lent_state(state, |state| {
    changeset_of(
      state,
      room,
      (repository, green),
      (before, version),
      parent,
      (user, message, seconds),
      copies,
    )
  })?;
  let Some(verified) = verified else {
    return Ok(false);
  };
  recorded(state, (repository, green, version), &verified)?;
  Ok(true)
}

/// The Mercurial repository `green` is the green of.
fn mercurial_repository_of(state: &ShardState, green: DbVolumeId) -> Option<RepositoryId> {
  let repositories = state.db.partition().repositories();
  let repository = repositories.member(green)?.repository;
  let record = repositories.repository(repository)?;
  matches!(record.tool, VcsTool::Mercurial).then_some(repository)
}

/// The revisions the server makes of `green`'s change from `before` to `version` over `parent` (module docs); none
/// where the change touches no tracked file.
fn changeset_of(
  state: &ShardState,
  room: u64,
  (repository, green): (RepositoryId, DbVolumeId),
  (before, version): (u64, u64),
  parent: Node,
  (user, message, seconds): (&str, &str, u64),
  copies: &[MadeCopy],
) -> Result<Option<Verified>, Unmade> {
  let engine = state.greens.get(&green).ok_or(Unmade::NotHead)?;
  if engine.head() != version {
    return Err(Unmade::NotHead);
  }
  let (content, store) = state
    .vcs_stores
    .mercurial_ref(repository)
    .ok_or(Unmade::NotHead)?;
  let paths = engine.changed_between(before, version);
  let sources: Vec<Option<String>> = paths.iter().map(|path| copied_from(path, copies)).collect();
  let changes: Vec<Change<'_>> = paths
    .iter()
    .zip(&sources)
    .map(|(path, source)| change_of(&engine, path, source.as_deref()))
    .collect();
  let branch = branch_of(state, (repository, green), (&store.index, content), parent);
  let authored = Authored {
    user: user.as_bytes(),
    seconds,
    branch: &branch,
    message: message.as_bytes(),
  };
  let mut room = slates_hg::Admission::new(room);
  match made_of(
    &IndexHeld::new(&store.index, content),
    parent,
    &changes,
    &authored,
    &mut room,
  ) {
    Ok(verified) => Ok(Some(verified)),
    Err(MadeRefused::NothingChanged) => Ok(None),
    Err(refused) => Err(Unmade::Made(refused)),
  }
}

/// One changed path as the changeset takes it: its bytes and flag at the head, or removed.
/// The path `path` was copied from, where one of `copies` (the version's, A-626 part 4) copied it: a copy's destination
/// itself, or a path under a copied directory, its source the same path under the copy's source; the last copy that
/// names it, as the version's operations apply in order.
fn copied_from(path: &str, copies: &[MadeCopy]) -> Option<String> {
  let path = path.trim_matches('/');
  copies.iter().rev().find_map(|copy| {
    let (to, from) = copy.green_paths();
    if path == to {
      return Some(from.to_owned());
    }
    let rest = path.strip_prefix(to)?.strip_prefix('/')?;
    Some(if from.is_empty() {
      rest.to_owned()
    } else {
      format!("{from}/{rest}")
    })
  })
}

fn change_of<'g>(engine: &'g Green, path: &'g str, source: Option<&'g str>) -> Change<'g> {
  let after = match engine.symlink(path) {
    Some(target) => Some((target.as_bytes(), Flag::Link)),
    None => engine.content(path).map(|bytes| {
      let executable = engine.mode(path).is_some_and(|mode| mode & EXECUTABLE != 0);
      (
        bytes,
        if executable {
          Flag::Executable
        } else {
          Flag::Regular
        },
      )
    }),
  };
  Change {
    path: path.as_bytes(),
    after,
    copied_from: source.map(str::as_bytes),
  }
}

/// The made changeset's branch: its parent's, or for a root its line's, else `default`.
fn branch_of(
  state: &ShardState,
  (repository, green): (RepositoryId, DbVolumeId),
  (index, content): (&HgIndex, &slates_vcs::ContentStore),
  parent: Node,
) -> Vec<u8> {
  let parents = index
    .rev(content, slates_hg::index::Log::Changelog, &parent)
    .and_then(|rev| index.changeset(content, rev))
    .map(|row| row.branch.to_vec());
  parents
    .or_else(|| {
      state
        .db
        .partition()
        .repositories()
        .branches(repository, LINE_TAG)
        .find(|(_, member)| *member == green)
        .and_then(|(name, _)| line_of(name).map(|(branch, _)| branch))
    })
    .unwrap_or_else(|| DEFAULT_BRANCH.to_vec())
}

/// Records the made revisions (module docs): the entry judged by the register's guard, the revisions kept in a fork,
/// the entry appended, then the fork put in the index's place and the made node noted; the fork given back where the
/// append is refused.
fn recorded(
  state: &mut ShardState,
  (repository, green, version): (RepositoryId, DbVolumeId, u64),
  verified: &Verified,
) -> Result<(), Unmade> {
  let node = verified
    .changesets
    .first()
    .map(|changeset| changeset.node)
    .ok_or(Unmade::Record)?;
  let room = crate::vcs_share::room(state, repository).bytes;
  // The made manifest's record is its delta against its first parent's text (A-729 part 2), read from the index.
  let parent = verified
    .manifests
    .first()
    .map(|manifest| manifest.p1)
    .filter(|p1| *p1 != slates_hg::NULL_NODE)
    .and_then(|p1| {
      let (content, store) = state.vcs_stores.mercurial_ref(repository)?;
      slates_hg::push::verify::Held::manifest_text(
        &slates_hg::push::held::IndexHeld::new(&store.index, content),
        &p1,
      )
      .map(std::borrow::Cow::into_owned)
    });
  let objects = slates_hg::import::record::write_made(
    verified,
    (green_key(green), version),
    parent.as_deref(),
    room,
  )
  .map_err(|_| Unmade::Record)?;
  let entry = Op::RepositoryAdvanced {
    entry: RepositoryEntry {
      repository,
      objects,
      ..RepositoryEntry::default()
    },
  };
  let now = state.clock.monotonic_ns();
  state
    .db
    .partition()
    .check(&entry, now)
    .map_err(|error| Unmade::Entry(refusal_of_db(&error)))?;
  let forked = super::within_budget(state, room, |stores| {
    let (content, store) = stores.mercurial(repository)?;
    Some(fork_with(
      &mut store.index,
      content,
      verified,
      &mut slates_hg::Admission::new(room),
    ))
  });
  let fork = match forked {
    Some(Ok(fork)) => fork,
    Some(Err(refused)) => return Err(Unmade::Index(refused)),
    None => return Err(Unmade::NotHead),
  };
  if let Err(error) = state.db.mutate(&mut state.segment, &entry, now) {
    let released = super::within_budget(state, 0, |stores| {
      stores
        .content_mut()
        .map_or(Ok(()), |content| fork.release(content))
    });
    if released.is_err() {
      state.count(super::RELEASE_REFUSED, 1);
    }
    return Err(Unmade::Entry(refusal_of_db(&error)));
  }
  let released = super::within_budget(state, 0, |stores| {
    let replaced = match stores.mercurial(repository) {
      Some((content, store)) => {
        store.made.insert(green_key(green), (version, node));
        core::mem::replace(&mut store.index, fork).release(content)
      }
      None => stores
        .content_mut()
        .map_or(Ok(()), |content| fork.release(content)),
    };
    stores.index_changed(repository);
    replaced
  });
  if released.is_err() {
    state.count(super::RELEASE_REFUSED, 1);
  }
  Ok(())
}

#[cfg(test)]
#[path = "hg_made/tests.rs"]
mod tests;
