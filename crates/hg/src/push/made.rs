//! The changeset the server makes of a version submitted to a Mercurial repository's green (A-731; A-664 item 4's
//! Mercurial half; §4.17.1 "A push is a work" items 3 and 4): a slates-native work carries no changeset, so the server
//! makes it after the verdict, as 7.2's `localrepo.commit` makes one (`commitctx`).
//!
//! **What.** From the parent changeset (the line's head before the version) and the version's change (each path the
//! green changed, its bytes and flag after, or none where it was removed): each changed file's revision, its text the
//! file's bytes (a metadata block's markers put before bytes that begin with one, as 7.2's `filelog.add` does), its
//! first parent the parent manifest's node for the path; the manifest, the parent's with each changed path set or
//! removed, its first parent the parent's manifest; the changeset, naming the manifest, the user, the time in whole
//! seconds at UTC, the branch where it is not `default`, the paths touched in their order, and the message. Each node is
//! 7.2's (SHA-1 of the parents and the text). A file whose text is its parent revision's makes no revision (7.2's
//! `filelog.cmp`): with the parent's flag too it is not listed, with another only its flag changes. What the version brought is a [`Verified`], which the server keeps and records as a push's (`commit::fork_with`,
//! `import::record::write_made`).
//!
//! **Not made** ([`Unmade`]): a parent the index does not hold, a field 7.2 would not write as it is, a version that
//! changed no tracked file.
//!
//! **Owed** (GAPS, A-731): a copy as a file revision naming its source (`copy`, `copyrev`); a merge's second parent.

use std::collections::BTreeMap;

use super::verify::{Arrived, Held, Verified};
use crate::decimal::Decimal;
use crate::manifest::{self, Entry, Flag};
use crate::pushkey::encode_hex;
use crate::read::to_u64;
use crate::{Admission, NULL_NODE, Node, PastAdmission, changeset, filerev, node};

/// Format: the branch 7.2 names in no extra.
const DEFAULT_BRANCH: &[u8] = b"default";
/// Format: the extra naming a changeset's branch.
const BRANCH: &[u8] = b"branch";
/// Format: the timezone of a made changeset, UTC (seconds west of it).
const UTC: &[u8] = b"0";

/// One path the version changed: its bytes and flag after, or none where it was removed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Change<'c> {
  /// The path, as the manifest names it.
  pub path: &'c [u8],
  /// Its bytes and flag after the version; none where the version removed it.
  pub after: Option<(&'c [u8], Flag)>,
}

/// Who made the version, when, on which branch, and why.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Authored<'a> {
  /// The user, as 7.2 writes it (`Name <email>`).
  pub user: &'a [u8],
  /// Seconds since the epoch, at UTC.
  pub seconds: u64,
  /// The branch the line is a head of.
  pub branch: &'a [u8],
  /// The message.
  pub message: &'a [u8],
}

/// Why no changeset was made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unmade {
  /// The parent changeset, or its manifest, is not one the index holds.
  ParentUnknown,
  /// A field 7.2 would not write as it is.
  Unencodable(&'static str),
  /// A text whose SHA-1 meets a known collision attack.
  Collision,
  /// The version changed no tracked file.
  NothingChanged,
  /// More than the admission had left.
  PastAdmission(PastAdmission),
}

impl From<PastAdmission> for Unmade {
  fn from(past: PastAdmission) -> Self {
    Unmade::PastAdmission(past)
  }
}

/// The manifest's entries, each path its node and flag.
type Entries = BTreeMap<Vec<u8>, (Node, Flag)>;

/// The changeset, manifest and file revisions the server makes of a version over `parent` (module docs), each text
/// charged to `admission`.
pub fn made(
  held: &(impl Held + ?Sized),
  parent: Node,
  changes: &[Change<'_>],
  authored: &Authored<'_>,
  admission: &mut Admission,
) -> Result<Verified, Unmade> {
  let (parent_manifest, mut entries) = parent_entries(held, parent, admission)?;
  let mut ordered: Vec<&Change<'_>> = changes.iter().collect();
  ordered.sort_by_key(|change| change.path);
  let mut files: Vec<(Vec<u8>, Vec<Arrived>)> = Vec::new();
  let mut touched: Vec<&[u8]> = Vec::new();
  for change in ordered {
    let previous = entries.get(change.path).copied();
    let Some((bytes, flag)) = change.after else {
      if entries.remove(change.path).is_some() {
        touched.push(change.path);
      }
      continue;
    };
    let text = file_text(bytes, admission)?;
    // The same text as the parent's revision keeps the parent's node, as 7.2's `filelog.cmp` passes it over: only a
    // changed flag is then the change.
    let kept = previous.filter(|(node, _)| {
      held
        .file_text(change.path, node)
        .is_some_and(|held| *held == *text)
    });
    let file = match kept {
      Some((node, held_flag)) => {
        if held_flag == flag {
          continue;
        }
        node
      }
      None => {
        let p1 = previous.map_or(NULL_NODE, |(node, _)| node);
        let file = node::compute(&p1, &NULL_NODE, &text).map_err(|_| Unmade::Collision)?;
        files.push((change.path.to_vec(), vec![arrived(file, p1, text)]));
        file
      }
    };
    entries.insert(change.path.to_vec(), (file, flag));
    touched.push(change.path);
  }
  if touched.is_empty() {
    return Err(Unmade::NothingChanged);
  }
  let manifest_text = manifest_text(&entries, admission)?;
  let manifest =
    node::compute(&parent_manifest, &NULL_NODE, &manifest_text).map_err(|_| Unmade::Collision)?;
  let changeset_text = changeset_text(&manifest, &touched, authored, admission)?;
  let changeset =
    node::compute(&parent, &NULL_NODE, &changeset_text).map_err(|_| Unmade::Collision)?;
  let mut made_changeset = arrived(changeset, parent, changeset_text);
  made_changeset.link = changeset;
  let mut made_manifest = arrived(manifest, parent_manifest, manifest_text);
  made_manifest.link = changeset;
  for (_, revisions) in &mut files {
    for revision in revisions {
      revision.link = changeset;
    }
  }
  Ok(Verified::of(
    vec![made_changeset],
    vec![made_manifest],
    files,
  ))
}

/// A revision made with its first parent, no second, its link set after.
fn arrived(node: Node, p1: Node, text: Vec<u8>) -> Arrived {
  Arrived {
    node,
    p1,
    p2: NULL_NODE,
    link: NULL_NODE,
    text,
  }
}

/// The parent's manifest node and entries; the null manifest's, none, for the null parent.
fn parent_entries(
  held: &(impl Held + ?Sized),
  parent: Node,
  admission: &mut Admission,
) -> Result<(Node, Entries), Unmade> {
  if parent == NULL_NODE {
    return Ok((NULL_NODE, Entries::new()));
  }
  let text = held.changeset_text(&parent).ok_or(Unmade::ParentUnknown)?;
  let manifest = changeset::decode(&text)
    .and_then(|read| read.manifest_node())
    .map_err(|_| Unmade::ParentUnknown)?;
  if manifest == NULL_NODE {
    return Ok((NULL_NODE, Entries::new()));
  }
  let listed = held.manifest_text(&manifest).ok_or(Unmade::ParentUnknown)?;
  admission.charge(to_u64(listed.len()))?;
  let entries = manifest::decode(&listed)
    .map_err(|_| Unmade::ParentUnknown)?
    .map(|entry| (entry.path.to_vec(), (entry.node, entry.flag)))
    .collect();
  Ok((manifest, entries))
}

/// A file's revision text: its bytes, behind an empty metadata block where they begin with its marker.
fn file_text(bytes: &[u8], admission: &mut Admission) -> Result<Vec<u8>, Unmade> {
  let revision = filerev::FileRevision {
    metadata: bytes.starts_with(filerev::MARKER).then_some(&[][..]),
    bytes,
  };
  let len = revision
    .encoded_len()
    .map_err(|_| Unmade::Unencodable("a file revision"))?;
  admission.charge(len)?;
  let mut text = Vec::new();
  revision
    .encode(&mut text)
    .map_err(|_| Unmade::Unencodable("a file revision"))?;
  Ok(text)
}

/// The manifest's text from its entries, in path order.
fn manifest_text(entries: &Entries, admission: &mut Admission) -> Result<Vec<u8>, Unmade> {
  let listed: Vec<Entry<'_>> = entries
    .iter()
    .map(|(path, (node, flag))| Entry {
      path,
      node: *node,
      flag: *flag,
    })
    .collect();
  let len = manifest::encoded_len(&listed).map_err(|_| Unmade::Unencodable("the manifest"))?;
  admission.charge(len)?;
  let mut text = Vec::new();
  manifest::encode(&listed, &mut text).map_err(|_| Unmade::Unencodable("the manifest"))?;
  Ok(text)
}

/// The changeset's text (module docs).
fn changeset_text(
  manifest: &Node,
  touched: &[&[u8]],
  authored: &Authored<'_>,
  admission: &mut Admission,
) -> Result<Vec<u8>, Unmade> {
  let mut hex = Vec::new();
  encode_hex(manifest, &mut hex);
  let time = Decimal::unsigned(authored.seconds);
  let mut extras = Vec::new();
  if authored.branch != DEFAULT_BRANCH {
    changeset::encode_extras(&[(BRANCH, authored.branch)], &mut extras)
      .map_err(|_| Unmade::Unencodable("the branch"))?;
  }
  let files = touched.join(&b'\n');
  let read = changeset::Changeset {
    manifest: &hex,
    user: authored.user,
    time: time.as_bytes(),
    timezone: UTC,
    extras: (!extras.is_empty()).then_some(extras.as_slice()),
    files: &files,
    description: authored.message,
  };
  let len = changeset::encoded_len(&read).map_err(|_| Unmade::Unencodable("the changeset"))?;
  admission.charge(len)?;
  let mut text = Vec::new();
  changeset::encode(&read, &mut text).map_err(|_| Unmade::Unencodable("the changeset"))?;
  Ok(text)
}

#[cfg(test)]
#[path = "made_tests.rs"]
mod tests;
