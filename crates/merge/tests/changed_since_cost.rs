//! Measurement (not a gate): what `changed_since` costs on a large green with a long history when it reports every
//! dimension (`Green::changed_between(v, head)`) against the content index alone (`Green::changed_since`, before
//! 2026-11-08), §4.16. Skips loudly without the tree; run in release:
//! `SLATES_KERNEL_TREE=<linux-v7.4-rc1> cargo test --release -p slates-merge --test changed_since_cost -- --ignored --nocapture`.
// Test harness code: an unwrap here is a failed run.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::disallowed_methods,
  clippy::cast_precision_loss,
  clippy::cast_possible_truncation
)]

mod common;

use std::time::Instant;

use common::Build;
use slates_merge::engine::Green;
use slates_merge::origin::Origin;

/// Shape: the submits of the history, each changing a few files.
const SUBMITS: u64 = 2_000;
/// Shape: the files each submit overwrites.
const FILES_A_SUBMIT: usize = 20;
/// Shape: every this many submits also changes a symlink, a mode, an xattr and a hard link.
const OTHER_DIMENSIONS_EVERY: u64 = 10;
/// Shape: the paths one page holds at these names (about 150 in a 4,096-byte reply, measured 2026-11-08).
const PAGE_PATHS: usize = 150;
/// Shape: the best of this many reads is reported.
const ROUNDS: usize = 5;

/// Every file under `root`, as paths relative to it.
fn files_under(root: &std::path::Path) -> Vec<String> {
  let mut out = Vec::new();
  let mut stack = vec![root.to_path_buf()];
  while let Some(dir) = stack.pop() {
    for entry in std::fs::read_dir(&dir).unwrap() {
      let entry = entry.unwrap();
      let kind = entry.file_type().unwrap();
      if kind.is_dir() {
        stack.push(entry.path());
      } else if kind.is_file() {
        out.push(
          entry
            .path()
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .into_owned(),
        );
      }
    }
  }
  out.sort();
  out
}

/// The best of [`ROUNDS`] runs of `read`, in nanoseconds, and its answer's length.
fn best(mut read: impl FnMut() -> usize) -> (u128, usize) {
  let mut fastest = u128::MAX;
  let mut len = 0;
  for _ in 0..ROUNDS {
    let started = Instant::now();
    len = read();
    fastest = fastest.min(started.elapsed().as_nanos());
  }
  (fastest, len)
}

#[test]
#[ignore = "measurement, not a gate; needs SLATES_KERNEL_TREE"]
fn changed_since_over_every_dimension_against_the_content_index() {
  let Some(root) = std::env::var_os("SLATES_KERNEL_TREE") else {
    eprintln!("skipped: set SLATES_KERNEL_TREE to a kernel tree");
    return;
  };
  let paths = files_under(std::path::Path::new(&root));
  let origin = Origin {
    files: paths
      .iter()
      .map(|path| (path.clone(), path.as_bytes().to_vec()))
      .collect(),
    ..Origin::default()
  };
  let built = Instant::now();
  let mut green = Green::with_origin(origin);
  eprintln!("seeded {} files in {:?}", paths.len(), built.elapsed());
  let built = Instant::now();
  for submit in 1..=SUBMITS {
    let mut build = Build::new();
    for n in 0..FILES_A_SUBMIT {
      let index = (submit as usize * 7919 + n * 104_729) % paths.len();
      build.overwrite(&paths[index], 0, &submit.to_le_bytes());
    }
    if submit % OTHER_DIMENSIONS_EVERY == 0 {
      let target = &paths[submit as usize % paths.len()];
      build
        .symlink(&format!("slates-cost/link-{submit}"), target)
        .setmode(target, 0o600 + (submit % 64) as u32)
        .setxattr(target, "user.cost", &submit.to_le_bytes())
        .link(&format!("slates-cost/hard-{submit}"), target);
    }
    let mut id = [0u8; 32];
    id[..8].copy_from_slice(&submit.to_le_bytes());
    let outcome = green.submit(&build.with_id(id, green.head()));
    assert!(
      matches!(outcome, slates_merge::engine::Outcome::Accepted { .. }),
      "submit {submit}: {outcome:?}"
    );
  }
  eprintln!("history of {SUBMITS} submits in {:?}", built.elapsed());
  let head = green.head();
  for version in [0, head / 2, head.saturating_sub(10)] {
    let expected = green.changed_between(version, head);
    let walked: Vec<String> = green.changed_since_after(version, None).cloned().collect();
    assert_eq!(
      walked, expected,
      "the merged walk is changed_between(v, head) exactly"
    );
    let (whole_ns, whole_len) = best(|| green.changed_between(version, head).len());
    let (merged_ns, merged_len) = best(|| green.changed_since_after(version, None).count());
    let (paged_ns, pages) = best(|| {
      let mut after: Option<String> = None;
      let mut pages = 0usize;
      loop {
        let page: Vec<String> = green
          .changed_since_after(version, after.as_deref())
          .take(PAGE_PATHS)
          .cloned()
          .collect();
        pages += 1;
        match page.last() {
          Some(last) if page.len() == PAGE_PATHS => after = Some(last.clone()),
          _ => break,
        }
      }
      pages
    });
    eprintln!(
      "since version {version} of {head}: changed_between {whole_ns} ns ({whole_len} paths) | merged walk {merged_ns} \
       ns ({merged_len}) | paged walk {paged_ns} ns ({pages} pages of {PAGE_PATHS}, {} ns a page)",
      paged_ns / pages.max(1) as u128
    );
  }
}
