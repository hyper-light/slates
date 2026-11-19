//! A-657's tests of a push's revisions kept in the index and read back through it: stock 7.2's bundle of a history
//! with a copy, a merge and a closed branch (`fixtures/cg03.hg`) kept in a fork, every text read back as it arrived,
//! the same bundle verified again against it bringing nothing, and a refused fork leaving nothing behind.

use std::borrow::Cow;

use slates_vcs::ContentStore;

use super::{Unindexed, fork_with};
use crate::bundle2::Bundle2;
use crate::fixtures::CG03;
use crate::index::{HgIndex, Log};
use crate::push::held::IndexHeld;
use crate::push::verify::{Held, Verified, verify};
use crate::push::{ChangegroupPart, read};
use crate::{Admission, Node};

/// The window the tests' stores are made with: room for a few rows a table, as the import's tests make theirs.
const WINDOW: usize = 1 << 12;

/// Room enough for anything a test holds.
fn room() -> Admission {
  Admission::new(u64::MAX)
}

/// Nothing held: an empty repository.
struct Nothing;

impl Held for Nothing {
  fn changeset_text(&self, _node: &Node) -> Option<Cow<'_, [u8]>> {
    None
  }

  fn manifest_text(&self, _node: &Node) -> Option<Cow<'_, [u8]>> {
    None
  }

  fn file_text(&self, _path: &[u8], _node: &Node) -> Option<Cow<'_, [u8]>> {
    None
  }
}

/// The changegroup part of stock 7.2's bundle.
fn stock() -> ChangegroupPart {
  let bundle = Bundle2::decode(CG03, &mut room()).unwrap();
  let unbundle = read(&bundle, &mut room()).unwrap();
  unbundle.changegroup().unwrap().1.clone()
}

/// A-657, AC-HG.2 (T-HG.2). Do: verify stock 7.2's bundle against an empty repository, keep its revisions in a fork of
/// an empty index, read every text back through the fork, verify the bundle again against the fork, then release both.
/// Expect: the index forked from still empty; each changeset, manifest and file revision read back as it arrived (a
/// copy's metadata block put back before its bytes); the second verification brings nothing, every revision passed
/// over and every changeset listed as held, in the bundle's order; and with both released the store holds nothing.
#[test]
fn a_push_kept_in_a_fork_is_held_whole_and_brings_nothing_again() {
  let part = stock();
  let verified = verify(&part, &Nothing, &mut room()).unwrap();
  let mut store = ContentStore::new(1 << 24, WINDOW).unwrap();
  let mut index = HgIndex::new();
  let fork = fork_with(&mut index, &mut store, &verified, &mut room()).unwrap();
  assert_eq!(index.len(&store, Log::Changelog), 0);
  assert_read_back(&IndexHeld::new(&fork, &store), &verified);
  let again = verify(&part, &IndexHeld::new(&fork, &store), &mut room()).unwrap();
  assert_brings_nothing(&again, &verified);
  fork.release(&mut store).unwrap();
  index.release(&mut store).unwrap();
  assert_eq!(store.len(), 0, "a hold was kept");
}

fn assert_read_back(held: &IndexHeld<'_>, verified: &Verified) {
  for changeset in &verified.changesets {
    let text = held.changeset_text(&changeset.node);
    assert_eq!(text.as_deref(), Some(changeset.text.as_slice()));
  }
  for manifest in &verified.manifests {
    let text = held.manifest_text(&manifest.node);
    assert_eq!(text.as_deref(), Some(manifest.text.as_slice()));
  }
  for (path, revisions) in &verified.files {
    for revision in revisions {
      let text = held.file_text(path, &revision.node);
      assert_eq!(text.as_deref(), Some(revision.text.as_slice()));
    }
  }
  assert!(held.changeset_text(&[0xee; 20]).is_none());
  assert!(held.file_text(b"a", &[0xee; 20]).is_none());
}

fn assert_brings_nothing(again: &Verified, verified: &Verified) {
  assert!(again.changesets.is_empty() && again.manifests.is_empty() && again.files.is_empty());
  let total = verified.changesets.len()
    + verified.manifests.len()
    + verified
      .files
      .iter()
      .map(|(_, revisions)| revisions.len())
      .sum::<usize>();
  assert_eq!(again.passed_over, u64::try_from(total).unwrap());
  let nodes: Vec<Node> = verified
    .changesets
    .iter()
    .map(|arrived| arrived.node)
    .collect();
  assert_eq!(again.held_changesets, nodes);
}

/// A-657 (§4.17.3 "Push" step 6: all of it or none). Do: keep stock 7.2's bundle in a fork of an empty index whose
/// store's budget is a few hundred bytes. Expect: refused by kind (the store's or the index's budget), the index
/// forked from still empty and the store holding nothing, every hold the fork took given back.
#[test]
fn a_refused_fork_leaves_the_index_and_the_store_as_they_were() {
  let verified = verify(&stock(), &Nothing, &mut room()).unwrap();
  let mut store = ContentStore::new(1 << 9, WINDOW).unwrap();
  let mut index = HgIndex::new();
  let refused = fork_with(&mut index, &mut store, &verified, &mut room());
  assert!(
    matches!(
      refused,
      Err(Unindexed::Content(_) | Unindexed::Index(_) | Unindexed::Fork(_))
    ),
    "{refused:?}"
  );
  assert_eq!(index.len(&store, Log::Changelog), 0);
  assert_eq!(store.len(), 0, "a refused fork kept a hold");
}
