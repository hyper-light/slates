//! Changeset evolution under stock Mercurial (goal 93, GAPS "Changeset evolution served"; §4.17.3 decision 4: obsolescence
//! markers kept and exchanged, refused on a public predecessor): a client with `experimental.evolution` rewrites its
//! drafts before it pushes them (an amend, a rebase) and pushes their successors with the markers; a clone takes the
//! markers back. Real `hg` against an in-process daemon (`common::hg`); skips, saying so, where `hg` is absent.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::print_stderr,
  clippy::disallowed_methods
)]

mod common;

use std::process::Output;

/// The hgrc an evolution-enabled client adds: markers made, kept and exchanged, and `rebase`.
const EVOLUTION: &str = "[experimental]\nevolution = all\n[extensions]\nrebase =\n";

/// `output`'s stdout, as text.
fn stdout(output: &Output) -> String {
  String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Panics with `what` and `output`'s words where it failed.
fn ran(what: &str, output: Output) -> Output {
  assert!(
    output.status.success(),
    "{what}: {}{}",
    stdout(&output),
    String::from_utf8_lossy(&output.stderr)
  );
  output
}

/// Checks a clone's changesets, the hidden ones too (`served_nodes`, a node a line): each `kept` node, by name, is
/// among them, and no `rewritten` one is.
fn assert_served_and_never_served(
  served_nodes: &str,
  kept: &[(&str, &str)],
  rewritten: &[(&str, &str)],
) {
  for (name, kept) in kept {
    assert!(served_nodes.contains(kept), "{name} served: {served_nodes}");
  }
  for (name, rewritten) in rewritten {
    assert!(
      !served_nodes.contains(rewritten),
      "the rewritten {name} never served: {served_nodes}"
    );
  }
}

/// Goal 93's next completeness item (main's ruling of 2026-11-23). Do: with an evolution-enabled client, commit and push
/// `a`; commit `b`, amend it, and push; commit `c` on `a`, rebase it onto the amended `b`, and push; clone the repository
/// with evolution and list its markers and changesets. Expect each push to land; the clone to hold the amend's and the
/// rebase's markers (2) and the successors, none of the rewritten drafts; the amend's push told `remote: 1 new
/// obsolescence markers`, as 7.2's server tells it. Then make a marker pruning the public `a` and
/// push it: expect it refused, its words naming the public changeset (decision 4). Red before: every push carrying a
/// marker was refused "obsolescence markers are not accepted by this server".
#[test]
fn an_evolution_clients_amend_and_rebase_land_with_their_markers() {
  if !common::hg::has_hg() {
    eprintln!(
      "SKIPPED an_evolution_clients_amend_and_rebase_land_with_their_markers: no `hg` on PATH"
    );
    return;
  }
  let served = common::hg::serve("hg-evolution", EVOLUTION);
  let url = served.url.clone();
  ran(
    "init",
    served.hg(".").args(["init", "source"]).output().unwrap(),
  );
  let commit = |file: &str, message: &str| {
    std::fs::write(format!("{}/source/{file}", served.base()), message).unwrap();
    ran(
      message,
      served
        .hg("source")
        .args(["commit", "-qA", "-m", message])
        .output()
        .unwrap(),
    );
  };
  let node = |revset: &str| {
    stdout(&ran(
      revset,
      served
        .hg("source")
        .args(["log", "-r", revset, "-T", "{node}"])
        .output()
        .unwrap(),
    ))
  };
  let push = || {
    served
      .hg("source")
      .args(["push", "-q", "--new-branch", &url])
      .output()
      .unwrap()
  };
  commit("a.txt", "a");
  ran("push a", push());
  let public_a = node(".");
  commit("b.txt", "b");
  let old_b = node(".");
  ran(
    "amend b",
    served
      .hg("source")
      .args(["commit", "-q", "--amend", "-m", "b amended"])
      .output()
      .unwrap(),
  );
  let new_b = node(".");
  let amended = ran(
    "push the amend",
    served
      .hg("source")
      .args(["push", "--new-branch", &url])
      .output()
      .unwrap(),
  );
  let told = stdout(&amended) + &String::from_utf8_lossy(&amended.stderr);
  ran(
    "update to a",
    served
      .hg("source")
      .args(["update", "-q", &public_a])
      .output()
      .unwrap(),
  );
  commit("c.txt", "c");
  let old_c = node(".");
  ran(
    "rebase c",
    served
      .hg("source")
      .args(["rebase", "-q", "-s", &old_c, "-d", &new_b])
      .output()
      .unwrap(),
  );
  let new_c = node("max(desc(c))");
  ran("push the rebase", push());
  ran(
    "clone",
    served
      .hg(".")
      .args(["clone", "-q", &url, "cloned"])
      .output()
      .unwrap(),
  );
  let markers = stdout(&ran(
    "markers",
    served
      .hg("cloned")
      .args(["debugobsolete"])
      .output()
      .unwrap(),
  ));
  let served_nodes = stdout(&ran(
    "log",
    served
      .hg("cloned")
      .args(["log", "--hidden", "-T", "{node}\n"])
      .output()
      .unwrap(),
  ));
  // A marker pruning the public `a`, pushed: refused.
  ran(
    "prune a",
    served
      .hg("source")
      .args(["debugobsolete", &public_a])
      .output()
      .unwrap(),
  );
  let pruned = served
    .hg("source")
    .args(["push", "-q", "-f", &url])
    .output()
    .unwrap();
  served.stop();
  assert!(
    told.contains("remote: 1 new obsolescence markers"),
    "the amend's push told its marker new, in 7.2's words: {told}"
  );
  let lines: Vec<&str> = markers.lines().collect();
  assert_eq!(
    lines.len(),
    2,
    "the clone holds the amend's and the rebase's markers: {markers}"
  );
  assert!(
    lines
      .iter()
      .any(|line| line.starts_with(&old_b) && line.contains(&new_b)),
    "the amend's marker: {markers}"
  );
  assert!(
    lines
      .iter()
      .any(|line| line.starts_with(&old_c) && line.contains(&new_c)),
    "the rebase's marker: {markers}"
  );
  assert_served_and_never_served(
    &served_nodes,
    &[
      ("a", &public_a),
      ("b amended", &new_b),
      ("c rebased", &new_c),
    ],
    &[("b", &old_b), ("c", &old_c)],
  );
  let words = String::from_utf8_lossy(&pruned.stderr).into_owned() + &stdout(&pruned);
  assert!(
    !pruned.status.success(),
    "a marker obsoleting public a refused: {words}"
  );
  assert!(
    words.contains("public"),
    "its words name the public changeset: {words}"
  );
}

/// Goal 93 (§4.17.3 decision 3: a non-publishing repository keeps a push's changesets draft; decision 4: an obsolete
/// draft leaves the served view). Do: make the repository non-publishing (its policy's `hg_publishing`); with an
/// evolution-enabled client, commit and push `base` and then `b` on it; amend `b` and push the amend; clone the
/// repository and list its changesets with their phases, the hidden ones too, and its markers. Expect `base` and the
/// amended `b` served as drafts, the pushed `b` the amend rewrote never served (its marker hides it), and the clone
/// holding the amend's one marker. Red before: every repository published, so `b` landed public and its amend was
/// refused by the client ("cannot amend public changesets").
#[test]
fn a_non_publishing_repository_keeps_drafts_and_hides_a_rewritten_one() {
  if !common::hg::has_hg() {
    eprintln!(
      "SKIPPED a_non_publishing_repository_keeps_drafts_and_hides_a_rewritten_one: no `hg` on PATH"
    );
    return;
  }
  let served = common::hg::serve("hg-drafts", EVOLUTION);
  served.set_publishing(false);
  let url = served.url.clone();
  ran(
    "init",
    served.hg(".").args(["init", "source"]).output().unwrap(),
  );
  let commit = |file: &str, message: &str| {
    std::fs::write(format!("{}/source/{file}", served.base()), message).unwrap();
    ran(
      message,
      served
        .hg("source")
        .args(["commit", "-qA", "-m", message])
        .output()
        .unwrap(),
    );
  };
  let node = |revset: &str| {
    stdout(&ran(
      revset,
      served
        .hg("source")
        .args(["log", "-r", revset, "-T", "{node}"])
        .output()
        .unwrap(),
    ))
  };
  let push = || {
    served
      .hg("source")
      .args(["push", "-q", "--new-branch", &url])
      .output()
      .unwrap()
  };
  commit("base.txt", "base");
  let base = node(".");
  commit("b.txt", "b");
  let old_b = node(".");
  ran("push b", push());
  ran(
    "amend b",
    served
      .hg("source")
      .args(["commit", "-q", "--amend", "-m", "b amended"])
      .output()
      .unwrap(),
  );
  let new_b = node(".");
  ran("push the amend", push());
  ran(
    "clone",
    served
      .hg(".")
      .args(["clone", "-q", &url, "cloned"])
      .output()
      .unwrap(),
  );
  let listed = stdout(&ran(
    "log",
    served
      .hg("cloned")
      .args(["log", "--hidden", "-T", "{node} {phase}\n"])
      .output()
      .unwrap(),
  ));
  let markers = stdout(&ran(
    "markers",
    served
      .hg("cloned")
      .args(["debugobsolete"])
      .output()
      .unwrap(),
  ));
  served.stop();
  for (name, kept) in [("base", &base), ("b amended", &new_b)] {
    assert!(
      listed.contains(&format!("{kept} draft")),
      "{name} served as a draft: {listed}"
    );
  }
  assert!(
    !listed.contains(old_b.as_str()),
    "the rewritten b never served: {listed}"
  );
  let lines: Vec<&str> = markers.lines().collect();
  assert!(
    lines.len() == 1 && lines[0].starts_with(&old_b) && lines[0].contains(&new_b),
    "the amend's marker: {markers}"
  );
}
