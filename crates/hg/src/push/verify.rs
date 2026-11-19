//! A push's changegroup verified against what the repository holds (A-641 part 2; §4.17.3 "Push" step 2), before any
//! of it is kept: every revision's text rebuilt from its delta, every node checked as Mercurial computes it, every
//! parent, delta base and link found, every path audited, and every manifest and file revision the new changesets name
//! present, in the bundle or in the repository.
//!
//! **7.2's rules, kept** (`revlog.addgroup`, `changegroup.apply`, `exchange._validatefiles` under `server.validate`
//! \[C\]). A group is taken in the order it arrives. A node its log holds already is passed over ("this can happen if
//! two branches make the same change"), so a push repeated after a lost answer verifies and adds nothing (§4.17.3
//! "Push", the repeat). Any other revision's parents are in its log or earlier in its group ("unknown parent"), its
//! delta's base too ("unknown delta base"; the null node a delta against nothing), and a manifest's or file revision's
//! link names a changeset of the push or of the repository ("unknown changeset"). Each text hashes to its node, with
//! SHA-1's collision detection on (A-588). What 7.2 checks only under `server.validate` is always checked here: each
//! new changeset's manifest is the push's or the repository's, and each file node a new manifest names beside its first
//! parent's is too ("missing file data").
//!
//! **Stricter than 7.2, each on purpose.** A revision flagged censored, ellipsis or externally stored, or with any flag
//! 7.2 defines, is refused (their handling is owed, §4.17.3 piece 10); a tree-manifest group is refused (slates keeps
//! flat manifests); and every path a push's files or new manifests name passes Mercurial's audit, which 7.2's server
//! applies only to a working copy (`pathutil.pathauditor`): no empty, `.` or `..` component, no `.hg` at the top, no
//! drive at the front, no NUL, and UTF-8, the form a slates path holds.
//!
//! **Bounds (§4.9).** The changegroup's decode is charged to the request's admission; each rebuilt text by the
//! applier before it is made; each lookup table before it is built.

use std::borrow::Cow;
use std::collections::BTreeMap;

use crate::changegroup::{self, Changegroup, Revision};
use crate::read::to_u64;
use crate::{Admission, NULL_NODE, Node, PastAdmission, changeset, delta, filerev, manifest, node};

use super::ChangegroupPart;

/// Format: the names a repository's own directory takes, after `_lowerclean` (`pathutil.pathauditor`: `.hg`, and
/// `.hg.` as Windows reads it).
const DOT_HG: [&[u8]; 2] = [b".hg", b".hg."];

/// Format: the separator of a tracked path's components.
const SLASH: u8 = b'/';

/// Format: what follows a drive's letter at a path's front (`os.path.splitdrive`).
const DRIVE: u8 = b':';

/// Format: the bytes of a drive's name at a path's front: its letter and [`DRIVE`].
const DRIVE_LEN: usize = 2;

/// Format: the byte between a Windows short name's stem and its number (`HG~1`), and the stems 7.2 refuses there
/// (`pathauditor`: `first.upper() in [b"HG", b"HG8B6C"]`).
const SHORT_NAME: u8 = b'~';
/// Format: see [`SHORT_NAME`].
const SHORT_STEMS: [&[u8]; 2] = [b"HG", b"HG8B6C"];

/// Format: the UTF-8 of the code points HFS+ ignores, which `encoding.hfsignoreclean` removes before a name is compared
/// (U+200C to U+200F, U+202A to U+202E, U+206A to U+206F, U+FEFF; `encoding.py` `_ignore`).
const HFS_IGNORED: [[u8; 3]; 16] = [
  [0xe2, 0x80, 0x8c],
  [0xe2, 0x80, 0x8d],
  [0xe2, 0x80, 0x8e],
  // Format: U+200F, then U+202A to U+202E.
  [0xe2, 0x80, 0x8f],
  [0xe2, 0x80, 0xaa],
  [0xe2, 0x80, 0xab],
  [0xe2, 0x80, 0xac],
  // Format: U+202D and U+202E, then U+206A and U+206B.
  [0xe2, 0x80, 0xad],
  [0xe2, 0x80, 0xae],
  [0xe2, 0x81, 0xaa],
  [0xe2, 0x81, 0xab],
  // Format: U+206C to U+206F.
  [0xe2, 0x81, 0xac],
  [0xe2, 0x81, 0xad],
  [0xe2, 0x81, 0xae],
  [0xe2, 0x81, 0xaf],
  // Format: U+FEFF, the byte-order mark.
  [0xef, 0xbb, 0xbf],
];

/// What a repository already holds, as a push's verification reads it: each log's full texts by node.
pub trait Held {
  /// The text of the changeset `node`, if the changelog holds it.
  fn changeset_text(&self, node: &Node) -> Option<Cow<'_, [u8]>>;
  /// The text of the manifest `node`, if the manifest log holds it.
  fn manifest_text(&self, node: &Node) -> Option<Cow<'_, [u8]>>;
  /// The text (its metadata block, then its bytes) of the revision `node` of `path`'s file log, if it holds it.
  fn file_text(&self, path: &[u8], node: &Node) -> Option<Cow<'_, [u8]>>;
}

/// A revision a push brought and its verification kept: its node, parents, link and full text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Arrived {
  /// The node.
  pub node: Node,
  /// The first parent.
  pub p1: Node,
  /// The second parent.
  pub p2: Node,
  /// The changeset that brought it (a changeset's own node).
  pub link: Node,
  /// The full text.
  pub text: Vec<u8>,
}

/// A push's changegroup, verified: what it brought that the repository did not hold, each log in arrival order, and
/// each revision found by its node.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Verified {
  /// The new changesets.
  pub changesets: Vec<Arrived>,
  /// The new manifests.
  pub manifests: Vec<Arrived>,
  /// The new file revisions, each file log by its path, in the bundle's order.
  pub files: Vec<(Vec<u8>, Vec<Arrived>)>,
  /// The revisions the repository held already, passed over.
  pub passed_over: u64,
  /// Each new changeset's position, by node.
  changeset_at: BTreeMap<Node, usize>,
  /// Each new manifest's position, by node.
  manifest_at: BTreeMap<Node, usize>,
  /// Each new file revision's file log and position, by path and node.
  file_at: BTreeMap<(Vec<u8>, Node), (usize, usize)>,
}

impl Verified {
  /// The new changeset `node`, if the push brought it.
  pub fn changeset(&self, node: &Node) -> Option<&Arrived> {
    self.changesets.get(*self.changeset_at.get(node)?)
  }

  /// The new manifest `node`, if the push brought it.
  pub fn manifest(&self, node: &Node) -> Option<&Arrived> {
    self.manifests.get(*self.manifest_at.get(node)?)
  }

  /// The new revision `node` of `path`'s file log, if the push brought it.
  pub fn file(&self, path: &[u8], node: &Node) -> Option<&Arrived> {
    let (log, at) = self.file_at.get(&(path.to_vec(), *node))?;
    self.files.get(*log)?.1.get(*at)
  }
}

/// Which log a refused revision is of.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogOf {
  /// The changelog.
  Changelog,
  /// The manifest log.
  Manifests,
  /// The file log of this path.
  File(Vec<u8>),
}

/// Why a push's changegroup was refused, typed, before any of it was kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unverified {
  /// The changegroup's framing.
  Changegroup(changegroup::Refusal),
  /// A parent neither the log holds nor the group brought earlier (7.2: "unknown parent").
  UnknownParent {
    /// The log.
    log: LogOf,
    /// The revision.
    node: Node,
    /// The parent.
    parent: Node,
  },
  /// A delta base neither the log holds nor the group brought earlier (7.2: "unknown delta base").
  UnknownDeltaBase {
    /// The log.
    log: LogOf,
    /// The revision.
    node: Node,
    /// The base.
    base: Node,
  },
  /// A delta that does not apply to its base.
  Delta {
    /// The log.
    log: LogOf,
    /// The revision.
    node: Node,
    /// What the applier found.
    fault: delta::Refusal,
  },
  /// A text that does not hash to its node, or whose hashing finds a known SHA-1 collision attack.
  Node {
    /// The log.
    log: LogOf,
    /// The revision.
    node: Node,
    /// What the hashing found.
    fault: node::Refusal,
  },
  /// A revision with a flag set: censored, ellipsis, externally stored or another 7.2 defines (owed).
  Flags {
    /// The log.
    log: LogOf,
    /// The revision.
    node: Node,
    /// The flags.
    flags: u16,
  },
  /// A link naming a changeset neither the push brought nor the repository holds (7.2: "unknown changeset").
  UnknownLink {
    /// The log.
    log: LogOf,
    /// The revision.
    node: Node,
    /// The link.
    link: Node,
  },
  /// A tree manifest: slates keeps flat manifests.
  TreeManifests,
  /// A path Mercurial's audit refuses, or one a slates path cannot hold.
  Path {
    /// The path.
    path: Vec<u8>,
    /// What the audit found.
    fault: PathFault,
  },
  /// A changeset text 7.2's reader refuses.
  Changeset {
    /// The changeset.
    node: Node,
    /// What the reader found.
    fault: changeset::Refusal,
  },
  /// A manifest text 7.2's reader refuses.
  Manifest {
    /// The manifest.
    node: Node,
    /// What the reader found.
    fault: manifest::Refusal,
  },
  /// A file revision whose metadata block 7.2's reader refuses.
  FileRevision {
    /// The path.
    path: Vec<u8>,
    /// The revision.
    node: Node,
    /// What the reader found.
    fault: filerev::Refusal,
  },
  /// A new changeset whose manifest neither the push brought nor the repository holds.
  MissingManifest {
    /// The changeset.
    changeset: Node,
    /// The manifest.
    manifest: Node,
  },
  /// A file node a new manifest names that neither the push brought nor the repository holds (7.2: "missing file
  /// data").
  MissingFile {
    /// The path.
    path: Vec<u8>,
    /// The file node.
    node: Node,
  },
  /// More than the request's admission had left.
  PastAdmission(PastAdmission),
}

impl From<PastAdmission> for Unverified {
  fn from(past: PastAdmission) -> Self {
    Self::PastAdmission(past)
  }
}

/// What a path's audit found (`pathutil.pathauditor`, whose words a reply gives each).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathFault {
  /// It ends in a separator (7.2: "path ends in directory separator: %s").
  EndsInSeparator,
  /// A drive at its front, `.hg` or `.hg.` or nothing as its first component, a `..` component, or a Windows short
  /// name of `.hg` (7.2: "path contains illegal component: %s").
  IllegalComponent,
  /// `.hg` or `.hg.` as a later component, the path under it a nested repository (7.2: "path '%s' is inside nested
  /// repo %r").
  NestedRepository {
    /// The components before it, joined.
    base: Vec<u8>,
  },
  /// What 7.2 takes and a slates path cannot hold: bytes not UTF-8, NUL, or an empty or `.` component past the first.
  NotHeld,
}

/// One log's group as it is verified: what arrived so far, by node.
struct Group<'g, H: ?Sized> {
  /// The log.
  log: LogOf,
  /// What the repository holds.
  held: &'g H,
  /// What the group brought so far.
  arrived: Vec<Arrived>,
  /// Each arrived node's position in `arrived`.
  by_node: BTreeMap<Node, usize>,
}

impl<'g, H: Held + ?Sized> Group<'g, H> {
  /// An empty group of `log`.
  fn new(log: LogOf, held: &'g H) -> Self {
    Group {
      log,
      held,
      arrived: Vec::new(),
      by_node: BTreeMap::new(),
    }
  }

  /// The repository's text of `node` in this log.
  fn held_text(&self, node: &Node) -> Option<Cow<'g, [u8]>> {
    match &self.log {
      LogOf::Changelog => self.held.changeset_text(node),
      LogOf::Manifests => self.held.manifest_text(node),
      LogOf::File(path) => self.held.file_text(path, node),
    }
  }

  /// The text of `node` in this log: what the group brought, or what the log holds; the empty text for the null node.
  fn text(&self, node: &Node) -> Option<Cow<'_, [u8]>> {
    if *node == NULL_NODE {
      return Some(Cow::Borrowed(&[]));
    }
    match self.by_node.get(node) {
      Some(at) => self
        .arrived
        .get(*at)
        .map(|arrived| Cow::Borrowed(arrived.text.as_slice())),
      None => self.held_text(node),
    }
  }

  /// Whether this log holds or the group brought `node`.
  fn knows(&self, node: &Node) -> bool {
    *node == NULL_NODE || self.by_node.contains_key(node) || self.held_text(node).is_some()
  }

  /// Takes one revision: passed over when the log holds it; otherwise its parents and base found, its text rebuilt and
  /// its node checked.
  fn take(
    &mut self,
    revision: &Revision<'_>,
    passed_over: &mut u64,
    admission: &mut Admission,
  ) -> Result<(), Unverified> {
    if self.by_node.contains_key(&revision.node) || self.held_text(&revision.node).is_some() {
      *passed_over = passed_over.saturating_add(1);
      return Ok(());
    }
    self.check_known(revision)?;
    let text = self.rebuild(revision, admission)?;
    node::check(&revision.node, &revision.p1, &revision.p2, &text).map_err(|fault| {
      Unverified::Node {
        log: self.log.clone(),
        node: revision.node,
        fault,
      }
    })?;
    admission.charge_items::<(Node, usize)>(1)?;
    admission.charge_items::<Arrived>(1)?;
    self.by_node.insert(revision.node, self.arrived.len());
    self.arrived.push(Arrived {
      node: revision.node,
      p1: revision.p1,
      p2: revision.p2,
      link: revision.link,
      text,
    });
    Ok(())
  }

  /// Refuses a revision with a flag, or one whose parent this log does not know.
  fn check_known(&self, revision: &Revision<'_>) -> Result<(), Unverified> {
    if revision.flags != 0 {
      return Err(Unverified::Flags {
        log: self.log.clone(),
        node: revision.node,
        flags: revision.flags,
      });
    }
    match [revision.p1, revision.p2]
      .into_iter()
      .find(|parent| !self.knows(parent))
    {
      Some(parent) => Err(Unverified::UnknownParent {
        log: self.log.clone(),
        node: revision.node,
        parent,
      }),
      None => Ok(()),
    }
  }

  /// The revision's text: its delta applied to its base's text.
  fn rebuild(
    &self,
    revision: &Revision<'_>,
    admission: &mut Admission,
  ) -> Result<Vec<u8>, Unverified> {
    let base = self
      .text(&revision.base)
      .ok_or_else(|| Unverified::UnknownDeltaBase {
        log: self.log.clone(),
        node: revision.node,
        base: revision.base,
      })?;
    delta::apply(&base, revision.data, admission).map_err(|fault| Unverified::Delta {
      log: self.log.clone(),
      node: revision.node,
      fault,
    })
  }
}

/// The changeset `link` names, as a manifest's or file revision's link must: one the push brought or the repository
/// holds.
fn link_known(
  log: &LogOf,
  arrived: &Arrived,
  changesets: &BTreeMap<Node, usize>,
  held: &(impl Held + ?Sized),
) -> Result<(), Unverified> {
  if changesets.contains_key(&arrived.link) || held.changeset_text(&arrived.link).is_some() {
    return Ok(());
  }
  Err(Unverified::UnknownLink {
    log: log.clone(),
    node: arrived.node,
    link: arrived.link,
  })
}

/// Verifies a push's changegroup against what `held` holds (module docs): every revision it brought that the
/// repository lacks, rebuilt and checked; or the first refusal.
pub fn verify(
  part: &ChangegroupPart,
  held: &(impl Held + ?Sized),
  admission: &mut Admission,
) -> Result<Verified, Unverified> {
  let group =
    Changegroup::decode(&part.payload, part.version, admission).map_err(Unverified::Changegroup)?;
  if !group.trees.is_empty() {
    return Err(Unverified::TreeManifests);
  }
  let mut verified = Verified::default();
  let changesets = changelog(&group, held, &mut verified, admission)?;
  let manifests = manifest_log(&group, held, &changesets, &mut verified, admission)?;
  file_logs(&group, held, &changesets, &mut verified, admission)?;
  manifests_present(&verified, &manifests, held)?;
  files_present(&verified, held, admission)?;
  Ok(verified)
}

/// The changelog's group, each changeset's text read as 7.2 reads it; the new changesets' positions by node.
fn changelog(
  group: &Changegroup<'_>,
  held: &(impl Held + ?Sized),
  verified: &mut Verified,
  admission: &mut Admission,
) -> Result<BTreeMap<Node, usize>, Unverified> {
  let mut log = Group::new(LogOf::Changelog, held);
  for revision in group.group(&group.changelog) {
    log.take(revision, &mut verified.passed_over, admission)?;
  }
  for arrived in &log.arrived {
    changeset::decode(&arrived.text)
      .and_then(|read| read.manifest_node())
      .map_err(|fault| Unverified::Changeset {
        node: arrived.node,
        fault,
      })?;
  }
  verified.changesets = log.arrived;
  verified.changeset_at = log.by_node.clone();
  Ok(log.by_node)
}

/// The manifest log's group, each manifest's link a known changeset and its text read as 7.2's reader reads it.
fn manifest_log(
  group: &Changegroup<'_>,
  held: &(impl Held + ?Sized),
  changesets: &BTreeMap<Node, usize>,
  verified: &mut Verified,
  admission: &mut Admission,
) -> Result<BTreeMap<Node, usize>, Unverified> {
  let mut log = Group::new(LogOf::Manifests, held);
  for revision in group.group(&group.manifests) {
    log.take(revision, &mut verified.passed_over, admission)?;
  }
  for arrived in &log.arrived {
    link_known(&LogOf::Manifests, arrived, changesets, held)?;
    manifest::decode(&arrived.text).map_err(|fault| Unverified::Manifest {
      node: arrived.node,
      fault,
    })?;
  }
  verified.manifests = log.arrived;
  verified.manifest_at = log.by_node.clone();
  Ok(log.by_node)
}

/// Every file log's group: its path audited, each revision's link a known changeset and its metadata block read.
fn file_logs(
  group: &Changegroup<'_>,
  held: &(impl Held + ?Sized),
  changesets: &BTreeMap<Node, usize>,
  verified: &mut Verified,
  admission: &mut Admission,
) -> Result<(), Unverified> {
  for file in &group.files {
    audit(file.name)?;
    let path = file.name;
    let mut log = Group::new(LogOf::File(path.to_vec()), held);
    for revision in group.group(&file.revisions) {
      log.take(revision, &mut verified.passed_over, admission)?;
    }
    for arrived in &log.arrived {
      link_known(&log.log, arrived, changesets, held)?;
      filerev::split(&arrived.text).map_err(|fault| Unverified::FileRevision {
        path: path.to_vec(),
        node: arrived.node,
        fault,
      })?;
    }
    // A file log whose every revision the repository held brought nothing: kept only where something arrived.
    if log.arrived.is_empty() {
      continue;
    }
    let index = verified.files.len();
    admission
      .charge(to_u64(path.len()).saturating_mul(to_u64(log.arrived.len()).saturating_add(1)))?;
    for (node, at) in &log.by_node {
      verified
        .file_at
        .insert((path.to_vec(), *node), (index, *at));
    }
    verified.files.push((path.to_vec(), log.arrived));
  }
  Ok(())
}

/// Every new changeset's manifest is the push's or the repository's.
fn manifests_present(
  verified: &Verified,
  manifests: &BTreeMap<Node, usize>,
  held: &(impl Held + ?Sized),
) -> Result<(), Unverified> {
  for arrived in &verified.changesets {
    let manifest = changeset::decode(&arrived.text)
      .and_then(|read| read.manifest_node())
      .map_err(|fault| Unverified::Changeset {
        node: arrived.node,
        fault,
      })?;
    let present = manifest == NULL_NODE
      || manifests.contains_key(&manifest)
      || held.manifest_text(&manifest).is_some();
    if !present {
      return Err(Unverified::MissingManifest {
        changeset: arrived.node,
        manifest,
      });
    }
  }
  Ok(())
}

/// Every file node a new manifest names beside its first parent's manifest is the push's or the repository's, and its
/// path passes the audit.
fn files_present(
  verified: &Verified,
  held: &(impl Held + ?Sized),
  admission: &mut Admission,
) -> Result<(), Unverified> {
  for arrived in &verified.manifests {
    let parent = manifest_text(verified, held, &arrived.p1).unwrap_or(Cow::Borrowed(&[]));
    let parent_entries = manifest::decode(&parent).ok();
    let count = parent_entries.clone().map_or(0, Iterator::count);
    admission.charge_items::<(&[u8], Node)>(count)?;
    let previous: BTreeMap<&[u8], Node> = parent_entries
      .into_iter()
      .flatten()
      .map(|entry| (entry.path, entry.node))
      .collect();
    let entries = manifest::decode(&arrived.text).map_err(|fault| Unverified::Manifest {
      node: arrived.node,
      fault,
    })?;
    for entry in entries.filter(|entry| previous.get(entry.path) != Some(&entry.node)) {
      audit(entry.path)?;
      let present = verified.file(entry.path, &entry.node).is_some()
        || held.file_text(entry.path, &entry.node).is_some();
      if !present {
        return Err(Unverified::MissingFile {
          path: entry.path.to_vec(),
          node: entry.node,
        });
      }
    }
  }
  Ok(())
}

/// The text of the manifest `node`: the push's, or the repository's; the empty manifest for the null node.
pub fn manifest_text<'v>(
  verified: &'v Verified,
  held: &'v (impl Held + ?Sized),
  node: &Node,
) -> Option<Cow<'v, [u8]>> {
  if *node == NULL_NODE {
    return Some(Cow::Borrowed(&[]));
  }
  match verified.manifest(node) {
    Some(arrived) => Some(Cow::Borrowed(arrived.text.as_slice())),
    None => held.manifest_text(node),
  }
}

/// Mercurial's audit of a tracked path, as `pathutil.pathauditor` audits one (lines 87 to 118 of 7.2's `pathutil.py`),
/// and what a slates path holds. 7.2's checks, in its order: no separator at the end; no drive at the front, no `.hg`,
/// `.hg.` or nothing as the first component and no `..` anywhere; no Windows short name of `.hg` (`HG~<digits>`,
/// `HG8B6C~<digits>`, in any case); no `.hg` or `.hg.` as a later component. A component is compared lower-cased with
/// the code points HFS+ ignores removed (`_lowerclean`). A drive is refused whatever the server's system, so every
/// platform judges a path alike (R8), where 7.2's `splitdrive` finds one only on Windows. Then slates': UTF-8, no NUL,
/// and no empty or `.` component, which 7.2 takes and a slates path cannot hold.
pub fn audit(path: &[u8]) -> Result<(), Unverified> {
  audited(path).map_err(|fault| Unverified::Path {
    path: path.to_vec(),
    fault,
  })
}

/// The fault [`audit`] finds in `path`, if any.
fn audited(path: &[u8]) -> Result<(), PathFault> {
  if path.last() == Some(&SLASH) {
    return Err(PathFault::EndsInSeparator);
  }
  let parts: Vec<&[u8]> = path.split(|byte| *byte == SLASH).collect();
  let first = parts.first().copied().unwrap_or_default();
  let drive = first.len() >= DRIVE_LEN
    && first.first().is_some_and(u8::is_ascii_alphabetic)
    && first.get(1) == Some(&DRIVE);
  let top_reserved = first.is_empty() || DOT_HG.contains(&lower_clean(first).as_slice());
  if drive || top_reserved || parts.contains(&b"..".as_slice()) || parts.iter().any(|part| short_name(part)) {
    return Err(PathFault::IllegalComponent);
  }
  let lowered: Vec<Vec<u8>> = parts.iter().map(|part| lower_clean(part)).collect();
  for reserved in DOT_HG {
    if let Some(at) = lowered.iter().skip(1).position(|part| part.as_slice() == reserved) {
      let base = parts.get(..at.saturating_add(1)).unwrap_or_default().join(&SLASH);
      return Err(PathFault::NestedRepository { base });
    }
  }
  let unheld = core::str::from_utf8(path).is_err()
    || path.contains(&0)
    || parts.iter().any(|part| part.is_empty() || *part == b".");
  if unheld {
    return Err(PathFault::NotHeld);
  }
  Ok(())
}

/// Whether `part` is a Windows short name 7.2 refuses: a stem of [`SHORT_STEMS`] in any case, `~`, and digits.
fn short_name(part: &[u8]) -> bool {
  let Some(at) = part.iter().position(|byte| *byte == SHORT_NAME) else {
    return false;
  };
  let (stem, number) = (
    part.get(..at).unwrap_or_default(),
    part.get(at.saturating_add(1)..).unwrap_or_default(),
  );
  !number.is_empty()
    && number.iter().all(u8::is_ascii_digit)
    && SHORT_STEMS.iter().any(|known| stem.eq_ignore_ascii_case(known))
}

/// `part` lower-cased as Python lower-cases bytes (ASCII alone), with the code points HFS+ ignores removed
/// (`_lowerclean`, `encoding.hfsignoreclean`).
fn lower_clean(part: &[u8]) -> Vec<u8> {
  let mut cleaned = part.to_ascii_lowercase();
  for ignored in HFS_IGNORED {
    while let Some(at) = cleaned
      .windows(ignored.len())
      .position(|window| window == ignored)
    {
      cleaned.drain(at..at.saturating_add(ignored.len()));
    }
  }
  cleaned
}
