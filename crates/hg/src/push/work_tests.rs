//! A-641 part 2's tests: a push's changegroup verified against what the repository holds, and each line's move made a
//! work through A-618's builder, held to what stock Mercurial 7.2 bundled and checked out (A-514's fixtures: the
//! bundles of one repository in versions 01 to 03, and `texts.txt`, `hg cat`'s record of every file at every
//! revision), and to hand-made changegroups that break each rule.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use slates_merge::push::{Built, HEAD_ATTRIBUTE, ROOT};
use slates_merge::{Green, Outcome};

use super::tree::{self, LayoutRefusal, Move, WorkRefused};
use super::verify::{self, Held, LogOf, PathFault, Unverified, Verified};
use super::{ChangegroupPart, read};
use crate::bundle1;
use crate::bundle2::Bundle2;
use crate::changegroup::{Changegroup, Group, Revision, Version};
use crate::fixtures::{self, node};
use crate::{Admission, NULL_NODE, Node, delta};

/// Room enough for anything a fixture holds.
fn room() -> Admission {
  Admission::new(u64::MAX)
}

/// Format: the room a test gives A-618's builder, past any work these histories make.
const WORK_ROOM: u64 = 1 << 30;

/// Format: the name every test push carries.
const NAME: [u8; 32] = [7; 32];

/// What a test's repository holds: each log's texts by node.
#[derive(Clone, Debug, Default)]
struct Kept {
  /// The changelog.
  changesets: BTreeMap<Node, Vec<u8>>,
  /// The manifest log.
  manifests: BTreeMap<Node, Vec<u8>>,
  /// Every file log, by path and node.
  files: BTreeMap<(Vec<u8>, Node), Vec<u8>>,
}

impl Held for Kept {
  fn changeset_text(&self, node: &Node) -> Option<Cow<'_, [u8]>> {
    self
      .changesets
      .get(node)
      .map(|text| Cow::Borrowed(text.as_slice()))
  }

  fn manifest_text(&self, node: &Node) -> Option<Cow<'_, [u8]>> {
    self
      .manifests
      .get(node)
      .map(|text| Cow::Borrowed(text.as_slice()))
  }

  fn file_text(&self, path: &[u8], node: &Node) -> Option<Cow<'_, [u8]>> {
    self
      .files
      .get(&(path.to_vec(), *node))
      .map(|text| Cow::Borrowed(text.as_slice()))
  }
}

impl Kept {
  /// Keeps what `verified` brought whose changeset `keeps` names: the repository after that much of the push landed.
  fn keep(&mut self, verified: &Verified, keeps: impl Fn(&Node) -> bool) {
    for arrived in verified
      .changesets
      .iter()
      .filter(|arrived| keeps(&arrived.node))
    {
      self.changesets.insert(arrived.node, arrived.text.clone());
    }
    for arrived in verified
      .manifests
      .iter()
      .filter(|arrived| keeps(&arrived.link))
    {
      self.manifests.insert(arrived.node, arrived.text.clone());
    }
    for (path, arrived) in &verified.files {
      for arrived in arrived.iter().filter(|arrived| keeps(&arrived.link)) {
        self
          .files
          .insert((path.clone(), arrived.node), arrived.text.clone());
      }
    }
  }
}

/// The changegroup part of A-514's bundle in `version`, as `read` takes it from a push (version 01 from its bundle1).
fn stock_part(version: Version) -> ChangegroupPart {
  let bundle = match version {
    Version::V01 => {
      return ChangegroupPart {
        version,
        changesets: None,
        target_phase: None,
        tree_manifests: false,
        payload: bundle1::changegroup(fixtures::CG01).unwrap().to_vec(),
      };
    }
    Version::V02 => fixtures::CG02,
    _ => fixtures::CG03,
  };
  let decoded = Bundle2::decode(bundle, &mut room()).unwrap();
  let unbundle = read(&decoded, &mut room()).unwrap();
  let (_, part) = unbundle.changegroup().unwrap();
  assert_eq!(part.version, version);
  part.clone()
}

/// The changeset node of revision `rev` in the fixtures' log (`log.txt`: rev, node, phase, branch, p1, p2, summary).
fn log_node(rev: u32) -> Node {
  let line = fixtures::LOG
    .lines()
    .find(|line| line.split(' ').next() == Some(&rev.to_string()))
    .unwrap();
  node(line.split(' ').nth(1).unwrap())
}

/// The files `hg cat` recorded at revision `rev` (`texts.txt`: rev, path, the bytes in base64).
fn checked_out(rev: u32) -> BTreeMap<String, Vec<u8>> {
  include_str!("../../fixtures/texts.txt")
    .lines()
    .filter_map(|line| {
      let mut fields = line.split(' ');
      let at: u32 = fields.next()?.parse().ok()?;
      let path = fields.next()?;
      (at == rev).then(|| (path.to_owned(), base64(fields.next().unwrap_or_default())))
    })
    .collect()
}

/// The bytes base64 text names (RFC 4648's alphabet, padded), as `texts.txt` writes them.
fn base64(text: &str) -> Vec<u8> {
  let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  let sextets: Vec<u32> = text
    .bytes()
    .filter(|byte| *byte != b'=')
    .map(|byte| u32::try_from(alphabet.iter().position(|letter| *letter == byte).unwrap()).unwrap())
    .collect();
  let mut bytes = Vec::new();
  for chunk in sextets.chunks(4) {
    let shift = u32::try_from(6 * (4 - chunk.len())).unwrap();
    let joined = chunk.iter().fold(0_u32, |all, sextet| (all << 6) | sextet) << shift;
    bytes.extend_from_slice(&joined.to_be_bytes()[1..chunk.len()]);
  }
  bytes
}

/// Builds the work moving `green` from `base` (its version and head changeset) to `head`, submits it, and expects it
/// accepted.
fn land(
  green: &mut Green,
  base: Option<(u64, &Node)>,
  head: &Node,
  revisions: (&Verified, &Kept),
) -> Built {
  let moved = Move {
    name: NAME,
    base_version: base.map_or(0, |(version, _)| version),
    base: base.map(|(_, node)| node),
    head,
  };
  let built = tree::work(&moved, green, revisions, WORK_ROOM, &mut room()).unwrap();
  let outcome = green.submit(&built.increment);
  assert!(matches!(outcome, Outcome::Accepted { .. }), "{outcome:?}");
  built
}

/// Holds `green` to exactly the files `hg cat` recorded at `rev`, and its head to that revision's node.
fn holds_checkout(green: &Green, rev: u32) {
  let files: BTreeMap<String, Vec<u8>> = green
    .files()
    .map(|(path, bytes)| (path, bytes.to_vec()))
    .collect();
  assert_eq!(files, checked_out(rev), "revision {rev}");
  assert_eq!(
    green.xattr(ROOT, HEAD_ATTRIBUTE),
    Some(log_node(rev).as_slice())
  );
}

/// A-641 part 2, AC-HG.2 and AC-HG.3 (golden). Do: verify the whole repository stock 7.2 bundled, in changegroup
/// versions 01, 02 and 03, into an empty repository, then make each branch's head a work on an empty green. Expect:
/// every revision verified (seven changesets, nothing passed over), and each green holding exactly the files `hg cat`
/// recorded at its head, `default`'s at revision 7 and `stable`'s at revision 2, with the head's node.
#[test]
fn each_line_of_a_stock_bundle_lands_holding_exactly_what_hg_checks_out() {
  for version in [Version::V01, Version::V02, Version::V03] {
    let verified = verify::verify(&stock_part(version), &Kept::default(), &mut room()).unwrap();
    assert_eq!(
      (verified.changesets.len(), verified.passed_over),
      (7, 0),
      "{version:?}"
    );
    for rev in [7, 2] {
      let mut green = Green::new();
      land(
        &mut green,
        None,
        &log_node(rev),
        (&verified, &Kept::default()),
      );
      holds_checkout(&green, rev);
    }
  }
}

/// A-641 part 2, AC-HG.3. Do: move one green along `default`, version by version: revisions 0, 1, 3, 4 (the merge of
/// `stable`), 5 (a new directory) and 7 (an amended file in it). Expect: each version accepted on the one before, and
/// the green holding exactly what `hg cat` recorded at each.
#[test]
fn a_line_moves_version_by_version_holding_each_checkout() {
  let verified = verify::verify(&stock_part(Version::V03), &Kept::default(), &mut room()).unwrap();
  let mut green = Green::new();
  let mut base: Option<(u64, Node)> = None;
  for rev in [0, 1, 3, 4, 5, 7] {
    let head = log_node(rev);
    land(
      &mut green,
      base.as_ref().map(|(version, node)| (*version, node)),
      &head,
      (&verified, &Kept::default()),
    );
    holds_checkout(&green, rev);
    base = Some((green.head(), head));
  }
}

/// A-641 part 2 (§4.17.3 "Push", a push repeated after a lost answer; 7.2's `addgroup`). Do: verify a bundle again into
/// the repository its first verification filled. Expect: nothing new, every revision passed over.
#[test]
fn a_push_repeated_after_its_answer_was_lost_brings_nothing_new() {
  let first = verify::verify(&stock_part(Version::V03), &Kept::default(), &mut room()).unwrap();
  let mut kept = Kept::default();
  kept.keep(&first, |_| true);
  let again = verify::verify(&stock_part(Version::V03), &kept, &mut room()).unwrap();
  let total = first.changesets.len()
    + first.manifests.len()
    + first
      .files
      .iter()
      .map(|(_, arrived)| arrived.len())
      .sum::<usize>();
  assert_eq!(
    (
      again.changesets.len(),
      again.manifests.len(),
      again.files.len()
    ),
    (0, 0, 0)
  );
  assert_eq!(again.passed_over, u64::try_from(total).unwrap());
}

/// A-641 part 2. Do: verify version 01's bundle (each delta against the revision before it) into a repository that
/// holds revisions 0 to 3 already, then move a green from revision 3 to 7 over both. Expect: only revisions 4, 5 and 7
/// new, the deltas whose bases the repository held rebuilt from its texts, and the green holding revision 7's checkout.
#[test]
fn a_push_partly_held_verifies_the_rest_against_the_repository() {
  let all = verify::verify(&stock_part(Version::V01), &Kept::default(), &mut room()).unwrap();
  let held: BTreeSet<Node> = [0, 1, 2, 3].into_iter().map(log_node).collect();
  let mut kept = Kept::default();
  kept.keep(&all, |node| held.contains(node));
  let rest = verify::verify(&stock_part(Version::V01), &kept, &mut room()).unwrap();
  let new: Vec<Node> = rest.changesets.iter().map(|arrived| arrived.node).collect();
  assert_eq!(new, [4, 5, 7].map(log_node));
  let mut green = Green::new();
  land(&mut green, None, &log_node(3), (&all, &Kept::default()));
  let version = green.head();
  land(
    &mut green,
    Some((version, &log_node(3))),
    &log_node(7),
    (&rest, &kept),
  );
  holds_checkout(&green, 7);
}

/// One revision a test makes: its log, node, parents, link, flags and text.
#[derive(Clone, Debug)]
struct Made {
  /// The node.
  node: Node,
  /// The first parent.
  p1: Node,
  /// The second parent.
  p2: Node,
  /// The delta's base: the null node for a full text.
  base: Node,
  /// The link.
  link: Node,
  /// The flags.
  flags: u16,
  /// The text.
  text: Vec<u8>,
}

/// A revision of `text` with parents `p1` and `p2`, linked to `link`, its node computed.
fn made(text: &[u8], p1: Node, p2: Node, link: Node) -> Made {
  Made {
    node: crate::node::compute(&p1, &p2, text).unwrap(),
    p1,
    p2,
    base: NULL_NODE,
    link,
    flags: 0,
    text: text.to_vec(),
  }
}

/// A manifest's text of `files` (path, file node, flag), sorted as Mercurial writes it.
fn manifest_of(files: &[(&str, Node, &str)]) -> Vec<u8> {
  let mut sorted = files.to_vec();
  sorted.sort_by(|left, right| left.0.cmp(right.0));
  let mut text = Vec::new();
  for (path, node, flag) in sorted {
    text.extend_from_slice(path.as_bytes());
    text.push(0);
    text.extend_from_slice(fixtures::hex(&node).as_bytes());
    text.extend_from_slice(flag.as_bytes());
    text.push(b'\n');
  }
  text
}

/// A changeset's text naming `manifest` and the `files` it touched.
fn changeset_of(manifest: &Node, files: &[&str], description: &str) -> Vec<u8> {
  let hex = fixtures::hex(manifest);
  format!("{hex}\ntest\n0 0\n{}\n\n{description}", files.join("\n")).into_bytes()
}

/// A changegroup a test makes, in version 03: its changesets, manifests and file logs, each revision a full text.
#[derive(Clone, Debug, Default)]
struct Made3 {
  /// The changesets.
  changesets: Vec<Made>,
  /// The manifests.
  manifests: Vec<Made>,
  /// The file logs.
  files: Vec<(Vec<u8>, Vec<Made>)>,
  /// The tree-manifest logs (none but where a test makes one).
  trees: Vec<(Vec<u8>, Vec<Made>)>,
}

impl Made3 {
  /// One changeset on `parent` holding `files` (path, bytes, flag) whole, every file new or changed against nothing.
  fn one(parent: &Node, files: &[(&str, &[u8], &str)]) -> (Made3, Node) {
    let mut made3 = Made3::default();
    let manifests: Vec<(&str, Node, &str)> = files
      .iter()
      .map(|(path, bytes, flag)| {
        (
          *path,
          made(bytes, NULL_NODE, NULL_NODE, NULL_NODE).node,
          *flag,
        )
      })
      .collect();
    let manifest = made(&manifest_of(&manifests), NULL_NODE, NULL_NODE, NULL_NODE);
    let paths: Vec<&str> = files.iter().map(|(path, ..)| *path).collect();
    let changeset = made(
      &changeset_of(&manifest.node, &paths, "made"),
      *parent,
      NULL_NODE,
      NULL_NODE,
    );
    let link = changeset.node;
    made3.changesets.push(Made {
      link,
      ..changeset.clone()
    });
    made3.manifests.push(Made { link, ..manifest });
    for (path, bytes, _) in files {
      let file = made(bytes, NULL_NODE, NULL_NODE, link);
      made3.files.push((path.as_bytes().to_vec(), vec![file]));
    }
    (made3, link)
  }

  /// The changegroup's part, as a push carries it.
  fn part(&self) -> ChangegroupPart {
    let data: Vec<Vec<u8>> = self.all().map(|made| full_text(&made.text)).collect();
    let mut revisions = Vec::new();
    let mut groups = Vec::new();
    let mut at = 0;
    let mut range = |count: usize| {
      at += count;
      (at - count)..at
    };
    let changelog = range(self.changesets.len());
    let manifests = range(self.manifests.len());
    let tree_ranges: Vec<_> = self
      .trees
      .iter()
      .map(|(name, made)| (name, range(made.len())))
      .collect();
    let file_ranges: Vec<_> = self
      .files
      .iter()
      .map(|(name, made)| (name, range(made.len())))
      .collect();
    for (made, data) in self.all().zip(&data) {
      revisions.push(Revision {
        node: made.node,
        p1: made.p1,
        p2: made.p2,
        base: made.base,
        link: made.link,
        flags: made.flags,
        info: None,
        protocol_flags: 0,
        data,
        sidedata: None,
      });
    }
    let trees: Vec<Group<'_>> = tree_ranges
      .into_iter()
      .map(|(name, revisions)| Group { name, revisions })
      .collect();
    groups.extend(
      file_ranges
        .into_iter()
        .map(|(name, revisions)| Group { name, revisions }),
    );
    let mut payload = Vec::new();
    Changegroup {
      version: Version::V03,
      revisions,
      changelog,
      manifests,
      trees,
      files: groups,
    }
    .encode(&mut payload)
    .unwrap();
    ChangegroupPart {
      version: Version::V03,
      changesets: None,
      target_phase: None,
      tree_manifests: false,
      payload,
    }
  }

  /// Every revision, in the changegroup's order.
  fn all(&self) -> impl Iterator<Item = &Made> {
    self
      .changesets
      .iter()
      .chain(&self.manifests)
      .chain(self.trees.iter().flat_map(|(_, made)| made))
      .chain(self.files.iter().flat_map(|(_, made)| made))
  }
}

/// A delta that writes `text` whole over nothing.
fn full_text(text: &[u8]) -> Vec<u8> {
  let mut out = Vec::new();
  delta::encode_full_text(text, &mut out).unwrap();
  out
}

/// `made3` verified into an empty repository.
fn verify_made(made3: &Made3) -> Result<Verified, Unverified> {
  verify::verify(&made3.part(), &Kept::default(), &mut room())
}

/// A-641 part 2, AC-HG.2 and AC-HG.9. Do: push changegroups that break each rule a push's revisions keep. Expect:
/// each refused by kind before anything is kept: a text one bit off its node, a parent and a delta base nobody holds,
/// a flag set, a link to a changeset nobody holds, a changeset whose manifest and a manifest whose file revision
/// nobody holds, a tree manifest.
#[test]
fn a_changegroup_breaking_a_rule_is_refused_by_kind() {
  let (good, top) = Made3::one(&NULL_NODE, &[("a", b"x\n", "")]);
  assert!(verify_made(&good).is_ok());
  revisions_off_their_rules_are_refused(&good);
  parents_and_bases_nobody_holds_are_refused(&good);
  revisions_naming_what_nobody_holds_are_refused(&good, top);
}

/// `good` with a text one bit off its node, or a flag set: each refused.
fn revisions_off_their_rules_are_refused(good: &Made3) {
  let mut bent = good.clone();
  bent.files[0].1[0].node[19] ^= 1;
  assert!(matches!(
    verify_made(&bent),
    Err(Unverified::Node {
      log: LogOf::File(_),
      ..
    })
  ));
  let mut flagged = good.clone();
  flagged.files[0].1[0].flags = 1 << 15;
  assert!(matches!(
    verify_made(&flagged),
    Err(Unverified::Flags { flags: 32768, .. })
  ));
}

/// `good` with a parent and a delta base nobody holds: each refused.
fn parents_and_bases_nobody_holds_are_refused(good: &Made3) {
  let stranger = [9; 20];
  let mut orphan = good.clone();
  orphan.manifests[0].p1 = stranger;
  assert!(
    matches!(verify_made(&orphan), Err(Unverified::UnknownParent { parent, .. }) if parent == stranger)
  );
  let mut baseless = good.clone();
  baseless.files[0].1[0].base = stranger;
  assert!(
    matches!(verify_made(&baseless), Err(Unverified::UnknownDeltaBase { base, .. }) if base == stranger)
  );
}

/// `good` with a link to a changeset nobody holds, a changeset whose manifest and a manifest whose file revision
/// nobody holds, or a tree manifest: each refused (`top` its changeset).
fn revisions_naming_what_nobody_holds_are_refused(good: &Made3, top: Node) {
  let stranger = [9; 20];
  let mut unlinked = good.clone();
  unlinked.manifests[0].link = stranger;
  assert!(matches!(
    verify_made(&unlinked),
    Err(Unverified::UnknownLink {
      log: LogOf::Manifests,
      ..
    })
  ));
  let mut fileless = good.clone();
  fileless.files.clear();
  assert!(matches!(
    verify_made(&fileless),
    Err(Unverified::MissingFile { .. })
  ));
  let mut manifestless = good.clone();
  manifestless.manifests.clear();
  assert_eq!(
    verify_made(&manifestless),
    Err(Unverified::MissingManifest {
      changeset: top,
      manifest: good.manifests[0].node,
    })
  );
  let mut treed = good.clone();
  treed.trees.push((b"dir/".to_vec(), Vec::new()));
  assert_eq!(verify_made(&treed), Err(Unverified::TreeManifests));
}

/// A-641 part 2 (§4.17.3 "Push" step 2; 7.2's `pathutil.pathauditor`, lines 87 to 118, and `encoding.hfsignoreclean`).
/// Do: audit paths each of 7.2's checks refuses, paths a slates path cannot hold, and paths both take; and push a file
/// under `.hg`. Expect: a separator at the end; `.hg` and `.hg.` at the top in any case and with code points HFS+
/// ignores inside, an absolute path, `..`, a drive and Windows short names of `.hg` refused as illegal components; `.hg`
/// further down refused as a nested repository naming the components before it; an empty or `.` component, NUL and
/// bytes not UTF-8 refused as not held; `.hgignore`, `.hgtags` in a directory and short names of other stems taken; the
/// push refused before anything is kept.
#[test]
fn a_path_mercurials_audit_refuses_is_refused() {
  paths_mercurials_audit_refuses_are_refused();
  for path in [
    ".hgignore",
    "a/.hgtags",
    "x~1/y",
    "HG~a",
    "hg~",
    "src/deep/mod.rs",
    "c",
  ] {
    assert_eq!(verify::audit(path.as_bytes()), Ok(()), "{path}");
  }
  let (mut hidden, _) = Made3::one(&NULL_NODE, &[("a", b"x\n", "")]);
  hidden.files[0].0 = b".hg/hgrc".to_vec();
  assert!(matches!(verify_made(&hidden), Err(Unverified::Path { .. })));
}

/// The paths 7.2's audit refuses, and paths a slates path cannot hold: each refused by its fault.
fn paths_mercurials_audit_refuses_are_refused() {
  let fault = |path: &[u8]| match verify::audit(path) {
    Err(Unverified::Path { fault, .. }) => Some(fault),
    _ => None,
  };
  let illegal: [&[u8]; 10] = [
    b".hg/hgrc",
    b".HG/x",
    b".hg./x",
    "\x2eh\u{200c}g/x".as_bytes(),
    b"/a",
    b"a/../b",
    b"c:/a",
    b"HG~1/x",
    b"a/hg8b6c~2/y",
    b"..",
  ];
  for path in illegal {
    assert_eq!(fault(path), Some(PathFault::IllegalComponent), "{path:?}");
  }
  assert_eq!(fault(b"a/"), Some(PathFault::EndsInSeparator));
  let nested = |base: &[u8]| {
    Some(PathFault::NestedRepository {
      base: base.to_vec(),
    })
  };
  assert_eq!(fault(b"a/.hg/b"), nested(b"a"));
  assert_eq!(fault("x/y/.H\u{feff}G./z".as_bytes()), nested(b"x/y"));
  let unheld: [&[u8]; 5] = [b"a//b", b"./a", b"a/./b", b"a\0b", b"\xff"];
  for path in unheld {
    assert_eq!(fault(path), Some(PathFault::NotHeld), "{path:?}");
  }
}

/// A-641 part 2, AC-HG.3 (§4.17.1 "A push is a work" items 2 and 3). Do: push an executable, a symbolic link and files
/// in three directories, then a second changeset that changes one file in one directory. Expect: the first work makes
/// the executable 0755 and the link with its target; the second reads the root and the changed directory and passes
/// over the two directories that did not change, unread (the laid-out directories' names are content), and the green
/// holds the new bytes.
#[test]
fn a_flat_manifest_is_laid_out_so_unchanged_directories_are_never_read() {
  let files: [(&str, &[u8], &str); 4] = [
    ("bin/run", b"#!/bin/sh\n", "x"),
    ("docs/a", b"docs\n", ""),
    ("link", b"bin/run", "l"),
    ("src/b", b"one\n", ""),
  ];
  let (first, top) = Made3::one(&NULL_NODE, &files);
  let verified = verify_made(&first).unwrap();
  let mut green = Green::new();
  land(&mut green, None, &top, (&verified, &Kept::default()));
  assert_eq!(green.mode("bin/run"), Some(0o755));
  assert_eq!(green.symlink("link"), Some("bin/run"));
  let mut kept = Kept::default();
  kept.keep(&verified, |_| true);
  let changed = [
    files[0],
    files[1],
    files[2],
    ("src/b", b"two\n".as_slice(), ""),
  ];
  let (second, next) = Made3::one(&top, &changed);
  let verified_next = verify::verify(&second.part(), &kept, &mut room()).unwrap();
  let version = green.head();
  let built = land(
    &mut green,
    Some((version, &top)),
    &next,
    (&verified_next, &kept),
  );
  assert_eq!((built.reads.trees, built.reads.skipped), (4, 2));
  let files: BTreeMap<String, Vec<u8>> = green
    .files()
    .map(|(path, bytes)| (path, bytes.to_vec()))
    .collect();
  assert_eq!(
    files.get("src/b").map(Vec::as_slice),
    Some(b"two\n".as_slice())
  );
}

/// A-641 part 2. Do: make a work whose head changeset nobody holds. Expect: refused by kind, naming it.
#[test]
fn a_work_whose_head_nobody_holds_is_refused() {
  let stranger = [3; 20];
  let moved = Move {
    name: NAME,
    base_version: 0,
    base: None,
    head: &stranger,
  };
  let refused = tree::work(
    &moved,
    &Green::new(),
    (&Verified::default(), &Kept::default()),
    WORK_ROOM,
    &mut room(),
  );
  assert_eq!(
    refused.map(|built| built.reads),
    Err(WorkRefused::Layout(LayoutRefusal::MissingChangeset {
      node: stranger
    }))
  );
}

/// A-641 part 2, §4.9. Do: verify the stock bundle with exactly the room it needs, and with less. Expect: verified with
/// exactly that room; refused past the admission with any less, nothing panicking.
#[test]
fn a_changegroup_is_verified_within_exactly_the_room_it_needs() {
  let part = stock_part(Version::V03);
  let mut measured = room();
  verify::verify(&part, &Kept::default(), &mut measured).unwrap();
  let needed = u64::MAX - measured.left();
  assert!(verify::verify(&part, &Kept::default(), &mut Admission::new(needed)).is_ok());
  for less in [0, needed / 2, needed - 1] {
    assert!(matches!(
      verify::verify(&part, &Kept::default(), &mut Admission::new(less)),
      Err(Unverified::PastAdmission(_) | Unverified::Changegroup(_) | Unverified::Delta { .. })
    ));
  }
}
