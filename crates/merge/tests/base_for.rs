//! The restricted base's oracle (§4.16 "Composition at seal"): composing a journal over
//! `Green::base_for(version, journal)`, the base restricted to the paths the journal names, gives the
//! same result as composing it over `Green::base_at(version)`, the whole base: the same ops document,
//! or the same typed refusal. A submit builds the restricted one, so its cost follows what the work
//! changed, not what the green holds.
//!
//! The green is generated with history in every dimension (files, directories, modes, symlinks, hard
//! links, extended attributes, FIFO/socket inodes) by generated increments, some of which conflict and
//! are not committed; then generated journals, valid or not, are composed at every version from the
//! origin to the head. Paths come from a small alphabet with nesting, so journals meet the base's
//! entries, their neighbours and their children.

// Test code may panic (CLAUDE.md §2 item 6 applies to shipped code).
#![allow(clippy::indexing_slicing)]

mod common;

use common::generated::{increment_of, op_of, origin_of, raw};
use proptest::prelude::*;
use slates_merge::engine::Green;
use slates_merge::increment::{Base, VolumeOp, compose_volume};
use slates_merge::origin::Origin;

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]
  /// T-6.x (§4.16 "Composition at seal"; the submit's restricted base): for any green built by
  /// generated increments over a generated origin, and any generated journal, composing over the
  /// base restricted to the journal's paths equals composing over the whole base at every version
  /// from the origin to the head — the same document or the same refusal.
  #[test]
  fn the_restricted_base_composes_as_the_whole_base(
    origin in proptest::collection::vec(raw(), 0..24),
    history in proptest::collection::vec(raw(), 0..16),
    journals in proptest::collection::vec(proptest::collection::vec(raw(), 1..8), 1..8),
  ) {
    let mut green = Green::with_origin(origin_of(&origin));
    for (index, step) in history.iter().enumerate() {
      let id = u8::try_from(index).unwrap_or(u8::MAX);
      // A conflicting step is not committed; the head moves only on an accept.
      let _ = green.submit(&increment_of(step).at(id, green.head()));
    }
    let mut compared = 0u32;
    for version in 0..=green.head() {
      for journal in &journals {
        let journal: Vec<VolumeOp> = journal.iter().map(op_of).collect();
        let whole = compose_volume(&green.base_at(version), &journal);
        let restricted = compose_volume(&green.base_for(version, &journal), &journal);
        prop_assert_eq!(
          restricted.as_ref().map(slates_merge::ops_doc::OpsDoc::encode),
          whole.as_ref().map(slates_merge::ops_doc::OpsDoc::encode),
          "version {} of {}, journal {:?}",
          version,
          green.head(),
          journal
        );
        compared += 1;
      }
    }
    // Non-vacuity: every version was composed at least once.
    prop_assert!(u64::from(compared) > green.head());
  }
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]
  /// A-247 (a work composes its increment on its own shard): for any green built by generated increments and any
  /// generated journal, the base assembled from each named path's own entries (`Green::path_base`, what a work's shard
  /// fetches a path at a time) is the restricted base itself, at every version from the origin to the head: the same
  /// entries in the same order, so the work's increment is byte for byte the owner's.
  #[test]
  fn a_base_assembled_a_path_at_a_time_is_the_restricted_base(
    origin in proptest::collection::vec(raw(), 0..24),
    history in proptest::collection::vec(raw(), 0..16),
    journals in proptest::collection::vec(proptest::collection::vec(raw(), 1..8), 1..8),
  ) {
    let mut green = Green::with_origin(origin_of(&origin));
    for (index, step) in history.iter().enumerate() {
      let id = u8::try_from(index).unwrap_or(u8::MAX);
      let _ = green.submit(&increment_of(step).at(id, green.head()));
    }
    for version in 0..=green.head() {
      for journal in &journals {
        let journal: Vec<VolumeOp> = journal.iter().map(op_of).collect();
        let named: std::collections::BTreeSet<&str> =
          journal.iter().flat_map(VolumeOp::names).flatten().collect();
        let per_path: std::collections::BTreeMap<String, Base> = named
          .iter()
          .map(|path| ((*path).to_owned(), green.path_base(version, path)))
          .collect();
        let restricted = green.base_for(version, &journal);
        prop_assert_eq!(
          Base::for_names(&named, &per_path),
          Some(restricted),
          "version {} of {}, journal {:?}",
          version,
          green.head(),
          journal
        );
        // A path left out is a base not known, never a smaller one.
        if let Some(first) = named.iter().next() {
          let mut missing = per_path.clone();
          missing.remove(*first);
          prop_assert_eq!(Base::for_names(&named, &missing), None);
        }
      }
    }
  }
}

/// T-6.x non-vacuity for the oracle above: the restricted base is smaller than the whole one when the
/// journal names one path of many, and still composes the same document.
#[test]
fn the_restricted_base_holds_only_what_the_journal_names() {
  let origin = Origin {
    files: (0..64).map(|n| (format!("f{n:02}"), vec![7; 8])).collect(),
    ..Origin::default()
  };
  let green = Green::with_origin(origin.clone());
  let journal = vec![VolumeOp::Overwrite {
    path: "f07".to_owned(),
    at: 0,
    len: 4,
  }];
  let restricted = green.base_for(0, &journal);
  assert_eq!(restricted.files, vec![("f07".to_owned(), 8)]);
  assert_eq!(green.base_at(0).files.len(), 64);
  assert_eq!(
    compose_volume(&restricted, &journal).map(|doc| doc.encode()),
    compose_volume(&green.base_at(0), &journal).map(|doc| doc.encode())
  );
}
