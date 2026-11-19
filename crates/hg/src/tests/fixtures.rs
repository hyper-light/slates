//! The golden vectors (A-514): the bundles stock Mercurial 7.2 made of one repository, as changegroup versions `01`
//! (a bundle1) and `02` to `04` (bundle2 streams with the changegroup, `cache:rev-branch-cache`, `obsmarkers` and
//! `phase-heads` parts), with 7.2's own `hg debugbundle` listing of each and the repository's log. Made by
//! `fixtures/make-fixtures.sh` (its header names the exact command and the machine), compiled in, never read at run
//! time.

use crate::Node;

/// The bundle1 (`hg bundle --all --type none-v1`).
pub(crate) const CG01: &[u8] = include_bytes!("../../fixtures/cg01.hg");
/// The bundle2 with changegroup 02 (`none-v2;cg.version=02;phases=true;obsolescence=true`).
pub(crate) const CG02: &[u8] = include_bytes!("../../fixtures/cg02.hg");
/// The same with changegroup 03.
pub(crate) const CG03: &[u8] = include_bytes!("../../fixtures/cg03.hg");
/// The same with changegroup 04.
pub(crate) const CG04: &[u8] = include_bytes!("../../fixtures/cg04.hg");
/// 7.2's `debugbundle --all` of [`CG01`].
pub(crate) const CG01_LISTING: &str = include_str!("../../fixtures/cg01.hg.debug");
/// 7.2's `debugbundle --all` of [`CG02`].
pub(crate) const CG02_LISTING: &str = include_str!("../../fixtures/cg02.hg.debug");
/// 7.2's `debugbundle --all` of [`CG03`]: the changelog and the manifest only (7.2's listing stops at the empty tree
/// list).
pub(crate) const CG03_LISTING: &str = include_str!("../../fixtures/cg03.hg.debug");
/// 7.2's plain `debugbundle` of [`CG04`]: the changesets' nodes (its `--all` fails on version 04's full texts).
pub(crate) const CG04_LISTING: &str = include_str!("../../fixtures/cg04.hg.debug");
/// `hg log -T '{rev} {node} {phase} {branch} {p1node} {p2node} {desc|firstline}\n'`, newest first.
pub(crate) const LOG: &str = include_str!("../../fixtures/log.txt");

/// Every file revision's text, in the order its group holds them, as `make-fixtures.sh` wrote the files (and
/// `texts.txt`, `hg cat`'s record, holds them). `c` is a copy of `b`, so its text begins with Mercurial's metadata
/// block naming the source and the source's file node.
pub(crate) const FILE_TEXTS: &[(&str, &[&[u8]])] = &[
  ("a", &[b"a\n", b"a\nb\n", b"a\nb\nc\n"]),
  ("b", &[b"x\n"]),
  (
    "c",
    &[b"\x01\ncopy: b\ncopyrev: 1406e74118627694268417491f018a4a883152f0\n\x01\nx\n"],
  ),
  ("dir/f", &[b"deep\n", b"deep\nmore, amended\n"]),
];

/// One revision as 7.2's `debugbundle --all` lists it: "id, p1, p2, cset, delta base, len(delta)".
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Row {
  pub(crate) node: Node,
  pub(crate) p1: Node,
  pub(crate) p2: Node,
  pub(crate) link: Node,
  pub(crate) base: Node,
  pub(crate) len: usize,
}

/// A node from its 40 hexadecimal digits.
pub(crate) fn node(hex: &str) -> Node {
  assert_eq!(hex.len(), 40, "a node is 40 hex digits: {hex}");
  let mut out = [0u8; 20];
  for (position, byte) in out.iter_mut().enumerate() {
    *byte = u8::from_str_radix(&hex[position * 2..position * 2 + 2], 16).unwrap();
  }
  out
}

/// A node's 40 hexadecimal digits.
pub(crate) fn hex(node: &Node) -> String {
  node.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The groups of the changegroup a listing shows, in order: each group's name ("changelog", "manifest", a file's
/// path) and its rows.
pub(crate) fn listing(text: &str) -> Vec<(String, Vec<Row>)> {
  let mut groups: Vec<(String, Vec<Row>)> = Vec::new();
  let mut in_changegroup = !text.contains(" -- ");
  for line in text.lines() {
    let line = line.trim();
    if line.contains(" -- ") {
      in_changegroup = line.starts_with("changegroup -- ");
      continue;
    }
    if !in_changegroup || line.is_empty() || line.starts_with("format:") {
      continue;
    }
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() == 6 && fields[0].len() == 40 {
      groups.last_mut().unwrap().1.push(Row {
        node: node(fields[0]),
        p1: node(fields[1]),
        p2: node(fields[2]),
        link: node(fields[3]),
        base: node(fields[4]),
        len: fields[5].parse().unwrap(),
      });
    } else {
      groups.push((line.to_string(), Vec::new()));
    }
  }
  groups
}

/// The part lines of a bundle2 listing: "<type> -- {<key>: <value>, ...} (mandatory: <True|False>)".
pub(crate) fn part_lines(text: &str) -> Vec<String> {
  text
    .lines()
    .filter(|line| line.contains(" -- ") && line.contains("(mandatory: "))
    .map(str::to_string)
    .collect()
}

/// The changeset nodes of a plain `debugbundle` listing (version 04's).
pub(crate) fn plain_nodes(text: &str) -> Vec<Node> {
  let mut out = Vec::new();
  let mut in_changegroup = false;
  for line in text.lines() {
    let line = line.trim();
    if line.contains(" -- ") {
      in_changegroup = line.starts_with("changegroup -- ");
    } else if in_changegroup && line.len() == 40 {
      out.push(node(line));
    }
  }
  out
}

/// The changesets' first description lines, oldest first, from [`LOG`] (whose newest-first lines end with them).
pub(crate) fn descriptions() -> Vec<String> {
  let mut out: Vec<String> = LOG
    .lines()
    .map(|line| line.splitn(7, ' ').nth(6).unwrap().to_string())
    .collect();
  out.reverse();
  out
}
