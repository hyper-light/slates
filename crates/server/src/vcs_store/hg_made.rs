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
//! once, its branch its parent's (a root takes its line's).
//!
//! **On the changeset store** (A-767 C1; main's ruling of 2026-11-21 09:00, one path). The server builds the made
//! revisions' changegroup (`slates_hg::push::made::changegroup`), takes it through the push's own intake into a fork
//! (`hg_push::take_made`), and lands it as a push's changeset is landed: `Record` (a made record, the green's key and
//! the version beside the push's head, `import::record::write_made_record`), `Payload`, `Texts`, then the register's
//! entry naming it by reference (`write_made_reference`). A recorded submit takes the deferred-submit hook (A-807,
//! `crate::made_commit`, [`make_deferred`]): the repository's landing turn before its verdict, the changeset made from a
//! preview of the held verdict, landed on the store by a task, then the version and the entry committed in one step,
//! then the answer; a store that refused it gives the accept up. A bare dispatch ([`make`]) takes the turn in its step
//! and a task lands and records the changeset after it, in its own transaction. A client's push landed between a made
//! changeset's making and its commit leaves its fork stale: the version stands alone, counted.

//! **Its head** ([`head_of`]): a line's or a bookmark's changeset is the one the server made of its green's head version
//! where it made one (the store's `made` map, green by green, the newest only), else the one its head names
//! (`HEAD_ATTRIBUTE`, which a push sets): a push on top of a made changeset is the newer version and wins as the head.
//! A recovery replays the changeset from the store and puts the made node back in the map from its record
//! (`super::replay_hg_referenced`).
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
/// Counter: a version of which no changeset was made since a file of it is held by its content-plane object alone
/// ([`Unmade::Named`]; A-760 (b) 4b; never met, since only a Subversion green holds a file so), beside [`UNMADE`].
/// Format: a counter name in the daemon's status report.
pub(crate) const NAMED_UNREAD: &str = "vcs.hg.named_unread";

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
  /// The record could not be written.
  Record,
  /// The register refused the entry.
  Entry(Refusal),
  /// The changeset store refused the changeset, or the task could not run.
  Unlanded,
  /// A client's push landed after the changeset was made: its fork is stale.
  Stale,
  /// A file of the change is held by its content-plane object alone, which an hg green never holds (A-760 (b) 4b).
  Named(super::named_objects::NamedUnread),
  /// The parent changeset's or its manifest's text, let go once landed, could not be read back from the node's
  /// changeset store (goal 93, C1 (1)).
  ParentUnread,
}

/// Written out rather than derived, as a derived `Debug` is all that reads the refusals and dead-code analysis ignores
/// derived impls; the output is the derived one's.
impl std::fmt::Debug for Unmade {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Unmade::Authorship => formatter.write_str("Authorship"),
      Unmade::NotHead => formatter.write_str("NotHead"),
      Unmade::Made(refused) => formatter.debug_tuple("Made").field(refused).finish(),
      Unmade::Record => formatter.write_str("Record"),
      Unmade::Entry(refused) => formatter.debug_tuple("Entry").field(refused).finish(),
      Unmade::Unlanded => formatter.write_str("Unlanded"),
      Unmade::Stale => formatter.write_str("Stale"),
      Unmade::Named(unread) => formatter.debug_tuple("Named").field(unread).finish(),
      Unmade::ParentUnread => formatter.write_str("ParentUnread"),
    }
  }
}

/// Makes the changeset of `green`'s `version`, where `green` is a Mercurial repository's (module docs; A-767 C1): the
/// repository's landing turn taken in the submit's own step where it is free, then the changeset made of the green as
/// the version left it, landed on the node's changeset store and recorded by a task holding the turn
/// ([`made_landing`]); counted.
pub(crate) fn make(
  state: &mut ShardState,
  green: DbVolumeId,
  version: u64,
  authorship: &Authorship,
  copies: &[MadeCopy],
) {
  let Some(repository) = mercurial_repository_of(state, green) else {
    return;
  };
  let turn = crate::repository::svn_commit::landing::Turn::take_now(state, repository);
  let task = made_landing(
    (repository, green, version),
    (authorship.clone(), copies.to_vec()),
    turn,
  );
  if slates_rt::futures::spawn(task)
    .and_then(slates_rt::futures::detach)
    .is_err()
  {
    unmade(state, &Unmade::Unlanded);
  }
}

/// Counts and logs a version of which no changeset was made.
fn unmade(state: &mut ShardState, why: &Unmade) {
  slates_machine::log_line!(
    "slates-server: partition {}: no changeset was made of a submitted version: {why:?}",
    state.partition
  );
  state.count(UNMADE, 1);
  if matches!(why, Unmade::Named(_)) {
    state.count(NAMED_UNREAD, 1);
  }
}

/// [`make`]'s task: the landing turn (`turn`, or awaited where the submit could not take it), then the changeset made
/// of `green`'s `version`, landed and recorded in its own transaction; counted.
async fn made_landing(
  (repository, green, version): (RepositoryId, DbVolumeId, u64),
  (authorship, copies): (Authorship, Vec<MadeCopy>),
  turn: Option<crate::repository::svn_commit::landing::Turn>,
) {
  let turn = match turn {
    Some(turn) => turn,
    None => match crate::repository::svn_commit::landing::turn(repository).await {
      Some(turn) => turn,
      None => return,
    },
  };
  let made = if parent_texts_at_hand(repository, (green, version)).await {
    crate::state::with_state(|state| {
      super::lent_state(state, |state| {
        made_applied(state, green, version, (&authorship, &copies))
      })
    })
  } else {
    Some(Err(Unmade::ParentUnread))
  };
  let Some(made) = made else {
    return;
  };
  let made = match made {
    Ok(Some(made)) => made,
    other => {
      let _ = crate::state::with_state(move |state| {
        if let Err(why) = &other {
          unmade(state, why);
        }
        turn.give_back(state);
      });
      return;
    }
  };
  let landed = landed(made).await;
  let _ = crate::state::with_state(move |state| {
    match landed {
      Some(Ok((made, prepared))) => {
        state.db.begin();
        let committed = commit_made(state, &made, prepared);
        let committed = match (committed, state.db.commit(&mut state.segment)) {
          (Ok(()), Ok(_)) => Ok(()),
          (Err(why), _) => Err(why),
          (Ok(()), Err(error)) => {
            crate::verbs::reconcile_unpublished_effects(state);
            Err(Unmade::Entry(refusal_of_db(&error)))
          }
        };
        match committed {
          Ok(()) => state.count(MADE, 1),
          Err(why) => unmade(state, &why),
        }
      }
      Some(Err(why)) => unmade(state, &why),
      None => {}
    }
    turn.give_back(state);
  });
}

/// Makes the changeset of the version `pending` holds, of a Mercurial repository's green, from its verdict `judged`,
/// held and not applied (A-805, `crate::made_commit`; A-767 C1): made from a preview of the verdict in this step, so the
/// green shows nothing of it; then a task holding the repository's landing turn, which the submit took before its
/// verdict, lands it on the node's changeset store and commits the version with it in one step, then answers.
pub(crate) fn make_deferred(
  state: &mut ShardState,
  turn: crate::repository::svn_commit::landing::Turn,
  pending: crate::made_commit::Pending,
  (copies, judged): (Vec<MadeCopy>, slates_merge::engine::Judged),
) {
  let green = pending.version().0;
  let request = pending.request();
  let task = made_commit_landing(turn, pending, (copies, judged));
  if slates_rt::futures::spawn(task)
    .and_then(slates_rt::futures::detach)
    .is_err()
  {
    // The task could not start (the shard's arena full): nothing of the accept is applied or durable.
    unmade(state, &Unmade::Unlanded);
    crate::made_commit::abandon(state, (request, green));
  }
}

/// [`make_deferred`]'s task: the changeset `made` landed on the store, then committed with the version
/// (`made_commit::commit`); a changeset not made, or one the commit refused, leaves the version standing alone, and a
/// store that refused it gives the accept up (`made_commit::abandon`).
async fn made_commit_landing(
  turn: crate::repository::svn_commit::landing::Turn,
  pending: crate::made_commit::Pending,
  (copies, judged): (Vec<MadeCopy>, slates_merge::engine::Judged),
) {
  let (green, version) = pending.version();
  // Made here, holding the landing turn, the verdict held and not applied (main's ruling of 2026-11-24 03:51, (i): one
  // place makes): the parent's texts let go once landed read back first (goal 93, C1 (1)).
  let repository =
    crate::state::with_state(|state| mercurial_repository_of(state, green)).flatten();
  let at_hand = match repository {
    Some(repository) => parent_texts_at_hand(repository, (green, version)).await,
    None => true,
  };
  let made = if at_hand {
    crate::state::with_state(|state| {
      super::lent_state(state, |state| {
        made_held(
          state,
          (green, version),
          (pending.authorship(), &copies),
          judged,
        )
      })
    })
    .unwrap_or(Err(Unmade::NotHead))
  } else {
    Err(Unmade::ParentUnread)
  };
  let made = match made {
    Ok(Some(made)) => made,
    other => {
      let _ = crate::state::with_state(move |state| {
        if let Err(why) = &other {
          unmade(state, why);
        }
        let _ = crate::made_commit::commit(state, pending, |_| Ok::<(), Unmade>(()));
        turn.give_back(state);
      });
      return;
    }
  };
  let landed = landed(made).await;
  let _ = crate::state::with_state(move |state| {
    match landed {
      Some(Ok((made, prepared))) => {
        match crate::made_commit::commit(state, pending, |state| {
          commit_made(state, &made, prepared)
        }) {
          Some(Ok(())) => state.count(MADE, 1),
          Some(Err(why)) => unmade(state, &why),
          None => {}
        }
      }
      // The store refused the changeset: nothing of the accept is applied or durable.
      Some(Err(Unmade::Unlanded)) => crate::made_commit::abandon(state, (pending.request(), green)),
      Some(Err(why)) => {
        unmade(state, &why);
        let _ = crate::made_commit::commit(state, pending, |_| Ok::<(), Unmade>(()));
      }
      None => {}
    }
    turn.give_back(state);
  });
}

/// Derived: the parent's texts a made changeset reads (goal 93, C1 (1)): the parent changeset's and the manifest it
/// names.
const PARENT_TEXTS: usize = 2;

/// Counter: a made changeset's parent text read back from the node's changeset store, let go once landed (goal 93, C1
/// (1)). Format: a counter name in the status report.
pub(crate) const PARENT_TEXTS_READ: &str = "vcs.hg.made_parent_texts_read";

/// Whether the parent's texts a changeset made of `green`'s `version` in `repository` reads are at hand, each let go
/// once landed brought back from the node's changeset store first (goal 93, C1 (1); main's ruling of 2026-11-24 03:51):
/// the parent changeset's, then the manifest it names, at most [`PARENT_TEXTS`]. True where none is needed.
async fn parent_texts_at_hand(
  repository: RepositoryId,
  (green, version): (DbVolumeId, u64),
) -> bool {
  for _ in 0..PARENT_TEXTS {
    let wanted =
      crate::state::with_state(|state| parent_text_wanted(state, repository, (green, version)))
        .flatten();
    let Some((log, node)) = wanted else {
      return true;
    };
    let path = match log {
      slates_hg::index::Log::Changelog => slates_hg::push::intake::CHANGELOG_PATH,
      _ => slates_hg::push::intake::MANIFEST_PATH,
    };
    let read = crate::repository::hg_push::bring_back_log_on(
      &mut crate::vcs_http::git::ThisShard,
      repository,
      None,
      (log, path.to_vec()),
      node,
    )
    .await;
    if read.is_err() {
      return false;
    }
    let _ = crate::state::with_state(|state| state.count(PARENT_TEXTS_READ, 1));
  }
  crate::state::with_state(|state| parent_text_wanted(state, repository, (green, version)))
    .flatten()
    .is_none()
}

/// The first of the parent's texts a changeset made of `green`'s `version` reads that is not at hand: the parent
/// changeset's, else the manifest it names; none where both are, or the parent is the null node.
fn parent_text_wanted(
  state: &mut ShardState,
  repository: RepositoryId,
  (green, version): (DbVolumeId, u64),
) -> Option<(slates_hg::index::Log<'static>, Node)> {
  use slates_hg::index::Log;
  let before = version.checked_sub(1)?;
  let parent = made_at(state, green, before).or_else(|| named_head(state, green))?;
  if parent == slates_hg::NULL_NODE {
    return None;
  }
  super::lent_state(state, |state| {
    super::within_budget(state, 0, |stores| {
      let (content, store) = stores.mercurial(repository)?;
      let index = &store.index;
      let at_hand = |log, node: &Node| {
        slates_hg::push::held::log_text_at_hand((index, content), log, node).map(|(_, at)| at)
      };
      if !at_hand(Log::Changelog, &parent)? {
        return Some((Log::Changelog, parent));
      }
      let rev = index.rev(content, Log::Changelog, &parent)?;
      let manifest = index.changeset(content, rev)?.manifest;
      if manifest == slates_hg::NULL_NODE || at_hand(Log::Manifest, &manifest)? {
        return None;
      }
      Some((Log::Manifest, manifest))
    })
  })
}

/// What a made changeset carries to its landing (module docs): its repository, its green and version, and the revisions
/// the server made.
struct Made {
  /// The repository.
  repository: RepositoryId,
  /// The green it was made of.
  green: DbVolumeId,
  /// The version it was made of.
  version: u64,
  /// Its revisions.
  verified: Verified,
}

/// What a made changeset is built from, read from the shard before the green is read: the repository, the version and
/// the one before, the parent, the user, message and time, the branch, and the room.
struct Inputs {
  /// The repository.
  repository: RepositoryId,
  /// The version before and the version.
  versions: (u64, u64),
  /// The parent changeset.
  parent: Node,
  /// The user, the message and the time in whole seconds.
  authored: (String, String, u64),
  /// The branch.
  branch: Vec<u8>,
  /// The room it is made within.
  room: u64,
}

/// [`Inputs`] of `green`'s `version`; none where `green` is no Mercurial repository's.
fn inputs(
  state: &mut ShardState,
  (green, version): (DbVolumeId, u64),
  authorship: &Authorship,
) -> Result<Option<Inputs>, Unmade> {
  let Some(repository) = mercurial_repository_of(state, green) else {
    return Ok(None);
  };
  // A store being made again from its records is lent to nothing (A-732): no changeset is made over half its history.
  if state.vcs_stores.rebuilding(repository) {
    return Err(Unmade::NotHead);
  }
  let user = authorship
    .identities
    .mercurial
    .clone()
    .ok_or(Unmade::Authorship)?;
  let message = authorship.message.clone().ok_or(Unmade::Authorship)?;
  // The changeset's time is the owner's instant, read once; its text carries it, so a replay reads no clock.
  let seconds = u64::try_from(state.clock.wall_ns().div_euclid(NANOS_PER_SECOND)).unwrap_or(0);
  let before = version.checked_sub(1).ok_or(Unmade::NotHead)?;
  let parent = made_at(state, green, before)
    .or_else(|| named_head(state, green))
    .unwrap_or(slates_hg::NULL_NODE);
  let room = crate::vcs_share::room(state, repository).bytes;
  let (content, store) = state
    .vcs_stores
    .mercurial_ref(repository)
    .ok_or(Unmade::NotHead)?;
  let branch = branch_of(state, (repository, green), (&store.index, content), parent);
  Ok(Some(Inputs {
    repository,
    versions: (before, version),
    parent,
    authored: (user, message, seconds),
    branch,
    room,
  }))
}

/// The changeset of `green`'s `version` made of the green as the version left it (a bare dispatch's, [`make`]).
fn made_applied(
  state: &mut ShardState,
  green: DbVolumeId,
  version: u64,
  (authorship, copies): (&Authorship, &[MadeCopy]),
) -> Result<Option<Made>, Unmade> {
  let Some(inputs) = inputs(state, (green, version), authorship)? else {
    return Ok(None);
  };
  let engine = state.greens.get(&green).ok_or(Unmade::NotHead)?;
  let (content, store) = state
    .vcs_stores
    .mercurial_ref(inputs.repository)
    .ok_or(Unmade::NotHead)?;
  let verified = verified_of(&engine, (&store.index, content), &inputs, copies)?;
  Ok(verified.map(|verified| Made {
    repository: inputs.repository,
    green,
    version,
    verified,
  }))
}

/// The changeset of `green`'s `version` made from the verdict `judged`, held and not applied: read from a preview of it
/// (`Greens::preview`), retired in this step.
fn made_held(
  state: &mut ShardState,
  (green, version): (DbVolumeId, u64),
  (authorship, copies): (&Authorship, &[MadeCopy]),
  judged: slates_merge::engine::Judged,
) -> Result<Option<Made>, Unmade> {
  let Some(inputs) = inputs(state, (green, version), authorship)? else {
    return Ok(None);
  };
  let repository = inputs.repository;
  // The preview is a branch whose head is its version 1, the verdict committed over its version 0 (`Lineage::preview`):
  // the change is read there, between those, whatever the green's own versions (goal 93 correctness: read at the
  // green's, a second version's preview was refused NotHead, its version committed alone).
  let previewed = Inputs {
    versions: (0, 1),
    ..inputs
  };
  let ShardState {
    greens, vcs_stores, ..
  } = &mut *state;
  let preview = greens
    .preview(&green, judged)
    .map_err(|_| Unmade::NotHead)?;
  let verified = greens
    .with_preview(&preview, |engine| {
      let (content, store) = vcs_stores
        .mercurial_ref(repository)
        .ok_or(Unmade::NotHead)?;
      verified_of(engine, (&store.index, content), &previewed, copies)
    })
    .unwrap_or(Err(Unmade::NotHead));
  greens.retire_preview(preview);
  Ok(verified?.map(|verified| Made {
    repository,
    green,
    version,
    verified,
  }))
}

/// A made changeset ready to land: its fork (the push's intake's, its changegroup taken into the index as it stood)
/// and its changeset with the register's reference to it.
type Prepared = (
  crate::repository::hg_push::Forked,
  crate::repository::hg_push::Changeset,
);

/// `made`'s changegroup (`slates_hg::push::made::changegroup`) taken through the push's own intake into a fork, its
/// changeset made with its made record and reference (`hg_push::changeset_landing`), then landed on the node's
/// changeset store and awaited (A-767 C1: a made changeset lands as a push's does). The fork is given back where the
/// store refused it. None where the shard's state is gone.
async fn landed(
  made: Made,
) -> Option<Result<(Made, (crate::repository::hg_push::Forked, Vec<u8>)), Unmade>> {
  let prepared = crate::state::with_state(|state| prepared(state, &made))?;
  let (forked, (disk, header, parts, reference)) = match prepared {
    Ok(prepared) => prepared,
    Err(why) => return Some(Err(why)),
  };
  if disk.land(header, parts).await.is_ok() {
    return Some(Ok((made, (forked, reference))));
  }
  crate::state::with_state(|state| give_back_fork(state, forked.fork))?;
  Some(Err(Unmade::Unlanded))
}

/// [`landed`]'s first half: the changegroup taken into a fork and the changeset made, its sequence taken now.
fn prepared(state: &mut ShardState, made: &Made) -> Result<Prepared, Unmade> {
  let room = crate::vcs_share::room(state, made.repository).bytes;
  let changegroup =
    slates_hg::push::made::changegroup(&made.verified, &mut slates_hg::Admission::new(room))
      .map_err(|_| Unmade::Record)?;
  let taken = crate::repository::hg_push::take_made(
    state,
    made.repository,
    (slates_hg::push::made::CHANGEGROUP_VERSION, changegroup),
    &mut slates_hg::Admission::new(room),
  );
  let mut forked = match taken {
    Some(Ok(forked)) => forked,
    Some(Err(_)) => return Err(Unmade::Record),
    None => return Err(Unmade::NotHead),
  };
  let made_key = (green_key(made.green), made.version);
  // A made changeset's payload is named whole, as its intake takes it whole (`hg_push::take_made`; owed, GAPS).
  let name = slates_hg::import::record::PayloadName::of(&forked.changegroup.1);
  match crate::repository::hg_push::changeset_landing(
    state,
    made.repository,
    (&mut forked, name),
    Some(made_key),
  ) {
    Some(changeset) => Ok((forked, changeset)),
    None => {
      give_back_fork(state, forked.fork);
      Err(Unmade::NotHead)
    }
  }
}

/// Records a landed made changeset in the caller's transaction (module docs): the register's entry naming its changeset
/// by reference, judged then appended, then its fork put in the index's place and the made node noted; the fork given
/// back where the entry is refused or a client's push made it stale meanwhile.
fn commit_made(
  state: &mut ShardState,
  made: &Made,
  (forked, reference): (crate::repository::hg_push::Forked, Vec<u8>),
) -> Result<(), Unmade> {
  let node = made
    .verified
    .changesets
    .first()
    .map(|changeset| changeset.node)
    .ok_or(Unmade::Record)?;
  if state.vcs_stores.generation_of(made.repository) != Some(forked.generation) {
    give_back_fork(state, forked.fork);
    return Err(Unmade::Stale);
  }
  let entry = Op::RepositoryAdvanced {
    entry: RepositoryEntry {
      repository: made.repository,
      objects: reference,
      ..RepositoryEntry::default()
    },
  };
  let now = state.clock.monotonic_ns();
  let appended = state
    .db
    .partition()
    .check(&entry, now)
    .and_then(|()| state.db.mutate(&mut state.segment, &entry, now).map(drop));
  if let Err(error) = appended {
    give_back_fork(state, forked.fork);
    return Err(Unmade::Entry(refusal_of_db(&error)));
  }
  let (repository, green, version) = (made.repository, made.green, made.version);
  let fork = forked.fork;
  super::hg_view::walks_noted(state, &fork);
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

/// Gives a made changeset's fork back, counted where a hold did not go back.
fn give_back_fork(state: &mut ShardState, fork: HgIndex) {
  let released = super::within_budget(state, 0, |stores| {
    stores
      .content_mut()
      .map_or(Ok(()), |content| fork.release(content))
  });
  if released.is_err() {
    state.count(super::RELEASE_REFUSED, 1);
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

/// The Mercurial repository `green` is the green of.
pub(crate) fn mercurial_repository_of(
  state: &ShardState,
  green: DbVolumeId,
) -> Option<RepositoryId> {
  let repositories = state.db.partition().repositories();
  let repository = repositories.member(green)?.repository;
  let record = repositories.repository(repository)?;
  matches!(record.tool, VcsTool::Mercurial).then_some(repository)
}

/// The revisions the server makes of `engine`'s change from the version before to the version over the parent
/// (module docs; `inputs`), read against `index`; none where the change touches no tracked file.
fn verified_of(
  engine: &Green,
  (index, content): (&HgIndex, &slates_vcs::ContentStore),
  inputs: &Inputs,
  copies: &[MadeCopy],
) -> Result<Option<Verified>, Unmade> {
  let (before, version) = inputs.versions;
  if engine.head() != version {
    return Err(Unmade::NotHead);
  }
  let paths = engine.changed_between(before, version);
  let sources: Vec<Option<String>> = paths.iter().map(|path| copied_from(path, copies)).collect();
  let changes: Vec<Change<'_>> = paths
    .iter()
    .zip(&sources)
    .map(|(path, source)| change_of(engine, path, source.as_deref()))
    .collect::<Result<_, _>>()?;
  let (user, message, seconds) = &inputs.authored;
  let authored = Authored {
    user: user.as_bytes(),
    seconds: *seconds,
    branch: &inputs.branch,
    message: message.as_bytes(),
  };
  let mut room = slates_hg::Admission::new(inputs.room);
  match made_of(
    &IndexHeld::new(index, content),
    inputs.parent,
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

/// The change at `path`; refused [`Unmade::Named`] at a file held by its content-plane object alone (A-760 (b) 4b).
fn change_of<'g>(
  engine: &'g Green,
  path: &'g str,
  source: Option<&'g str>,
) -> Result<Change<'g>, Unmade> {
  let after = match engine.symlink(path) {
    Some(target) => Some((target.as_bytes(), Flag::Link)),
    None => match engine.content(path) {
      None => None,
      Some(content) => {
        let bytes = super::named_objects::lent_bytes_of(path, content).map_err(Unmade::Named)?;
        let executable = engine.mode(path).is_some_and(|mode| mode & EXECUTABLE != 0);
        Some((
          bytes,
          if executable {
            Flag::Executable
          } else {
            Flag::Regular
          },
        ))
      }
    },
  };
  Ok(Change {
    path: path.as_bytes(),
    after,
    copied_from: source.map(str::as_bytes),
  })
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

#[cfg(test)]
#[path = "hg_made/tests.rs"]
mod tests;
