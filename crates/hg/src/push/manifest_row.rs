//! How a pushed manifest's row keeps its content: one rule for the live intake, a pushed segment's replay and a stale
//! fork's re-base, so the three make the same rows by construction (goal 93, C1 (2); main's ruling (B') of 2026-11-24).
//!
//! **The rule**, over the manifest the fork holds last (A-729: a row is a delta against the one before it, over a chain
//! resting on a whole text, the chain's delta bytes within the text's own length):
//! 1. the client's own delta, where its base is that last row and the chain stays within bound ([`as_sent`]): no diff
//!    made at all;
//! 2. else the manifest's diff against that last row, a merge of the two sorted texts ([`crate::manifest_diff`]), where
//!    the chain stays within bound ([`diffed`]);
//! 3. else whole: the first manifest the fork holds, or a chain past its bound.
//!
//! The intake applies it as its text streams (`push::intake`'s manifests); the replay and the re-base, which hold the text
//! whole for their step already, through [`kept_from_text`]. Every diff is made within the content store's window, so
//! its hunks are cut alike wherever it is made.

use slates_vcs::ContentStore;

use crate::Node;
use crate::delta::BaseRead;
use crate::index::{HgIndex, Log, NONE, NewRevision, Refused, Stored};
use crate::manifest_diff::{Diffing, Stopped};
use crate::read::to_u64;

/// Format: a whole text's delta fields: no base, no delta, no chain.
pub(crate) const WHOLE: Stored = (NONE, 0, 0);

/// The manifest the fork holds last: its number, node and chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Last {
  /// Its number.
  pub(crate) rev: u32,
  /// Its node.
  pub(crate) node: Node,
  /// The delta bytes of its chain since the whole text it rests on.
  pub(crate) chain: u64,
}

/// The manifest `index` holds last, if any.
pub(crate) fn last(index: &HgIndex, store: &ContentStore) -> Option<Last> {
  let rev = index.len(store, Log::Manifest).checked_sub(1)?;
  let row = index.revision(store, Log::Manifest, rev)?;
  Some(Last {
    rev,
    node: row.node,
    chain: row.chain,
  })
}

/// Rule 1: the stored fields of a manifest `len` bytes long kept as its client's delta, `delta_len` bytes against
/// `base`, where `base` is `last` and the chain stays within bound; none otherwise.
pub(crate) fn as_sent(
  last: Option<&Last>,
  base: &Node,
  delta_len: u64,
  len: u64,
) -> Option<Stored> {
  let last = last.filter(|last| last.node == *base)?;
  let chain = last.chain.saturating_add(delta_len);
  (chain <= len).then_some((last.rev, delta_len, chain))
}

/// Rule 2: the stored fields of a manifest `len` bytes long kept as its diff against `last`, `diff_len` bytes, where the
/// chain stays within bound; none (rule 3, whole) otherwise.
pub(crate) fn diffed(last: &Last, diff_len: u64, len: u64) -> Option<Stored> {
  let chain = last.chain.saturating_add(diff_len);
  (chain <= len).then_some((last.rev, diff_len, chain))
}

/// The diff of `text` against `previous`, the last row's text, made as [`crate::manifest_diff::Diffing`] makes it within
/// `window`: the bytes the intake's streaming makes of the same texts.
pub(crate) fn diff_of(previous: &[u8], text: &[u8], window: usize) -> Result<Vec<u8>, Stopped> {
  let mut diffing = Diffing::within(window);
  let mut base: &[u8] = previous;
  let mut out = Vec::new();
  diffing.update(text, &mut base as &mut dyn BaseRead, &mut out)?;
  diffing.finish(&mut base as &mut dyn BaseRead, &mut out)?;
  Ok(out)
}

/// Appends `row`, a manifest whose whole text is `text`, to `index` by rules 2 and 3 (the replay's and the re-base's
/// form): its diff against the manifest `index` holds last, read over `given` texts where that one's was let go, where
/// the chain allows; else whole. Each content put is held by the row and its own hold given back. Its numbers.
pub(crate) fn kept_from_text(
  index: &mut HgIndex,
  store: &mut ContentStore,
  (mut row, text): (NewRevision, &[u8]),
  given: &[(Node, Vec<u8>)],
) -> Result<Vec<u32>, Refused> {
  let len = to_u64(text.len());
  let unread = |rev| Refused::Unread { rev };
  let content = |refused| Refused::Index(slates_vcs::IndexRefused::Content(refused));
  let stored = match last(index, store) {
    None => None,
    Some(last) => {
      let previous = index
        .revision_text_over(store, Log::Manifest, last.rev, given)
        .ok_or_else(|| unread(last.rev))?;
      let diff = diff_of(&previous, text, store.window()).map_err(|_| unread(last.rev))?;
      diffed(&last, to_u64(diff.len()), len).map(|stored| (stored, diff))
    }
  };
  let (put, stored) = match stored {
    Some((stored, diff)) => (store.put_object(&diff).map_err(content)?, stored),
    None => (store.put_object(text).map_err(content)?, WHOLE),
  };
  row.bytes = put;
  row.len = len;
  let appended = index.append_stored(store, Log::Manifest, &[(row, stored)]);
  store.release(&put).map_err(content)?;
  appended
}
