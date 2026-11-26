//! The pushed objects' fsck and the hidden refs (A-620; §4.17.2 "A push" item 4; git's
//! `receive.fsckObjects`, `transfer.hideRefs` and `receive.hideRefs`).

use super::{
  ADVERTISED, Applies, HIDDEN, Repo, answer_to, bands, commit_text, named, outcomes_hiding,
  pack_of, refs, report_lines, request, scenario, unbounded, zero,
};
use crate::object_name::{Kind, ObjectFormat, ObjectId, name_of};
use crate::objects::{FsckId, Strictness, tree};
use crate::receive::{
  Declined, NoneHidden, Outcome, PackRefusal, PackRefused, fsck, message, receive, visible,
  write_advertisement,
};
use crate::unpack::{self, NoIndex};

/// The name of `content` as an object of `kind`.
fn id_of(kind: Kind, content: &[u8]) -> ObjectId {
  name_of(ObjectFormat::Sha1, kind, content).unwrap()
}

/// A tree entry: `mode SP name NUL` and the id's bytes.
fn tree_entry(mode: &str, name: &str, id: ObjectId) -> Vec<u8> {
  [format!("{mode} {name}\0").as_bytes(), id.as_bytes()].concat()
}

/// The blob every pack here holds first, which no check refuses.
const GOOD: &[u8] = b"good";

/// Objects a push's check refuses, each with the finding git names: a commit with no author email,
/// a tag of an unknown type, a tree holding `.git`, and a tree out of order.
fn refused_objects() -> [(Kind, Vec<u8>, FsckId); 4] {
  let blob = id_of(Kind::Blob, GOOD);
  let empty_tree = id_of(Kind::Tree, b"");
  let commit = format!(
    "tree {empty_tree}\nauthor A 1 +0000\ncommitter A <a@example.com> 1 +0000\n\nno email\n"
  );
  let tag = format!("object {blob}\ntype bogus\ntag t\ntagger A <a@example.com> 1 +0000\n\nm\n");
  let unsorted = [
    tree_entry("100644", "b", blob),
    tree_entry("100644", "a", blob),
  ]
  .concat();
  [
    (Kind::Commit, commit.into_bytes(), FsckId::MissingEmail),
    (Kind::Tag, tag.into_bytes(), FsckId::BadType),
    (
      Kind::Tree,
      tree_entry("100644", ".git", blob),
      FsckId::HasDotgit,
    ),
    (Kind::Tree, unsorted, FsckId::TreeNotSorted),
  ]
}

/// Asserts that a push creating `refs/heads/x` at `id` with a pack of `objects` is refused for
/// `id`'s `finding`, answered as git answers an fsck error, its command never judged.
fn assert_refused_by_fsck(objects: &[(Kind, &[u8])], id: ObjectId, finding: FsckId) {
  let label = finding.name();
  let command: [(ObjectId, ObjectId, &[u8]); 1] = [(zero(), id, b"refs/heads/x")];
  let capabilities = b" report-status side-band-64k";
  let bytes = request(&[], &command, capabilities, &[], &pack_of(objects));
  let mut judge = Applies::default();
  let received = receive(
    &bytes,
    &ADVERTISED,
    &Repo::default(),
    &NoneHidden,
    &mut judge,
    &mut unbounded(),
  )
  .unwrap();
  let refused = PackRefused {
    objects: Some(u32::try_from(objects.len()).unwrap()),
    why: PackRefusal::Fsck { id, finding },
  };
  assert_eq!(received.unpacked, Some(Err(refused)), "{label}");
  assert!(judge.0.is_empty(), "{label}");
  let (report, text) = bands(&answer_to(&received));
  let lines = [
    "unpack unpack-objects abnormal exit".to_owned(),
    "ng refs/heads/x unpacker error".to_owned(),
  ];
  assert_eq!(report, report_lines(&lines), "{label}");
  let error = message(format!("object {id}: {label}").as_bytes());
  let fatal = b"fatal: fsck error in packed object\n";
  assert_eq!(text, [error.as_slice(), fatal].concat(), "{label}");
}

/// A-620 (fsck before any ref moves: §4.17.2 "A push" item 4; git's `receive.fsckObjects`, its
/// unpacking under `--strict`). Do: push each object a push's check refuses, after a good blob in
/// a pack of their own, creating `refs/heads/x` at it. Expect: the pack refused for that object's
/// finding; the command `unpacker error`, never judged; the report `unpack unpack-objects abnormal
/// exit`, and on band 2 `error: object <id>: <finding>`, then `fatal: fsck error in packed
/// object`.
#[test]
fn every_object_a_push_brings_is_checked_before_any_ref_moves() {
  for (kind, content, finding) in refused_objects() {
    let objects: [(Kind, &[u8]); 2] = [(Kind::Blob, GOOD), (kind, &content)];
    assert_refused_by_fsck(&objects, id_of(kind, &content), finding);
  }
}

/// A-620 (a push's strictness: git's `--strict`, under which a warning refuses). Do: check a tree
/// holding `.git` as `git fsck` checks by default, then the objects of a pack holding it as a
/// push's check does. Expect: kept by the default check, `hasDotgit` a warning; refused by the
/// push's, `hasDotgit`.
#[test]
fn a_push_refuses_what_a_default_fsck_only_warns_of() {
  let dotgit = tree_entry("100644", ".git", id_of(Kind::Blob, GOOD));
  let default = tree::fsck(
    &dotgit,
    ObjectFormat::Sha1,
    Strictness::Default,
    &mut unbounded(),
  )
  .unwrap();
  assert_eq!(default.report.refusal(), None);
  let warnings: Vec<FsckId> = default.report.warnings().collect();
  assert_eq!(warnings, [FsckId::HasDotgit]);
  let pack = pack_of(&[(Kind::Tree, &dotgit)]);
  let unpacked = unpack::read_whole(&pack, ObjectFormat::Sha1, &NoIndex, &mut unbounded()).unwrap();
  let refused = fsck::first_refusal(&unpacked.objects, ObjectFormat::Sha1);
  assert_eq!(
    refused,
    Some((id_of(Kind::Tree, &dotgit), FsckId::HasDotgit))
  );
}

/// A-620 (the object named is the first refused in pack order). Do: push a pack holding a tag of
/// an unknown type, then a commit with no author email. Expect: the tag named, `badType`.
#[test]
fn the_first_object_refused_in_pack_order_is_named() {
  let [commit, tag, ..] = refused_objects();
  let objects: [(Kind, &[u8]); 2] = [(tag.0, &tag.1), (commit.0, &commit.1)];
  assert_refused_by_fsck(&objects, id_of(tag.0, &tag.1), FsckId::BadType);
}

/// A refusal for `reason`, with no band-2 line.
fn refused_with(reason: &[u8]) -> Outcome {
  Outcome::Ng(Declined {
    reason: reason.to_vec(),
    hook: Vec::new(),
    message: None,
  })
}

/// A-620 (hidden refs left out of the advertisement: `show_ref`). Do: write the captured hidden
/// push's server's advertisement, `refs/hidden` hidden, and again with nothing hidden. Expect: the
/// first the bytes git advertised; the second not, since the server holds `refs/hidden/x`.
#[test]
fn a_hidden_ref_is_left_out_of_the_advertisement() {
  let hidden = scenario("hidden");
  let refs = refs(hidden.before_refs);
  let named = named(&refs);
  assert!(named.iter().any(|(_, name)| *name == b"refs/hidden/x"));
  let written = |shown: Vec<(ObjectId, &[u8])>| {
    let mut out = Vec::new();
    write_advertisement(&shown, &ADVERTISED, &mut out).unwrap();
    out
  };
  let hiding = written(visible(&named, &HIDDEN));
  assert!(hidden.response.starts_with(&hiding));
  let showing = written(visible(&named, &NoneHidden));
  assert!(!hidden.response.starts_with(&showing));
}

/// A-620 (hidden refs out of reach: `reject_updates_to_hidden`). Do: push to a repository whose
/// `refs/hidden` is hidden a create of `refs/hidden/y`, an update of `refs/hidden/x`, a delete of
/// `refs/hidden/z`, a create of `refs/hidden` itself (a name git's ref-name check would refuse
/// after), and a create of `refs/heads/topic`, every tip complete. Expect: the hidden creates and
/// update refused `deny updating a hidden ref`, the delete `deny deleting a hidden ref`, none
/// judged; `refs/heads/topic` judged and applied.
#[test]
fn a_hidden_ref_is_neither_moved_nor_judged() {
  let mut repository = Repo::default();
  let tree = repository.with(Kind::Tree, b"");
  let old = repository.with(Kind::Commit, &commit_text(tree, &[]));
  let new_text = commit_text(tree, &[old]);
  let new = id_of(Kind::Commit, &new_text);
  let commands: [(ObjectId, ObjectId, &[u8]); 5] = [
    (zero(), new, b"refs/hidden/y"),
    (old, new, b"refs/hidden/x"),
    (old, zero(), b"refs/hidden/z"),
    (zero(), new, b"refs/hidden"),
    (zero(), new, b"refs/heads/topic"),
  ];
  let pack = pack_of(&[(Kind::Commit, &new_text)]);
  let bytes = request(&[], &commands, b" report-status", &[], &pack);
  let (outcomes, judged) = outcomes_hiding(&bytes, &repository, &HIDDEN);
  let update = refused_with(b"deny updating a hidden ref");
  let delete = refused_with(b"deny deleting a hidden ref");
  let applied = Outcome::Ok(Vec::new());
  let expected = [update.clone(), update.clone(), delete, update, applied];
  assert_eq!(outcomes, expected);
  assert_eq!(judged, [b"refs/heads/topic".to_vec()]);
}

/// A-620 (git's order: `set_connectivity_errors` before `reject_updates_to_hidden`). Do: create a
/// hidden ref at a commit whose tree nothing holds. Expect: `missing necessary objects`, the
/// connectivity check's reason, not the hidden ref's.
#[test]
fn a_hidden_ref_missing_objects_is_refused_for_its_objects_first() {
  let lacking = commit_text(id_of(Kind::Tree, b"absent"), &[]);
  let command: [(ObjectId, ObjectId, &[u8]); 1] =
    [(zero(), id_of(Kind::Commit, &lacking), b"refs/hidden/y")];
  let pack = pack_of(&[(Kind::Commit, &lacking)]);
  let bytes = request(&[], &command, b" report-status", &[], &pack);
  let (outcomes, _) = outcomes_hiding(&bytes, &Repo::default(), &HIDDEN);
  assert_eq!(outcomes, [refused_with(b"missing necessary objects")]);
}

/// A-620 (a hidden ref in an atomic push). Do: push, with `atomic`, a create of `refs/hidden/y`
/// and one of `refs/heads/topic`. Expect: the hidden one refused `deny updating a hidden ref`, the
/// other `atomic push failure`, nothing judged.
#[test]
fn an_atomic_push_naming_a_hidden_ref_moves_no_ref() {
  let mut repository = Repo::default();
  let tree = repository.with(Kind::Tree, b"");
  let commit = commit_text(tree, &[]);
  let id = id_of(Kind::Commit, &commit);
  let commands: [(ObjectId, ObjectId, &[u8]); 2] = [
    (zero(), id, b"refs/hidden/y"),
    (zero(), id, b"refs/heads/topic"),
  ];
  let pack = pack_of(&[(Kind::Commit, &commit)]);
  let bytes = request(&[], &commands, b" report-status atomic", &[], &pack);
  let (outcomes, judged) = outcomes_hiding(&bytes, &repository, &HIDDEN);
  let expected = [
    refused_with(b"deny updating a hidden ref"),
    refused_with(b"atomic push failure"),
  ];
  assert_eq!(outcomes, expected);
  assert!(judged.is_empty());
}

/// Git 2.56.1 `.gitmodules` and `.gitattributes` blob fsck: hostile `.gitmodules` (bad name, `-` path, `-` or `%0a`
/// url, `!` update, non-blob, missing blob) and hostile `.gitattributes` (line >= 2048 bytes, non-blob, missing blob)
/// are refused on push before any ref moves, while valid `.gitmodules` and `.gitattributes` blobs pass.
#[test]
fn special_dotfile_blobs_are_checked_on_push_as_git_checks_them() {
  let bad_cases: [(&str, &[u8], FsckId); 7] = [
    (
      ".gitmodules",
      b"[submodule \"../escape\"]\n\tpath = sub\n\turl = https://example.com/r.git\n",
      FsckId::GitmodulesName,
    ),
    (
      ".gitmodules",
      b"[submodule \"a\\\\..\\\\b\"]\n\tpath = sub\n\turl = https://example.com/r.git\n",
      FsckId::GitmodulesName,
    ),
    (
      ".gitmodules",
      b"[submodule \"ok\"]\n\tpath = -dash\n\turl = https://example.com/r.git\n",
      FsckId::GitmodulesPath,
    ),
    (
      ".gitmodules",
      b"[submodule \"ok\"]\n\tpath = sub\n\turl = -u./payload\n",
      FsckId::GitmodulesUrl,
    ),
    (
      ".gitmodules",
      b"[submodule \"ok\"]\n\tpath = sub\n\turl = https://example.com/a%0ab\n",
      FsckId::GitmodulesUrl,
    ),
    (
      ".gitmodules",
      b"[submodule \"ok\"]\n\tpath = sub\n\turl = https://example.com/r.git\n\tupdate = !evil\n",
      FsckId::GitmodulesUpdate,
    ),
    (
      ".gitattributes",
      &[b'a'; 2048],
      FsckId::GitattributesLineLength,
    ),
  ];
  for (name, blob_bytes, expected) in bad_cases {
    let blob_id = id_of(Kind::Blob, blob_bytes);
    let tree_bytes = tree_entry("100644", name, blob_id);
    // Test with tree BEFORE blob in pack order so deferred special-blob checking is exercised.
    let objects: [(Kind, &[u8]); 2] = [(Kind::Tree, &tree_bytes), (Kind::Blob, blob_bytes)];
    assert_refused_by_fsck(&objects, blob_id, expected);
  }
  // Non-blob at `.gitmodules` and `.gitattributes` (directory mode 40000).
  let empty_tree = id_of(Kind::Tree, b"");
  for (name, expected) in [
    (".gitmodules", FsckId::GitmodulesBlob),
    (".gitattributes", FsckId::GitattributesBlob),
  ] {
    let tree_bytes = tree_entry("40000", name, empty_tree);
    let objects: [(Kind, &[u8]); 2] = [(Kind::Tree, b""), (Kind::Tree, &tree_bytes)];
    assert_refused_by_fsck(&objects, empty_tree, expected);
  }
  // Missing `.gitmodules` blob.
  let missing_id = id_of(Kind::Blob, b"never sent");
  let tree_bytes = tree_entry("100644", ".gitmodules", missing_id);
  let objects: [(Kind, &[u8]); 1] = [(Kind::Tree, &tree_bytes)];
  assert_refused_by_fsck(&objects, missing_id, FsckId::GitmodulesMissing);
  // Valid `.gitmodules` and 2047-byte `.gitattributes` line pass.
  let ok_modules = b"[submodule \"ok\"]\n\tpath = sub\n\turl = https://example.com/r.git\n";
  let mut ok_attrs = vec![b'a'; 2047];
  ok_attrs.push(b'\n');
  let mod_id = id_of(Kind::Blob, ok_modules);
  let attr_id = id_of(Kind::Blob, &ok_attrs);
  let tree_bytes = [
    tree_entry("100644", ".gitattributes", attr_id),
    tree_entry("100644", ".gitmodules", mod_id),
  ]
  .concat();
  let tree_id = id_of(Kind::Tree, &tree_bytes);
  let commit = commit_text(tree_id, &[]);
  let commit_id = id_of(Kind::Commit, &commit);
  let pack = pack_of(&[
    (Kind::Blob, ok_modules),
    (Kind::Blob, &ok_attrs),
    (Kind::Tree, &tree_bytes),
    (Kind::Commit, &commit),
  ]);
  let bytes = request(
    &[],
    &[(zero(), commit_id, b"refs/heads/ok")],
    b" report-status",
    &[],
    &pack,
  );
  let mut judge = Applies::default();
  let received = receive(
    &bytes,
    &ADVERTISED,
    &Repo::default(),
    &NoneHidden,
    &mut judge,
    &mut unbounded(),
  )
  .unwrap();
  assert!(received.unpacked.as_ref().is_some_and(Result::is_ok));
  assert_eq!(received.outcomes, [Outcome::Ok(Vec::new())]);
  // Also verify the streaming `Session` path refuses a hostile `.gitmodules` blob even when the tree arrives before it.
  let bad_modules = b"[submodule \"ok\"]\n\tpath = sub\n\turl = -u./evil\n";
  let bad_id = id_of(Kind::Blob, bad_modules);
  let bad_tree = tree_entry("100644", ".gitmodules", bad_id);
  let bad_tree_id = id_of(Kind::Tree, &bad_tree);
  let bad_pack = pack_of(&[(Kind::Tree, &bad_tree), (Kind::Blob, bad_modules)]);
  let bad_req = request(
    &[],
    &[(zero(), bad_tree_id, b"refs/heads/evil")],
    b" report-status",
    &[],
    &bad_pack,
  );
  let mut session = crate::receive::Session::new(&ADVERTISED, NoneHidden);
  let mut admission = unbounded();
  assert!(
    session
      .input(&bad_req, &Repo::default(), &mut admission)
      .unwrap()
  );
  struct Still;
  impl crate::store::learn::Clock for Still {
    fn now_ns(&mut self) -> u64 {
      0
    }
  }
  let mut clock = Still;
  while !session
    .check_turn(
      &Repo::default(),
      &mut admission,
      crate::store::hook_view::Quantum {
        clock: &mut clock,
        ns: u64::MAX,
      },
    )
    .unwrap()
  {}
  let checked = session.checked().unwrap();
  assert_eq!(
    checked.unpacked,
    Some(Err(PackRefused {
      objects: Some(2),
      why: PackRefusal::Fsck {
        id: bad_id,
        finding: FsckId::GitmodulesUrl,
      },
    }))
  );
}

/// Builds an annotated tag targeting `target` with declared type `declared_type`.
fn tag_text(target: ObjectId, declared_type: &str, name: &str) -> Vec<u8> {
  format!(
    "object {target}\ntype {declared_type}\ntag {name}\ntagger A <a@example.com> 1 +0000\n\nm\n"
  )
  .into_bytes()
}

/// §4.17.2 "A push" item 5 (connectivity edge-kind parity with stock Git): do push every
/// wrong-kind edge shape inside the incoming pack (tag `type commit` -> blob, commit `tree` ->
/// blob, tree `40000` -> blob, tree `100644`/`100755`/`120000` -> tree, commit `parent` -> blob
/// both normal and shallow), plus a valid gitlink `160000` to an absent commit and an atomic batch
/// pairing a valid branch with a wrong-kind edge. Expect: every wrong-kind edge is refused
/// `missing necessary objects` (never admitted, never `shallow update not allowed`, and rolling
/// back atomic batches), while the gitlink succeeds.
#[test]
fn wrong_kind_object_edges_in_pack_are_refused() {
  let blob_bytes = b"hello blob\n";
  let blob_id = id_of(Kind::Blob, blob_bytes);
  let empty_tree_id = id_of(Kind::Tree, b"");
  let valid_commit = commit_text(empty_tree_id, &[]);
  let valid_commit_id = id_of(Kind::Commit, &valid_commit);

  // 1. Tag whose `type` header says `commit` while its target is a `blob`.
  let bad_tag_blob = tag_text(blob_id, "commit", "bad-tag-blob");
  let bad_tag_blob_id = id_of(Kind::Tag, &bad_tag_blob);
  // 1b. Tag whose `type` header says `blob` while its target is a `tree`.
  let bad_tag_tree = tag_text(empty_tree_id, "blob", "bad-tag-tree");
  let bad_tag_tree_id = id_of(Kind::Tag, &bad_tag_tree);

  // 2. Commit whose `tree` points to a `blob`.
  let bad_commit_tree = commit_text(blob_id, &[]);
  let bad_commit_tree_id = id_of(Kind::Commit, &bad_commit_tree);

  // 3. Tree entry with mode `40000` (tree) pointing to a `blob`.
  let dir_to_blob_tree = tree_entry("40000", "sub", blob_id);
  let dir_to_blob_tree_id = id_of(Kind::Tree, &dir_to_blob_tree);
  let dir_to_blob_commit = commit_text(dir_to_blob_tree_id, &[]);
  let dir_to_blob_commit_id = id_of(Kind::Commit, &dir_to_blob_commit);

  // 4. Tree entries with blob modes (`100644`, `100755`, `120000`) pointing to a `tree`.
  let file_to_tree = tree_entry("100644", "f", empty_tree_id);
  let file_to_tree_commit = commit_text(id_of(Kind::Tree, &file_to_tree), &[]);
  let file_to_tree_commit_id = id_of(Kind::Commit, &file_to_tree_commit);

  let exec_to_tree = tree_entry("100755", "x", empty_tree_id);
  let exec_to_tree_commit = commit_text(id_of(Kind::Tree, &exec_to_tree), &[]);
  let exec_to_tree_commit_id = id_of(Kind::Commit, &exec_to_tree_commit);

  let link_to_tree = tree_entry("120000", "l", empty_tree_id);
  let link_to_tree_commit = commit_text(id_of(Kind::Tree, &link_to_tree), &[]);
  let link_to_tree_commit_id = id_of(Kind::Commit, &link_to_tree_commit);

  // 5. Commit whose `parent` points to a `blob`.
  let bad_parent_commit = commit_text(empty_tree_id, &[blob_id]);
  let bad_parent_commit_id = id_of(Kind::Commit, &bad_parent_commit);

  // Positive control: tree entry with mode `160000` (gitlink) pointing to an absent commit.
  let absent_sub = id_of(Kind::Commit, b"external submodule commit");
  let gitlink_tree = tree_entry("160000", "mod", absent_sub);
  let gitlink_commit = commit_text(id_of(Kind::Tree, &gitlink_tree), &[]);
  let gitlink_commit_id = id_of(Kind::Commit, &gitlink_commit);

  let pack = pack_of(&[
    (Kind::Blob, blob_bytes),
    (Kind::Tree, b""),
    (Kind::Commit, &valid_commit),
    (Kind::Tag, &bad_tag_blob),
    (Kind::Tag, &bad_tag_tree),
    (Kind::Commit, &bad_commit_tree),
    (Kind::Tree, &dir_to_blob_tree),
    (Kind::Commit, &dir_to_blob_commit),
    (Kind::Tree, &file_to_tree),
    (Kind::Commit, &file_to_tree_commit),
    (Kind::Tree, &exec_to_tree),
    (Kind::Commit, &exec_to_tree_commit),
    (Kind::Tree, &link_to_tree),
    (Kind::Commit, &link_to_tree_commit),
    (Kind::Commit, &bad_parent_commit),
    (Kind::Tree, &gitlink_tree),
    (Kind::Commit, &gitlink_commit),
  ]);
  let commands: [(ObjectId, ObjectId, &[u8]); 9] = [
    (zero(), bad_tag_blob_id, b"refs/tags/bad-tag-blob"),
    (zero(), bad_tag_tree_id, b"refs/tags/bad-tag-tree"),
    (zero(), bad_commit_tree_id, b"refs/heads/bad-commit-tree"),
    (zero(), dir_to_blob_commit_id, b"refs/heads/dir-to-blob"),
    (zero(), file_to_tree_commit_id, b"refs/heads/file-to-tree"),
    (zero(), exec_to_tree_commit_id, b"refs/heads/exec-to-tree"),
    (zero(), link_to_tree_commit_id, b"refs/heads/link-to-tree"),
    (zero(), bad_parent_commit_id, b"refs/heads/bad-parent"),
    (zero(), gitlink_commit_id, b"refs/heads/gitlink-ok"),
  ];
  let bytes = request(
    &[bad_parent_commit_id],
    &commands,
    b" report-status",
    &[],
    &pack,
  );
  let (outcomes, judged) = outcomes_hiding(&bytes, &Repo::default(), &NoneHidden);
  let missing = refused_with(b"missing necessary objects");
  for (at, (_, _, name)) in commands.iter().take(8).enumerate() {
    assert_eq!(
      outcomes[at],
      missing,
      "expected missing necessary objects for {}",
      String::from_utf8_lossy(name)
    );
  }
  assert_eq!(outcomes[8], Outcome::Ok(Vec::new()));
  assert_eq!(judged, [b"refs/heads/gitlink-ok".to_vec()]);

  // Atomic push pairing a valid branch with a wrong-kind edge must reject the entire batch.
  let atomic_cmds: [(ObjectId, ObjectId, &[u8]); 2] = [
    (zero(), valid_commit_id, b"refs/heads/good"),
    (zero(), bad_tag_blob_id, b"refs/tags/bad-tag"),
  ];
  let atomic_bytes = request(&[], &atomic_cmds, b" report-status atomic", &[], &pack);
  let (atomic_outcomes, atomic_judged) =
    outcomes_hiding(&atomic_bytes, &Repo::default(), &NoneHidden);
  assert_eq!(
    atomic_outcomes,
    [refused_with(b"atomic push failure"), missing]
  );
  assert!(atomic_judged.is_empty());
}

/// §4.17.2 "A push" item 5 (wrong-kind edges targeting objects ALREADY stored in the repository):
/// do push tags, commits, and trees referencing pre-existing repository objects of the wrong kind.
/// Expect: every wrong-kind edge is refused `missing necessary objects`, while matching-kind edges
/// to the same repository objects succeed.
#[test]
fn wrong_kind_object_edges_to_existing_repository_objects_are_refused() {
  let mut repository = Repo::default();
  let existing_blob = repository.with(Kind::Blob, b"already stored blob\n");
  let existing_tree = repository.with(Kind::Tree, b"");
  let existing_commit = repository.with(Kind::Commit, &commit_text(existing_tree, &[]));

  // Wrong-kind edges referencing existing repository objects (targets NOT resent in pack).
  let bad_tag = tag_text(existing_blob, "commit", "bad-tag-existing");
  let bad_commit_tree = commit_text(existing_blob, &[]);
  let bad_dir_tree = tree_entry("40000", "d", existing_blob);
  let bad_dir_commit = commit_text(id_of(Kind::Tree, &bad_dir_tree), &[]);
  let bad_file_tree = tree_entry("100644", "f", existing_tree);
  let bad_file_commit = commit_text(id_of(Kind::Tree, &bad_file_tree), &[]);
  let bad_parent_commit = commit_text(existing_tree, &[existing_tree]);

  // Valid matching-kind edges referencing the exact same existing repository objects.
  let ok_tag = tag_text(existing_commit, "commit", "ok-tag-existing");
  let ok_tree = [
    tree_entry("40000", "d", existing_tree),
    tree_entry("100644", "f", existing_blob),
  ]
  .concat();
  let ok_commit = commit_text(id_of(Kind::Tree, &ok_tree), &[existing_commit]);

  let pack = pack_of(&[
    (Kind::Tag, &bad_tag),
    (Kind::Commit, &bad_commit_tree),
    (Kind::Tree, &bad_dir_tree),
    (Kind::Commit, &bad_dir_commit),
    (Kind::Tree, &bad_file_tree),
    (Kind::Commit, &bad_file_commit),
    (Kind::Commit, &bad_parent_commit),
    (Kind::Tag, &ok_tag),
    (Kind::Tree, &ok_tree),
    (Kind::Commit, &ok_commit),
  ]);
  let commands: [(ObjectId, ObjectId, &[u8]); 7] = [
    (zero(), id_of(Kind::Tag, &bad_tag), b"refs/tags/bad-tag"),
    (
      zero(),
      id_of(Kind::Commit, &bad_commit_tree),
      b"refs/heads/bad-tree",
    ),
    (
      zero(),
      id_of(Kind::Commit, &bad_dir_commit),
      b"refs/heads/bad-dir",
    ),
    (
      zero(),
      id_of(Kind::Commit, &bad_file_commit),
      b"refs/heads/bad-file",
    ),
    (
      zero(),
      id_of(Kind::Commit, &bad_parent_commit),
      b"refs/heads/bad-parent",
    ),
    (zero(), id_of(Kind::Tag, &ok_tag), b"refs/tags/ok-tag"),
    (
      zero(),
      id_of(Kind::Commit, &ok_commit),
      b"refs/heads/ok-commit",
    ),
  ];
  let bytes = request(&[], &commands, b" report-status", &[], &pack);
  let (outcomes, judged) = outcomes_hiding(&bytes, &repository, &NoneHidden);
  let missing = refused_with(b"missing necessary objects");
  for at in 0..5 {
    assert_eq!(outcomes[at], missing, "case {at}");
  }
  assert_eq!(outcomes[5], Outcome::Ok(Vec::new()));
  assert_eq!(outcomes[6], Outcome::Ok(Vec::new()));
  assert_eq!(
    judged,
    [
      b"refs/tags/ok-tag".to_vec(),
      b"refs/heads/ok-commit".to_vec()
    ]
  );
}

/// §4.17.2 "A push" (branch refs under `refs/heads/` must point to a commit, matching stock Git
/// `refs.c` / `receive-pack.c`): do push non-commit targets (blob, tree, annotated tag — both in
/// the incoming pack and already in the repository) to `refs/heads/*`, alongside valid branch
/// commits and non-commit `refs/tags/*` refs, both non-atomic and `--atomic`, plus a branch delete.
/// Expect: every non-delete `refs/heads/*` pointing to a non-commit is refused `failed to update
/// ref` with Git's non-commit diagnostic and never judged; `--atomic` rolls back the entire batch
/// with `atomic push failure`; valid branches, non-commit tags, and branch deletes succeed.
#[test]
fn branch_ref_pointing_to_non_commit_is_refused_atomic_and_non_atomic() {
  let mut repository = Repo::default();
  let existing_blob = repository.with(Kind::Blob, b"existing repo blob\n");
  let existing_tree = repository.with(Kind::Tree, b"");

  let pack_blob = b"incoming pack blob\n";
  let pack_blob_id = id_of(Kind::Blob, pack_blob);
  let pack_tree = tree_entry("100644", "a", pack_blob_id);
  let pack_tree_id = id_of(Kind::Tree, &pack_tree);
  let commit = commit_text(pack_tree_id, &[]);
  let commit_id = id_of(Kind::Commit, &commit);
  let tag = tag_text(commit_id, "commit", "v1");
  let tag_id = id_of(Kind::Tag, &tag);

  let pack = pack_of(&[
    (Kind::Blob, pack_blob),
    (Kind::Tree, &pack_tree),
    (Kind::Commit, &commit),
    (Kind::Tag, &tag),
  ]);

  // Non-atomic: bad branches refused, good branch and non-commit tags applied.
  let commands: [(ObjectId, ObjectId, &[u8]); 8] = [
    (zero(), commit_id, b"refs/heads/good"),
    (zero(), pack_blob_id, b"refs/heads/blob-pack"),
    (zero(), pack_tree_id, b"refs/heads/tree-pack"),
    (zero(), tag_id, b"refs/heads/tag-pack"),
    (zero(), existing_blob, b"refs/heads/blob-repo"),
    (zero(), existing_tree, b"refs/heads/tree-repo"),
    (zero(), pack_blob_id, b"refs/tags/blob-ok"),
    (zero(), tag_id, b"refs/tags/v1"),
  ];
  let bytes = request(&[], &commands, b" report-status side-band-64k", &[], &pack);
  let (outcomes, judged) = outcomes_hiding(&bytes, &repository, &NoneHidden);
  assert_eq!(outcomes[0], Outcome::Ok(Vec::new()));
  for at in 1..6 {
    match &outcomes[at] {
      Outcome::Ng(declined) => {
        assert_eq!(declined.reason, b"failed to update ref", "case {at}");
        let msg = declined.message.as_deref().unwrap_or_default();
        assert!(
          String::from_utf8_lossy(msg).contains("non-commit"),
          "case {at}: {msg:?}"
        );
      }
      other => panic!("case {at} should be refused, got {other:?}"),
    }
  }
  assert_eq!(outcomes[6], Outcome::Ok(Vec::new()));
  assert_eq!(outcomes[7], Outcome::Ok(Vec::new()));
  assert_eq!(
    judged,
    [
      b"refs/heads/good".to_vec(),
      b"refs/tags/blob-ok".to_vec(),
      b"refs/tags/v1".to_vec(),
    ]
  );

  // Atomic push with incoming-pack blob on refs/heads/bad -> rolls back refs/heads/good too.
  let atomic_pack_cmds: [(ObjectId, ObjectId, &[u8]); 2] = [
    (zero(), commit_id, b"refs/heads/good"),
    (zero(), pack_blob_id, b"refs/heads/bad"),
  ];
  let atomic_bytes = request(&[], &atomic_pack_cmds, b" report-status atomic", &[], &pack);
  let (atomic_outcomes, atomic_judged) = outcomes_hiding(&atomic_bytes, &repository, &NoneHidden);
  assert_eq!(atomic_outcomes[0], refused_with(b"atomic push failure"));
  assert!(matches!(
    &atomic_outcomes[1],
    Outcome::Ng(declined) if declined.reason == b"failed to update ref"
  ));
  assert!(atomic_judged.is_empty());

  // Atomic push with already-stored repository blob on refs/heads/bad -> rolls back too.
  let atomic_repo_cmds: [(ObjectId, ObjectId, &[u8]); 2] = [
    (zero(), commit_id, b"refs/heads/good"),
    (zero(), existing_blob, b"refs/heads/bad-repo"),
  ];
  let atomic_repo_bytes = request(&[], &atomic_repo_cmds, b" report-status atomic", &[], &pack);
  let (atomic_repo_outcomes, atomic_repo_judged) =
    outcomes_hiding(&atomic_repo_bytes, &repository, &NoneHidden);
  assert_eq!(
    atomic_repo_outcomes[0],
    refused_with(b"atomic push failure")
  );
  assert!(matches!(
    &atomic_repo_outcomes[1],
    Outcome::Ng(declined) if declined.reason == b"failed to update ref"
  ));
  assert!(atomic_repo_judged.is_empty());

  // Deleting a branch whose old value was a blob must still be permitted.
  let delete_bytes = request(
    &[],
    &[(existing_blob, zero(), b"refs/heads/old-bad")],
    b" report-status",
    &[],
    &[],
  );
  let (delete_outcomes, delete_judged) = outcomes_hiding(&delete_bytes, &repository, &NoneHidden);
  assert_eq!(delete_outcomes, [Outcome::Ok(Vec::new())]);
  assert_eq!(delete_judged, [b"refs/heads/old-bad".to_vec()]);

  // A funny branch refname pointing to a blob must still report `funny refname` (not masked).
  let funny_bytes = request(
    &[],
    &[(zero(), pack_blob_id, b"refs/heads/a..b")],
    b" report-status",
    &[],
    &pack,
  );
  let (funny_outcomes, funny_judged) = outcomes_hiding(&funny_bytes, &repository, &NoneHidden);
  assert!(matches!(
    &funny_outcomes[0],
    Outcome::Ng(declined) if declined.reason == b"funny refname"
  ));
  assert!(funny_judged.is_empty());

  // Wrong-kind edge deep in a pushed chain (tag -> commit -> root tree -> dir `40000` -> blob)
  // propagates back to the ref tip and rejects it.
  let deep_bad_sub = tree_entry("40000", "leaf", pack_blob_id);
  let deep_bad_sub_id = id_of(Kind::Tree, &deep_bad_sub);
  let deep_root = tree_entry("40000", "nested", deep_bad_sub_id);
  let deep_commit = commit_text(id_of(Kind::Tree, &deep_root), &[]);
  let deep_commit_id = id_of(Kind::Commit, &deep_commit);
  let deep_tag = tag_text(deep_commit_id, "commit", "deep-bad");
  let deep_tag_id = id_of(Kind::Tag, &deep_tag);
  let deep_pack = pack_of(&[
    (Kind::Blob, pack_blob),
    (Kind::Tree, &deep_bad_sub),
    (Kind::Tree, &deep_root),
    (Kind::Commit, &deep_commit),
    (Kind::Tag, &deep_tag),
  ]);
  let deep_bytes = request(
    &[],
    &[(zero(), deep_tag_id, b"refs/tags/deep-bad")],
    b" report-status",
    &[],
    &deep_pack,
  );
  let (deep_outcomes, deep_judged) = outcomes_hiding(&deep_bytes, &Repo::default(), &NoneHidden);
  assert_eq!(deep_outcomes, [refused_with(b"missing necessary objects")]);
  assert!(deep_judged.is_empty());
}

/// §4.17.2 "A push" (let-go pack window during `Walk` and `Stage::BranchTargets`):
/// do wrap a repository whose `kind` lookup returns `None` with `away() == Some(offset)` on the
/// first probe, then bring the window back and retry `Session::check_turn`.
/// Expect: both connectivity edge-kind checks and `refs/heads/*` branch-target checks pause on the
/// first turn (`Ok(false)` with `away().is_some()`), then after bring-back complete with exact
/// accept/refuse outcomes (no premature `missing necessary objects` or false non-commit refusal).
#[test]
fn away_repository_kind_pauses_walk_and_branch_target_check_until_brought_back() {
  struct AwayRepo<'a> {
    inner: &'a Repo,
    evicted: std::cell::Cell<Option<ObjectId>>,
    away: std::cell::Cell<Option<u64>>,
    pauses: std::cell::Cell<usize>,
  }

  impl crate::receive::Repository for AwayRepo<'_> {
    fn object(&self, id: &ObjectId) -> Option<(Kind, &[u8])> {
      self.inner.object(id)
    }

    fn holds(&self, id: &ObjectId) -> bool {
      self.inner.holds(id)
    }

    fn kind(&self, id: &ObjectId) -> Option<Kind> {
      if self.evicted.get() == Some(*id) {
        self.away.set(Some(42));
        self.pauses.set(self.pauses.get() + 1);
        return None;
      }
      self.away.set(None);
      self.inner.kind(id)
    }

    fn away(&self) -> Option<u64> {
      self.away.get()
    }
  }

  let mut base = Repo::default();
  let repo_tree = base.with(Kind::Tree, b"");
  let repo_commit = base.with(Kind::Commit, &commit_text(repo_tree, &[]));
  let repo_blob = base.with(Kind::Blob, b"not a commit\n");

  // 1. Walk pauses when `repository.kind` hits a let-go window for a commit parent, then succeeds.
  let child_commit = commit_text(repo_tree, &[repo_commit]);
  let child_commit_id = id_of(Kind::Commit, &child_commit);
  let pack = pack_of(&[(Kind::Commit, &child_commit)]);
  let bytes = request(
    &[],
    &[(zero(), child_commit_id, b"refs/heads/child")],
    b" report-status",
    &[],
    &pack,
  );
  let repo = AwayRepo {
    inner: &base,
    evicted: std::cell::Cell::new(Some(repo_commit)),
    away: std::cell::Cell::new(None),
    pauses: std::cell::Cell::new(0),
  };
  let mut session = crate::receive::Session::new(&ADVERTISED, NoneHidden);
  let mut admission = unbounded();
  assert_eq!(session.input(&bytes, &repo, &mut admission), Ok(true));
  let mut stopped = crate::receive::connect::Stopped;
  while session.is_resolving() {
    assert_eq!(
      session.check_turn(
        &repo,
        &mut admission,
        crate::receive::connect::unbounded(&mut stopped),
      ),
      Ok(false)
    );
  }
  assert_eq!(
    session.check_turn(
      &repo,
      &mut admission,
      crate::receive::connect::unbounded(&mut stopped),
    ),
    Ok(false),
    "walk must pause when repository.kind reports a let-go window"
  );
  assert_eq!(repo.away.get(), Some(42));
  assert!(repo.pauses.get() > 0);
  // Bring window back and retry.
  repo.evicted.set(None);
  repo.away.set(None);
  assert_eq!(
    session.check_turn(
      &repo,
      &mut admission,
      crate::receive::connect::unbounded(&mut stopped),
    ),
    Ok(true)
  );
  let checked = session.checked().expect("checked after bring-back");
  assert_eq!(checked.early, [None]);

  // 2. Stage::BranchTargets pauses when `refs/heads/*` targets a repo object whose kind is away.
  let ref_bytes = request(
    &[],
    &[
      (zero(), repo_commit, b"refs/heads/ok-repo"),
      (zero(), repo_blob, b"refs/heads/bad-repo"),
    ],
    b" report-status",
    &[],
    &[],
  );
  let repo2 = AwayRepo {
    inner: &base,
    evicted: std::cell::Cell::new(Some(repo_commit)),
    away: std::cell::Cell::new(None),
    pauses: std::cell::Cell::new(0),
  };
  let mut checking = {
    let mut reader = crate::receive::RequestReader::new(&ADVERTISED);
    reader.read(&ref_bytes, &mut unbounded()).unwrap();
    crate::receive::Checking::new(reader.request().unwrap(), None)
  };
  let empty_held = crate::unpack::Held::default();
  let mut adm2 = unbounded();
  assert_eq!(
    checking.turn(
      (&repo2, &empty_held),
      &mut adm2,
      crate::receive::connect::unbounded(&mut stopped),
    ),
    Ok(false),
    "BranchTargets must pause when repository.kind reports a let-go window"
  );
  assert_eq!(repo2.away.get(), Some(42));
  assert!(repo2.pauses.get() > 0);
  repo2.evicted.set(None);
  repo2.away.set(None);
  assert_eq!(
    checking.turn(
      (&repo2, &empty_held),
      &mut adm2,
      crate::receive::connect::unbounded(&mut stopped),
    ),
    Ok(true)
  );
  let checked2 = checking
    .checked((&repo2, &empty_held), &NoneHidden, &mut adm2)
    .unwrap();
  assert_eq!(checked2.early[0], None);
  assert!(matches!(
    &checked2.early[1],
    Some(Outcome::Ng(declined)) if declined.reason == b"failed to update ref"
  ));
}
