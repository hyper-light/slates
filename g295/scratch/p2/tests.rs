/// A green at version 3 whose trunk changed after version 1 in every dimension it holds: `a` overwritten and its mode
/// changed, `c` removed, `new` made and `d/b`'s property changed at 2; `d` given a property and `e` made at 3.
fn trunk_changed_after_1() -> Green {
  let mut green = trunk();
  let mut second = Work::default();
  second
    .overwrite("trunk/a", 0, b"AL")
    .unlink("trunk/c")
    .create("trunk/new", b"new")
    .xattr("trunk/d/b", PROPS, b"q2")
    .mode("trunk/a", 0o600);
  assert_eq!(accept(&mut green, &second, 2), 2);
  let mut third = Work::default();
  third.xattr("trunk/d", PROPS, b"pd").mkdir("trunk/e");
  assert_eq!(accept(&mut green, &third, 3), 3);
  green
}

/// What `base` holds of `root`'s tree, every path moved to its place in `to`'s, in [`sorted`]'s order.
fn tree_moved(base: &Base, root: &str, to: &str) -> Base {
  let place = |path: &str| relative(path, root).map(|rel| joined(to, rel));
  sorted(Base {
    files: base
      .files
      .iter()
      .filter_map(|(path, len)| Some((place(path)?, *len)))
      .collect(),
    specials: Vec::new(),
    dirs: base.dirs.iter().filter_map(|path| place(path)).collect(),
    modes: base
      .modes
      .iter()
      .filter_map(|(path, mode)| Some((place(path)?, *mode)))
      .collect(),
    symlinks: base
      .symlinks
      .iter()
      .filter_map(|(path, target)| Some((place(path)?, target.clone())))
      .collect(),
    xattrs: base
      .xattrs
      .iter()
      .filter_map(|(path, name, value)| Some((place(path)?, name.clone(), value.clone())))
      .collect(),
    hardlinks: Vec::new(),
  })
}

/// Submits `work` at `green`'s head as increment `id`, which must be accepted: the increment and what its entry
/// records.
fn submit_recorded(
  green: &mut Green,
  work: &Work,
  id: u64,
) -> (Increment, Vec<chain_entry::Applied>) {
  let increment = work.at(id, green.head());
  let (outcome, applied) = green.submit_resolved(&increment);
  assert!(
    matches!(outcome, Outcome::Accepted { .. }),
    "increment {id}: {outcome:?}"
  );
  (increment, applied.expect("an accepted version's effects"))
}

/// Replays `entries`, each encoded and decoded, on `replica`, and asserts it reads at every version and hashes as
/// `green`.
fn assert_replay_reads_alike(
  green: &mut Green,
  mut replica: Green,
  entries: &[(Increment, Vec<chain_entry::Applied>)],
) {
  for (increment, applied) in entries {
    let entry = chain_entry::encode(&increment.encode(), increment, applied, None, None, None);
    let decoded = chain_entry::decode(&entry).expect("the entry decodes");
    assert_eq!(&decoded.applied, applied);
    replica.apply_recorded(&decoded.increment, &decoded.applied);
  }
  assert_eq!(replica.head(), green.head());
  for version in 0..=green.head() {
    assert_eq!(
      replica.base_at(version),
      green.base_at(version),
      "the replay at {version}"
    );
  }
  assert_eq!(replica.head_identity(), green.head_identity());
}

/// Asserts that `applied` records a fix, that `room` (the green's, derived before the verdict) is its size, and that
/// the encoded entry fits the increment's own bound plus `room`.
fn assert_fits_its_room(increment: &Increment, applied: &[chain_entry::Applied], room: usize) {
  let fixes: usize = applied
    .iter()
    .filter_map(|applied| match applied {
      chain_entry::Applied::CopyTree { fix: Some(fix), .. } => Some(chain_entry::fix_bytes(fix)),
      _ => None,
    })
    .sum();
  assert!(fixes > 0, "no fix recorded");
  assert_eq!(room, fixes);
  let bytes = increment.encode();
  let entry = chain_entry::encode(&bytes, increment, applied, None, None, None);
  let bound = chain_entry::entry_bound(bytes.len(), increment) + room;
  assert!(entry.len() <= bound, "{} bytes past {bound}", entry.len());
}

/// A-626 part 2 (a copy from an older version of a source that changed since, as FSFS accepts it). Do: copy the trunk
/// at version 1 to `b` after versions 2 and 3 changed it in every dimension, and replay that version's chain entry on
/// a green holding the version before. Expect: accepted as version 4; `b` holds the trunk as it stood at version 1 (the
/// trunk's base at 1 moved to `b`) and the trunk keeps its head; the change index names `b` alone; no map refused the
/// copy; the identity equals its recount; the entry records the fix within the room the green derived for it before
/// the verdict, and a copy of an unchanged source needs none; and the replica reads and hashes alike.
#[test]
fn a_copy_from_an_older_version_brings_the_source_as_it_stood() {
  let mut green = trunk_changed_after_1();
  let increment = one_copy("trunk", "b", 1).at(4, 3);
  let room = green.copy_entry_room(&increment);
  let (outcome, applied) = green.submit_resolved(&increment);
  assert_eq!(outcome, Outcome::Accepted { version: 4 });
  assert_holds_the_trunk_at(&green, "b");
  assert_eq!(
    tree_moved(&green.base_at(1), "trunk", "b"),
    tree_moved(&green.base_at(4), "b", "b")
  );
  assert_eq!(green.content("trunk/a"), Some(&b"ALpha"[..]));
  assert_eq!(green.changed_between(3, 4), ["b"]);
  assert_eq!(green.copies_refused(), 0);
  assert_eq!(green.head_identity(), green.head_identity_from_scratch());
  let applied = applied.expect("an accepted version's effects");
  assert_fits_its_room(&increment, &applied, room);
  assert_eq!(green.copy_entry_room(&one_copy("trunk", "z", 3).at(9, 4)), 0);
  assert_replays_alike(&mut green, trunk_changed_after_1(), &increment, &applied);
}

/// A-626 part 2 (copies from before a copy inside the source and from before one over it). Do: on [`trunk_and_t2`],
/// copy `t2` to `trunk/t` (version 3), the trunk at version 2 to `b` (4), replace the trunk with `t2` (5), and copy the
/// trunk at version 4 to `c` (6); replay every one of those entries on a green holding version 2. Expect: each copy
/// holds the trunk as the green reads it at the copy's version (`b` without `t`, the inner copy cut; `c` the trunk
/// before its replace, `t` among it); the identity equals its recount; the replica reads at every version and hashes
/// alike.
#[test]
fn copies_from_before_a_copy_inside_or_over_the_source_bring_it_as_it_stood() {
  let mut green = trunk_and_t2();
  let entries = vec![
    submit_recorded(&mut green, &one_copy("t2", "trunk/t", 2), 3),
    submit_recorded(&mut green, &one_copy("trunk", "b", 2), 4),
    submit_recorded(&mut green, &replace_copy("t2", "trunk", 2), 5),
    submit_recorded(&mut green, &one_copy("trunk", "c", 4), 6),
  ];
  assert_eq!(
    tree_moved(&green.base_at(2), "trunk", "b"),
    tree_moved(&green.base_at(6), "b", "b")
  );
  assert_eq!(
    tree_moved(&green.base_at(4), "trunk", "c"),
    tree_moved(&green.base_at(6), "c", "c")
  );
  assert_eq!(green.content("c/t/f"), Some(&b"f2"[..]));
  assert_eq!(green.content("b/t/f"), None);
  assert_eq!(green.copies_refused(), 0);
  assert_eq!(green.head_identity(), green.head_identity_from_scratch());
  assert_replay_reads_alike(&mut green, trunk_and_t2(), &entries);
}

/// A-626 part 2 (an edit beneath a copy from an older version in the copy's own increment: a working copy's commit of
/// `svn cp -r 1 trunk b` with an edit). Do: in one increment copy the trunk at version 1 to `b` and overwrite `b/a` at
/// its fourth byte, after version 2 changed `trunk/a`'s first two; replay the entry on a green holding the version
/// before. Expect: `b/a` holds the edit spliced into the bytes `trunk/a` held at 1 (`alpHA`, not the head's `ALpHA`),
/// the identity equals its recount, and the replica reads and hashes alike.
#[test]
fn an_edit_beneath_a_copy_from_an_older_version_lands_on_the_bytes_it_brings() {
  let mut green = trunk_changed_after_1();
  let mut work = Work::exclusive();
  work.copy("trunk", "b", 1).overwrite("b/a", 3, b"HA");
  let increment = work.at(4, 3);
  let (outcome, applied) = green.submit_resolved(&increment);
  assert_eq!(outcome, Outcome::Accepted { version: 4 });
  assert_eq!(green.content("b/a"), Some(&b"alpHA"[..]));
  assert_eq!(green.head_identity(), green.head_identity_from_scratch());
  let applied = applied.expect("an accepted version's effects");
  assert_replays_alike(&mut green, trunk_changed_after_1(), &increment, &applied);
}

/// A-626 part 2 (a copy from a version whose values are folded away). Do: fold the green's histories below version 3,
/// then copy the trunk at version 1 (changed at 2, so its values then are needed) and at version 3 (unchanged since).
/// Expect: the first a type conflict naming its destination, the green unchanged; the second accepted.
#[test]
fn a_copy_from_a_folded_version_is_refused_typed() {
  let mut green = trunk_changed_after_1();
  green.fold_history_before(3);
  let head = green.head();
  let stale = green.submit(&one_copy("trunk", "b", 1).at(4, head));
  assert_eq!(
    windows(&stale),
    Some(vec![("b".to_owned(), MergeConflictClass::TypeChanged)])
  );
  assert_eq!(green.head(), head);
  assert_eq!(
    green.submit(&one_copy("trunk", "c", 3).at(5, head)),
    Outcome::Accepted { version: 4 }
  );
}

