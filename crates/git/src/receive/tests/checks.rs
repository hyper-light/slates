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
