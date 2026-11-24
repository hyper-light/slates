//! A Mercurial green's file held by its object's name alone, its windows let go, brought back from the node's
//! changeset store by the revision its green's changeset names (A-887; A-760 (b) 4d; main 01a20182's ruling (B) of
//! 2026-11-24 00:16).
//!
//! A push names each file whose bytes the content store holds whole (A-887), so the green keeps only the object's
//! identity (`slates_merge::named::Identity`): its hash and its length. Once the push's segment is durable the
//! repository lets its texts' windows go (A-767 C1), and the node's store holds them by Mercurial's names, a file's path
//! and its revision's node, never by the object's identity. Only a home a let-go window is pinned for notes those names
//! (`homes::Landed::Hg`), so an identity alone cannot be asked for.
//!
//! **Resolution, not a record.** The green already holds what names the revision: the reader's path, and at the
//! version it read, the changeset its root's head attribute names (`slates_merge::push::HEAD_ATTRIBUTE`). That
//! changeset's text names its manifest, and the manifest names the path's file revision, found by halving its sorted
//! lines (`slates_hg::manifest::find`). Each path of a parked set is found in one read of the one manifest. A
//! changeset's or a manifest's text let go itself comes back first through the same bring-back by its log's name
//! (`hg_push::bring_back` with `CHANGELOG_PATH` or `MANIFEST_PATH`), never a side read. Main chose this over a record
//! per named file: such a record would keep about 100 bytes and the path for every named file as long as its green
//! lives, paid always for a cold path's benefit.
//!
//! **Checked twice.** The revision's row names its bytes' object and its length (`IndexHeld::file_names`,
//! `file_bytes_len`), and they must be the identity the green named: if they are not, the read is refused typed
//! ([`HgNamedRefused::Mismatch`], naming both), never installed. The content store then hashes the bytes the store
//! gives back against that identity before it takes them (`ContentStore::bring_back_object`).

use slates_db::catalog::{RepositoryId, VolumeId};
use slates_hg::Node;
use slates_hg::push::verify::Held;
use slates_merge::named::Identity;
use slates_merge::push::{HEAD_ATTRIBUTE, ROOT};

use crate::state::ShardState;
use crate::vcs_http::git::{ShardLender, ThisShard};

/// Where a read met a named text whose windows were let go: the green, the path it read there (a work's renamed file
/// names its base's path), and the version it read at. What the reader knew when it noted the read
/// (`named_objects::AwayRead`), so nothing is kept to find the text again.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ReadAt {
  /// The green.
  pub(crate) green: VolumeId,
  /// The version it was read at.
  pub(crate) version: u64,
  /// The path read, the green's own.
  pub(crate) path: String,
}

/// Why a Mercurial green's named text was not brought back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HgNamedRefused {
  /// No Mercurial store holds the repository here, or no shard's state was lent.
  NoStore {
    /// The repository.
    repository: RepositoryId,
  },
  /// The green's version names no changeset (no head attribute at it).
  NoChangeset {
    /// The green.
    green: VolumeId,
    /// The version.
    version: u64,
  },
  /// The revision was not brought back whole (`hg_push::bring_back`'s refusal), or a changeset's or manifest's text
  /// brought back was let go again before the next resolution read it (`why` none).
  NotBroughtBack {
    /// The log's name: a file's path, or the changelog's or the manifest log's (`CHANGELOG_PATH`, `MANIFEST_PATH`).
    path: Vec<u8>,
    /// The revision's node.
    node: Node,
    /// Why, where the bring-back refused it.
    why: Option<crate::repository::hg_push::BringBackRefused>,
  },
  /// A changeset's or a manifest's text that does not decode.
  Malformed {
    /// The revision's node.
    node: Node,
  },
  /// The changeset's manifest names no file at the path.
  NotInManifest {
    /// The path.
    path: String,
    /// The changeset.
    changeset: Node,
  },
  /// The revision the manifest names is not the object the green named.
  Mismatch {
    /// What the green named.
    wanted: Identity,
    /// What the revision's row names, if it names one.
    found: Option<Identity>,
  },
}

/// What one resolution found.
enum Resolution {
  /// Each wanted path's revision: its identity, its path's bytes and its node.
  Revisions(Vec<(Identity, Vec<u8>, Node)>),
  /// A changeset's or a manifest's text let go, which comes back first: its log's name and its node.
  Bring(Vec<u8>, Node),
}

/// Bound: the resolutions one bring-back makes. Derived: a resolution reads a changeset's text and then its
/// manifest's, and each let go comes back once before the next resolution, so at most two come back and the third
/// resolution finds every revision. A text let go again in between refuses the read (`NotBroughtBack`), never a loop.
const RESOLUTIONS: usize = 3;

/// The Mercurial repository whose green `green` is, with the store holding it here.
pub(crate) fn repository_of(state: &ShardState, green: VolumeId) -> Option<RepositoryId> {
  let repository = super::hg_made::mercurial_repository_of(state, green)?;
  state
    .vcs_stores
    .mercurial_ref(repository)
    .map(|_| repository)
}

/// The changeset green `green` names at `version` (its root's head attribute then), if any.
fn changeset_at(state: &ShardState, (green, version): (VolumeId, u64)) -> Option<Node> {
  state
    .greens
    .get(&green)
    .and_then(|engine| engine.xattr_at(ROOT, HEAD_ATTRIBUTE, version))
    .and_then(|bytes| Node::try_from(bytes.as_slice()).ok())
}

/// The revisions of `wanted` (each an object a green named and the path it is the file at) in changeset `changeset` of
/// Mercurial repository `repository`, each checked against its object; or the changeset's or manifest's text to bring
/// back first. The one resolver of a read's bring-back and a recovery's (main 01a20182's ruling of 2026-11-24 02:17).
fn resolve_at(
  state: &ShardState,
  repository: RepositoryId,
  changeset: Node,
  wanted: &[(Identity, String)],
) -> Result<Resolution, HgNamedRefused> {
  let (content, store) = state
    .vcs_stores
    .mercurial_ref(repository)
    .ok_or(HgNamedRefused::NoStore { repository })?;
  let held = slates_hg::push::held::IndexHeld::new(&store.index, content);
  let Some(text) = held.changeset_text(&changeset) else {
    return Ok(Resolution::Bring(
      slates_hg::push::intake::CHANGELOG_PATH.to_vec(),
      changeset,
    ));
  };
  let manifest = slates_hg::changeset::decode(&text)
    .and_then(|decoded| decoded.manifest_node())
    .map_err(|_| HgNamedRefused::Malformed { node: changeset })?;
  let Some(lines) = held.manifest_text(&manifest) else {
    return Ok(Resolution::Bring(
      slates_hg::push::intake::MANIFEST_PATH.to_vec(),
      manifest,
    ));
  };
  let mut revisions = Vec::with_capacity(wanted.len());
  for (identity, path) in wanted {
    let entry = slates_hg::manifest::find(&lines, path.as_bytes()).ok_or_else(|| {
      HgNamedRefused::NotInManifest {
        path: path.clone(),
        changeset,
      }
    })?;
    let found = held
      .file_names(path.as_bytes(), &entry.node)
      .zip(held.file_bytes_len(path.as_bytes(), &entry.node))
      .map(|(names, len)| Identity {
        hash: names.bytes.0,
        len,
      });
    if found != Some(*identity) {
      return Err(HgNamedRefused::Mismatch {
        wanted: *identity,
        found,
      });
    }
    revisions.push((*identity, path.as_bytes().to_vec(), entry.node));
  }
  Ok(Resolution::Revisions(revisions))
}

/// Brings back from the node's changeset store every one of `wanted` (an object green `green` named at `version` and
/// the path it read) that Mercurial repository `repository` let go: each path's revision found through the changeset
/// the green names at that version (module docs), brought back by its path and node (`hg_push::bring_back`), and held
/// whole after. Refused typed where a revision is not found, not the object the green named, or not given back.
pub(crate) async fn bring_back(
  repository: RepositoryId,
  (green, version): (VolumeId, u64),
  wanted: &[(Identity, String)],
) -> Result<(), HgNamedRefused> {
  let changeset = crate::state::with_state(|state| changeset_at(state, (green, version)))
    .flatten()
    .ok_or(HgNamedRefused::NoChangeset { green, version })?;
  bring_back_on(&mut ThisShard, repository, changeset, wanted).await
}

/// Brings back every one of `wanted` (an object and the path it is the file at) that Mercurial repository `repository`
/// let go, by the revision changeset `changeset` names for its path ([`resolve_at`]), over the shard's state as
/// `lender` lends it a step at a time: this thread's, or a recovery's (`super::HeldShard`).
pub(crate) async fn bring_back_on<L: ShardLender>(
  lender: &mut L,
  repository: RepositoryId,
  changeset: Node,
  wanted: &[(Identity, String)],
) -> Result<(), HgNamedRefused> {
  let unlent = HgNamedRefused::NoStore { repository };
  for resolution in 1..=RESOLUTIONS {
    // The texts are read from their windows, which live in the shard's chunk store: lent for the read.
    let resolved = lender
      .lend(|state| {
        super::lent_state(state, |state| {
          resolve_at(state, repository, changeset, wanted)
        })
      })
      .ok_or_else(|| unlent.clone())??;
    let revisions = match resolved {
      Resolution::Bring(path, node) => {
        if resolution == RESOLUTIONS {
          return Err(HgNamedRefused::NotBroughtBack {
            path,
            node,
            why: None,
          });
        }
        crate::repository::hg_push::bring_back_on(lender, repository, (path.clone(), node))
          .await
          .map_err(|why| HgNamedRefused::NotBroughtBack {
            path,
            node,
            why: Some(why),
          })?;
        continue;
      }
      Resolution::Revisions(revisions) => revisions,
    };
    for (identity, path, node) in revisions {
      let brought =
        crate::repository::hg_push::bring_back_on(lender, repository, (path.clone(), node)).await;
      let why = match brought {
        Ok(()) if lender.lend(|state| held_whole(state, identity)) == Some(true) => continue,
        Ok(()) => crate::repository::hg_push::BringBackRefused::NotWhole,
        Err(why) => why,
      };
      return Err(HgNamedRefused::NotBroughtBack {
        path,
        node,
        why: Some(why),
      });
    }
    return Ok(());
  }
  // Every resolution either found the revisions or refused at the last, so none ends here.
  Err(unlent)
}

/// Whether the content store holds every window of `identity`'s object now.
pub(crate) fn held_whole(state: &ShardState, identity: Identity) -> bool {
  state.vcs_stores.content().is_some_and(|content| {
    let root = slates_vcs::ContentId(identity.hash);
    let layout = content.object_layout(&root);
    !layout.is_empty() && layout.iter().all(|(window, _)| !content.is_away(window))
  })
}

/// Counted: a Mercurial green's named text a recovery brought back before the green replayed ([`bring_back_named`]).
/// Format: a counter name in the status report.
pub(crate) const RECOVERY_BROUGHT_BACK: &str = "vcs.hg.named_recovery_brought_back";

/// A recovery's pre-pass for Mercurial green `green` (A-887; main 01a20182's ruling of 2026-11-24 02:17, beside
/// `svn_texts::bring_back_named`): every named base its replay will read that the content store does not hold whole,
/// brought back before the green replays, by the revision the changeset its checkpoint or chain names then gives its
/// path ([`resolve_at`]), a group of one changeset a recovery unit. What it brought, for the caller to let go again once
/// the green is replayed. Nothing for a green of no Mercurial repository; a group refused is left to the replay, which
/// fences the green `ContentMissing` as before.
pub(crate) async fn bring_back_named(
  state: &mut ShardState,
  green: VolumeId,
  slices: &mut crate::recovery_slices::RecoverySlices,
) -> Vec<[u8; 32]> {
  let Some(repository) = repository_of(state, green) else {
    return Vec::new();
  };
  let plan = named_bases(state, green);
  let mut brought = Vec::new();
  for (changeset, wanted) in plan {
    let mut lender = super::HeldShard(&mut *state);
    if bring_back_on(&mut lender, repository, changeset, &wanted)
      .await
      .is_ok()
    {
      let count = u64::try_from(wanted.len()).unwrap_or(u64::MAX);
      state.count(RECOVERY_BROUGHT_BACK, count);
      brought.extend(wanted.iter().map(|(identity, _)| identity.hash));
    }
    slices.unit_done().await;
  }
  brought
}

/// Whether Mercurial green `green`'s replay reads a named base the content store does not hold whole ([`named_bases`]),
/// which only its repository's index resolves: what has a recovery rebuild it after the repositories' stores (A-887;
/// main 01a20182's ruling of 2026-11-24 03:00). Decided from its checkpoint and chain alone, before any store is made.
pub(crate) fn needs_store(state: &ShardState, green: VolumeId) -> bool {
  super::hg_made::mercurial_repository_of(state, green).is_some()
    && !named_bases(state, green).is_empty()
}

/// What green `green`'s replay reads of the files it holds by name that the content store does not hold whole, by the
/// changeset that names their revisions: its checkpoint's named files and head attribute, then each chain entry in
/// turn, whose splices that read a base read the file named then (`slates_merge::engine::splice_reads_base`), and
/// whose changes leave files named whole (`Increment::named_whole_files`) or not, and move the head attribute.
fn named_bases(state: &ShardState, green: VolumeId) -> Vec<(Node, Vec<(Identity, String)>)> {
  use slates_merge::chain_entry::Applied;
  let mut named: std::collections::BTreeMap<String, Identity> = std::collections::BTreeMap::new();
  let mut changeset: Option<Node> = None;
  let partition = state.db.partition();
  if let Some((_, checkpointed)) = partition.green_checkpoint(green)
    && let Some(bytes) = crate::merge_service::checkpoint_bytes(state, green, &checkpointed)
    && let Ok(checkpoint) = slates_merge::checkpoint::Checkpoint::decode(&bytes)
  {
    named.extend(
      checkpoint
        .named
        .iter()
        .map(|file| (file.key.clone(), file.identity)),
    );
    changeset = checkpoint
      .xattrs
      .iter()
      .find(|xattr| xattr.key.0 == ROOT && xattr.key.1 == HEAD_ATTRIBUTE)
      .and_then(|xattr| xattr.value.as_deref())
      .and_then(|bytes| Node::try_from(bytes).ok());
  }
  let mut plan: Vec<(Node, Vec<(Identity, String)>)> = Vec::new();
  let mut want = |changeset: Option<Node>, identity: Identity, path: &str| {
    let held = state
      .vcs_stores
      .content()
      .is_some_and(|content| content.holds_whole(&slates_vcs::ContentId(identity.hash)));
    let Some(changeset) = changeset.filter(|_| !held) else {
      return;
    };
    let entry = (identity, path.to_owned());
    match plan.iter_mut().find(|(at, _)| *at == changeset) {
      Some((_, set)) if set.contains(&entry) => {}
      Some((_, set)) => set.push(entry),
      None => plan.push((changeset, vec![entry])),
    }
  };
  for bytes in partition.green_chain(green).iter() {
    if slates_merge::push::DeferredPush::is_record(bytes) {
      continue;
    }
    let Ok((inputs, carried)) = slates_merge::chain_entry::placement(bytes) else {
      continue;
    };
    let increment = match carried {
      Some(carried) => std::borrow::Cow::Borrowed(carried),
      None => match crate::merge_service::increment_of(state, green, &inputs) {
        Some(increment) => increment,
        None => continue,
      },
    };
    let Ok(entry) = slates_merge::chain_entry::decode(bytes, &increment) else {
      continue;
    };
    // The bases it reads, as the files were named before it.
    for change in &entry.applied {
      let read = match change {
        Applied::Spliced(path, ops) if slates_merge::engine::splice_reads_base(ops) => Some(path),
        Applied::Copied { from, .. } => Some(from),
        _ => None,
      };
      if let Some(path) = read
        && let Some(identity) = named.get(path)
      {
        want(changeset, *identity, path);
      }
    }
    // What it leaves named, and the changeset it names.
    for change in &entry.applied {
      match change {
        Applied::Spliced(path, _) | Applied::Created(path, _) | Applied::RemoveContent(path) => {
          named.remove(path);
        }
        Applied::Copied { dst, .. } => {
          named.remove(dst);
        }
        Applied::CopyTree { dst, .. } => {
          let under = format!("{dst}/");
          named.retain(|path, _| path != dst && !path.starts_with(&under));
        }
        Applied::SetXattr(path, name, value) if path == ROOT && name == HEAD_ATTRIBUTE => {
          changeset = Node::try_from(value.as_slice()).ok();
        }
        _ => {}
      }
    }
    named.extend(entry.increment.named_whole_files(&entry.applied));
  }
  plan
}
