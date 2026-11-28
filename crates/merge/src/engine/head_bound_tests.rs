//! Every read names its version (A-752 part 3c U0; main 01a20182's rulings of 2026-11-28 01:35 and 01:42): a reader at
//! the green's head reads the latest entry at or below the head, never a history's last entry as the head's value, so a
//! commit that appends its version's entries in place before it publishes is unseen by every reader until its head
//! moves. Held over generated works by a twin carrying an entry above its head in every history, a copy record and a
//! change-index entry above it: each reader at every version up to the head reads the twin's.

use proptest::prelude::*;

use super::copy::Appearance;
use super::fold_tests::{PATHS, change};
use super::replaced_node_tests::Work;
use super::{ChangeEntry, Green, Increment};

/// What every reader of `green` reads at each version up to its head: each path's bytes and every dimension, the
/// changed paths between each version and the head, the change index's walk, and a probe work's verdict at the head
/// and the version before it.
fn read_to_head(green: &mut Green) -> String {
  let head = green.head();
  let mut read = format!("head {head}\n");
  for version in 0..=head {
    for path in PATHS {
      read.push_str(&format!(
        "{version} {path}: {:?} {:?} {} {:?} {:?} {:?} {:?}\n",
        green.content_ref_at_bytes(path, version),
        green.special_at(path, version),
        green.dir_at(path, version),
        green.mode_at(path, version),
        green.symlink_at(path, version),
        green.hardlink_at(path, version),
        green.xattr_at(path, "user.k", version),
      ));
    }
    read.push_str(&format!(
      "changed since {version}: {:?}\n",
      green.changed_between(version, head)
    ));
  }
  let walk: Vec<String> = green
    .changes_after(0, None)
    .map(|change| format!("{change:?}"))
    .collect();
  read.push_str(&format!("walk {walk:?}\n"));
  read.push_str(&probes(green));
  read
}

/// The verdict of a probe work changing each path in every way, based at the head and at the version before it.
fn probes(green: &mut Green) -> String {
  let mut read = String::new();
  let head = green.head();
  for base in [head, head.saturating_sub(1)] {
    for (id, path) in (1_000u64..).zip(PATHS) {
      for how in 0u8..10 {
        let probe: Increment = change(Work::default(), path, how, 7).at(base, id);
        read.push_str(&format!(
          "probe {base} {path} {how}: {:?}\n",
          green.judge(&probe)
        ));
      }
    }
  }
  read
}

/// Carries `green` an entry above its head in every history, a copy record above it over `a`'s tree and a change-index
/// entry above it: what a commit appending its version in place leaves before it publishes.
fn carried_above_head(green: &mut Green) {
  let above = green.head().saturating_add(1);
  for path in PATHS {
    let value = crate::history::Value::whole(&mut green.chunks, b"above the head");
    // The content history is a lineage `Dim` (A-920 arena unit 1), its copied nodes holding their chunks (A-752 part
    // 3c U8): its entry list changed in place, or made.
    let mut holders = crate::history::HistoryHolders {
      chunks: &mut green.chunks,
      blocks: green.content.blocks_mut(),
    };
    match green.content_history.get_mut_held(path, &mut holders) {
      Some(history) => history.push((above, Some(value))),
      None => {
        green
          .content_history
          .insert_held(path, vec![(above, Some(value))], &mut holders);
      }
    }
    push(&mut green.special_history, path.to_owned(), (above, None));
    push(&mut green.dir_history, path.to_owned(), (above, true));
    push(
      &mut green.mode_history,
      path.to_owned(),
      (above, Some(0o777)),
    );
    push(
      &mut green.symlink_history,
      path.to_owned(),
      (above, Some("above".to_owned())),
    );
    push(
      &mut green.hardlink_history,
      path.to_owned(),
      (above, Some("above".to_owned())),
    );
    push(
      &mut green.xattr_history,
      (path.to_owned(), "user.k".to_owned()),
      (above, Some(b"above".to_vec())),
    );
  }
  green
    .copies
    .entry("a".to_owned())
    .or_default()
    .push(Appearance {
      version: above,
      source: "e".to_owned(),
      source_version: 0,
      fix: None,
    });
  green.changes.entry(above).or_default().push(ChangeEntry {
    index: 0,
    path: "a".to_owned(),
    last: true,
  });
}

/// Appends `entry` to `key`'s history in `histories`.
fn push<K: Ord, T>(
  histories: &mut std::collections::BTreeMap<K, Vec<(u64, T)>>,
  key: K,
  entry: (u64, T),
) {
  histories.entry(key).or_default().push(entry);
}

/// The lines `carried` reads otherwise than `read`, each pair.
fn differing(carried: &str, read: &str) -> String {
  carried
    .lines()
    .zip(read.lines())
    .filter(|(carried, read)| carried != read)
    .map(|(carried, read)| format!("carrying: {carried}\n   plain: {read}"))
    .collect::<Vec<_>>()
    .join("\n")
}

/// The works [`proptest`] generates: one to eleven, each of one to four changes.
fn works() -> impl Strategy<Value = Vec<(Vec<(usize, u8)>, u8)>> {
  prop::collection::vec(
    (
      prop::collection::vec((0usize..5, any::<u8>()), 1..5),
      any::<u8>(),
    ),
    1..12,
  )
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::default()))]

  /// A-752 part 3c U0 (oracle). Do: give two greens the same generated works, each submitted whole; then carry one an
  /// entry above its head in every history, a copy record above it over `a`'s tree and a change-index entry above it
  /// ([`carried_above_head`]). Expect every reader of the two at every version up to the head to read alike
  /// ([`read_to_head`]). Red before: readers took a history's last entry, the latest copy record and the change index
  /// unbounded as the head's.
  #[test]
  fn a_reader_at_the_head_reads_no_entry_above_it(steps in works()) {
    let (mut plain, mut carrying) = (Green::new(), Green::new());
    for (id, (changes, mark)) in (0u64..).zip(&steps) {
      let mut work = Work::default();
      for (index, how) in changes {
        work = change(work, PATHS[*index], *how, *mark);
      }
      let inc = work.at(plain.head(), id);
      let _ = plain.submit_resolved(&inc);
      let _ = carrying.submit_resolved(&inc);
    }
    carried_above_head(&mut carrying);
    let (carried, read) = (read_to_head(&mut carrying), read_to_head(&mut plain));
    prop_assert!(carried == read, "{}", differing(&carried, &read));
  }
}
