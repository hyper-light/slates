//! How many runs a lookup in a Mercurial repository's index visits after an import, and what one merge of them buys
//! (goal 93 perf; main's order of 2026-11-21 00:22: name the step of a one-changeset pull that scales with the history).
//!
//! The index is `slates-vcs`'s shared index: a tail over runs, one run a seal, and a lookup visits the tail and then the
//! runs from the newest until one holds the key. The merge that bounds that is learned (`slates_vcs::rule`), but only a
//! caller that reports its timings and asks `compaction_due` ever merges. This imports a repository in-process (the
//! server's import, its texts in a content store of the given window), then times `samples` lookups of changesets and
//! manifests drawn at random (xorshift, seed 0x2026_1121), counting the runs each visited, before and after merging every
//! run the index alone holds into one (`Index::compact`).
//!
//! Command: `cargo run --release -p slates-hg --example index_runs -- <hg repository> <window bytes> <samples>`.

#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::indexing_slicing,
  clippy::cast_possible_truncation,
  clippy::cast_precision_loss,
  clippy::print_stdout,
  clippy::disallowed_methods
)]

use std::time::Instant;

use slates_hg::Admission;
use slates_hg::import::{FileRefused, RepositoryFiles};
use slates_hg::index::Log;
use slates_vcs::ContentStore;

/// Shape: the room the import and the store are given (the whole history fits).
const ROOM: u64 = 1 << 36;

/// A repository's `.hg` read from the host's disk.
struct Disk {
  dot_hg: std::path::PathBuf,
}

impl RepositoryFiles for Disk {
  type Refused = std::io::ErrorKind;

  fn len(&mut self, path: &[&[u8]]) -> Result<Option<u64>, FileRefused<Self::Refused>> {
    let at = self.at(path);
    match std::fs::metadata(&at) {
      Ok(meta) => Ok(Some(meta.len())),
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
      Err(error) => Err(FileRefused::Host(error.kind())),
    }
  }

  fn read(
    &mut self,
    path: &[&[u8]],
    offset: u64,
    len: u64,
    room: &mut Admission,
  ) -> Result<Option<Vec<u8>>, FileRefused<Self::Refused>> {
    use std::io::{Read, Seek, SeekFrom};
    let at = self.at(path);
    let mut file = match std::fs::File::open(&at) {
      Ok(file) => file,
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
      Err(error) => return Err(FileRefused::Host(error.kind())),
    };
    file
      .seek(SeekFrom::Start(offset))
      .map_err(|error| FileRefused::Host(error.kind()))?;
    let mut out = Vec::new();
    file
      .take(len)
      .read_to_end(&mut out)
      .map_err(|error| FileRefused::Host(error.kind()))?;
    room
      .charge(out.len() as u64)
      .map_err(FileRefused::PastAdmission)?;
    Ok(Some(out))
  }
}

impl Disk {
  fn at(&self, path: &[&[u8]]) -> std::path::PathBuf {
    let mut at = self.dot_hg.clone();
    for name in path {
      at.push(String::from_utf8_lossy(name).as_ref());
    }
    at
  }
}

/// The `per_mille`th of `values` (rounded to the nearest rank, as `((n - 1) * p).round()`).
fn percentile(values: &mut [f64], per_mille: usize) -> f64 {
  values.sort_by(f64::total_cmp);
  values[(values.len().saturating_sub(1) * per_mille + 500) / 1000]
}

fn main() {
  let arguments: Vec<String> = std::env::args().skip(1).collect();
  let root = std::path::PathBuf::from(&arguments[0]);
  let window: usize = arguments[1].parse().unwrap();
  let samples: usize = arguments[2].parse().unwrap();
  let mut store = ContentStore::new(ROOM, window).unwrap();
  let mut files = Disk {
    dot_hg: root.join(".hg"),
  };
  let started = Instant::now();
  let mut imported = slates_hg::import::import(&mut files, &mut store, Admission::new(ROOM))
    .map_err(|refused| format!("{refused:?}"))
    .unwrap();
  println!(
    "imported {} changesets, {} manifests, {} file revisions in {:.1} s (window {window})",
    imported.changesets,
    imported.manifests,
    imported.file_revisions,
    started.elapsed().as_secs_f64()
  );
  let changesets = imported.changesets;
  let manifests = imported.manifests;
  let mut rng = 0x2026_1121_u64;
  let mut next = move || {
    rng ^= rng << 13;
    rng ^= rng >> 7;
    rng ^= rng << 17;
    rng
  };
  let picks: Vec<(bool, u32)> = (0..samples)
    .map(|at| {
      let changeset = at % 2 == 0;
      let of = if changeset { changesets } else { manifests };
      (changeset, (next() % u64::from(of.max(1))) as u32)
    })
    .collect();
  let measure = |label: &str, imported: &mut slates_hg::import::Imported, store: &ContentStore| {
    let counters = imported.index.rows().counters();
    let mut micros = Vec::with_capacity(samples);
    let mut visits = Vec::with_capacity(samples);
    for (changeset, rev) in &picks {
      let before = imported.index.rows().visits();
      let started = Instant::now();
      let found = if *changeset {
        imported.index.changeset(store, *rev).is_some()
      } else {
        imported
          .index
          .revision(store, Log::Manifest, *rev)
          .is_some()
      };
      micros.push(started.elapsed().as_secs_f64() * 1e6);
      visits.push(imported.index.rows().visits().saturating_sub(before) as f64);
      assert!(found, "a revision the import kept");
    }
    println!(
      "{label}: seals {} compactions {} runs merged {}; lookup µs p50 {:.1} p99 {:.1}; runs visited p50 {:.0} p99 {:.0} max {:.0}",
      counters.seals,
      counters.compactions,
      counters.runs_merged,
      percentile(&mut micros.clone(), 500),
      percentile(&mut micros, 990),
      percentile(&mut visits.clone(), 500),
      percentile(&mut visits.clone(), 990),
      percentile(&mut visits, 1000),
    );
  };
  measure("after the import", &mut imported, &store);
  let started = Instant::now();
  let rows = imported
    .index
    .rows()
    .compact(&mut store, usize::MAX)
    .unwrap();
  println!(
    "merged every run the index alone holds: {rows} rows in {:.2} s",
    started.elapsed().as_secs_f64()
  );
  measure("after one merge", &mut imported, &store);
}
