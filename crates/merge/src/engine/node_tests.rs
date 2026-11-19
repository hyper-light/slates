//! A hard link names its node, never a path (A-646; A-643's second sibling; §4.16 "The verdict",
//! POSIX `link`, `unlink` and `rename`). A node is its home, the one path holding its content or
//! IPC node, and the hard links naming it there. Removing or replacing the home leaves the node to
//! its links, the least of them its new home with its content, mode and attributes; renaming the
//! home moves its links with it; and a later file at a path the node left is another node, which
//! no link names. A link a work makes names the node its base held at the target.

use std::collections::BTreeMap;

use super::replaced_node_tests::{
  FILE, HARDLINK, Kind, NAME, SPECIAL, TARGET, Work, accept, classes_on, green, kinds_at,
  removal_of,
};
use super::{ConflictWindow, Green, Increment, Outcome, Rebased};
use crate::increment::{DeriveError, VolumeOp, compose_volume_planned};
use crate::ops_doc::OpKind;
use crate::special::SpecialNode;
use crate::verdict::MergeConflictClass;

/// Shape: a second hard link to `TARGET` the tests make.
const SECOND: &str = "second";
/// Shape: where a node is made after the base of the work that links to it.
const MADE: &str = "made";

/// A green whose `TARGET` (mode 0o600, `NAME` set to `w`) is named by `HARDLINK` and `SECOND`.
fn linked() -> Result<Green, String> {
  let mut green = green();
  accept(
    &mut green,
    Work::default().naming(OpKind::Link, SECOND, TARGET),
    1,
  )?;
  accept(&mut green, Work::default().mode(TARGET, 0o600), 2)?;
  accept(&mut green, Work::default().set_xattr(TARGET, NAME, b"w"), 3)?;
  Ok(green)
}

/// What a green answers of `path` as a node's home: its kinds, bytes, mode and `NAME`'s value.
type Home = (Vec<Kind>, Option<Vec<u8>>, Option<u32>, Option<Vec<u8>>);

/// What `green` answers of `path` as a node's home.
fn home(green: &Green, path: &str) -> Home {
  (
    kinds_at(green, path),
    green.content(path).map(<[u8]>::to_vec),
    green.mode(path),
    green.xattr(path, NAME).map(<[u8]>::to_vec),
  )
}

/// `TARGET`'s node as `linked` made it, held at a home.
fn targets_node() -> Home {
  (
    vec![Kind::File],
    Some(b"<g>\n".to_vec()),
    Some(0o600),
    Some(b"w".to_vec()),
  )
}

/// A-646. Do: remove `TARGET`, which `HARDLINK` and `SECOND` name, by a work based at the head.
/// Expect it accepted and the node kept at its links: `HARDLINK`, the least, holds its bytes,
/// mode and attribute, `SECOND` names it there, and nothing stands at `TARGET`. Before, both links
/// named the empty path.
#[test]
fn a_removed_files_links_keep_its_node() -> Result<(), String> {
  let mut green = linked()?;
  accept(&mut green, removal_of(TARGET, Kind::File), 4)?;
  let found = (
    home(&green, HARDLINK),
    green.hardlink(SECOND),
    kinds_at(&green, TARGET),
  );
  if found != (targets_node(), Some(HARDLINK), Vec::new()) {
    return Err(format!("{found:?}"));
  }
  Ok(())
}

/// A-646. Do: remove `TARGET`, which `HARDLINK` and `SECOND` name, then make a new file there.
/// Expect the new file another node: `SECOND` still names `HARDLINK`, which keeps the old bytes,
/// and no link names `TARGET`. Before, the links named whatever was made at the path.
#[test]
fn a_file_made_again_where_a_node_was_is_another_node() -> Result<(), String> {
  let mut green = linked()?;
  accept(&mut green, removal_of(TARGET, Kind::File), 4)?;
  accept(&mut green, Work::default().create(TARGET, b"<new>\n"), 5)?;
  let found = (
    green.hardlink(SECOND),
    green.content(HARDLINK).map(<[u8]>::to_vec),
    green.content(TARGET).map(<[u8]>::to_vec),
  );
  let expected = (
    Some(HARDLINK),
    Some(b"<g>\n".to_vec()),
    Some(b"<new>\n".to_vec()),
  );
  if found != expected {
    return Err(format!("{found:?}"));
  }
  Ok(())
}

/// A-646 (POSIX `rename` moves the node, and its links name it at its new path). Do: rename
/// `TARGET`, which `HARDLINK` and `SECOND` name, to a free path. Expect both links to name the new
/// path, which holds the bytes. Before, both named the path the file had left.
#[test]
fn a_renamed_files_links_name_it_at_its_new_path() -> Result<(), String> {
  let mut green = linked()?;
  accept(
    &mut green,
    Work::default().naming(OpKind::Rename, "moved", TARGET),
    4,
  )?;
  let found = (
    green.hardlink(HARDLINK),
    green.hardlink(SECOND),
    green.content("moved").map(<[u8]>::to_vec),
  );
  if found != (Some("moved"), Some("moved"), Some(b"<g>\n".to_vec())) {
    return Err(format!("{found:?}"));
  }
  Ok(())
}

/// A-646. Do: rename `FILE` over `TARGET`, which `HARDLINK` and `SECOND` name. Expect `TARGET` to
/// hold `FILE`'s bytes, a new node, and the old node kept at `HARDLINK` with its bytes, mode and
/// attribute, `SECOND` naming it there. Before, the links named the bytes renamed over the path.
#[test]
fn a_file_replaced_by_a_rename_leaves_its_node_to_its_links() -> Result<(), String> {
  let mut green = linked()?;
  accept(
    &mut green,
    Work::default().naming(OpKind::Rename, TARGET, FILE),
    4,
  )?;
  let found = (
    home(&green, HARDLINK),
    green.hardlink(SECOND),
    green.content(TARGET).map(<[u8]>::to_vec),
  );
  if found != (targets_node(), Some(HARDLINK), Some(b"<f>\n".to_vec())) {
    return Err(format!("{found:?}"));
  }
  Ok(())
}

/// A-646. Do: link `pipe` to the FIFO `SPECIAL`, then remove `SPECIAL` by a work based at the
/// head. Expect it accepted and the FIFO kept at `pipe`, nothing at `SPECIAL`. Before, the removal
/// was refused `DeleteModify`, since the link would have named nothing.
#[test]
fn an_ipc_nodes_links_keep_it_when_its_home_is_removed() -> Result<(), String> {
  let mut green = green();
  let fifo = green.special(SPECIAL);
  accept(
    &mut green,
    Work::default().naming(OpKind::Link, "pipe", SPECIAL),
    1,
  )?;
  accept(&mut green, removal_of(SPECIAL, Kind::Special), 2)?;
  let found = (
    kinds_at(&green, "pipe"),
    green.special("pipe"),
    kinds_at(&green, SPECIAL),
  );
  if fifo.is_none() || found != (vec![Kind::Special], fifo, Vec::new()) {
    return Err(format!("{found:?}"));
  }
  Ok(())
}

/// A-646. Do: link `third` to `HARDLINK`, itself a link to `TARGET`, by a work based at the head.
/// Expect `third` to name `TARGET`, the node's home, never the link's path. Before, it named the
/// link.
#[test]
fn a_link_to_a_link_names_its_node() -> Result<(), String> {
  let mut green = green();
  accept(
    &mut green,
    Work::default().naming(OpKind::Link, "third", HARDLINK),
    1,
  )?;
  match green.hardlink("third") {
    Some(TARGET) => Ok(()),
    other => Err(format!("{other:?}")),
  }
}

/// A-646. Do: rename `TARGET` after version 0, then submit a work based at 0 that links `third` to
/// `TARGET`. Expect `DeleteModify` on `third`: the node the work saw there has moved since, and a
/// link never names whatever stands at a path now (A-643's rule, kept).
#[test]
fn a_link_based_before_its_target_moved_conflicts() -> Result<(), String> {
  let mut green = green();
  accept(
    &mut green,
    Work::default().naming(OpKind::Rename, "moved", TARGET),
    1,
  )?;
  let answer = green.submit(
    &Work::default()
      .naming(OpKind::Link, "third", TARGET)
      .at(0, 2),
  );
  let refused = classes_on(&answer, "third") == Some(vec![MergeConflictClass::DeleteModify]);
  if !refused || green.hardlink("third").is_some() {
    return Err(format!("{answer:?}"));
  }
  Ok(())
}

/// A-676 (found by reading, g307). Do: make a file at `MADE` after version 0, then submit a work
/// based at 0 that links `third` to the node at `MADE`; and the same with a FIFO made there.
/// Expect `DeleteModify` on `third` and no link, each time: nothing stood at `MADE` at the work's
/// base, so the link names no node the work saw (A-646), as only a forged increment can, since
/// `link(2)` answers `ENOENT` there. Before, `Green::node_home` judged only a removal since the
/// base and accepted both links, naming nodes made after it.
#[test]
fn a_link_to_a_node_made_after_its_base_conflicts() -> Result<(), String> {
  let made = [
    ("a file", Work::default().create(MADE, b"<made>\n")),
    ("a FIFO", Work::default().fifo(MADE)),
  ];
  let mut misjudged = Vec::new();
  for (kind, work) in made {
    let mut green = green();
    accept(&mut green, work, 1)?;
    let link = Work::default().naming(OpKind::Link, "third", MADE);
    let answer = green.submit(&link.at(0, 2));
    let refused = classes_on(&answer, "third") == Some(vec![MergeConflictClass::DeleteModify]);
    if !refused || green.hardlink("third").is_some() {
      misjudged.push(format!("{kind}: {answer:?}"));
    }
  }
  if misjudged.is_empty() {
    Ok(())
  } else {
    Err(misjudged.join("; "))
  }
}

/// Submits `work` at the head of `live` and replays its chain entry onto `replica`.
fn submit_and_replay(
  live: &mut Green,
  replica: &mut Green,
  work: Work,
  id: u64,
) -> Result<(), String> {
  let increment = work.at(live.head(), id);
  match live.submit_resolved(&increment) {
    (Outcome::Accepted { .. }, Some(applied)) => {
      replica.apply_recorded(&increment, &applied);
      Ok(())
    }
    other => Err(format!("{other:?}")),
  }
}

/// A-646 (the node moves are derived, never recorded). Do: link `SECOND` to `TARGET`, rename it,
/// then remove the renamed home, each version's chain entry replayed onto a second green. Expect
/// both greens' identities equal after every version, and each equal to its recount from scratch:
/// a replay derives the same node moves, and every key they change is marked for the identity.
#[test]
fn a_replay_derives_the_same_node_moves() -> Result<(), String> {
  let (mut live, mut replica) = (green(), green());
  let works = [
    Work::default().naming(OpKind::Link, SECOND, TARGET),
    Work::default().naming(OpKind::Rename, "moved", TARGET),
    removal_of("moved", Kind::File),
  ];
  for (id, work) in (1..).zip(works) {
    submit_and_replay(&mut live, &mut replica, work, id)?;
    let identities = (
      live.head_identity(),
      replica.head_identity(),
      live.head_identity_from_scratch(),
    );
    if identities.0 != identities.1 || identities.0 != identities.2 {
      return Err(format!("version {id}: {identities:?}"));
    }
  }
  let found = (live.hardlink(SECOND), live.content(HARDLINK));
  if found != (Some(HARDLINK), Some(&b"<g>\n"[..])) {
    return Err(format!("{found:?}"));
  }
  Ok(())
}

/// A-646 part 2b (the ops document names a node). Do: create `new` and link `linked` to the node
/// the work makes there, in one work based at the head. Expect it accepted and `linked` naming
/// `new`: a node the work makes is its own, never looked for at the base, where nothing stood.
#[test]
fn a_link_to_a_node_the_work_makes_names_it() -> Result<(), String> {
  let mut green = green();
  accept(
    &mut green,
    Work::default()
      .create("new", b"<new>\n")
      .link_made("linked", "new"),
    1,
  )?;
  match green.hardlink("linked") {
    Some("new") => Ok(()),
    other => Err(format!("{other:?}")),
  }
}

/// A-646 part 2b. Do: link `linked` to a node the work says it makes at `nothing`, which it does
/// not make. Expect `TypeChanged` on `linked` and no link: the document names a node that does not
/// exist.
#[test]
fn a_link_to_a_node_the_work_does_not_make_is_refused() -> Result<(), String> {
  let mut green = green();
  let answer = green.submit(&Work::default().link_made("linked", "nothing").at(0, 1));
  let refused = classes_on(&answer, "linked") == Some(vec![MergeConflictClass::TypeChanged]);
  if !refused || green.hardlink("linked").is_some() {
    return Err(format!("{answer:?}"));
  }
  Ok(())
}

/// What a green answers of `linked`, `HARDLINK` and `TARGET` after `FILE` was renamed over
/// `TARGET`.
fn after_the_rename(green: &Green) -> (Option<&str>, Option<&[u8]>, Option<&[u8]>) {
  (
    green.hardlink("linked"),
    green.content(HARDLINK),
    green.content(TARGET),
  )
}

/// A-646 part 2b (POSIX: `ln g linked; mv f g` keeps `linked` on `g`'s old file, `mv f g; ln g
/// linked` puts it on `f`'s). Do: in one work, link `linked` to the node at `TARGET` at the base
/// and rename `FILE` over `TARGET`; and on another green, rename `FILE` over `TARGET` and link
/// `linked` to the node the work makes there. Expect the first `linked` to name the old node,
/// handed to its least link `HARDLINK` with `TARGET`'s bytes, and the second to name the node
/// renamed there, while `HARDLINK` keeps `TARGET`'s old bytes. The document's origin tells the two
/// works apart.
#[test]
fn a_link_names_the_node_its_work_named_before_or_after_a_rename_onto_its_target()
-> Result<(), String> {
  let renamed = |work: Work| work.naming(OpKind::Rename, TARGET, FILE);
  let mut before = green();
  accept(
    &mut before,
    renamed(Work::default().naming(OpKind::Link, "linked", TARGET)),
    1,
  )?;
  let mut after = green();
  accept(
    &mut after,
    renamed(Work::default()).link_made("linked", TARGET),
    1,
  )?;
  let (old, new) = (Some(&b"<g>\n"[..]), Some(&b"<f>\n"[..]));
  let found = (after_the_rename(&before), after_the_rename(&after));
  let expected = ((Some(HARDLINK), old, new), (Some(TARGET), old, new));
  if found != expected {
    return Err(format!("{found:?}"));
  }
  Ok(())
}

/// A green's files at its head, owned.
type Files = BTreeMap<String, Vec<u8>>;

/// `green`'s files at its head.
fn files_of(green: &Green) -> Files {
  green
    .files()
    .map(|(path, bytes)| (path, bytes.to_vec()))
    .collect()
}

/// What a rebase at the head of `linked`'s green restates of `work`, beside what a twin holds once
/// it accepts it: the rebase's files (the head's with `changed` over them), whether its journal
/// names `HARDLINK`, and the twin's files.
fn restated_beside_accepted(work: fn() -> Work) -> Result<(Files, bool, Files), String> {
  let (rebasing, mut accepting) = (linked()?, linked()?);
  let increment = work().at(rebasing.head(), 4);
  let Rebased::Rebased {
    changed, journal, ..
  } = rebasing.rebase(&increment)
  else {
    return Err("the work did not rebase cleanly".to_owned());
  };
  if !matches!(accepting.submit(&increment), Outcome::Accepted { .. }) {
    return Err("the twin did not accept the work".to_owned());
  }
  let mut restated = files_of(&rebasing);
  for (path, after) in changed {
    match after {
      Some(bytes) => restated.insert(path, bytes),
      None => restated.remove(&path),
    };
  }
  let names_the_link = journal
    .iter()
    .any(|op| op.names().contains(&Some(HARDLINK)));
  Ok((restated, names_the_link, files_of(&accepting)))
}

/// A-677 (A-669's rule: the link a removed or replaced home leaves its node to takes the node as
/// the version leaves it; §4.16 "Rebase, the only corrective path"). Do: rebase at the head a work
/// that removes `TARGET`, which `HARDLINK` and `SECOND` name, and one that renames `FILE` over it,
/// and submit each on a twin. Expect the rebase's files to be the twin's, `HARDLINK` holding
/// `TARGET`'s old bytes, and the journal to name no operation at `HARDLINK`: the hand-over is the
/// commit's, and a volume's link names the node already. Before, the rebase restated the removal
/// alone, so its files lost the link the submit kept (rebase.rs's oracle: `{}` against
/// `{"b": []}`).
#[test]
fn a_rebase_restates_the_node_a_removed_home_hands_to_its_link() -> Result<(), String> {
  let removed = restated_beside_accepted(|| removal_of(TARGET, Kind::File))?;
  let replaced = restated_beside_accepted(|| Work::default().naming(OpKind::Rename, TARGET, FILE))?;
  for (restated, names_the_link, accepted) in [removed, replaced] {
    let heir = accepted.get(HARDLINK).map(Vec::as_slice);
    if restated != accepted || names_the_link || heir != Some(&b"<g>\n"[..]) {
      return Err(format!(
        "restated {restated:?}, a journal operation at the link {names_the_link}, accepted \
         {accepted:?}"
      ));
    }
  }
  Ok(())
}

/// What a green holds at a path of a node's own metadata: its mode and `NAME`'s value.
type Metadata = (Option<u32>, Option<Vec<u8>>);

/// What `green` holds at `path` of a node's own metadata.
fn metadata_at(green: &Green, path: &str) -> Metadata {
  (
    green.mode(path),
    green.xattr(path, NAME).map(<[u8]>::to_vec),
  )
}

/// A work rebased at the head of a green `made` makes; the rebased work's next submit as the
/// server makes it (its restated journal composed again over its new base, the post-state empty,
/// since these works add no bytes) accepted by a second such green; and a third that accepts the
/// work itself: what the second and the third hold at `heir`.
fn resubmitted_beside_submitted(
  made: fn() -> Result<Green, String>,
  work: fn() -> Work,
  heir: &str,
) -> Result<(Metadata, Metadata), String> {
  let rebasing = made()?;
  let increment = work().at(rebasing.head(), 4);
  let Rebased::Rebased {
    version, journal, ..
  } = rebasing.rebase(&increment)
  else {
    return Err("the work did not rebase cleanly".to_owned());
  };
  let (doc, _) = compose_volume_planned(&rebasing.base_for(version, &journal), &journal)
    .map_err(|refused| format!("the restated journal does not compose: {refused:?}"))?;
  let next = Increment {
    id: [5; 32],
    base: version,
    doc,
    post_state: Vec::new(),
    evidence: Vec::new(),
  };
  let mut resubmitting = made()?;
  if !matches!(resubmitting.submit(&next), Outcome::Accepted { .. }) {
    return Err("the rebased work's next submit was not accepted".to_owned());
  }
  let mut submitting = made()?;
  if !matches!(submitting.submit(&increment), Outcome::Accepted { .. }) {
    return Err("the work itself was not accepted".to_owned());
  }
  Ok((
    metadata_at(&resubmitting, heir),
    metadata_at(&submitting, heir),
  ))
}

/// A work that removes `TARGET`.
fn removed_target() -> Work {
  removal_of(TARGET, Kind::File)
}

/// A work that renames `FILE` over `TARGET`.
fn replaced_target() -> Work {
  Work::default().naming(OpKind::Rename, TARGET, FILE)
}

/// A green whose FIFO `SPECIAL` the hard link `SECOND` names.
fn linked_fifo() -> Result<Green, String> {
  let mut green = green();
  accept(
    &mut green,
    Work::default().naming(OpKind::Link, SECOND, SPECIAL),
    1,
  )?;
  Ok(green)
}

/// A work that removes the FIFO `SPECIAL`.
fn removed_fifo() -> Work {
  removal_of(SPECIAL, Kind::Special)
}

/// A work that renames `FILE` over the FIFO `SPECIAL`.
fn replaced_fifo() -> Work {
  Work::default().naming(OpKind::Rename, SPECIAL, FILE)
}

/// A-677's sibling (A-646's hand-over: the least surviving link takes its node's mode). Do: rebase
/// at the head a work that removes `TARGET` (mode 0o600), which `HARDLINK` and `SECOND` name, and
/// one that renames `FILE` over it; submit the rebased work's next submit beside a twin that
/// submits the work itself. Expect `HARDLINK` to hold the node's mode after both. A rebased work's
/// view shows no mode (its files' bytes and sizes, its declared directories and symlinks), so its
/// next submit, composed from its restated journal, is where the mode it holds lands.
#[test]
fn a_rebased_works_next_submit_leaves_its_link_the_nodes_mode() -> Result<(), String> {
  let works: [fn() -> Work; 2] = [removed_target, replaced_target];
  for work in works {
    let (resubmitted, submitted) = resubmitted_beside_submitted(linked, work, HARDLINK)?;
    if resubmitted.0 != submitted.0 || submitted.0 != Some(0o600) {
      return Err(format!(
        "the next submit left {resubmitted:?}, the work {submitted:?}"
      ));
    }
  }
  Ok(())
}

/// A-677's sibling (A-646's hand-over: the least surviving link takes its node's attributes). Do:
/// as for the mode, with `TARGET`'s `NAME` set to `w`. Expect `HARDLINK` to hold `NAME` set to `w`
/// after both: a rebased work's view shows no attribute, so its next submit is where they land.
#[test]
fn a_rebased_works_next_submit_leaves_its_link_the_nodes_attributes() -> Result<(), String> {
  let works: [fn() -> Work; 2] = [removed_target, replaced_target];
  for work in works {
    let (resubmitted, submitted) = resubmitted_beside_submitted(linked, work, HARDLINK)?;
    if resubmitted.1 != submitted.1 || submitted.1.as_deref() != Some(&b"w"[..]) {
      return Err(format!(
        "the next submit left {resubmitted:?}, the work {submitted:?}"
      ));
    }
  }
  Ok(())
}

/// The classes of `windows` at `SPECIAL`.
fn classes_at_the_fifo(windows: &[ConflictWindow]) -> Vec<MergeConflictClass> {
  windows
    .iter()
    .filter(|window| window.path == SPECIAL)
    .map(|window| window.class)
    .collect()
}

/// What the server answers of a work at the linked FIFO `SPECIAL`: its verdict's conflict classes
/// there, the deriver's refusal of its journal, or what `SECOND` holds once it is accepted.
#[derive(Debug, PartialEq)]
enum AtTheFifo {
  Conflict(Vec<MergeConflictClass>),
  Refused(DeriveError),
  Holds(Option<SpecialNode>),
}

/// `journal` composed over a `linked_fifo` green at its head, as the server seals a work's journal
/// (the post-state empty, since these journals add no bytes), and submitted to it as increment `id`.
fn submitted_at_the_fifo(journal: &[VolumeOp], id: u8) -> Result<AtTheFifo, String> {
  let mut green = linked_fifo()?;
  let head = green.head();
  let (doc, _) = match compose_volume_planned(&green.base_for(head, journal), journal) {
    Ok(planned) => planned,
    Err(refused) => return Ok(AtTheFifo::Refused(refused)),
  };
  let increment = Increment {
    id: [id; 32],
    base: head,
    doc,
    post_state: Vec::new(),
    evidence: Vec::new(),
  };
  Ok(match green.submit(&increment) {
    Outcome::Conflict { windows } => AtTheFifo::Conflict(classes_at_the_fifo(&windows)),
    _ => AtTheFifo::Holds(green.special(SECOND)),
  })
}

/// What the server answers of `work` at a linked FIFO once it is rebased at the head: the rebase's
/// conflict, or its restated journal sealed and submitted, as its next submit.
fn rebased_at_the_fifo(work: fn() -> Work) -> Result<AtTheFifo, String> {
  let rebasing = linked_fifo()?;
  match rebasing.rebase(&work().at(rebasing.head(), 4)) {
    Rebased::Conflict { windows } => Ok(AtTheFifo::Conflict(classes_at_the_fifo(&windows))),
    Rebased::Rebased { journal, .. } => submitted_at_the_fifo(&journal, 5),
    other => Err(format!("the rebase answered {other:?}")),
  }
}

/// A work and its own declared journal, as its volume records it.
type Declared = (fn() -> Work, Vec<VolumeOp>);

/// A-677's sibling (A-646's hand-over of an IPC node; GAPS, the inode-aware namespace journal
/// owed). Do: rebase at the head a work that removes the FIFO `SPECIAL`, which `SECOND` names, and
/// one that renames `FILE` over it, and seal and submit each rebased work's restated journal, as its
/// next submit; beside each, seal and submit the work's own declared journal, as the server makes
/// its submit. Expect each rebased work answered as its own submit is: a refusal alike, or `SECOND`
/// holding alike. A rebased work's view lists no IPC node, so its next submit is where what it
/// holds of the FIFO lands.
#[test]
fn a_rebased_work_is_answered_as_its_own_submit_at_a_linked_fifo() -> Result<(), String> {
  let cases: [Declared; 2] = [
    (
      removed_fifo,
      vec![VolumeOp::Unlink {
        path: SPECIAL.to_owned(),
      }],
    ),
    (
      replaced_fifo,
      vec![VolumeOp::Rename {
        from: FILE.to_owned(),
        to: SPECIAL.to_owned(),
      }],
    ),
  ];
  for (work, declared) in cases {
    let (rebased, own) = (
      rebased_at_the_fifo(work)?,
      submitted_at_the_fifo(&declared, 6)?,
    );
    eprintln!("A-677's sibling, a linked FIFO: rebased {rebased:?}, its own submit {own:?}");
    if rebased != own {
      return Err(format!("rebased {rebased:?}, its own submit {own:?}"));
    }
  }
  Ok(())
}
