//! A landing's revisions sized where they land (goal 93, cut 2, the kept rendered length; main 01a20182's rulings of
//! 2026-11-27 18:56 and 18:58): each kept revision's chunk length as a stream sends it, kept in the index that takes
//! the revisions' place ([`HgIndex::keep_sized`]), so no stream counts its logs before its head.
//!
//! A revision's length is had once, where it lands: a segment's revision held as stock's revlog chunk by its chunk's
//! length ([`chunked`], the chunks made as the segment lands, `chunked`); any other by compressing its delta as a
//! stream would ([`raw`], `crate::index::chunk_len_of`), once, at the landing rather than at every stream. The lengths
//! are kept in one place, the index the landing puts in the repository's ([`keep`]): a push's fork just before its
//! landing (after any re-base, whose rows are its own), an import's index before it is placed. A changelog delta
//! against another revision than the one before it is kept unstreamable, as the stream's own rule has it
//! (`render`'s `changelog_servable`): its repository serves no stream and the client clones.

use core::ops::Range;
use std::collections::BTreeMap;

use slates_vcs::ContentStore;

use super::intake::{CHANGELOG_PATH, KeptText, MANIFEST_PATH};
use crate::Node;
use crate::index::{HgIndex, KeptSized, Log, Placed, Refused, ToSize, chunk_len_of};

/// A kept revision's length as a stream sends its chunk, with what its changelog's streamability is judged by: its log
/// (its path, or [`CHANGELOG_PATH`] or [`MANIFEST_PATH`]), its node, its text's identity, and its delta's base's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Landed {
  /// Its log.
  pub path: Vec<u8>,
  /// Its node.
  pub node: Node,
  /// Its chunk's length.
  pub len: u32,
  /// Its whole text's identity.
  pub identity: [u8; 32],
  /// The identity of the text its delta rests on; none for a whole.
  pub base: Option<[u8; 32]>,
}

/// What [`keep`] kept: the revisions newly sized, and the logs it left unsized (an earlier revision of each not sized:
/// a repository replayed and not yet recounted, `render::Recount`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Kept {
  /// The revisions newly sized.
  pub sized: u64,
  /// The logs left unsized.
  pub unsized_logs: u64,
}

/// `texts`, held as chunks at `places` (one a text, in order: a segment's, made as it landed), each sized by its chunk's
/// length; refused [`Refused::Unsizable`] where the places are not one a text.
///
/// # Errors
/// [`Refused::Unsizable`].
pub fn chunked(texts: &[KeptText], places: &[Range<u64>]) -> Result<Vec<Landed>, Refused> {
  if texts.len() != places.len() {
    return Err(Refused::Unsizable { rev: 0 });
  }
  texts
    .iter()
    .zip(places)
    .map(|(kept, place)| {
      let len = place.end.saturating_sub(place.start);
      chunk_len_of(Placed::Chunk(len), 0).map(|len| landed(kept, len))
    })
    .collect()
}

/// `texts`, each delta in `payload` (the changegroup as it arrived) sized as a stream compresses it, once.
///
/// # Errors
/// [`Refused::Unsizable`] where a place is past the payload or the compressor refuses.
pub fn raw(texts: &[KeptText], payload: &[u8]) -> Result<Vec<Landed>, Refused> {
  texts
    .iter()
    .map(|kept| {
      let from = usize::try_from(kept.place.start).map_err(|_| Refused::Unsizable { rev: 0 })?;
      let to = usize::try_from(kept.place.end).map_err(|_| Refused::Unsizable { rev: 0 })?;
      let delta = payload.get(from..to).ok_or(Refused::Unsizable { rev: 0 })?;
      chunk_len_of(Placed::Raw(delta), 0).map(|len| landed(kept, len))
    })
    .collect()
}

/// `kept`'s length `len`, as [`Landed`].
fn landed(kept: &KeptText, len: u32) -> Landed {
  Landed {
    path: kept.path.clone(),
    node: kept.node,
    len,
    identity: kept.identity,
    base: kept.base,
  }
}

/// The log `path` names.
fn log_of(path: &[u8]) -> Log<'_> {
  match path {
    CHANGELOG_PATH => Log::Changelog,
    MANIFEST_PATH => Log::Manifest,
    path => Log::File(path),
  }
}

/// Keeps every revision of `landed` sized in `index` ([`HgIndex::keep_sized`]), each log's in the order of its numbers:
/// what it kept. A log whose revisions are not the run at its end ([`Refused::SizedOutOfOrder`]) is refused, nothing of
/// it kept; a log with an earlier revision unsized is left so, counted.
///
/// # Errors
/// [`Refused::SizedOutOfOrder`] where a node is not the log's or the revisions are not one run; the index's refusal.
pub fn keep(
  index: &mut HgIndex,
  store: &mut ContentStore,
  landed: &[Landed],
) -> Result<Kept, Refused> {
  let mut logs: BTreeMap<&[u8], Vec<(u32, &Landed)>> = BTreeMap::new();
  for revision in landed {
    let log = log_of(&revision.path);
    let rev = index
      .rev(store, log, &revision.node)
      .ok_or(Refused::SizedOutOfOrder { rev: 0 })?;
    logs
      .entry(&revision.path)
      .or_default()
      .push((rev, revision));
  }
  let mut kept = Kept::default();
  for (path, mut revisions) in logs {
    revisions.sort_by_key(|(rev, _)| *rev);
    let log = log_of(path);
    match keep_log(index, store, log, &revisions)? {
      KeptSized::Sized { new } => kept.sized = kept.sized.saturating_add(new),
      KeptSized::Unsized => kept.unsized_logs = kept.unsized_logs.saturating_add(1),
    }
  }
  Ok(kept)
}

/// Keeps `log`'s `revisions` (each its number, in order: one run) sized.
fn keep_log(
  index: &mut HgIndex,
  store: &mut ContentStore,
  log: Log<'_>,
  revisions: &[(u32, &Landed)],
) -> Result<KeptSized, Refused> {
  let Some((from, _)) = revisions.first() else {
    return Ok(KeptSized::Sized { new: 0 });
  };
  let from = *from;
  for (at, (rev, _)) in (from..).zip(revisions) {
    if at != *rev {
      return Err(Refused::SizedOutOfOrder { rev: *rev });
    }
  }
  let lengths: Vec<u32> = revisions.iter().map(|(_, revision)| revision.len).collect();
  let streamable = log != Log::Changelog || changelog_streamable(index, store, (from, revisions));
  index.keep_sized(
    store,
    log,
    ToSize {
      from,
      lengths: &lengths,
      streamable,
    },
  )
}

/// Whether a stream can send the changelog's `revisions` (from number `from`) as they are kept: each a whole, or a delta
/// against the text of the revision before it (`render`'s `changelog_servable`; 7.2 writes a new changelog without
/// generaldelta). The revision before the first is read back from the index once, its text's identity hashed.
fn changelog_streamable(
  index: &HgIndex,
  store: &ContentStore,
  (from, revisions): (u32, &[(u32, &Landed)]),
) -> bool {
  let before = from
    .checked_sub(1)
    .and_then(|rev| index.revision_text(store, Log::Changelog, rev))
    .map(|text| *blake3::hash(&text).as_bytes());
  let mut previous = before;
  revisions.iter().all(|(_, revision)| {
    let streamable = revision.base.is_none_or(|base| Some(base) == previous);
    previous = Some(revision.identity);
    streamable
  })
}
