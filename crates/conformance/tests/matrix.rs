//! Doc-truth for the evidence matrix (CLAUDE.md §4 "Doc-truth tests"; AC-9.7 "publish
//! capability-specific results"; GAP-A9-15 "historical stages overstate coverage"). The matrix in
//! `docs/wip/conformance.md` is generated from the tracked records under
//! `docs/wip/conformance/records/`; these tests re-render it and fail on any drift in either
//! direction, require every cell of the matrix to have a record (so no cell is ever an unexplained
//! blank), check that every tracked record sits under the file name its transport and suite give
//! it, and check that a record judged against an expected-failure list names the list as it is
//! tracked now — so editing the list without re-running the suite is caught, which is how "the
//! list only shrinks" holds across commits. The `--ignored regenerate_the_evidence_matrix` writer
//! rewrites the block deliberately; a normal run never mutates the tree.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

use slates_conformance::expected::ExpectedFailures;
use slates_conformance::matrix;
use slates_conformance::record::Record;
use slates_conformance::{Suite, Transport};

/// The workspace root, from this crate's manifest directory.
fn workspace_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR"))
    .join("../..")
    .canonicalize()
    .expect("the workspace root")
}

fn records_dir() -> PathBuf {
  workspace_root().join("docs/wip/conformance/records")
}

fn document_path() -> PathBuf {
  workspace_root().join("docs/wip/conformance.md")
}

/// Every tracked record with the file it came from, sorted by file name.
fn tracked_records() -> Vec<(String, Record)> {
  let mut paths: Vec<PathBuf> = std::fs::read_dir(records_dir())
    .expect("docs/wip/conformance/records exists")
    .map(|e| e.expect("a directory entry").path())
    .filter(|p| p.extension().is_some_and(|x| x == "json"))
    .collect();
  paths.sort();
  paths
    .into_iter()
    .map(|path| {
      let name = path.file_name().unwrap().to_string_lossy().into_owned();
      let bytes = std::fs::read(&path).expect("a readable record");
      let record = Record::parse(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
      (name, record)
    })
    .collect()
}

fn rendered() -> String {
  let records: Vec<Record> = tracked_records().into_iter().map(|(_, r)| r).collect();
  matrix::render(&records)
}

/// The document's generated block, between the markers.
fn document_block() -> String {
  let document =
    std::fs::read_to_string(document_path()).expect("docs/wip/conformance.md is readable");
  let (start, end) = matrix::block_of(&document).expect("the document carries both matrix markers");
  document[start..end].to_owned()
}

/// Do: re-render the matrix from the tracked records. Expect: the document's block, byte for byte;
/// a drift in either direction fails and names the writer.
#[test]
fn the_documents_matrix_is_rendered_from_the_tracked_records() {
  assert_eq!(
    document_block(),
    rendered(),
    "docs/wip/conformance.md's matrix drifted from docs/wip/conformance/records; run \
     `cargo test -p slates-conformance --test matrix -- --ignored regenerate_the_evidence_matrix` \
     (or `cargo xtask conformance matrix --write`)"
  );
}

/// Do: list the tracked records. Expect: every (transport × suite) cell has one, so the matrix never
/// shows a cell without a typed reason.
#[test]
fn every_cell_has_a_record() {
  let records = tracked_records();
  let mut missing = Vec::new();
  for transport in Transport::ALL {
    for suite in Suite::ALL {
      if !records
        .iter()
        .any(|(_, r)| r.transport == transport && r.suite == suite)
      {
        missing.push(format!("{}.{}.json", transport.slug(), suite.slug()));
      }
    }
  }
  assert!(
    missing.is_empty(),
    "cells without a record (run `cargo xtask conformance plan` on each lane's host): {}",
    missing.join(", ")
  );
}

/// Do: read each tracked record. Expect: it sits under the file name its transport and suite give
/// it, so it renders in the cell it claims.
#[test]
fn every_tracked_record_sits_under_its_own_name() {
  for (name, record) in tracked_records() {
    assert_eq!(
      name,
      record.file_name(),
      "{name} holds a record for {} × {}",
      record.transport.slug(),
      record.suite.slug()
    );
  }
}

/// Do: for each record judged against an expected-failure list, parse the tracked list it names.
/// Expect: the list's digest and entry count are the record's, so a list edited after the run (or a
/// run judged against a list since changed) is caught and the suite re-run.
#[test]
fn a_record_names_the_expected_failure_list_as_tracked() {
  for (name, record) in tracked_records() {
    let Some(list) = &record.expected_failure_list else {
      continue;
    };
    let text = std::fs::read_to_string(workspace_root().join(&list.path)).unwrap_or_default();
    let parsed = ExpectedFailures::parse(&text).unwrap_or_else(|e| panic!("{}: {e}", list.path));
    assert_eq!(
      parsed.digest(),
      list.blake3,
      "{name}: {} changed since the run",
      list.path
    );
    assert_eq!(
      usize::try_from(list.entries).unwrap(),
      parsed.len(),
      "{name}: {} has a different entry count than the run saw",
      list.path
    );
  }
}

/// The deliberate writer: rewrites the document's generated block from the tracked records.
/// Ignored, so a normal test run never mutates the tree; run with `--ignored` after a record changes.
#[test]
#[ignore = "rewrites docs/wip/conformance.md from the tracked records; run deliberately with --ignored"]
fn regenerate_the_evidence_matrix() {
  let document =
    std::fs::read_to_string(document_path()).expect("docs/wip/conformance.md is readable");
  let rewritten =
    matrix::rewrite(&document, &rendered()).expect("the document carries both markers");
  // The design's `--ignored regenerate` writer rewriting a tracked document in the repository (CLAUDE
  // §4 "Doc-truth tests"); shipped code never reaches this call.
  #[allow(clippy::disallowed_methods)]
  std::fs::write(document_path(), rewritten).expect("the document is writable");
}
