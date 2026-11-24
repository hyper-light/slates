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
  /// The green is not here, or no Mercurial repository holds it here.
  NoRepository {
    /// The green.
    green: VolumeId,
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

/// The revisions of `wanted` (each an identity a green named and the path it read) in the changeset green `green`
/// names at `version`, each checked against its identity; or the changeset's or manifest's text to bring back first.
fn resolve(
  state: &ShardState,
  repository: RepositoryId,
  (green, version): (VolumeId, u64),
  wanted: &[(Identity, String)],
) -> Result<Resolution, HgNamedRefused> {
  let changeset = state
    .greens
    .get(&green)
    .and_then(|engine| engine.xattr_at(ROOT, HEAD_ATTRIBUTE, version))
    .and_then(|bytes| Node::try_from(bytes.as_slice()).ok())
    .ok_or(HgNamedRefused::NoChangeset { green, version })?;
  let (content, store) = state
    .vcs_stores
    .mercurial_ref(repository)
    .ok_or(HgNamedRefused::NoRepository { green })?;
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

/// Brings back from the node's changeset store every one of `wanted` (an identity green `green` named at `version` and
/// the path it read) that Mercurial repository `repository` let go: each path's revision found through the changeset
/// the green names at that version (module docs), brought back by its path and node (`hg_push::bring_back`), and held
/// whole after. Refused typed where a revision is not found, not the object the green named, or not given back.
pub(crate) async fn bring_back(
  repository: RepositoryId,
  (green, version): (VolumeId, u64),
  wanted: &[(Identity, String)],
) -> Result<(), HgNamedRefused> {
  for resolution in 1..=RESOLUTIONS {
    // The texts are read from their windows, which live in the shard's chunk store: lent for the read.
    let resolved = crate::state::with_state(|state| {
      super::lent_state(state, |state| {
        resolve(state, repository, (green, version), wanted)
      })
    })
    .ok_or(HgNamedRefused::NoRepository { green })??;
    let revisions = match resolved {
      Resolution::Bring(path, node) => {
        if resolution == RESOLUTIONS {
          return Err(HgNamedRefused::NotBroughtBack {
            path,
            node,
            why: None,
          });
        }
        crate::repository::hg_push::bring_back(repository, (path.clone(), node))
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
      let brought = crate::repository::hg_push::bring_back(repository, (path.clone(), node)).await;
      let why = match brought {
        Ok(()) if crate::state::with_state(|state| held_whole(state, identity)).unwrap_or(false) => {
          continue;
        }
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
  Err(HgNamedRefused::NoChangeset { green, version })
}

/// Whether the content store holds every window of `identity`'s object now.
pub(crate) fn held_whole(state: &ShardState, identity: Identity) -> bool {
  state.vcs_stores.content().is_some_and(|content| {
    let root = slates_vcs::ContentId(identity.hash);
    let layout = content.object_layout(&root);
    !layout.is_empty() && layout.iter().all(|(window, _)| !content.is_away(window))
  })
}
