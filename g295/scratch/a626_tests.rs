/// A green at version 2 holding the trunk and a branch `b` of its own: a file named as the trunk's is, a file and a
/// directory the trunk has none of, the directory's mode, and the branch's property.
fn trunk_and_branch() -> Green {
  let mut green = trunk();
  let mut own = Work::default();
  own
    .mkdir("b")
    .xattr("b", PROPS, b"pb")
    .create("b/a", b"mine")
    .create("b/only", b"only")
    .mkdir("b/sub")
    .mode("b/sub", 0o700);
  assert_eq!(accept(&mut green, &own, 2), 2);
  green
}

/// One node-exclusive copy of `from` at `version` to `to` that replaces what `to` holds (A-626).
fn replace_copy(from: &str, to: &str, version: u64) -> Work {
  let mut work = Work::exclusive();
  work.replace(from, to, version);
  work
}

/// Asserts that `green` holds the trunk's tree of [`trunk`] at `at`.
fn assert_holds_the_trunk_at(green: &Green, at: &str) {
  let path = |rel: &str| joined(at, rel);
  assert_eq!(green.content(&path("a")), Some(&b"alpha"[..]));
  assert_eq!(green.content(&path("c")), Some(&b"gamma"[..]));
  assert_eq!(green.content(&path("d/b")), Some(&b"beta"[..]));
  assert!(green.is_dir(at) && green.is_dir(&path("d")));
  assert_eq!(green.mode(at), Some(0o755));
  assert_eq!(green.mode(&path("a")), Some(0o644));
  assert_eq!(green.xattr(at, PROPS), Some(&b"p1"[..]));
  assert_eq!(green.xattr(&path("d/b"), PROPS), Some(&b"p2"[..]));
}

/// Asserts that `green` read at version 2 holds the branch `b` of [`trunk_and_branch`], byte for byte.
fn assert_holds_the_branch_at_2(green: &Green) {
  assert_eq!(green.content_at("b/a", 2), Some(b"mine".to_vec()));
  assert_eq!(green.content_at("b/only", 2), Some(b"only".to_vec()));
  let before = green.base_at(2);
  assert!(before.dirs.contains(&"b/sub".to_owned()));
  assert!(before.modes.contains(&("b/sub".to_owned(), 0o700)));
  assert!(
    before
      .xattrs
      .contains(&("b".to_owned(), PROPS.to_owned(), b"pb".to_vec()))
  );
  assert!(!before.files.iter().any(|(path, _)| path == "b/c"));
}

/// Replays `increment`'s chain entry (`applied`, encoded and decoded) on `replica`, which holds the version before it,
/// and asserts it reads at every version and hashes as `green`.
fn assert_replays_alike(
  green: &mut Green,
  mut replica: Green,
  increment: &Increment,
  applied: &[chain_entry::Applied],
) {
  let entry = chain_entry::encode(&increment.encode(), increment, applied, None, None, None);
  let decoded = chain_entry::decode(&entry).expect("the entry decodes");
  assert_eq!(decoded.applied, applied);
  assert_eq!(
    replica.apply_recorded(&decoded.increment, &decoded.applied),
    green.head()
  );
  assert_eq!(replica.head_identity(), green.head_identity());
  for version in 0..=green.head() {
    assert_eq!(
      replica.base_at(version),
      green.base_at(version),
      "the replay at {version}"
    );
  }
}

/// A-626 (Subversion's replace with history, §4.17.4 item 3.5: a copy onto a path its own commit removes). Do: replace
/// the branch `b` of [`trunk_and_branch`] with the trunk at version 1, node-exclusively, then replay that version's
/// chain entry on a green holding the version before. Expect: accepted as version 3; `b` holds the trunk's tree and
/// nothing of its own (`b/only`, `b/sub` and its mode gone, `b/a` the trunk's bytes, `b`'s property the trunk's); at
/// version 2 its own tree, byte for byte; the change index names `b` and every path the replace took away; no map
/// refused the copy; the identity equals its recount and the change index's bytes theirs; and the replica reads at
/// every version and hashes alike.
#[test]
fn a_replace_takes_its_destination_away_and_copies_the_source_there() {
  let mut green = trunk_and_branch();
  let increment = replace_copy("trunk", "b", 1).at(3, 2);
  let (outcome, applied) = green.submit_resolved(&increment);
  assert_eq!(outcome, Outcome::Accepted { version: 3 });
  assert_holds_the_trunk_at(&green, "b");
  assert_eq!(green.content("b/only"), None);
  assert!(!green.is_dir("b/sub"));
  assert_eq!(green.mode("b/sub"), None);
  assert_holds_the_branch_at_2(&green);
  assert_eq!(green.changed_between(2, 3), ["b", "b/a", "b/only", "b/sub"]);
  assert_eq!(green.copies_refused(), 0);
  assert_eq!(green.head_identity(), green.head_identity_from_scratch());
  assert_eq!(green.changes_bytes(), green.changes_recounted());
  let applied = applied.expect("an accepted version's effects");
  assert_replays_alike(&mut green, trunk_and_branch(), &increment, &applied);
}

/// A-626 (the histories a replace leaves: a key it took away that the copy gave back, changed later; a directory it took
/// away that the copy did not give back, made again later). Do: replace the branch `b` with the trunk at version 3, then
/// at version 4 overwrite `b/a` and make `b/sub` again with another mode. Expect: `b/a` reads the branch's bytes at 2,
/// the trunk's at 3 and the new ones at 4; `b/sub` stands at 2 with its first mode, not at 3, and again at 4 with its
/// new mode; and the identity equals its recount.
#[test]
fn a_key_a_replace_took_away_reads_at_every_version_once_it_changes_again() {
  let mut green = trunk_and_branch();
  accept(&mut green, &replace_copy("trunk", "b", 1), 3);
  accept(
    &mut green,
    Work::exclusive()
      .overwrite("b/a", 0, b"AL")
      .mkdir("b/sub")
      .mode("b/sub", 0o711),
    4,
  );
  let bytes: Vec<Option<Vec<u8>>> = (2..=4)
    .map(|version| green.content_at("b/a", version))
    .collect();
  assert_eq!(
    bytes,
    [
      Some(b"mine".to_vec()),
      Some(b"alpha".to_vec()),
      Some(b"ALpha".to_vec())
    ]
  );
  let stood: Vec<(bool, Option<u32>)> = (2..=4)
    .map(|version| {
      let base = green.base_at(version);
      let mode = base
        .modes
        .iter()
        .find(|(path, _)| path == "b/sub")
        .map(|(_, mode)| *mode);
      (base.dirs.contains(&"b/sub".to_owned()), mode)
    })
    .collect();
  assert_eq!(
    stood,
    [(true, Some(0o700)), (false, None), (true, Some(0o711))]
  );
  assert_eq!(green.head_identity(), green.head_identity_from_scratch());
}

/// A green at version 2 holding the trunk and `t2`, a second tree: a directory holding a file and a directory with a
/// file.
fn trunk_and_t2() -> Green {
  let mut green = trunk();
  let mut work = Work::default();
  work
    .mkdir("t2")
    .create("t2/f", b"f2")
    .mkdir("t2/e")
    .create("t2/e/g", b"g2");
  assert_eq!(accept(&mut green, &work, 2), 2);
  green
}

/// Asserts that `green` holds `t2`'s tree of [`trunk_and_t2`] at `at`.
fn assert_holds_t2_at(green: &Green, at: &str) {
  assert!(green.is_dir(at) && green.is_dir(&joined(at, "e")));
  assert_eq!(green.content(&joined(at, "f")), Some(&b"f2"[..]));
  assert_eq!(green.content(&joined(at, "e/g")), Some(&b"g2"[..]));
}

/// A-626 (copies into a copied tree in one commit: Subversion's second copy into a new branch, and its move inside one, a
/// copy and a removal). Do: in one node-exclusive increment, copy the trunk at version 2 to `b`, `t2` into `b/new` (a
/// place the trunk's tree leaves empty), `t2` over `b/d` replacing it (a place the trunk's tree fills), and move `b/a` to
/// `b/moved` as a stock client does (the trunk's `a` copied there, `b/a` removed); then replay that version's chain entry
/// on a green holding the version before. Expect: accepted as version 3; `b/new` and `b/d` hold `t2`'s tree, and `b/d/b`
/// and its property are gone; `b/moved` holds the trunk's `a`, and `b/a` is gone; the rest of `b` is the trunk's;
/// version 2 holds nothing of `b`; the change index names the four destinations, `b/a`, and what the replace took away
/// (`b/d/b`); no map refused a copy; the identity equals its recount; and the replica reads and hashes alike.
#[test]
fn copies_nest_in_one_increment_and_a_move_inside_a_copy_lands() {
  let mut green = trunk_and_t2();
  let mut work = Work::exclusive();
  work
    .copy("trunk", "b", 2)
    .copy("t2", "b/new", 2)
    .replace("t2", "b/d", 2)
    .copy("trunk/a", "b/moved", 2)
    .unlink("b/a");
  let increment = work.at(3, 2);
  let (outcome, applied) = green.submit_resolved(&increment);
  assert_eq!(outcome, Outcome::Accepted { version: 3 });
  assert_holds_t2_at(&green, "b/new");
  assert_holds_t2_at(&green, "b/d");
  assert_eq!(green.content("b/d/b"), None);
  assert_eq!(green.xattr("b/d/b", PROPS), None);
  assert_eq!(green.content("b/moved"), Some(&b"alpha"[..]));
  assert_eq!(green.content("b/a"), None);
  assert_eq!(green.content("b/c"), Some(&b"gamma"[..]));
  assert_eq!(green.xattr("b", PROPS), Some(&b"p1"[..]));
  assert!(
    base_paths(&green.base_at(2))
      .iter()
      .all(|path| !in_tree(path, "b"))
  );
  assert_eq!(
    green.changed_between(2, 3),
    ["b", "b/a", "b/d", "b/d/b", "b/moved", "b/new"]
  );
  assert_eq!(green.copies_refused(), 0);
  assert_eq!(green.head_identity(), green.head_identity_from_scratch());
  let applied = applied.expect("an accepted version's effects");
  assert_replays_alike(&mut green, trunk_and_t2(), &increment, &applied);
}

/// A-626 (a replace's charge, secured before its verdict, §4.2 all-cost admission): its history and change-index
/// reservations cover what its commit adds. Do: hold a branch of 64 files, reserve for replacing it with the trunk, and
/// for copying the trunk onto an empty path, then commit the replace. Expect: each of the replace's reservations at
/// least what its total grew by, and more than the plain copy's: the replaced tree is charged.
#[test]
fn a_replaces_reservations_cover_what_its_commit_adds() {
  let mut green = trunk();
  let mut work = Work::default();
  for index in 0..64u32 {
    work.create(&format!("b/{index}"), &index.to_le_bytes());
  }
  accept(&mut green, &work, 2);
  let replace = replace_copy("trunk", "b", 1).at(3, 2);
  let plain = one_copy("trunk", "e", 1).at(4, 2);
  let reserved = (
    green.history_reservation(&replace),
    green.changes_reservation(&replace),
  );
  let alone = (
    green.history_reservation(&plain),
    green.changes_reservation(&plain),
  );
  let before = (green.retained_bytes().history, green.changes_bytes());
  assert_eq!(green.submit(&replace), Outcome::Accepted { version: 3 });
  let grown = (
    green.retained_bytes().history - before.0,
    green.changes_bytes() - before.1,
  );
  assert!(
    grown.0 <= reserved.0 && grown.1 <= reserved.1,
    "grew {grown:?}, reserved {reserved:?}"
  );
  assert!(
    reserved.0 > alone.0 && reserved.1 > alone.1,
    "the replace reserved {reserved:?}, a copy alone {alone:?}"
  );
}

/// [`trunk_and_branch`] with the versions of [`a_branch_before_a_replace_holds_the_tree_it_took_away`] up to `head`:
/// the branch replaced with the trunk at 3, and at 4 the trunk copied to `e` with `b/d` copied into it.
fn replaced_up_to(head: u64) -> Green {
  let mut green = trunk_and_branch();
  if head >= 3 {
    accept(&mut green, &replace_copy("trunk", "b", 1), 3);
  }
  if head >= 4 {
    accept(
      &mut green,
      Work::exclusive()
        .copy("trunk", "e", 3)
        .copy("b/d", "e/x", 3),
      4,
    );
  }
  green
}

/// A-626 (a green made at another's version, before and after a replace and copies nested in one increment). Do: replace
/// the branch `b` at version 3, copy the trunk to `e` with `b/d` copied into it at 4, and branch the green at every
/// version. Expect: each branch reads as a green that applied the same versions reads at its head, and hashes as it; no
/// branch walks a map.
#[test]
fn a_branch_before_a_replace_holds_the_tree_it_took_away() {
  let expected: Vec<([u8; 32], Base)> = (1..=4)
    .map(|version| {
      let mut prefix = replaced_up_to(version);
      (prefix.head_identity(), sorted(prefix.base_at(version)))
    })
    .collect();
  let (mut lineage, source) = Lineage::new(replaced_up_to(4));
  let walked = lineage.walked();
  for (version, expected) in (1..=4).zip(expected) {
    let branch = lineage.branch(source, version).expect("a branch");
    let found = lineage
      .with(branch, |green| {
        (green.head_identity(), sorted(green.base_at(0)))
      })
      .expect("the branch");
    assert_eq!(found, expected, "the branch at {version}");
  }
  assert_eq!(lineage.walked(), walked, "a branch walked a map");
}
