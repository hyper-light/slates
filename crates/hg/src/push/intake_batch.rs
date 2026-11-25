//! Multi-shard parallel verification and hashing seam for Mercurial push intake (§4.17.3, R2, R8).
//!
//! `Intake::prepare_batch` reads a bounded batch of changegroup revisions from the owner shard
//! without mutating the intake's state (so cancellation mid-scatter leaves nothing half-applied).
//! `verify_batch_item` runs purely stateless SHA-1DC node checks (`node::check`), BLAKE3 content
//! hashing (`slates_vcs::Hashed`), changeset decoding, delta-scoped manifest validation
//! (`manifest::validate_delta`), file-revision splitting, and self-contained file-group delta
//! reconstruction on any shard via `xshard::scatter_compute`. `Intake::apply_batch` then commits
//! the verified results in exact changegroup order on the owner shard with zero re-hashing, zero
//! redundant parsing, and exact refusal and admission-retry precedence.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use slates_vcs::{ContentStore, Hashed};

use super::super::commit::{
  KeptOne, PrecomputedFile, PrecomputedKeep, PrecomputedManifest, keep_one_prepared,
};
use super::super::held::IndexHeld;
use super::super::verify::{Arrived, Held, LogOf, Unverified};
use super::{
  CHANGELOG_PATH, Intake, KeptText, MANIFEST_PATH, Refused, Stage, Taker, audit, log_of,
  written_entries_parts,
};
use crate::changegroup::Revision;
use crate::filerev::FileContents;
use crate::framing::Source;
use crate::index::{IncomingDelta, Log};
use crate::read::to_u64;
use crate::{Admission, NULL_NODE, Node, changeset, delta, filerev, manifest, node};

/// Fixed revision metadata carried across shards for one changegroup revision.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RevMeta {
  /// The revision's node hash.
  pub(crate) node: Node,
  /// First parent node.
  pub(crate) p1: Node,
  /// Second parent node.
  pub(crate) p2: Node,
  /// Delta base node.
  pub(crate) base: Node,
  /// Link changeset node.
  pub(crate) link: Node,
}

/// One self-contained work unit prepared by [`Intake::prepare_batch`] and executed by
/// [`verify_batch_item`] on any shard (R2: zero `Arc`/locks, pure value movement).
pub enum BatchWork {
  /// One reconstructed changelog revision ready for SHA-1DC check, decode, and BLAKE3 hash.
  Changelog(Box<ChangelogWork>),
  /// One reconstructed manifest revision ready for SHA-1DC check, delta validation, and BLAKE3.
  Manifest(Box<ManifestWork>),
  /// One self-contained file group whose deltas, SHA-1DC checks, splits, and BLAKE3 hashes all run
  /// on a worker shard.
  FileGroup(Box<FileGroupWork>),
  /// One reconstructed file revision (for large multi-revision files split across batches).
  FileRev(Box<FileRevWork>),
}

impl BatchWork {
  /// Estimated byte cost used by `xshard::scatter_compute` for LPT load balancing across shards.
  pub fn weight(&self) -> u64 {
    match self {
      BatchWork::Changelog(work) => to_u64(work.text.len()).max(1),
      BatchWork::Manifest(work) => to_u64(work.text.len()).max(1),
      BatchWork::FileGroup(work) => work
        .revisions
        .iter()
        .fold(0u64, |acc, rev| acc.saturating_add(to_u64(rev.delta.len())))
        .max(1),
      BatchWork::FileRev(work) => to_u64(work.text.len()).max(1),
    }
  }
}

/// Worker input for one changelog revision.
pub struct ChangelogWork {
  meta: RevMeta,
  place: Range<u64>,
  text: Vec<u8>,
  base_identity: Option<[u8; 32]>,
  room_left: u64,
  base_rebuilt: bool,
}

/// Worker input for one manifest revision.
pub struct ManifestWork {
  meta: RevMeta,
  place: Range<u64>,
  text: Vec<u8>,
  delta: Vec<u8>,
  is_full_text: bool,
  base_matches_prev: bool,
  prev_for_delta: Option<(Node, Vec<u8>)>,
  base_identity: Option<[u8; 32]>,
  based_later: bool,
  last_on_base: bool,
  base_rebuilt: bool,
}

/// One delta specification inside [`FileGroupWork`].
struct FileDeltaSpec {
  at: usize,
  meta: RevMeta,
  place: Range<u64>,
  delta: Vec<u8>,
  based_later: bool,
  last_on_base: bool,
}

/// Worker input for one file group.
pub struct FileGroupWork {
  file_idx: usize,
  path: Vec<u8>,
  window: usize,
  max_group_bytes: u64,
  room_left: u64,
  seed_base: Option<(Node, Vec<u8>, [u8; 32])>,
  revisions: Vec<FileDeltaSpec>,
}

/// Worker input for one reconstructed file revision.
pub struct FileRevWork {
  file_idx: usize,
  at: usize,
  path: Vec<u8>,
  meta: RevMeta,
  place: Range<u64>,
  text: Vec<u8>,
  base_identity: Option<[u8; 32]>,
  based_later: bool,
  last_on_base: bool,
  keep_text: bool,
  base_rebuilt: bool,
}

/// Verified output produced by [`verify_batch_item`] and committed by [`Intake::apply_batch`].
pub enum BatchDone {
  /// Verified changelog revision.
  Changelog(Box<Result<ChangelogDone, Refused>>),
  /// Verified manifest revision.
  Manifest(Box<Result<ManifestDone, Refused>>),
  /// Verified (complete or budget-sliced) file group.
  FileGroup(Box<FileGroupDone>),
  /// Verified single file revision.
  FileRev(Box<Result<FileRevDone, Refused>>),
}

/// Verified changelog output.
pub struct ChangelogDone {
  meta: RevMeta,
  place: Range<u64>,
  hashed: Hashed<Vec<u8>>,
  manifest: Node,
  branch: Vec<u8>,
  closed: bool,
  base_identity: Option<[u8; 32]>,
  base_rebuilt: bool,
}

/// Verified manifest output.
pub struct ManifestDone {
  meta: RevMeta,
  place: Range<u64>,
  hashed_text: Hashed<Vec<u8>>,
  delta_base: Node,
  delta: Vec<u8>,
  hashed_delta: Option<Hashed<Vec<u8>>>,
  written: Vec<(Vec<u8>, Node)>,
  base_identity: Option<[u8; 32]>,
  based_later: bool,
  last_on_base: bool,
  base_rebuilt: bool,
}

/// One verified revision inside [`FileGroupDone`].
struct FileGroupRevDone {
  at: usize,
  meta: RevMeta,
  place: Range<u64>,
  text_len: usize,
  text_identity: [u8; 32],
  base_identity: Option<[u8; 32]>,
  stored_p1: Node,
  stored_p2: Node,
  hashed_bytes: Hashed<Vec<u8>>,
  hashed_block: Option<Hashed<Vec<u8>>>,
}

/// Resume state handed back when a [`FileGroupWork`] stops early (e.g. for a `> window` revision
/// or per-group resident byte budget).
struct FileGroupResume {
  stopped_at: usize,
  last: Option<(Node, Vec<u8>, [u8; 32])>,
  last_later: bool,
  cached: Vec<(Node, Vec<u8>, [u8; 32])>,
}

/// Verified file group output.
pub struct FileGroupDone {
  file_idx: usize,
  path: Vec<u8>,
  revs: Vec<FileGroupRevDone>,
  resume: Option<FileGroupResume>,
  fault: Option<Refused>,
}

/// Verified single file revision output.
pub struct FileRevDone {
  file_idx: usize,
  at: usize,
  meta: RevMeta,
  place: Range<u64>,
  text_len: usize,
  text_identity: [u8; 32],
  kept_text: Option<Vec<u8>>,
  base_identity: Option<[u8; 32]>,
  stored_p1: Node,
  stored_p2: Node,
  hashed_bytes: Hashed<Vec<u8>>,
  hashed_block: Option<Hashed<Vec<u8>>>,
  based_later: bool,
  last_on_base: bool,
  base_rebuilt: bool,
}

/// Pure stateless worker function: verifies node hash (`sha1_checked`), decodes/validates format,
/// and computes BLAKE3 content identities (`Hashed::of`) for one [`BatchWork`] item.
pub fn verify_batch_item(work: BatchWork) -> BatchDone {
  match work {
    BatchWork::Changelog(work) => BatchDone::Changelog(Box::new(verify_changelog(*work))),
    BatchWork::Manifest(work) => BatchDone::Manifest(Box::new(verify_manifest(*work))),
    BatchWork::FileGroup(work) => BatchDone::FileGroup(Box::new(verify_file_group(*work))),
    BatchWork::FileRev(work) => BatchDone::FileRev(Box::new(verify_file_rev(*work))),
  }
}

/// Verifies one changelog revision on a worker shard.
fn verify_changelog(work: ChangelogWork) -> Result<ChangelogDone, Refused> {
  node::check(&work.meta.node, &work.meta.p1, &work.meta.p2, &work.text).map_err(|fault| {
    Unverified::Node {
      log: LogOf::Changelog,
      node: work.meta.node,
      fault,
    }
  })?;
  let read = changeset::decode(&work.text).map_err(|fault| Unverified::Changeset {
    node: work.meta.node,
    fault,
  })?;
  let manifest = read
    .manifest_node()
    .map_err(|fault| Unverified::Changeset {
      node: work.meta.node,
      fault,
    })?;
  let mut room = Admission::new(work.room_left);
  let (branch, closed) =
    crate::import::branch_of(&read, &mut room).map_err(|fault| Unverified::Changeset {
      node: work.meta.node,
      fault,
    })?;
  let hashed = Hashed::of(work.text);
  Ok(ChangelogDone {
    meta: work.meta,
    place: work.place,
    hashed,
    manifest,
    branch,
    closed,
    base_identity: work.base_identity,
    base_rebuilt: work.base_rebuilt,
  })
}

/// Verifies one manifest revision on a worker shard.
fn verify_manifest(work: ManifestWork) -> Result<ManifestDone, Refused> {
  node::check(&work.meta.node, &work.meta.p1, &work.meta.p2, &work.text).map_err(|fault| {
    Unverified::Node {
      log: LogOf::Manifests,
      node: work.meta.node,
      fault,
    }
  })?;
  let checked = if work.meta.base == NULL_NODE || work.is_full_text {
    manifest::decode(&work.text).map(|_| ())
  } else {
    manifest::validate_delta(&work.text, &work.delta)
  };
  checked.map_err(|fault| Unverified::Manifest {
    node: work.meta.node,
    fault,
  })?;
  let written = written_entries_parts(&work.text, work.meta.base, work.is_full_text, &work.delta)
    .map_err(|fault| Unverified::Delta {
    log: LogOf::Manifests,
    node: work.meta.node,
    fault,
  })?;
  for (path, _) in &written {
    audit(path)?;
  }
  let hashed_text = Hashed::of(work.text);
  let (delta_base, delta, hashed_delta) = encode_manifest_delta(
    work.meta.base,
    work.base_matches_prev,
    work.delta,
    work.prev_for_delta,
    hashed_text.bytes(),
  );
  Ok(ManifestDone {
    meta: work.meta,
    place: work.place,
    hashed_text,
    delta_base,
    delta,
    hashed_delta,
    written,
    base_identity: work.base_identity,
    based_later: work.based_later,
    last_on_base: work.last_on_base,
    base_rebuilt: work.base_rebuilt,
  })
}

/// Computes and hashes the storage delta for a manifest on a worker shard: reuses the verified
/// incoming changegroup delta when its base is the previous manifest revision, or runs
/// `delta::encode_lines` against `prev_for_delta` in parallel on the worker shard otherwise.
fn encode_manifest_delta(
  incoming_base: Node,
  base_matches_prev: bool,
  incoming_delta: Vec<u8>,
  prev_for_delta: Option<(Node, Vec<u8>)>,
  text: &[u8],
) -> (Node, Vec<u8>, Option<Hashed<Vec<u8>>>) {
  if base_matches_prev {
    return (incoming_base, Vec::new(), Some(Hashed::of(incoming_delta)));
  }
  if let Some((prev_node, prev_text)) = prev_for_delta {
    let mut encoded = Vec::new();
    if delta::encode_lines(&prev_text, text, &mut encoded).is_ok() {
      return (prev_node, Vec::new(), Some(Hashed::of(encoded)));
    }
  }
  (incoming_base, incoming_delta, None)
}

/// Pre-split and BLAKE3-hashed file revision parts returned by [`split_and_hash_file`].
struct SplitHashedFile {
  stored_p1: Node,
  stored_p2: Node,
  hashed_bytes: Hashed<Vec<u8>>,
  hashed_block: Option<Hashed<Vec<u8>>>,
  text_identity: [u8; 32],
  kept_text: Option<Vec<u8>>,
}

/// Verifies one reconstructed file revision on a worker shard.
fn verify_file_rev(work: FileRevWork) -> Result<FileRevDone, Refused> {
  node::check(&work.meta.node, &work.meta.p1, &work.meta.p2, &work.text).map_err(|fault| {
    Unverified::Node {
      log: LogOf::File(work.path.clone()),
      node: work.meta.node,
      fault,
    }
  })?;
  let text_len = work.text.len();
  let split = split_and_hash_file(&work.path, work.meta, work.text, work.keep_text)?;
  Ok(FileRevDone {
    file_idx: work.file_idx,
    at: work.at,
    meta: work.meta,
    place: work.place,
    text_len,
    text_identity: split.text_identity,
    kept_text: split.kept_text,
    base_identity: work.base_identity,
    stored_p1: split.stored_p1,
    stored_p2: split.stored_p2,
    hashed_bytes: split.hashed_bytes,
    hashed_block: split.hashed_block,
    based_later: work.based_later,
    last_on_base: work.last_on_base,
    base_rebuilt: work.base_rebuilt,
  })
}

/// Splits a file revision's text into stored parents, optional metadata block, and body, computing
/// BLAKE3 identities once without double-hashing metadata-free revisions.
fn split_and_hash_file(
  path: &[u8],
  meta: RevMeta,
  text: Vec<u8>,
  keep_text: bool,
) -> Result<SplitHashedFile, Refused> {
  let split = filerev::split(&text).map_err(|fault| Unverified::FileRevision {
    path: path.to_vec(),
    node: meta.node,
    fault,
  })?;
  let (stored_p1, stored_p2) = split.parents_as_stored(meta.p1, meta.p2);
  if let Some(block) = split.metadata {
    let hashed_block = Hashed::of(block.to_vec());
    let hashed_bytes = Hashed::of(split.bytes.to_vec());
    let text_identity = *blake3::hash(&text).as_bytes();
    let kept_text = keep_text.then_some(text);
    return Ok(SplitHashedFile {
      stored_p1,
      stored_p2,
      hashed_bytes,
      hashed_block: Some(hashed_block),
      text_identity,
      kept_text,
    });
  }
  if keep_text {
    let hashed_bytes = Hashed::of(text.clone());
    let text_identity = hashed_bytes.identity().0;
    return Ok(SplitHashedFile {
      stored_p1,
      stored_p2,
      hashed_bytes,
      hashed_block: None,
      text_identity,
      kept_text: Some(text),
    });
  }
  let hashed_bytes = Hashed::of(text);
  let text_identity = hashed_bytes.identity().0;
  Ok(SplitHashedFile {
    stored_p1,
    stored_p2,
    hashed_bytes,
    hashed_block: None,
    text_identity,
    kept_text: None,
  })
}

/// Reconstructs, verifies, splits, and hashes every revision of a self-contained file group on a
/// worker shard, stopping cleanly if a revision exceeds `window` or the group hits `max_group_bytes`.
fn verify_file_group(work: FileGroupWork) -> FileGroupDone {
  let mut revs = Vec::with_capacity(work.revisions.len());
  let mut last: Option<(Node, Vec<u8>, [u8; 32])> = work.seed_base;
  let mut last_later = false;
  let mut cache: BTreeMap<Node, (Vec<u8>, [u8; 32])> = BTreeMap::new();
  let mut accumulated: u64 = 0;
  let total = work.revisions.len();
  for idx in 0..total {
    let Some(spec) = work.revisions.get(idx) else {
      break;
    };
    let next_base = work
      .revisions
      .get(idx.saturating_add(1))
      .map(|next| next.meta.base);
    let keep_for_later = spec.based_later || next_base == Some(spec.meta.node);
    let (base_slice, base_identity) = match find_local_base(spec.meta.base, &last, &cache) {
      Some(found) => found,
      None => {
        return FileGroupDone {
          file_idx: work.file_idx,
          path: work.path,
          revs,
          resume: Some(make_resume(spec.at, last, last_later, cache)),
          fault: None,
        };
      }
    };
    let mut room = Admission::new(work.room_left);
    let text = match delta::apply(base_slice, &spec.delta, &mut room) {
      Ok(text) => text,
      Err(fault) => {
        let log = LogOf::File(work.path.clone());
        return FileGroupDone {
          file_idx: work.file_idx,
          path: work.path,
          revs,
          resume: None,
          fault: Some(
            Unverified::Delta {
              log,
              node: spec.meta.node,
              fault,
            }
            .into(),
          ),
        };
      }
    };
    let text_u64 = to_u64(text.len());
    if text.len() > work.window
      || (!revs.is_empty() && accumulated.saturating_add(text_u64) > work.max_group_bytes)
    {
      return FileGroupDone {
        file_idx: work.file_idx,
        path: work.path,
        revs,
        resume: Some(make_resume(spec.at, last, last_later, cache)),
        fault: None,
      };
    }
    let place = if base_identity.is_none() {
      match delta::whole_text_start(&spec.delta) {
        Some(start) => spec.place.start.saturating_add(to_u64(start))..spec.place.end,
        None => {
          let log = LogOf::File(work.path.clone());
          return FileGroupDone {
            file_idx: work.file_idx,
            path: work.path,
            revs,
            resume: None,
            fault: Some(
              Unverified::NotWhole {
                log,
                node: spec.meta.node,
              }
              .into(),
            ),
          };
        }
      }
    } else {
      spec.place.clone()
    };
    if let Err(fault) = node::check(&spec.meta.node, &spec.meta.p1, &spec.meta.p2, &text) {
      let log = LogOf::File(work.path.clone());
      return FileGroupDone {
        file_idx: work.file_idx,
        path: work.path,
        revs,
        resume: None,
        fault: Some(
          Unverified::Node {
            log,
            node: spec.meta.node,
            fault,
          }
          .into(),
        ),
      };
    }
    let text_len = text.len();
    let split = match split_and_hash_file(&work.path, spec.meta, text, keep_for_later) {
      Ok(split) => split,
      Err(fault) => {
        return FileGroupDone {
          file_idx: work.file_idx,
          path: work.path,
          revs,
          resume: None,
          fault: Some(fault),
        };
      }
    };
    if spec.last_on_base {
      cache.remove(&spec.meta.base);
    }
    if let Some((prev_node, prev_text, prev_id)) = last.take()
      && last_later
    {
      cache.insert(prev_node, (prev_text, prev_id));
    }
    last = split
      .kept_text
      .map(|kept| (spec.meta.node, kept, split.text_identity));
    last_later = spec.based_later;
    accumulated = accumulated
      .saturating_add(to_u64(split.hashed_bytes.bytes().len()))
      .saturating_add(
        split
          .hashed_block
          .as_ref()
          .map_or(0, |b| to_u64(b.bytes().len())),
      );
    revs.push(FileGroupRevDone {
      at: spec.at,
      meta: spec.meta,
      place,
      text_len,
      text_identity: split.text_identity,
      base_identity,
      stored_p1: split.stored_p1,
      stored_p2: split.stored_p2,
      hashed_bytes: split.hashed_bytes,
      hashed_block: split.hashed_block,
    });
  }
  FileGroupDone {
    file_idx: work.file_idx,
    path: work.path,
    revs,
    resume: None,
    fault: None,
  }
}

/// Builds a [`FileGroupResume`] snapshot when a file group stops early on a worker shard.
fn make_resume(
  stopped_at: usize,
  last: Option<(Node, Vec<u8>, [u8; 32])>,
  last_later: bool,
  cache: BTreeMap<Node, (Vec<u8>, [u8; 32])>,
) -> FileGroupResume {
  let cached = cache
    .into_iter()
    .map(|(node, (text, id))| (node, text, id))
    .collect();
  FileGroupResume {
    stopped_at,
    last,
    last_later,
    cached,
  }
}

/// Looks up `base` inside a worker's local file group state.
fn find_local_base<'a>(
  base: Node,
  last: &'a Option<(Node, Vec<u8>, [u8; 32])>,
  cache: &'a BTreeMap<Node, (Vec<u8>, [u8; 32])>,
) -> Option<(&'a [u8], Option<[u8; 32]>)> {
  if base == NULL_NODE {
    return Some((&[], None));
  }
  if let Some((node, text, id)) = last
    && *node == base
  {
    return Some((text.as_slice(), Some(*id)));
  }
  cache
    .get(&base)
    .map(|(text, id)| (text.as_slice(), Some(*id)))
}

/// Resolved delta base text and metadata returned by [`Intake::lookup_linear_base`].
struct LinearBase<'a> {
  bytes: Cow<'a, [u8]>,
  identity: Option<[u8; 32]>,
  rebuilt: bool,
}

/// Adjusts a changegroup revision's `place` when its base is `NULL_NODE` (raw text past header).
fn whole_place(base: Node, delta: &[u8], place: Range<u64>) -> Option<Range<u64>> {
  if base != NULL_NODE {
    return Some(place);
  }
  let start = delta::whole_text_start(delta)?;
  Some(place.start.saturating_add(to_u64(start))..place.end)
}

impl Intake {
  /// Prepares a read-only batch of self-contained [`BatchWork`] items for parallel multi-shard
  /// verification, or `None` when the current step must run serially (pending large revision,
  /// active LSM merge, cut segment waiting to land, `StoreAsk`, or final presence checks).
  pub fn prepare_batch<S: Source + ?Sized>(
    &self,
    payload: &S,
    store: &ContentStore,
    admission: &Admission,
    (max_bytes, max_items): (u64, usize),
  ) -> Option<Vec<BatchWork>> {
    if self.taker.pending.is_some()
      || self.taker.merging.is_some()
      || self.has_segment()
      || max_bytes == 0
      || max_items == 0
    {
      return None;
    }
    match self.taker.stage {
      Stage::Changelog(at) => {
        self.prepare_changelog_batch(payload, store, admission, at, (max_bytes, max_items))
      }
      Stage::Manifests(at) => {
        self.prepare_manifest_batch(payload, store, admission, at, (max_bytes, max_items))
      }
      Stage::Files { file, at } => {
        if at == 0
          && self.taker.path.is_none()
          && let Some(groups) =
            self.prepare_file_groups_batch(payload, store, admission, file, (max_bytes, max_items))
        {
          return Some(groups);
        }
        self.prepare_file_revs_batch(
          payload,
          store,
          admission,
          (file, at),
          (max_bytes, max_items),
        )
      }
      Stage::ManifestsPresent(_) | Stage::FilesPresent | Stage::Done => None,
    }
  }

  /// Prepares a batch of changelog revisions for parallel verification.
  fn prepare_changelog_batch<S: Source + ?Sized>(
    &self,
    payload: &S,
    store: &ContentStore,
    admission: &Admission,
    start: usize,
    (max_bytes, max_items): (u64, usize),
  ) -> Option<Vec<BatchWork>> {
    let window = store.window();
    let mut items: Vec<ChangelogWork> = Vec::new();
    let mut batch_idx: BTreeMap<Node, usize> = BTreeMap::new();
    let mut total_bytes: u64 = 0;
    let mut at = start;
    while items.len() < max_items {
      let Ok(Some((revision, place))) = self.frame.placed(payload, &self.frame.0.changelog, at)
      else {
        break;
      };
      if !self.can_batch_linear(Log::Changelog, &revision, &batch_idx, store) {
        break;
      }
      let Some(base) =
        self.lookup_linear_base(Log::Changelog, &revision, &items, &batch_idx, store, |w| {
          w.text.as_slice()
        })
      else {
        break;
      };
      let mut trial = *admission;
      let Ok(text) = delta::apply(base.bytes.as_ref(), revision.data, &mut trial) else {
        break;
      };
      if text.len() > window {
        break;
      }
      let Some(place) = whole_place(revision.base, revision.data, place) else {
        break;
      };
      let text_u64 = to_u64(text.len());
      if !items.is_empty() && total_bytes.saturating_add(text_u64) > max_bytes {
        break;
      }
      let idx_in_batch = items.len();
      batch_idx.insert(revision.node, idx_in_batch);
      total_bytes = total_bytes.saturating_add(text_u64);
      items.push(ChangelogWork {
        meta: rev_meta(&revision),
        place,
        text,
        base_identity: base.identity,
        room_left: admission.left(),
        base_rebuilt: base.rebuilt,
      });
      at = at.saturating_add(1);
    }
    (items.len() >= 2).then(|| {
      items
        .into_iter()
        .map(|work| BatchWork::Changelog(Box::new(work)))
        .collect()
    })
  }

  /// Prepares a batch of manifest revisions for parallel verification.
  fn prepare_manifest_batch<S: Source + ?Sized>(
    &self,
    payload: &S,
    store: &ContentStore,
    admission: &Admission,
    start: usize,
    (max_bytes, max_items): (u64, usize),
  ) -> Option<Vec<BatchWork>> {
    let initial_prev = self.initial_manifest_prev(store)?;
    let mut prev_node = initial_prev.as_ref().map(|(node, _)| *node);
    let window = store.window();
    let mut items: Vec<ManifestWork> = Vec::new();
    let mut batch_idx: BTreeMap<Node, usize> = BTreeMap::new();
    let mut total_bytes: u64 = 0;
    let mut at = start;
    while items.len() < max_items {
      let Ok(Some((revision, place))) = self.frame.placed(payload, &self.frame.0.manifests, at)
      else {
        break;
      };
      if !self.can_batch_linear(Log::Manifest, &revision, &batch_idx, store) {
        break;
      }
      let Some(base) =
        self.lookup_linear_base(Log::Manifest, &revision, &items, &batch_idx, store, |w| {
          w.text.as_slice()
        })
      else {
        break;
      };
      let mut trial = *admission;
      let Ok(text) = delta::apply(base.bytes.as_ref(), revision.data, &mut trial) else {
        break;
      };
      if text.len() > window {
        break;
      }
      let Some(place) = whole_place(revision.base, revision.data, place) else {
        break;
      };
      let text_u64 = to_u64(text.len());
      if !items.is_empty() && total_bytes.saturating_add(text_u64) > max_bytes {
        break;
      }
      let base_matches_prev = prev_node.is_some_and(|prev| prev == revision.base);
      let prev_for_delta = if !base_matches_prev {
        prev_node.and_then(|pnode| {
          let bytes = items
            .last()
            .map(|w| w.text.as_slice())
            .or_else(|| initial_prev.as_ref().map(|(_, text)| text.as_ref()))?;
          Some((pnode, bytes.to_vec()))
        })
      } else {
        None
      };
      let idx_in_batch = items.len();
      batch_idx.insert(revision.node, idx_in_batch);
      prev_node = Some(revision.node);
      total_bytes = total_bytes.saturating_add(text_u64);
      items.push(ManifestWork {
        meta: rev_meta(&revision),
        place,
        text,
        delta: revision.data.to_vec(),
        is_full_text: revision.is_full_text(),
        base_matches_prev,
        prev_for_delta,
        base_identity: base.identity,
        based_later: self.frame.based_later(&self.frame.0.manifests, at),
        last_on_base: self.frame.last_on_base(&self.frame.0.manifests, at),
        base_rebuilt: base.rebuilt,
      });
      at = at.saturating_add(1);
    }
    (items.len() >= 2).then(|| {
      items
        .into_iter()
        .map(|work| BatchWork::Manifest(Box::new(work)))
        .collect()
    })
  }

  /// Returns the immediately preceding manifest's `(node, text)` when resident in RAM (`self.last`
  /// or `fork`), `Some(None)` if the manifest log is empty, or `None` if a `StoreAsk` is needed.
  fn initial_manifest_prev<'a>(
    &'a self,
    store: &ContentStore,
  ) -> Option<Option<(Node, Cow<'a, [u8]>)>> {
    if let Some((node, text)) = &self.taker.last {
      return Some(Some((*node, Cow::Borrowed(text.as_slice()))));
    }
    let Some(prev_rev) = self.taker.fork.len(store, Log::Manifest).checked_sub(1) else {
      return Some(None);
    };
    let row = self.taker.fork.revision(store, Log::Manifest, prev_rev)?;
    let text = self
      .taker
      .fork
      .revision_text(store, Log::Manifest, prev_rev)?;
    Some(Some((row.node, Cow::Owned(text))))
  }

  /// Prepares consecutive self-contained file groups for parallel delta-apply + SHA-1DC + BLAKE3.
  fn prepare_file_groups_batch<S: Source + ?Sized>(
    &self,
    payload: &S,
    store: &ContentStore,
    admission: &Admission,
    start_file: usize,
    (max_bytes, max_items): (u64, usize),
  ) -> Option<Vec<BatchWork>> {
    let window = store.window();
    let mut groups: Vec<BatchWork> = Vec::new();
    let mut raw_bytes: u64 = 0;
    let mut file = start_file;
    while groups.len() < max_items {
      let Some((name, revisions)) = self.frame.0.files.get(file).cloned() else {
        break;
      };
      let Ok(path) = payload.bytes(name) else {
        break;
      };
      if audit(path).is_err() || revisions.is_empty() {
        break;
      }
      let Some((work, group_raw)) = self.build_file_group_work(
        payload,
        store,
        admission.left(),
        (file, path, &revisions),
        (window, max_bytes),
      ) else {
        break;
      };
      if !groups.is_empty() && raw_bytes.saturating_add(group_raw) > max_bytes {
        break;
      }
      raw_bytes = raw_bytes.saturating_add(group_raw);
      groups.push(BatchWork::FileGroup(Box::new(work)));
      file = file.saturating_add(1);
    }
    if groups.is_empty() {
      return None;
    }
    let single_rev_only = groups.len() == 1
      && matches!(
        groups.first(),
        Some(BatchWork::FileGroup(group)) if group.revisions.len() < 2
      );
    (!single_rev_only).then_some(groups)
  }

  /// Packages one file group into [`FileGroupWork`] if every revision in it is new, well-linked,
  /// and resolves cleanly within the group (or against one resident fork base).
  fn build_file_group_work<S: Source + ?Sized>(
    &self,
    payload: &S,
    store: &ContentStore,
    room_left: u64,
    (file_idx, path, revisions): (usize, &[u8], &Range<usize>),
    (window, max_group_bytes): (usize, u64),
  ) -> Option<(FileGroupWork, u64)> {
    let log = Log::File(path);
    let mut seen: BTreeSet<Node> = BTreeSet::new();
    let mut specs: Vec<FileDeltaSpec> = Vec::with_capacity(revisions.len());
    let mut seed_base: Option<(Node, Vec<u8>, [u8; 32])> = None;
    let mut raw_bytes: u64 = 0;
    for at in 0..revisions.len() {
      let (revision, place) = self.frame.placed(payload, revisions, at).ok()??;
      if revision.flags != 0
        || seen.contains(&revision.node)
        || self.taker.fork.rev(store, log, &revision.node).is_some()
        || self
          .taker
          .fork
          .rev(store, Log::Changelog, &revision.link)
          .is_none()
      {
        return None;
      }
      for parent in [revision.p1, revision.p2] {
        if parent != NULL_NODE
          && !seen.contains(&parent)
          && self.taker.fork.rev(store, log, &parent).is_none()
        {
          return None;
        }
      }
      if revision.base != NULL_NODE && !seen.contains(&revision.base) {
        if let Some((seed_node, _, _)) = &seed_base {
          if *seed_node != revision.base {
            return None;
          }
        } else {
          let held = IndexHeld::new(&self.taker.fork, store);
          let text = held.file_text(path, &revision.base)?.into_owned();
          let id = *blake3::hash(&text).as_bytes();
          seed_base = Some((revision.base, text, id));
        }
      }
      seen.insert(revision.node);
      raw_bytes = raw_bytes.saturating_add(to_u64(revision.data.len()));
      specs.push(FileDeltaSpec {
        at,
        meta: rev_meta(&revision),
        place,
        delta: revision.data.to_vec(),
        based_later: self.frame.based_later(revisions, at),
        last_on_base: self.frame.last_on_base(revisions, at),
      });
    }
    Some((
      FileGroupWork {
        file_idx,
        path: path.to_vec(),
        window,
        max_group_bytes,
        room_left,
        seed_base,
        revisions: specs,
      },
      raw_bytes,
    ))
  }

  /// Prepares a batch of revisions within the currently open file group (`Stage::Files { file, at }`
  /// with `self.taker.path.is_some()`), enabling multi-shard verification of huge single files.
  fn prepare_file_revs_batch<S: Source + ?Sized>(
    &self,
    payload: &S,
    store: &ContentStore,
    admission: &Admission,
    (file_idx, start_at): (usize, usize),
    (max_bytes, max_items): (u64, usize),
  ) -> Option<Vec<BatchWork>> {
    let path = self.taker.path.as_deref()?;
    let (_, revisions) = self.frame.0.files.get(file_idx)?;
    let log = Log::File(path);
    let window = store.window();
    let mut items: Vec<FileRevWork> = Vec::new();
    let mut batch_idx: BTreeMap<Node, usize> = BTreeMap::new();
    let mut total_bytes: u64 = 0;
    let mut at = start_at;
    while items.len() < max_items {
      let Ok(Some((revision, place))) = self.frame.placed(payload, revisions, at) else {
        break;
      };
      if !self.can_batch_linear(log, &revision, &batch_idx, store) {
        break;
      }
      let Some(base) = self.lookup_linear_base(log, &revision, &items, &batch_idx, store, |w| {
        w.text.as_slice()
      }) else {
        break;
      };
      let mut trial = *admission;
      let Ok(text) = delta::apply(base.bytes.as_ref(), revision.data, &mut trial) else {
        break;
      };
      if text.len() > window {
        break;
      }
      let Some(place) = whole_place(revision.base, revision.data, place) else {
        break;
      };
      let text_u64 = to_u64(text.len());
      if !items.is_empty() && total_bytes.saturating_add(text_u64) > max_bytes {
        break;
      }
      let based_later = self.frame.based_later(revisions, at);
      let idx_in_batch = items.len();
      batch_idx.insert(revision.node, idx_in_batch);
      total_bytes = total_bytes.saturating_add(text_u64);
      items.push(FileRevWork {
        file_idx,
        at,
        path: path.to_vec(),
        meta: rev_meta(&revision),
        place,
        text,
        base_identity: base.identity,
        based_later,
        last_on_base: self.frame.last_on_base(revisions, at),
        keep_text: true,
        base_rebuilt: base.rebuilt,
      });
      at = at.saturating_add(1);
    }
    // Only the last item or items marked `based_later` need their text returned to shard 0.
    let last_idx = items.len().saturating_sub(1);
    for (idx, item) in items.iter_mut().enumerate() {
      item.keep_text = item.based_later || idx == last_idx;
    }
    (items.len() >= 2).then(|| {
      items
        .into_iter()
        .map(|work| BatchWork::FileRev(Box::new(work)))
        .collect()
    })
  }

  /// Checks fast-path eligibility for one revision in a linear batch (`Changelog`, `Manifest`, or
  /// `FileRev`): new, zero flags, known parents, and (for non-changelog) known link.
  fn can_batch_linear(
    &self,
    log: Log<'_>,
    revision: &Revision<'_>,
    batch_idx: &BTreeMap<Node, usize>,
    store: &ContentStore,
  ) -> bool {
    if revision.flags != 0
      || batch_idx.contains_key(&revision.node)
      || self
        .taker
        .seen_revision(log, &revision.node, store)
        .is_some()
    {
      return false;
    }
    if log != Log::Changelog
      && self
        .taker
        .fork
        .rev(store, Log::Changelog, &revision.link)
        .is_none()
    {
      return false;
    }
    [revision.p1, revision.p2].into_iter().all(|parent| {
      parent == NULL_NODE
        || batch_idx.contains_key(&parent)
        || self.taker.fork.rev(store, log, &parent).is_some()
    })
  }

  /// Resolves `revision.base` from the current in-progress batch, `self.taker.last`,
  /// `self.taker.cache`, or the resident fork without allocating when already in RAM.
  fn lookup_linear_base<'a, T>(
    &'a self,
    log: Log<'_>,
    revision: &Revision<'_>,
    items: &'a [T],
    batch_idx: &BTreeMap<Node, usize>,
    store: &'a ContentStore,
    text_of: impl Fn(&'a T) -> &'a [u8],
  ) -> Option<LinearBase<'a>> {
    if revision.base == NULL_NODE {
      return Some(LinearBase {
        bytes: Cow::Borrowed(&[]),
        identity: None,
        rebuilt: false,
      });
    }
    if let Some(&idx) = batch_idx.get(&revision.base) {
      let item = items.get(idx)?;
      return Some(LinearBase {
        bytes: Cow::Borrowed(text_of(item)),
        identity: None,
        rebuilt: false,
      });
    }
    if let Some((node, text)) = &self.taker.last
      && *node == revision.base
    {
      let id = self
        .taker
        .last_identity
        .filter(|(n, _)| *n == revision.base)
        .map(|(_, id)| id)
        .unwrap_or_else(|| *blake3::hash(text).as_bytes());
      return Some(LinearBase {
        bytes: Cow::Borrowed(text.as_slice()),
        identity: Some(id),
        rebuilt: false,
      });
    }
    if matches!(log, Log::File(_) | Log::Manifest)
      && let Some(text) = self.taker.cache.held.get(&revision.base)
    {
      let id = self
        .taker
        .cache
        .identity(&revision.base)
        .unwrap_or_else(|| *blake3::hash(text).as_bytes());
      return Some(LinearBase {
        bytes: Cow::Borrowed(text.as_slice()),
        identity: Some(id),
        rebuilt: false,
      });
    }
    let held = IndexHeld::new(&self.taker.fork, store);
    let text = match log {
      Log::Changelog => held.changeset_text(&revision.base),
      Log::Manifest => held.manifest_text(&revision.base),
      Log::File(path) => held.file_text(path, &revision.base),
    }?;
    let owned = text.into_owned();
    let id = *blake3::hash(&owned).as_bytes();
    Some(LinearBase {
      bytes: Cow::Owned(owned),
      identity: Some(id),
      rebuilt: true,
    })
  }

  /// Commits a batch of verified [`BatchDone`] results in exact changegroup order on the owner
  /// shard. Refusal or `PastAdmission` at item `k` stops cleanly after committing `0..k`, leaving
  /// `self.taker.stage` positioned exactly at `k`.
  pub fn apply_batch(
    &mut self,
    store: &mut ContentStore,
    admission: &mut Admission,
    done: Vec<BatchDone>,
  ) -> Result<(), Refused> {
    let mut batch_identities: BTreeMap<Node, [u8; 32]> = BTreeMap::new();
    for item in done {
      match item {
        BatchDone::Changelog(res) => {
          self
            .taker
            .apply_changelog_done((*res)?, &mut batch_identities, (store, admission))?;
        }
        BatchDone::Manifest(res) => {
          self
            .taker
            .apply_manifest_done((*res)?, &mut batch_identities, (store, admission))?;
        }
        BatchDone::FileGroup(group) => {
          if !self
            .taker
            .apply_file_group_done(*group, (store, admission))?
          {
            break;
          }
        }
        BatchDone::FileRev(res) => {
          self
            .taker
            .apply_file_rev_done((*res)?, &mut batch_identities, (store, admission))?;
        }
      }
      if self.taker.merge_step(store)?.is_some() {
        // Keep LSM merge work bounded between items; finish any remaining steps before next batch.
      }
    }
    Ok(())
  }
}

impl Taker {
  /// Commits one worker-verified changelog revision in stream order.
  fn apply_changelog_done(
    &mut self,
    done: ChangelogDone,
    batch_identities: &mut BTreeMap<Node, [u8; 32]>,
    (store, admission): (&mut ContentStore, &mut Admission),
  ) -> Result<(), Refused> {
    let mut trial = *admission;
    trial.charge_items::<(Node, Node)>(2)?;
    let next_rev = self
      .next_rev
      .unwrap_or_else(|| self.fork.len(store, Log::Changelog));
    let logged = self.logged_of(Log::Changelog, next_rev, &mut trial)?;
    trial.charge_items::<Node>(1)?;
    trial.charge_items::<KeptText>(1)?;
    trial.charge(to_u64(CHANGELOG_PATH.len()))?;
    let base = resolve_base_identity(done.meta.base, done.base_identity, batch_identities);
    let identity = done.hashed.identity().0;
    let text_len = done.hashed.bytes().len();
    let arrived = Arrived {
      node: done.meta.node,
      p1: done.meta.p1,
      p2: done.meta.p2,
      link: done.meta.link,
      text: Vec::new(),
    };
    let mut room = *admission;
    let kept_one = keep_one_prepared(
      &mut self.fork,
      store,
      Log::Changelog,
      (&arrived, None),
      (
        None,
        Some(next_rev),
        Some(PrecomputedKeep::Changeset {
          hashed: done.hashed.as_ref(),
          manifest: done.manifest,
          branch: &done.branch,
          closed: done.closed,
        }),
      ),
      &mut room,
    )?;
    let text = done.hashed.into_bytes();
    *admission = trial;
    self.next_rev = Some(kept_one.map_or(next_rev, |info| info.rev).saturating_add(1));
    self.manifest_of.push((done.meta.node, done.manifest));
    self.intaken.changesets.push(done.meta.node);
    self.note_logged(logged);
    self.brought.insert(done.meta.node);
    self.intaken.kept = self.intaken.kept.saturating_add(1);
    self.intaken.bytes = self.intaken.bytes.saturating_add(to_u64(text_len));
    self.intaken.longest_step_bytes = self.intaken.longest_step_bytes.max(to_u64(text_len));
    if done.base_rebuilt {
      self.intaken.rebuilt = self.intaken.rebuilt.saturating_add(1);
    }
    self.record_kept_text(
      Log::Changelog,
      CHANGELOG_PATH,
      done.meta.node,
      (identity, to_u64(text_len), base, done.place),
      (kept_one, next_rev),
    );
    batch_identities.insert(done.meta.node, identity);
    self.last_identity = Some((done.meta.node, identity));
    if let Some((_, old)) = self.last.replace((done.meta.node, text)) {
      self.spares.give(old);
    }
    if let Stage::Changelog(at) = self.stage {
      self.stage = Stage::Changelog(at.saturating_add(1));
    }
    Ok(())
  }

  /// Commits one worker-verified manifest revision in stream order.
  fn apply_manifest_done(
    &mut self,
    done: ManifestDone,
    batch_identities: &mut BTreeMap<Node, [u8; 32]>,
    (store, admission): (&mut ContentStore, &mut Admission),
  ) -> Result<(), Refused> {
    let mut trial = *admission;
    let awaited = self.filter_awaited_written(done.written, (store, &mut trial))?;
    let next_rev = self
      .next_rev
      .unwrap_or_else(|| self.fork.len(store, Log::Manifest));
    let logged = self.logged_of(Log::Manifest, next_rev, &mut trial)?;
    trial.charge_items::<Node>(1)?;
    trial.charge_items::<KeptText>(1)?;
    trial.charge(to_u64(MANIFEST_PATH.len()))?;
    let base = resolve_base_identity(done.meta.base, done.base_identity, batch_identities);
    let identity = done.hashed_text.identity().0;
    let text_len = done.hashed_text.bytes().len();
    let arrived = Arrived {
      node: done.meta.node,
      p1: done.meta.p1,
      p2: done.meta.p2,
      link: done.meta.link,
      text: Vec::new(),
    };
    let delta_slice = done
      .hashed_delta
      .as_ref()
      .map_or(done.delta.as_slice(), Hashed::bytes);
    let incoming_delta = Some(IncomingDelta {
      base: done.delta_base,
      bytes: delta_slice,
      hashed: done.hashed_delta.as_ref().map(Hashed::as_ref),
    });
    let previous = self
      .last
      .as_ref()
      .map(|(node, text)| (node, text.as_slice()));
    let mut room = *admission;
    let kept_one = keep_one_prepared(
      &mut self.fork,
      store,
      Log::Manifest,
      (&arrived, previous),
      (
        None,
        Some(next_rev),
        Some(PrecomputedKeep::Manifest(PrecomputedManifest {
          text: done.hashed_text.bytes(),
          hashed_text: Some(done.hashed_text.as_ref()),
          incoming_delta,
        })),
      ),
      &mut room,
    )?;
    let text = done.hashed_text.into_bytes();
    *admission = trial;
    self.next_rev = Some(kept_one.map_or(next_rev, |info| info.rev).saturating_add(1));
    self.awaited.extend(awaited);
    self.note_logged(logged);
    self.brought.insert(done.meta.node);
    self.intaken.kept = self.intaken.kept.saturating_add(1);
    self.intaken.bytes = self.intaken.bytes.saturating_add(to_u64(text_len));
    self.intaken.longest_step_bytes = self.intaken.longest_step_bytes.max(to_u64(text_len));
    if done.base_rebuilt {
      self.intaken.rebuilt = self.intaken.rebuilt.saturating_add(1);
    }
    self.record_kept_text(
      Log::Manifest,
      MANIFEST_PATH,
      done.meta.node,
      (identity, to_u64(text_len), base, done.place),
      (kept_one, next_rev),
    );
    batch_identities.insert(done.meta.node, identity);
    if done.last_on_base {
      self.cache.release(&done.meta.base, &mut self.spares);
    }
    let prev_id = self.last_identity;
    if let Some((prev_node, prev_text)) = self.last.take() {
      if self.last_later {
        self.cache.put_with_identity(
          prev_node,
          prev_text,
          prev_id.filter(|(n, _)| *n == prev_node).map(|(_, id)| id),
          &mut self.spares,
        );
      } else {
        self.spares.give(prev_text);
      }
    }
    self.last_identity = Some((done.meta.node, identity));
    self.last_later = done.based_later;
    self.last = Some((done.meta.node, text));
    if let Stage::Manifests(at) = self.stage {
      self.stage = Stage::Manifests(at.saturating_add(1));
    }
    Ok(())
  }

  /// Commits one worker-verified file group in stream order; returns `Ok(true)` if the entire
  /// group finished (caller may continue to the next group), or `Ok(false)` if the group stopped
  /// early for a large revision or byte bound (caller stops applying further speculative groups).
  fn apply_file_group_done(
    &mut self,
    group: FileGroupDone,
    (store, admission): (&mut ContentStore, &mut Admission),
  ) -> Result<bool, Refused> {
    if self.path.is_none() {
      admission.charge(to_u64(group.path.len()))?;
      self.path = Some(group.path.clone());
    }
    let log = Log::File(&group.path);
    for rev in group.revs {
      let mut trial = *admission;
      let next_rev = self.next_rev.unwrap_or_else(|| self.fork.len(store, log));
      let logged = self.logged_of(log, next_rev, &mut trial)?;
      trial.charge_items::<Node>(1)?;
      trial.charge_items::<KeptText>(1)?;
      trial.charge(to_u64(group.path.len()))?;
      let arrived = Arrived {
        node: rev.meta.node,
        p1: rev.meta.p1,
        p2: rev.meta.p2,
        link: rev.meta.link,
        text: Vec::new(),
      };
      let mut room = *admission;
      let kept_one = keep_one_prepared(
        &mut self.fork,
        store,
        log,
        (&arrived, None),
        (
          None,
          Some(next_rev),
          Some(PrecomputedKeep::File(PrecomputedFile {
            text_len: rev.text_len,
            p1: rev.stored_p1,
            p2: rev.stored_p2,
            hashed_bytes: rev.hashed_bytes.as_ref(),
            hashed_block: rev.hashed_block.as_ref().map(Hashed::as_ref),
          })),
        ),
        &mut room,
      )?;
      *admission = trial;
      self.next_rev = Some(kept_one.map_or(next_rev, |info| info.rev).saturating_add(1));
      self.note_logged(logged);
      self.brought.insert(rev.meta.node);
      self.positions.insert(rev.meta.node, rev.at);
      self.intaken.kept = self.intaken.kept.saturating_add(1);
      self.intaken.bytes = self.intaken.bytes.saturating_add(to_u64(rev.text_len));
      self.intaken.longest_step_bytes = self.intaken.longest_step_bytes.max(to_u64(rev.text_len));
      self.record_kept_text(
        log,
        &group.path,
        rev.meta.node,
        (
          rev.text_identity,
          to_u64(rev.text_len),
          rev.base_identity,
          rev.place,
        ),
        (kept_one, next_rev),
      );
      self.stage = Stage::Files {
        file: group.file_idx,
        at: rev.at.saturating_add(1),
      };
    }
    if let Some(fault) = group.fault {
      return Err(fault);
    }
    if let Some(resume) = group.resume {
      self.restore_file_resume(resume, group.file_idx);
      return Ok(false);
    }
    self.enter(Stage::Files {
      file: group.file_idx.saturating_add(1),
      at: 0,
    });
    Ok(true)
  }

  /// Restores `self.last` and `self.cache` when a worker file group stops early mid-file.
  fn restore_file_resume(&mut self, resume: FileGroupResume, file_idx: usize) {
    if let Some((_, old)) = self.last.take() {
      self.spares.give(old);
    }
    self.cache.clear(&mut self.spares);
    if let Some((node, text, id)) = resume.last {
      self.last = Some((node, text));
      self.last_identity = Some((node, id));
    } else {
      self.last_identity = None;
    }
    self.last_later = resume.last_later;
    for (node, text, id) in resume.cached {
      self
        .cache
        .put_with_identity(node, text, Some(id), &mut self.spares);
    }
    self.stage = Stage::Files {
      file: file_idx,
      at: resume.stopped_at,
    };
  }

  /// Commits one worker-verified single file revision in stream order.
  fn apply_file_rev_done(
    &mut self,
    done: FileRevDone,
    batch_identities: &mut BTreeMap<Node, [u8; 32]>,
    (store, admission): (&mut ContentStore, &mut Admission),
  ) -> Result<(), Refused> {
    let path = self.path.clone().unwrap_or_default();
    let log = Log::File(&path);
    let mut trial = *admission;
    let next_rev = self.next_rev.unwrap_or_else(|| self.fork.len(store, log));
    let logged = self.logged_of(log, next_rev, &mut trial)?;
    trial.charge_items::<Node>(1)?;
    trial.charge_items::<KeptText>(1)?;
    trial.charge(to_u64(path.len()))?;
    let base = resolve_base_identity(done.meta.base, done.base_identity, batch_identities);
    let arrived = Arrived {
      node: done.meta.node,
      p1: done.meta.p1,
      p2: done.meta.p2,
      link: done.meta.link,
      text: Vec::new(),
    };
    let mut room = *admission;
    let kept_one = keep_one_prepared(
      &mut self.fork,
      store,
      log,
      (&arrived, None),
      (
        None,
        Some(next_rev),
        Some(PrecomputedKeep::File(PrecomputedFile {
          text_len: done.text_len,
          p1: done.stored_p1,
          p2: done.stored_p2,
          hashed_bytes: done.hashed_bytes.as_ref(),
          hashed_block: done.hashed_block.as_ref().map(Hashed::as_ref),
        })),
      ),
      &mut room,
    )?;
    *admission = trial;
    self.next_rev = Some(kept_one.map_or(next_rev, |info| info.rev).saturating_add(1));
    self.note_logged(logged);
    self.brought.insert(done.meta.node);
    self.positions.insert(done.meta.node, done.at);
    self.intaken.kept = self.intaken.kept.saturating_add(1);
    self.intaken.bytes = self.intaken.bytes.saturating_add(to_u64(done.text_len));
    self.intaken.longest_step_bytes = self.intaken.longest_step_bytes.max(to_u64(done.text_len));
    if done.base_rebuilt {
      self.intaken.rebuilt = self.intaken.rebuilt.saturating_add(1);
    }
    self.record_kept_text(
      log,
      &path,
      done.meta.node,
      (done.text_identity, to_u64(done.text_len), base, done.place),
      (kept_one, next_rev),
    );
    batch_identities.insert(done.meta.node, done.text_identity);
    if done.last_on_base {
      self.cache.release(&done.meta.base, &mut self.spares);
    }
    let prev_id = self.last_identity;
    if let Some((prev_node, prev_text)) = self.last.take() {
      if self.last_later {
        self.cache.put_with_identity(
          prev_node,
          prev_text,
          prev_id.filter(|(n, _)| *n == prev_node).map(|(_, id)| id),
          &mut self.spares,
        );
      } else {
        self.spares.give(prev_text);
      }
    }
    self.last_identity = Some((done.meta.node, done.text_identity));
    self.last_later = done.based_later;
    self.last = done.kept_text.map(|text| (done.meta.node, text));
    self.stage = Stage::Files {
      file: done.file_idx,
      at: done.at.saturating_add(1),
    };
    Ok(())
  }

  /// Filters pre-extracted manifest written entries against `self.fork` and `self.awaited`,
  /// charging `admission` identically to [`Taker::awaited_of`].
  fn filter_awaited_written(
    &self,
    written: Vec<(Vec<u8>, Node)>,
    (store, admission): (&ContentStore, &mut Admission),
  ) -> Result<Vec<(Vec<u8>, Node)>, Refused> {
    let mut working = *admission;
    working.charge_items::<(Vec<u8>, Node)>(written.len())?;
    let mut awaited = Vec::new();
    for (path, node) in written {
      let held = self.fork.rev(store, Log::File(&path), &node).is_some();
      if held || self.awaited.contains(&(path.clone(), node)) {
        continue;
      }
      if to_u64(self.awaited.len().saturating_add(awaited.len())) >= self.bringing {
        return Err(Unverified::MissingFile { path, node }.into());
      }
      admission.charge_items::<(Vec<u8>, Node)>(1)?;
      admission.charge(to_u64(path.len()))?;
      awaited.push((path, node));
    }
    Ok(awaited)
  }

  /// Records a committed revision's [`KeptText`] into `self.cutting` or `self.intaken.texts`.
  fn record_kept_text(
    &mut self,
    log: Log<'_>,
    path: &[u8],
    node: Node,
    (identity, len, base, place): ([u8; 32], u64, Option<[u8; 32]>, Range<u64>),
    (kept_one, next_rev): (Option<KeptOne>, u32),
  ) {
    let kept = KeptText {
      path: path.to_vec(),
      node,
      identity,
      len,
      base,
      place,
    };
    match self.cutting.as_mut() {
      Some(cutting) => {
        let rev = kept_one.map_or(next_rev, |info| info.rev);
        let (contents, stored_bytes) = kept_one.map_or((None, 0), |info| {
          (
            Some(FileContents {
              path: path.to_vec(),
              node,
              bytes: info.bytes,
              block: info.block,
            }),
            info.stored_bytes,
          )
        });
        let charged = to_u64(core::mem::size_of::<KeptText>())
          .saturating_add(to_u64(path.len()))
          .saturating_add(contents.as_ref().map_or(0, |contents| {
            to_u64(core::mem::size_of::<FileContents>()).saturating_add(to_u64(contents.path.len()))
          }));
        cutting.kept((log_of(log), rev), kept, (charged, stored_bytes, contents));
      }
      None => self.intaken.texts.push(kept),
    }
  }
}

/// Extracts [`RevMeta`] from a changegroup [`Revision`].
fn rev_meta(revision: &Revision<'_>) -> RevMeta {
  RevMeta {
    node: revision.node,
    p1: revision.p1,
    p2: revision.p2,
    base: revision.base,
    link: revision.link,
  }
}

/// Resolves a revision's base content identity from either pre-computed `known` or earlier items in
/// the same batch.
fn resolve_base_identity(
  base: Node,
  known: Option<[u8; 32]>,
  batch_identities: &BTreeMap<Node, [u8; 32]>,
) -> Option<[u8; 32]> {
  if base == NULL_NODE {
    return None;
  }
  known.or_else(|| batch_identities.get(&base).copied())
}
