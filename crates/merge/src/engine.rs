//! The merge engine on one node (§4.16 "The verdict", "Splice", "Commit"; D-27). A green volume
//! is written only by its merge task. Agents clone a version, edit their clone, seal it, and
//! `submit` an increment against a base version. An increment is the deriver's canonical ops
//! document (`crate::ops_doc::OpsDoc`, every §4.16 operation kind, paths interned, content named by
//! offsets into the sealed post-state) plus that post-state's bytes. The engine resolves the
//! document, maps each content edit forward through everything committed since the base, decides the
//! pure verdict, splices the accepted edits into a new version, and commits it. Conflicts are
//! byte-exact windows (each with its class) the agent rebases against; the merge task never stalls.
//!
//! Consuming the whole ops document (rather than one change per path) is what lets every operation
//! kind merge, and merge together: a file can be edited and chmod'd and xattr'd in one increment; a
//! rename carries the file's edits; a directory move is the deriver's child renames plus mkdir and
//! rmdir, so it merges with no special case here; a hard link is a namespace edge (its content
//! sharing is the volume's concern at apply time, §4.16, not the merge's). The deriver has already
//! composed each path's operations and resolved their intra-increment ordering, so the engine sees
//! a clean, canonical set and only has to decide it against the intervening history.
//!
//! The dimensions merge independently, the way the deriver composes them:
//! - **Content**: each net op's base range is mapped forward; a range disjoint from every
//!   intervening change is accepted at the shifted position, a range that meets one is a conflict
//!   unless the agent produced exactly the green's current bytes for that file (both made the same
//!   edit, which accepts).
//! - **Create / unlink**: a create conflicts with an intervening differing create, accepts an
//!   identical one, else creates; an unlink accepts (or is a no-op if already gone), and a
//!   delete/modify conflict guards an intervening edit.
//! - **Directory, mode, symlink, hard link, xattr**: each is a per-path (xattr: per path and name)
//!   value; two differing changes since the base conflict, an identical one accepts; a file and a
//!   non-file at one path is a type conflict; a directory is removed only once the increment's own
//!   removals empty it.
//!
//! This is the laptop-degenerate engine (R8): one green, one merge task, an in-memory version chain
//! and a `seen` set for idempotent retries. The same pipeline runs in a fleet, where the commit is
//! a fenced ledger-register entry to the green's candidate holders (§4.8, `crate::` is pure here)
//! and holders recompute the verdict before serving — those are the engine's remaining pieces
//! (owed). The per-path content history kept for base reconstruction is a full byte copy per change;
//! the design's chain shares it copy-on-write, the measured optimization (owed).
//!
//! **The state is persistent maps (A-534).** Each dimension's current values are a persistent radix tree
//! ([`crate::dims`]), so a green made at another green's version ([`Lineage::branch`]) shares every node of the other's
//! state and costs what changed since that version, never the tree (§4.17.1 item 4).
//!
//! **One history rule (A-534).** A key with no history has held its current value since version 0. A key's first
//! recorded change first records, at version 0, the value it held, when it held one; a key that appears records only
//! its appearance. So a green seeded from its origin records no history for the origin's keys (until 2026-11-19 every
//! origin key had a version-0 entry, which held a second copy of every origin file's bytes), a green made at another's
//! version starts with none, and every reader of a key's value at a version answers from the history when the key has
//! one and from the current value when it has none.
//!
//! **Copies (A-586).** `Copy` copies a path's tree by reference, as Subversion's copy makes a branch or a tag, recorded
//! once at its destination and never key by key: a key in a copied tree reads by the latest copy above it while it holds
//! what that copy gave it, and its first change after the copy records that value at the copy's version, as the one
//! history rule records a key's first change at version 0 (`copy`, its module docs).

use crate::special::SpecialNode;

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use crate::dims::{Dim, Files, Xattrs};
use crate::ops_doc::{Op, OpKind, OpsDoc};
use crate::range::Range;
use crate::verdict::MergeConflictClass;

/// An increment submitted against a base version: the deriver's ops document and the sealed
/// post-state its content operations' `src` offsets index into, and the evidence references the
/// submitter attached (§4.16 `Increment.evidence`: "opaque to slates" — a green with
/// `require_evidence` refuses an increment without any; the engine never reads them).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Increment {
  /// The increment's identity (`blake3` of its declared work).
  pub id: [u8; 32],
  /// The version it was based on.
  pub base: u64,
  /// The canonical ops document (every declared operation, paths interned).
  pub doc: OpsDoc,
  /// The sealed post-state bytes; a content op adds `post_state[src .. src + len]`.
  pub post_state: Vec<u8>,
  /// The evidence references (BLAKE3 identities, opaque here) the submitter attached.
  pub evidence: Vec<[u8; 32]>,
}

/// Format: the width of one evidence reference (a BLAKE3 identity) in the increment encoding — the
/// divisor a declared evidence count is checked against before allocation.
const EVIDENCE_BYTES: usize = size_of::<[u8; 32]>();

impl Increment {
  /// Serializes the increment for the durable green chain (§4.16, §4.8): the identity, the base
  /// version, then the ops document and the post-state, each length-delimited, then the evidence
  /// references as a count and the identities. The db stores these bytes opaquely (it never parses
  /// a merge structure); the server encodes here on commit and decodes on recovery. Little-endian
  /// and length-delimited so [`Increment::decode`] is exact.
  pub fn encode(&self) -> Vec<u8> {
    let doc = self.doc.encode();
    let mut out = Vec::with_capacity(
      [
        self.id.len(),
        size_of::<u64>(), // the base version
        size_of::<u64>(), // the ops document's length
        size_of::<u64>(), // the post-state's length
        size_of::<u64>(), // the evidence count
        doc.len(),
        self.post_state.len(),
        self.evidence.len().saturating_mul(EVIDENCE_BYTES),
      ]
      .into_iter()
      .fold(0, usize::saturating_add),
    );
    out.extend_from_slice(&self.id);
    out.extend_from_slice(&self.base.to_le_bytes());
    out.extend_from_slice(&(doc.len() as u64).to_le_bytes());
    out.extend_from_slice(&doc);
    out.extend_from_slice(&(self.post_state.len() as u64).to_le_bytes());
    out.extend_from_slice(&self.post_state);
    out.extend_from_slice(&(self.evidence.len() as u64).to_le_bytes());
    for reference in &self.evidence {
      out.extend_from_slice(reference);
    }
    out
  }

  /// Decodes an increment persisted by [`Increment::encode`] — the inverse used to replay a green's
  /// chain on recovery. Every length is bounds-checked against the bytes that remain before it is
  /// read (a torn db entry never allocates a wild length; a wild evidence count is refused before
  /// its table is reserved), and any malformation is a typed [`crate::ops_doc::DocDecodeError`], never a panic.
  pub fn decode(bytes: &[u8]) -> Result<Increment, crate::ops_doc::DocDecodeError> {
    use crate::ops_doc::{DocDecodeError, OpsDoc, Reader};
    let mut reader = Reader::new(bytes);
    let mut id = [0u8; 32];
    let id_len = id.len();
    id.copy_from_slice(reader.bytes(id_len)?);
    let base = reader.u64()?;
    let doc_len = usize::try_from(reader.u64()?).map_err(|_| DocDecodeError::Truncated)?;
    let doc = OpsDoc::decode(reader.bytes(doc_len)?)?;
    let post_len = usize::try_from(reader.u64()?).map_err(|_| DocDecodeError::Truncated)?;
    let post_state = reader.bytes(post_len)?.to_vec();
    for op in doc.ops.iter().filter(|op| op.kind == OpKind::Mknod) {
      special_payload(&doc, &post_state, op)?;
    }
    let evidence_count = usize::try_from(reader.u64()?).map_err(|_| DocDecodeError::Truncated)?;
    if evidence_count > reader.remaining() / EVIDENCE_BYTES {
      return Err(DocDecodeError::Truncated);
    }
    let mut evidence = Vec::with_capacity(evidence_count);
    for _ in 0..evidence_count {
      let mut reference = [0u8; 32];
      reference.copy_from_slice(reader.bytes(EVIDENCE_BYTES)?);
      evidence.push(reference);
    }
    if !reader.is_empty() {
      return Err(DocDecodeError::TrailingBytes);
    }
    Ok(Increment {
      id,
      base,
      doc,
      post_state,
      evidence,
    })
  }
}

/// A conflict window: the file, the range (base coordinates) that met an intervening change, and
/// the class of the conflict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictWindow {
  /// The file.
  pub path: String,
  /// The conflicting range (a zero-length anchor for a whole-path namespace conflict).
  pub range: Range,
  /// The class of the conflict.
  pub class: MergeConflictClass,
}

/// The result of a submit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
  /// The increment was merged; the green's new head version.
  Accepted {
    /// The committed version.
    version: u64,
  },
  /// The increment conflicts; the windows to rebase against.
  Conflict {
    /// The conflicting windows.
    windows: Vec<ConflictWindow>,
  },
  /// The increment's base is below the version the green's chain was checkpointed at
  /// ([`Green::checkpoint`]): the deltas it would be mapped through are folded away, so it is never judged. No live
  /// reader is below that version (the service checkpoints at the reachable floor), so a caller meets this only with a
  /// base no longer anyone's; the service answers it `UnknownBase`.
  BaseUnavailable {
    /// The increment's base.
    base: u64,
    /// The oldest base the green still judges.
    floor: u64,
  },
}

/// The result of a rebase (§4.16 "Rebase, the only corrective path"). The green is never changed by
/// a rebase; only the work moves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rebased {
  /// The work's pending operations all mapped cleanly onto the head. The work's base becomes
  /// `version`, its content becomes the head's with `changed` over it (the files the work's mapped
  /// edits leave different from the head's, and the head's files they remove), and its declared
  /// operations are restated in head coordinates as `journal` (so the next submit composes against
  /// the new base with no further mapping). O(what the work changed), never the head's other files
  /// (the whole content was cloned and restated before 2026-11-05).
  Rebased {
    /// The head the work is now based on.
    version: u64,
    /// Per path the work changed: its bytes after the rebase where they differ from the head's, or
    /// `None` where the work removes a file the head holds.
    changed: BTreeMap<String, Option<Vec<u8>>>,
    /// The work's declared operations restated relative to the new base.
    journal: Vec<crate::increment::VolumeOp>,
  },
  /// The pending operations conflict; the windows to resolve, nothing changed.
  Conflict {
    /// The conflicting windows.
    windows: Vec<ConflictWindow>,
  },
  /// The work's base is below the version the green's chain was checkpointed at ([`Outcome::BaseUnavailable`]).
  BaseUnavailable {
    /// The work's base.
    base: u64,
    /// The oldest base the green still maps.
    floor: u64,
  },
}

/// A per-path content history: for each path, the versions at which its bytes changed and the bytes
/// then (`None` when removed), so any base version's content can be reconstructed for the identity
/// check. The design's chain shares these copy-on-write; here they are full copies (owed).
type ContentHistory = BTreeMap<String, Vec<(u64, Option<crate::history::Value>)>>;

/// A per-path value history for a namespace dimension: the value at each version the path changed,
/// `None` marking a removal (mode, symlink target, hard-link target). Reconstructs a base version.
type ValueHistory<T> = BTreeMap<String, Vec<(u64, Option<T>)>>;

/// A per-`(path, name)` xattr value history: the value at each version it changed, `None` a removal.
type XattrHistory = BTreeMap<(String, String), Vec<(u64, Option<Vec<u8>>)>>;

/// A per-path directory-presence history: whether the directory existed at each version it changed.
type DirHistory = BTreeMap<String, Vec<(u64, bool)>>;

/// Format: the kind tags of [`Green::head_identity`]'s canonical encoding, one per dimension, so an
/// entry of one table can never hash as an entry of another (a file named `d` and a directory `d`
/// are distinct states).
const IDENTITY_TAG_FILE: &[u8] = b"F";
/// Format: see [`IDENTITY_TAG_FILE`].
const IDENTITY_TAG_DIR: &[u8] = b"D";
/// Format: see [`IDENTITY_TAG_FILE`].
const IDENTITY_TAG_MODE: &[u8] = b"M";
/// Format: see [`IDENTITY_TAG_FILE`].
const IDENTITY_TAG_SYMLINK: &[u8] = b"S";
/// Format: see [`IDENTITY_TAG_FILE`].
const IDENTITY_TAG_HARDLINK: &[u8] = b"L";
/// Format: see [`IDENTITY_TAG_FILE`].
const IDENTITY_TAG_XATTR: &[u8] = b"X";
/// Format: IPC metadata has a separate identity domain from regular files and links.
const IDENTITY_TAG_SPECIAL: &[u8] = b"I";
/// Format: a copy's entry (A-586), which stands for every entry the copy brought.
const IDENTITY_TAG_COPY: &[u8] = b"C";
/// A key of the green's state whose identity entry a commit changed (A-202): which dimension, and its path (and an
/// attribute's name); or the destination of a copy made since (A-586).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum IdentityKey {
  /// A file.
  File(String),
  /// A FIFO/socket inode.
  Special(String),
  /// A directory.
  Dir(String),
  /// A path's mode.
  Mode(String),
  /// A symlink.
  Symlink(String),
  /// A hard link.
  Hardlink(String),
  /// An extended attribute: its path and name.
  Xattr(String, String),
  /// The copies made at a destination (A-586).
  Copy(String),
}

/// A copy's identity entry (A-586): its tag, destination, source and the source's version. It stands for every entry the
/// copy brought, which the set hash never adds one by one (`copy`'s module docs, "The identity").
fn copy_entry(dst: &str, src: &str, source_version: u64) -> crate::identity::Entry {
  crate::identity::Entry::new(IDENTITY_TAG_COPY)
    .field(dst.as_bytes())
    .field(src.as_bytes())
    .fixed(&source_version.to_le_bytes())
}

/// A FIFO/socket inode's identity entry: its tag, path and canonical metadata.
fn special_entry(path: &str, node: &SpecialNode) -> crate::identity::Entry {
  crate::identity::Entry::new(IDENTITY_TAG_SPECIAL)
    .field(path.as_bytes())
    .field(&node.encode())
}

/// A file's identity entry: its tag, path and bytes.
fn file_entry(path: &str, bytes: &[u8]) -> crate::identity::Entry {
  crate::identity::Entry::new(IDENTITY_TAG_FILE)
    .field(path.as_bytes())
    .field(bytes)
}

/// A directory's identity entry: its tag and path.
fn dir_entry(path: &str) -> crate::identity::Entry {
  crate::identity::Entry::new(IDENTITY_TAG_DIR).field(path.as_bytes())
}

/// A mode's identity entry: its tag, path and the mode's four bytes.
fn mode_entry(path: &str, mode: u32) -> crate::identity::Entry {
  crate::identity::Entry::new(IDENTITY_TAG_MODE)
    .field(path.as_bytes())
    .fixed(&mode.to_le_bytes())
}

/// A symlink's or hard link's identity entry: its kind's tag, path and target.
fn link_entry(tag: &[u8], path: &str, target: &str) -> crate::identity::Entry {
  crate::identity::Entry::new(tag)
    .field(path.as_bytes())
    .field(target.as_bytes())
}

/// An extended attribute's identity entry: its tag, path, name and value.
fn xattr_entry(path: &str, name: &str, value: &[u8]) -> crate::identity::Entry {
  crate::identity::Entry::new(IDENTITY_TAG_XATTR)
    .field(path.as_bytes())
    .field(name.as_bytes())
    .field(value)
}

/// Derived: a retained IPC metadata payload occupies its Rust value's size, including alignment
/// padding. Anchor: the actual `SpecialNode` layout on the target, measured by `size_of`.
const SPECIAL_NODE_BYTES: usize = size_of::<SpecialNode>();

/// A green being seeded from its origin a share at a time (§4.16 "the origin version from a snapshot"; §4.3 "Bounded
/// work everywhere"): the origin's entries are moved into the engine — each one's identity added and its value set,
/// with no history, as the module's one history rule has it (A-534) — until a share's bytes are in, so a kernel-sized
/// origin (97,229 files) seeds over many of a shard's steps instead of one poll. The order entries go in does not reach
/// the result: the identity is a set hash and every table is keyed. [`Green::with_origin`] is the load run to its end.
#[derive(Debug)]
pub struct OriginLoad {
  green: Green,
  origin: crate::origin::Origin,
}

impl OriginLoad {
  /// A load of `origin`, canonicalized in place unless it already is (later declarations of a repeated key win, as
  /// [`crate::origin::Origin::canonicalize`] keeps them); a walk of a snapshot gives a canonical one.
  pub fn new(mut origin: crate::origin::Origin) -> OriginLoad {
    if !origin.is_canonical() {
      origin.canonicalize();
    }
    OriginLoad {
      green: Green::default(),
      origin,
    }
  }

  /// Moves entries in until at least `budget_bytes` of their values and paths have gone in, or the origin is empty (an
  /// entry is the unit, so a share overruns by at most one file); the seeded green once the last is in.
  pub fn step(&mut self, budget_bytes: usize) -> Option<Green> {
    let mut moved = 0usize;
    while moved < budget_bytes {
      match self.move_one() {
        Some(bytes) => moved = moved.saturating_add(bytes),
        None => return Some(std::mem::take(&mut self.green)),
      }
    }
    None
  }

  /// Moves one entry of the origin into the green, the files first; the bytes it moved, `None` once the origin is
  /// empty.
  fn move_one(&mut self) -> Option<usize> {
    let green = &mut self.green;
    let origin = &mut self.origin;
    if let Some((path, bytes)) = origin.files.pop() {
      let moved = path.len().saturating_add(bytes.len());
      green.identity.add(&file_entry(&path, &bytes));
      green.content.insert(&path, bytes);
      return Some(moved);
    }
    if let Some((path, node)) = origin.specials.pop() {
      let moved = path.len();
      green.identity.add(&special_entry(&path, &node));
      green.specials.insert(&path, node);
      return Some(moved);
    }
    if let Some(path) = origin.dirs.pop() {
      let moved = path.len();
      if green.dirs.insert(&path, ()).is_none() {
        green.identity.add(&dir_entry(&path));
      }
      return Some(moved);
    }
    if let Some((path, mode)) = origin.modes.pop() {
      let moved = path.len();
      green.identity.add(&mode_entry(&path, mode));
      green.modes.insert(&path, mode);
      return Some(moved);
    }
    if let Some((path, target)) = origin.symlinks.pop() {
      let moved = path.len().saturating_add(target.len());
      green
        .identity
        .add(&link_entry(IDENTITY_TAG_SYMLINK, &path, &target));
      green.symlinks.insert(&path, target);
      return Some(moved);
    }
    if let Some((path, target)) = origin.hardlinks.pop() {
      let moved = path.len().saturating_add(target.len());
      green
        .identity
        .add(&link_entry(IDENTITY_TAG_HARDLINK, &path, &target));
      relink(&mut green.links_to, &path, None, Some(&target));
      green.hardlinks.insert(&path, target);
      return Some(moved);
    }
    if let Some((path, name, value)) = origin.xattrs.pop() {
      let moved = path
        .len()
        .saturating_add(name.len())
        .saturating_add(value.len());
      green.identity.add(&xattr_entry(&path, &name, &value));
      green.xattrs.insert(&path, &name, value);
      return Some(moved);
    }
    None
  }
}

/// A green volume's merge state. Each dimension is a current value plus the version it last changed
/// at (the base-comparison index), and a per-path/key *history* of `(version, value)` entries so any
/// base version can be reconstructed for a lagging work. The content dimension's history is copy-on-write
/// (`crate::history`, W2): each version of a file is pieces of shared immutable chunks, so a version costs the bytes it
/// added, not the file; the namespace dimensions' histories hold presence or the value. The current values are
/// persistent maps ([`crate::dims`]) and a key with no history has held its current value since version 0 (the module
/// docs, A-534).
#[derive(Debug, Default)]
pub struct Green {
  /// The set hash of every entry at `identity_version` (`crate::identity`, A-202): brought to the head only when
  /// the identity is read ([`Green::head_identity`]), by the keys in `identity_dirty`.
  identity: crate::identity::SetHash,
  /// The version `identity` is the set hash of.
  identity_version: u64,
  /// The keys whose entries any commit after `identity_version` changed: bounded by the keys the green's histories
  /// hold, each once however often it changed.
  identity_dirty: BTreeSet<IdentityKey>,
  /// The current files (persistent, [`crate::dims`]).
  content: Files,
  content_history: ContentHistory,
  /// The chunks the content history's values share (`crate::history`).
  chunks: crate::history::Chunks,
  /// The bytes every content history value's piece list costs, a running total.
  history_lists: usize,
  /// The bytes of every path's latest content value and its piece list (the current files, as the history holds
  /// them), a running total: what [`Green::history_bytes`] leaves out, as the current values were always left out.
  specials: Dim<SpecialNode>,
  special_history: ValueHistory<SpecialNode>,
  special_changed: BTreeMap<String, u64>,
  dirs: Dim<()>,
  modes: Dim<u32>,
  symlinks: Dim<String>,
  hardlinks: Dim<String>,
  xattrs: Xattrs,
  dir_history: DirHistory,
  mode_history: ValueHistory<u32>,
  symlink_history: ValueHistory<String>,
  hardlink_history: ValueHistory<String>,
  xattr_history: XattrHistory,
  /// The deltas of the versions after `deltas_from`, in order: `deltas[i]` is version `deltas_from + i + 1`'s.
  deltas: Vec<BTreeMap<String, Vec<Op>>>,
  /// The version the green was restored at from a checkpoint ([`Green::restore`]), 0 for a green built from its origin:
  /// the deltas up to it are folded into the checkpoint, so a base below it is not mapped.
  deltas_from: u64,
  last_changed: BTreeMap<String, u64>,
  /// The last version that removed the file at each path, a file of any type: a regular file (an
  /// unlink, a rename away, or a rename onto it, which removes the file it replaces; A-612), a
  /// symbolic link, a hard link's name, an IPC node or a directory (A-631). A work based before it
  /// names a file that is gone, whatever stands at the path now (made again, renamed onto it), so
  /// the verdict answers delete against modify for an edit, a mode change or an attribute set or
  /// removed there, and rename against rename for a rename of it, never a change or a move of the
  /// file that took its place. Unlike the content history it is never folded, so a base below the
  /// fold floor meets it too. One entry a path the green ever removed a file at, as `last_changed`
  /// holds one a path it ever changed; empty in a green restored from a checkpoint or made as a
  /// branch, whose bases are never before its first version.
  file_removed: BTreeMap<String, u64>,
  mode_changed: BTreeMap<String, u64>,
  symlink_changed: BTreeMap<String, u64>,
  hardlink_changed: BTreeMap<String, u64>,
  xattr_changed: BTreeMap<(String, String), u64>,
  /// Every accepted increment's identity and the version it committed: the idempotency record a
  /// retry of an accepted submit is answered from. Bounded by the chain (one entry per committed
  /// version), which the partition caps in bytes; recovery rebuilds it by replaying the chain.
  accepted: BTreeMap<[u8; 32], u64>,
  /// How the signature of each version's submit was judged, for every version a signed submit committed (signed work,
  /// A-321): the version's provenance, kept with the accepted record and bounded as it is (at most one entry per
  /// committed version), carried by the chain entries and then by the checkpoint that folds them, so a version's verdict
  /// reads back for as long as the green answers its idempotency record.
  signatures: BTreeMap<u64, crate::chain_entry::Signature>,
  /// The **rejected-result cache** (§4.16; AUD-16): the conflict windows of increments that were
  /// refused, kept so a retry of the same increment is answered without re-judging — but a conflict
  /// is not in the chain, so nothing else bounds this; it is bounded here by `rejected_budget` bytes,
  /// the oldest entries evicted first (`rejected_order`). A retry of an evicted conflict is simply
  /// judged again: the verdict is deterministic in the increment, its base and the head, so the
  /// answer is the same while the head stands and honestly newer once it moved.
  rejected: BTreeMap<[u8; 32], Vec<ConflictWindow>>,
  /// The rejected identities in insertion order — the eviction order.
  rejected_order: std::collections::VecDeque<[u8; 32]>,
  /// The bytes the rejected cache holds ([`window_bytes`] per window, plus the identity).
  rejected_bytes: usize,
  /// The bytes the rejected cache may hold; zero keeps no rejected result (every conflict retry is
  /// judged again). Set by the service from its derived merge budget (`set_rejected_budget`).
  rejected_budget: usize,
  /// Rejected results evicted or never retained for want of budget — the non-vacuity counter the
  /// bound's regression asserts.
  rejected_evicted: u64,
  history_current: usize,
  /// The bytes the IPC (special node) history retains beyond the current values, a running total: with the content
  /// history's chunks and lists less its current values, [`Green::history_bytes`].
  special_history_bytes: usize,
  /// The version below which the histories have been **folded** (`fold_history_before`): values
  /// recorded before it survive only as the one entry in effect at it, so a version below the floor
  /// is no longer reconstructible. The service keeps the floor at the oldest version any live reader
  /// — a work's base, a pinned attachment — can still name.
  folded_below: u64,
  fast_path_hits: u64,
  /// The change index (§4.16 `changed_since`, A-264): the paths each version changed in any dimension, each version's
  /// sorted and each path once, recorded as the version commits at the cost of its own changes, so what changed since a
  /// version is read in the changes since it and never the green's whole tree. A hard link is recorded at a version that
  /// changed the special it names, the rule [`Green::changed_between`] applies. Each entry carries whether its version
  /// is the last its path changed at (cleared on the earlier entry when a later version records the path again), and
  /// its place in its version's list as recorded, so a walk's cursor holds when a fold drops entries. Below the fold
  /// floor each path keeps only its last change before the floor, as the folded histories keep each key's value in
  /// effect at the floor, and a restore from a checkpoint indexes the same.
  changes: BTreeMap<u64, Vec<ChangeEntry>>,
  /// The last version in `changes` each path changed at: a path changed at several versions of a span is answered once,
  /// at the last of them. One entry a path ever changed, as the histories keep a key a path.
  latest_change: BTreeMap<String, u64>,
  /// Each path's last change below the fold floor, whose entry the change index keeps there ([`Green::fold_history_before`]).
  below_floor_last: BTreeMap<String, u64>,
  /// The hard links naming each target, the reverse of `hardlinks`: the links a special's change changes.
  links_to: Dim<BTreeSet<String>>,
  /// The bytes `changes` and `latest_change` hold, a running total counted in [`Green::history_bytes`], so the index
  /// folds under the same budget as the histories it indexes.
  changes_bytes: usize,
  /// The deltas the verdict's position mapping consulted (`intervening`): the non-vacuity count that it reads only the
  /// deltas that changed a path, never every delta since a base. A cell, as the deltas it counts are borrowed.
  deltas_visited: std::cell::Cell<u64>,
  /// Every copy committed (A-586), at its destination, in version order: one record a copy, never one a key. Each lives
  /// as long as the green, since a key it covers reads by it whatever the fold; its bytes are charged with the change
  /// index's (`changes_bytes`), and a green holding one is not checkpointed (`Green::checkpoint`), so the chain bounds
  /// them.
  copies: BTreeMap<String, Vec<copy::Appearance>>,
  /// Copies whose maps brought a tree: the non-vacuity count of the copy's path.
  copies_grafted: u64,
  /// Copies a map refused at commit after the verdict judged them possible: zero in every correct use.
  copies_refused: u64,
}

/// The paths a node-exclusive operation changes, each with the class its conflict is named by (§4.17.4 item 3): the
/// operation's own path, and a move's source, which Subversion sends as a copy and a delete of the source.
fn node_exclusive_paths(op: &Op) -> impl Iterator<Item = (u64, MergeConflictClass)> {
  let class = match op.kind {
    OpKind::Create
    | OpKind::Mkdir
    | OpKind::Symlink
    | OpKind::Link
    | OpKind::Mknod
    | OpKind::Copy => MergeConflictClass::CreateCreate,
    OpKind::Unlink | OpKind::Rmdir => MergeConflictClass::DeleteModify,
    OpKind::Rename => MergeConflictClass::RenameRename,
    OpKind::SetMode | OpKind::SetXattr | OpKind::RemoveXattr => MergeConflictClass::MetaMeta,
    OpKind::Overwrite | OpKind::Extend | OpKind::Truncate | OpKind::Insert | OpKind::Delete => {
      MergeConflictClass::Overlap
    }
  };
  let source = (op.kind == OpKind::Rename).then_some((op.src, MergeConflictClass::RenameRename));
  std::iter::once((op.path, class)).chain(source)
}

/// The bytes a retained conflict window costs the rejected-result cache: its path, its range and
/// its class — what the cache actually holds for it.
fn window_bytes(window: &ConflictWindow) -> usize {
  window
    .path
    .len()
    .saturating_add(size_of::<Range>())
    .saturating_add(size_of::<MergeConflictClass>())
}

/// The bytes a rejected result costs: its identity plus its windows.
fn rejected_entry_bytes(windows: &[ConflictWindow]) -> usize {
  windows
    .iter()
    .map(window_bytes)
    .fold(size_of::<[u8; 32]>(), usize::saturating_add)
}

/// What a green holds in memory beyond its durable chain (§4.2 all-cost admission; AUD-16): the
/// current files, the history retained for reconstructing earlier versions, and the rejected-result
/// cache. The service charges `history` and `rejected` as retention against the shard's budget.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RetainedBytes {
  /// Current file bytes and fixed IPC metadata payloads.
  pub content: usize,
  /// Earlier file content and IPC metadata retained for reconstruction.
  pub history: usize,
  /// The change index's bytes: the paths each version above the fold floor changed (§4.16 `changed_since`).
  pub changes: usize,
  /// The rejected-result cache's bytes.
  pub rejected: usize,
}

/// Every key of `history` whose value in effect at `at` was set after the origin (version 0): the key, that version and
/// that value (`None`: removed), for a checkpoint at `at`.
fn changed_at<K: Clone, V: Clone>(
  history: &BTreeMap<K, Vec<(u64, Option<V>)>>,
  at: u64,
) -> Vec<crate::checkpoint::Changed<K, V>> {
  history
    .iter()
    .filter_map(|(key, entries)| {
      let (version, value) = entries.iter().rev().find(|(v, _)| *v <= at)?;
      (*version > 0).then(|| crate::checkpoint::Changed {
        key: key.clone(),
        at: *version,
        value: value.clone(),
      })
    })
    .collect()
}

/// What a path's latest content value counts among the current values ([`Green::history_bytes`] leaves them out): its
/// bytes and its piece list.
fn current_bytes(value: &crate::history::Value) -> usize {
  value.len().saturating_add(value.list_bytes())
}

/// Folds one `(version, value)` history below `floor`: drops every entry recorded before `floor`
/// except the last of them (the value in effect at `floor`), so lookups at or above the floor are
/// unchanged. The dropped entries are returned, for the caller to release what they held. Entries are in version
/// order, as the histories keep them.
fn fold_entries<T>(history: &mut Vec<(u64, T)>, floor: u64) -> Vec<(u64, T)> {
  let below = history
    .iter()
    .take_while(|(version, _)| *version < floor)
    .count();
  // Every entry below the floor can go when one is recorded exactly at it (that one is the value at
  // the floor); otherwise the last entry below the floor is the value in effect there and stays.
  let at_floor = history
    .get(below)
    .is_some_and(|(version, _)| *version == floor);
  let drop = if at_floor {
    below
  } else {
    below.saturating_sub(1)
  };
  if drop == 0 {
    return Vec::new();
  }
  let drop = drop.min(history.len());
  history.drain(..drop).collect()
}

/// The value of a `(version, Option<value>)` history at `version`: the last entry recorded at or
/// before it, cloned. `None` when nothing was recorded by then or the last entry was a removal.
fn history_at<T: Clone>(history: &[(u64, Option<T>)], version: u64) -> Option<T> {
  history
    .get(..history.partition_point(|(recorded, _)| *recorded <= version))
    .and_then(<[_]>::last)
    .and_then(|(_, value)| value.clone())
}

/// Whether a per-version history has an entry at a version in `(from, to]` (the model's, `test-model`). Histories are in
/// version order, so this is a seek to the first entry past `from`, never a scan of the path's whole history (a file
/// many clients edit has an entry a version until the fold).
#[cfg(feature = "test-model")]
fn history_touches<T>(history: &[(u64, T)], from: u64, to: u64) -> bool {
  history
    .get(history.partition_point(|(version, _)| *version <= from))
    .is_some_and(|(version, _)| *version <= to)
}

/// Moves hard link `alias` from naming `old` to naming `new` in the reverse map of the links naming each target.
fn relink(links_to: &mut Dim<BTreeSet<String>>, alias: &str, old: Option<&str>, new: Option<&str>) {
  if let Some(old) = old
    && let Some(aliases) = links_to.get(old)
  {
    let mut aliases = aliases.clone();
    aliases.remove(alias);
    if aliases.is_empty() {
      links_to.remove(old);
    } else {
      links_to.insert(old, aliases);
    }
  }
  if let Some(new) = new {
    let mut aliases = links_to.get(new).cloned().unwrap_or_default();
    aliases.insert(alias.to_owned());
    links_to.insert(new, aliases);
  }
}

/// The version of a history's last entry: `None` for a key with no history.
fn last_version<T>(history: Option<&Vec<(u64, T)>>) -> Option<u64> {
  history
    .and_then(|history| history.last())
    .map(|(version, _)| *version)
}

/// Where a file's bytes at a version are read from ([`Green::content_read`]).
enum ContentRead<'a> {
  /// The head's: the file holds its current value then.
  Head,
  /// A value of its history: the file changed after the version.
  Value(&'a crate::history::Value),
  /// Nowhere: the file was absent then.
  Absent,
}

/// A key's value at `version` by the module's one history rule (A-534): from its history when it has one, else its
/// current value, which it has held since version 0.
fn value_at<T: Clone>(
  history: Option<&Vec<(u64, Option<T>)>>,
  current: Option<&T>,
  version: u64,
) -> Option<T> {
  match history {
    Some(history) => history_at(history, version),
    None => current.cloned(),
  }
}

/// When a key's held value is recorded before a change ([`seed`]): the version the value dates from, where the key's
/// history does not hold it already (`Green::seed_since`), and the change's version.
#[derive(Clone, Copy)]
struct Seed {
  since: Option<u64>,
  version: u64,
}

/// Before `key`'s first recorded change since it held a value it did not record, records at `at.since` the value it held
/// (`held`, `None` when it held nothing). At version 0 by the module's one history rule (A-534): a key that held a value
/// keeps it reconstructible at every version before the change, and a key that appears records only its appearance. At
/// the version of the copy that gave it the value, for a key in a copied tree (A-586). Nothing when its history holds
/// the value already, or when the copy is the change's own version, since the copy's value then never stood at one.
fn seed<K: Ord + Clone, T>(
  history: &mut BTreeMap<K, Vec<(u64, T)>>,
  key: &K,
  held: Option<T>,
  at: Seed,
) {
  if let Some(held) = held
    && let Some(since) = at.since.filter(|since| *since != at.version)
  {
    history.entry(key.clone()).or_default().push((since, held));
  }
}

/// A value history's entry for a key's current value, for [`seed`]: `None` when the key holds none.
fn held<T: Clone>(current: Option<&T>) -> Option<Option<T>> {
  current.map(|value| Some(value.clone()))
}

/// The keys of two sorted, distinct key sequences together, sorted and each once: a green's current keys and its
/// history's, which by the module's one history rule are every key it held at any version (A-534).
fn union_keys<K: Ord>(
  current: impl Iterator<Item = K>,
  recorded: impl Iterator<Item = K>,
) -> impl Iterator<Item = K> {
  let mut current = current.peekable();
  let mut recorded = recorded.peekable();
  std::iter::from_fn(move || match (current.peek(), recorded.peek()) {
    (Some(left), Some(right)) => match left.cmp(right) {
      std::cmp::Ordering::Less => current.next(),
      std::cmp::Ordering::Greater => recorded.next(),
      std::cmp::Ordering::Equal => {
        recorded.next();
        current.next()
      }
    },
    (Some(_), None) => current.next(),
    (None, _) => recorded.next(),
  })
}

/// Format: what the change index holds for each path a restore indexes: its entry in its version, its last change
/// (`latest_change`) and its last change below the floor (`below_floor_last`).
const RESTORED_INDEX_ENTRIES: usize = 3;

/// Notes in `last` that `path` changed at `version`, when that is after the origin and later than what it holds.
fn note_last(last: &mut BTreeMap<String, u64>, path: &str, version: Option<u64>) {
  if let Some(version) = version.filter(|version| *version > 0) {
    let at = last.entry(path.to_owned()).or_insert(version);
    *at = (*at).max(version);
  }
}

/// `paths` (sorted) as a version's entries of the change index, each numbered by its place and each its path's last.
/// A version holds far fewer than 2^32 paths: the green's memory is bounded long before.
fn numbered(paths: BTreeSet<String>) -> Vec<ChangeEntry> {
  paths
    .into_iter()
    .enumerate()
    .map(|(index, path)| ChangeEntry {
      index: u32::try_from(index).unwrap_or(u32::MAX),
      path,
      last: true,
    })
    .collect()
}

/// The bytes one path costs the change index, an entry of a version's list or of the last-change map: its own bytes and
/// the version beside it.
fn change_bytes(path: &str) -> usize {
  path.len().saturating_add(size_of::<u64>())
}

/// A path a version changed, in the change index (`Green::changes`).
#[derive(Clone, Debug)]
struct ChangeEntry {
  /// Its place in its version's sorted list as the version recorded it: kept when a fold drops the entries before it.
  index: u32,
  path: String,
  /// Whether its version is the last its path changed at.
  last: bool,
}

/// One entry of the change index ([`Green::changes_after`]): a path a version changed, where it stands in that version's
/// sorted list (a walk's cursor), and whether that version is the last the path changed at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChangeAt<'a> {
  /// The version that changed the path.
  pub version: u64,
  /// The path's place among the version's changed paths.
  pub index: usize,
  /// The path.
  pub path: &'a str,
  /// Whether `version` is the last version the path changed at: the entry a walk answers it at, once.
  pub last: bool,
}

/// The paths of a per-path history that changed at a version in `(from, to]` (the model's, `test-model`).
#[cfg(feature = "test-model")]
fn paths_changed_in<T>(
  histories: &BTreeMap<String, Vec<(u64, T)>>,
  from: u64,
  to: u64,
) -> impl Iterator<Item = String> + '_ {
  histories
    .iter()
    .filter(move |(_, history)| history_touches(history, from, to))
    .map(|(path, _)| path.clone())
}

/// Whether a `(version, present)` history has the entity present at `version`: the last entry at or
/// before it says so (absent when nothing was recorded by then).
fn present_at(history: &[(u64, bool)], version: u64) -> bool {
  history
    .get(..history.partition_point(|(recorded, _)| *recorded <= version))
    .and_then(<[_]>::last)
    .is_some_and(|(_, present)| *present)
}

/// The base range an op touches, for overlap testing.
fn touched_range(op: &Op) -> Range {
  match op.kind {
    OpKind::Overwrite | OpKind::Delete | OpKind::Truncate => Range::new(op.at, op.len),
    _ => Range::new(op.at, 0),
  }
}

/// Whether an op kind carries content bytes (its `src` names post-state bytes).
pub(crate) fn is_content(kind: OpKind) -> bool {
  matches!(
    kind,
    OpKind::Overwrite | OpKind::Extend | OpKind::Truncate | OpKind::Insert | OpKind::Delete
  )
}

/// Whether a path's content ops are a net op set the deriver could have composed against `base_len` bytes of
/// base content and a post-state of `post_len` bytes (`derive::compose_content`, §4.16 "Composition at seal"):
/// every op starts within the base, a removal or overwrite ends within it, a truncate ends at its end, an extend
/// starts at its end, and the bytes an op adds lie in the post-state. An ops document arrives as bytes and is
/// not checked against the file it edits when decoded, so a document that fails this is refused typed rather
/// than spliced by clamping (2026-10-06: a delete of 20 bytes from a 10-byte base, then an insert at 30, was
/// accepted as "XXZ").
fn fits_base(ops: &[Op], base_len: u64, post_len: u64) -> bool {
  ops.iter().all(|op| {
    let end = op.at.checked_add(op.len);
    let within = op.at <= base_len;
    let fits = match op.kind {
      OpKind::Delete | OpKind::Overwrite => end.is_some_and(|end| end <= base_len),
      OpKind::Truncate => end == Some(base_len),
      OpKind::Extend => op.at == base_len,
      _ => true,
    };
    let adds = matches!(op.kind, OpKind::Overwrite | OpKind::Insert | OpKind::Extend);
    let sourced = !adds
      || op
        .src
        .checked_add(op.len)
        .is_some_and(|end| end <= post_len);
    within && fits && sourced
  })
}

/// Applies content ops to `base`, drawing added bytes from `post_state` — the byte splice.
fn apply(base: &[u8], ops: &[Op], post_state: &[u8]) -> Vec<u8> {
  let base_len = base.len() as u64;
  let mut out = Vec::new();
  let mut pos = 0u64;
  for op in ops {
    if op.at > pos {
      let start = usize::try_from(pos).unwrap_or(0);
      let end = usize::try_from(op.at).unwrap_or(start).min(base.len());
      out.extend_from_slice(base.get(start..end).unwrap_or_default());
      pos = op.at;
    }
    match op.kind {
      OpKind::Delete => pos = pos.saturating_add(op.len),
      OpKind::Truncate => pos = base_len,
      OpKind::Overwrite => {
        push_slice(&mut out, post_state, op.src, op.len);
        pos = pos.saturating_add(op.len);
      }
      OpKind::Insert | OpKind::Extend => push_slice(&mut out, post_state, op.src, op.len),
      _ => {}
    }
  }
  if pos < base_len {
    out.extend_from_slice(
      base
        .get(usize::try_from(pos).unwrap_or(usize::MAX)..)
        .unwrap_or_default(),
    );
  }
  out
}

/// [`apply`], for the history's oracle (`crate::history`'s tests).
#[cfg(test)]
pub(crate) fn apply_for_tests(base: &[u8], ops: &[Op], post_state: &[u8]) -> Vec<u8> {
  apply(base, ops, post_state)
}

/// Appends `len` bytes at `src` of `from` to `out`, clamped to `from`'s bounds.
fn push_slice(out: &mut Vec<u8>, from: &[u8], src: u64, len: u64) {
  out.extend_from_slice(span_bytes(from, src, len));
}

/// The `len` bytes at `src` of `from`, clamped to `from`'s bounds (an op's declared bytes in a
/// post-state). Used by the per-range verdict's byte check and by [`apply`]'s splice.
fn span_bytes(from: &[u8], src: u64, len: u64) -> &[u8] {
  let start = usize::try_from(src).unwrap_or(usize::MAX).min(from.len());
  let end = start
    .saturating_add(usize::try_from(len).unwrap_or(0))
    .min(from.len());
  from.get(start..end).unwrap_or_default()
}

/// The changes an increment declares, resolved from the ops document into per-dimension groups by
/// path (xattrs by path and name). The deriver has already composed each path's operations.
#[derive(Default)]
struct Resolved {
  content: BTreeMap<String, Vec<Op>>,
  creates: BTreeSet<String>,
  specials: BTreeMap<String, SpecialNode>,
  invalid: BTreeSet<String>,
  unlinks: BTreeSet<String>,
  mkdirs: BTreeSet<String>,
  rmdirs: BTreeSet<String>,
  modes: BTreeMap<String, u32>,
  symlinks: BTreeMap<String, String>,
  hardlinks: BTreeMap<String, String>,
  renames: BTreeMap<String, String>,
  xattr_sets: Vec<(String, String, Vec<u8>)>,
  xattr_removes: Vec<(String, String)>,
  /// Each copy's destination, with its source and the source's version (A-586).
  copies: BTreeMap<String, (String, u64)>,
  /// The operations on paths in the copies' trees, judged against the copied trees (`copy`, its module docs).
  copied: Option<Box<Resolved>>,
}

impl Resolved {
  /// The paths this increment removes (unlinks, rmdirs and rename sources) — a directory removal's
  /// emptiness check consults this so a directory emptied within the same increment is removable.
  fn cleared(&self) -> BTreeSet<String> {
    let mut cleared = self.unlinks.clone();
    cleared.extend(self.rmdirs.iter().cloned());
    cleared.extend(self.renames.values().cloned());
    cleared
  }

  /// Whether this increment itself brings `path` into being — a file it creates or renames into, or a
  /// directory it makes. The metadata dimensions (mode, xattr) consult this so a file created and
  /// chmod'd (or xattr'd) in one increment is not a delete/modify conflict against the not-yet-
  /// committed path: the deriver composed them into one increment, so they belong together.
  fn establishes(&self, path: &str) -> bool {
    self.creates.contains(path)
      || self.mkdirs.contains(path)
      || self.renames.contains_key(path)
      || self.specials.contains_key(path)
  }

  /// Notes a copy of `src` at `version` to `dst` (A-586); a second copy onto one destination in one increment is no copy
  /// anyone can mean, so the destination is invalid.
  fn declare_copy(&mut self, dst: String, src: String, version: u64) {
    if self.copies.insert(dst.clone(), (src, version)).is_some() {
      self.invalid.insert(dst);
    }
  }
}

/// One accepted per-path effect to apply at commit.
enum Effect {
  /// Set an IPC inode without acquiring a live endpoint.
  Special(String, SpecialNode),
  /// Remove an IPC inode.
  RemoveSpecial(String),
  /// Set the file to these bytes, recording these ops (head coordinates) as its content delta; the last field says how a
  /// replay arrives at the same bytes (`crate::chain_entry`).
  SetContent(String, Vec<u8>, Vec<Op>, ContentSource),
  /// Remove the file.
  RemoveContent(String),
  /// Create a directory.
  MakeDir(String),
  /// Remove a directory.
  RemoveDir(String),
  /// Set a path's mode.
  Mode(String, u32),
  /// Create or retarget a symlink.
  Symlink(String, String),
  /// Remove a symlink (its path is unlinked).
  RemoveSymlink(String),
  /// Create or retarget a hard link (a namespace edge to the target file).
  Hardlink(String, String),
  /// Remove a hard link's name (the shared file's fate is the volume's concern at apply time).
  RemoveHardlink(String),
  /// Set an xattr value.
  SetXattr(String, String, Vec<u8>),
  /// Remove an xattr.
  RemoveXattr(String, String),
  /// Copy a tree by reference (A-586): its destination, its source and the source's version.
  CopyTree(String, String, u64),
}

/// How a [`Effect::SetContent`]'s bytes come from what a chain entry records (`crate::chain_entry`).
#[derive(Clone, Debug)]
enum ContentSource {
  /// The effect's mapped ops, spliced into the file, from the increment's post-state.
  Mapped,
  /// These declared ops over nothing, from the increment's post-state.
  Created(Vec<Op>),
  /// The bytes another file holds at this version, with these ops, in those bytes' coordinates, spliced into them from
  /// the increment's post-state (A-547: a rename's destination the work edited after the rename; none for a rename
  /// alone).
  Copied(String, Vec<Op>),
}

impl Effect {
  /// What a chain entry records of this effect: no file's bytes, only how to arrive at them from the increment.
  fn applied(&self) -> crate::chain_entry::Applied {
    use crate::chain_entry::Applied;
    match self {
      Effect::Special(path, node) => Applied::Special(path.clone(), *node),
      Effect::RemoveSpecial(path) => Applied::RemoveSpecial(path.clone()),
      Effect::SetContent(path, _, ops, ContentSource::Mapped) => {
        Applied::Spliced(path.clone(), ops.clone())
      }
      Effect::SetContent(path, _, _, ContentSource::Created(declared)) => {
        Applied::Created(path.clone(), declared.clone())
      }
      Effect::SetContent(path, _, _, ContentSource::Copied(from, ops)) => Applied::Copied {
        dst: path.clone(),
        from: from.clone(),
        ops: ops.clone(),
      },
      Effect::RemoveContent(path) => Applied::RemoveContent(path.clone()),
      Effect::MakeDir(path) => Applied::MakeDir(path.clone()),
      Effect::RemoveDir(path) => Applied::RemoveDir(path.clone()),
      Effect::Mode(path, mode) => Applied::Mode(path.clone(), *mode),
      Effect::Symlink(path, target) => Applied::Symlink(path.clone(), target.clone()),
      Effect::RemoveSymlink(path) => Applied::RemoveSymlink(path.clone()),
      Effect::Hardlink(path, target) => Applied::Hardlink(path.clone(), target.clone()),
      Effect::RemoveHardlink(path) => Applied::RemoveHardlink(path.clone()),
      Effect::SetXattr(path, name, value) => {
        Applied::SetXattr(path.clone(), name.clone(), value.clone())
      }
      Effect::RemoveXattr(path, name) => Applied::RemoveXattr(path.clone(), name.clone()),
      Effect::CopyTree(dst, from, version) => Applied::CopyTree {
        dst: dst.clone(),
        from: from.clone(),
        version: *version,
      },
    }
  }

  /// The one key of the green's state this effect sets or removes (A-202).
  /// The path the effect changes: the key of the history it records an entry in (an xattr's, the path it is on).
  fn path(&self) -> &str {
    match self {
      Effect::Special(path, _)
      | Effect::RemoveSpecial(path)
      | Effect::SetContent(path, ..)
      | Effect::RemoveContent(path)
      | Effect::MakeDir(path)
      | Effect::RemoveDir(path)
      | Effect::Mode(path, _)
      | Effect::Symlink(path, _)
      | Effect::RemoveSymlink(path)
      | Effect::Hardlink(path, _)
      | Effect::RemoveHardlink(path)
      | Effect::SetXattr(path, ..)
      | Effect::RemoveXattr(path, _)
      | Effect::CopyTree(path, ..) => path,
    }
  }

  fn identity_key(&self) -> IdentityKey {
    match self {
      Effect::Special(path, _) | Effect::RemoveSpecial(path) => IdentityKey::Special(path.clone()),
      Effect::SetContent(path, ..) | Effect::RemoveContent(path) => IdentityKey::File(path.clone()),
      Effect::MakeDir(path) | Effect::RemoveDir(path) => IdentityKey::Dir(path.clone()),
      Effect::Mode(path, _) => IdentityKey::Mode(path.clone()),
      Effect::Symlink(path, _) | Effect::RemoveSymlink(path) => IdentityKey::Symlink(path.clone()),
      Effect::Hardlink(path, _) | Effect::RemoveHardlink(path) => {
        IdentityKey::Hardlink(path.clone())
      }
      Effect::SetXattr(path, name, _) | Effect::RemoveXattr(path, name) => {
        IdentityKey::Xattr(path.clone(), name.clone())
      }
      Effect::CopyTree(dst, ..) => IdentityKey::Copy(dst.clone()),
    }
  }
}

impl Green {
  /// An empty green volume at version 0.
  pub fn new() -> Green {
    Green::default()
  }

  /// A green whose version 0 is `origin` — the state captured from a complete immutable base
  /// (§4.16 "the origin version from a snapshot"; the A-9 requirement that a chain starts "from
  /// scratch or a complete immutable base"). Every dimension's current values are the origin's, with
  /// no history: by the module's one history rule each has held its value since version 0 (A-534), so
  /// a work based on version 0 composes against the origin, an untouched path takes the fast path
  /// (never changed, so last changed at 0 ≤ base 0), and `base_at(0)` reconstructs it. The head stays
  /// 0: the origin is not a delta. Deterministic: the same origin seeds the same state, which is what
  /// lets a holder recompute version 0 from the placed origin bytes.
  /// Takes the origin by value and moves its entries in, so seeding costs no copy of the origin (it cloned the whole
  /// origin to canonicalize it before 2026-11-07); [`OriginLoad`] is the same seeding in shares.
  pub fn with_origin(origin: crate::origin::Origin) -> Green {
    let mut load = OriginLoad::new(origin);
    loop {
      if let Some(green) = load.step(usize::MAX) {
        return green;
      }
    }
  }

  /// The green's checkpoint at version `at` (§4.16; [`crate::checkpoint`]): every key whose value in effect at `at` was
  /// set after the origin, with that version and value, and every increment accepted at or before `at`. `None` when
  /// `at` is past the head or below the folded floor (its values are no longer reconstructible). With the origin it
  /// restores ([`Green::restore`]) a green that answers as this one does for every version from `at` on.
  pub fn checkpoint(&self, at: u64) -> Option<crate::checkpoint::Checkpoint> {
    if at > self.head() || at < self.folded_below || at < self.deltas_from || self.copied_by(at) {
      return None;
    }
    let mut checkpoint = crate::checkpoint::Checkpoint {
      version: at,
      accepted: self
        .accepted
        .iter()
        .filter(|(_, version)| **version <= at)
        .map(|(id, version)| (*id, *version))
        .collect(),
      signatures: self
        .signatures
        .range(..=at)
        .map(|(version, signature)| (*version, *signature))
        .collect(),
      files: self.changed_content_at(at),
      dirs: self
        .dir_history
        .iter()
        .filter_map(|(path, history)| {
          let (version, present) = history.iter().rev().find(|(v, _)| *v <= at)?;
          (*version > 0).then(|| crate::checkpoint::Changed {
            key: path.clone(),
            at: *version,
            value: present.then_some(()),
          })
        })
        .collect(),
      modes: changed_at(&self.mode_history, at),
      symlinks: changed_at(&self.symlink_history, at),
      hardlinks: changed_at(&self.hardlink_history, at),
      specials: changed_at(&self.special_history, at),
      xattrs: changed_at(&self.xattr_history, at),
      head_check: None,
    };
    checkpoint.canonicalize();
    Some(checkpoint)
  }

  /// Whether a copy was made at or before `at` (A-586): a checkpoint there would have to list its tree key by key and
  /// carry the identity's sum (a copy's entry stands for its tree's, which a restore from the origin and the keys cannot
  /// recompute), owed; until then such a green keeps its chain whole (`None` from [`Green::checkpoint`], which the service
  /// counts and answers by keeping every version).
  fn copied_by(&self, at: u64) -> bool {
    self
      .copies
      .values()
      .flatten()
      .any(|record| record.version <= at)
  }

  /// A green restored from its `origin` and a `checkpoint` of it (§4.16, §4.8 recovery): the origin's state with every
  /// key the checkpoint names set to its value there (its history one entry, its last change the checkpoint's), the
  /// accepted identities restored, and the head at the checkpoint's version with no delta before it, so every later
  /// increment replays onto it as onto the green the checkpoint was taken from, and a base below it is refused
  /// ([`Outcome::BaseUnavailable`]). Deterministic: the same origin and checkpoint restore the same green.
  pub fn restore(
    origin: crate::origin::Origin,
    checkpoint: &crate::checkpoint::Checkpoint,
  ) -> Green {
    Green::with_origin(origin).with_checkpoint(checkpoint)
  }

  /// This green, seeded from its origin and at version 0 with no increment yet, with `checkpoint` applied: what
  /// [`Green::restore`] does after its seeding, so a recovery that seeds the origin a share at a time ([`OriginLoad`])
  /// restores the same green as the one call does.
  pub fn with_checkpoint(mut self, checkpoint: &crate::checkpoint::Checkpoint) -> Green {
    let green = &mut self;
    for changed in &checkpoint.files {
      green.restore_file(changed);
    }
    for changed in &checkpoint.dirs {
      green.restore_dir(changed);
    }
    for changed in &checkpoint.modes {
      green.restore_mode(changed);
    }
    for changed in &checkpoint.symlinks {
      green.restore_link(IDENTITY_TAG_SYMLINK, changed);
    }
    for changed in &checkpoint.hardlinks {
      green.restore_link(IDENTITY_TAG_HARDLINK, changed);
    }
    for changed in &checkpoint.specials {
      green.restore_special(changed);
    }
    for changed in &checkpoint.xattrs {
      green.restore_xattr(changed);
    }
    green.accepted = checkpoint.accepted.iter().copied().collect();
    green.signatures = checkpoint.signatures.iter().copied().collect();
    green.deltas_from = checkpoint.version;
    green.folded_below = checkpoint.version;
    green.identity_version = checkpoint.version;
    green.special_history_bytes = green.special_history_recounted();
    green.index_restored_changes();
    self
  }

  fn restore_file(&mut self, changed: &crate::checkpoint::Changed<String, Vec<u8>>) {
    if let Some(old) = self.content.remove(&changed.key) {
      self.identity.remove(&file_entry(&changed.key, &old));
    }
    if let Some(bytes) = &changed.value {
      self.identity.add(&file_entry(&changed.key, bytes));
      self.content.insert(&changed.key, bytes.clone());
    }
    self.last_changed.insert(changed.key.clone(), changed.at);
    self.clear_content_history(&changed.key);
    let value = changed
      .value
      .as_ref()
      .map(|bytes| crate::history::Value::whole(&mut self.chunks, bytes));
    self.push_content(changed.key.clone(), changed.at, value);
  }

  fn restore_dir(&mut self, changed: &crate::checkpoint::Changed<String, ()>) {
    if self.dirs.remove(&changed.key).is_some() {
      self.identity.remove(&dir_entry(&changed.key));
    }
    if changed.value.is_some() {
      self.identity.add(&dir_entry(&changed.key));
      self.dirs.insert(&changed.key, ());
    }
    self.dir_history.insert(
      changed.key.clone(),
      vec![(changed.at, changed.value.is_some())],
    );
  }

  fn restore_mode(&mut self, changed: &crate::checkpoint::Changed<String, u32>) {
    if let Some(old) = self.modes.remove(&changed.key) {
      self.identity.remove(&mode_entry(&changed.key, old));
    }
    if let Some(mode) = changed.value {
      self.identity.add(&mode_entry(&changed.key, mode));
      self.modes.insert(&changed.key, mode);
    }
    self.mode_changed.insert(changed.key.clone(), changed.at);
    self
      .mode_history
      .insert(changed.key.clone(), vec![(changed.at, changed.value)]);
  }

  fn restore_link(&mut self, tag: &[u8], changed: &crate::checkpoint::Changed<String, String>) {
    let (map, changed_map, history) = if tag == IDENTITY_TAG_SYMLINK {
      (
        &mut self.symlinks,
        &mut self.symlink_changed,
        &mut self.symlink_history,
      )
    } else {
      (
        &mut self.hardlinks,
        &mut self.hardlink_changed,
        &mut self.hardlink_history,
      )
    };
    let old = map.remove(&changed.key);
    if let Some(old) = &old {
      self.identity.remove(&link_entry(tag, &changed.key, old));
    }
    if let Some(target) = &changed.value {
      self.identity.add(&link_entry(tag, &changed.key, target));
      map.insert(&changed.key, target.clone());
    }
    if tag != IDENTITY_TAG_SYMLINK {
      relink(
        &mut self.links_to,
        &changed.key,
        old.as_deref(),
        changed.value.as_deref(),
      );
    }
    changed_map.insert(changed.key.clone(), changed.at);
    history.insert(
      changed.key.clone(),
      vec![(changed.at, changed.value.clone())],
    );
  }

  fn restore_special(&mut self, changed: &crate::checkpoint::Changed<String, SpecialNode>) {
    if let Some(old) = self.specials.remove(&changed.key) {
      self.identity.remove(&special_entry(&changed.key, &old));
    }
    if let Some(node) = changed.value {
      self.identity.add(&special_entry(&changed.key, &node));
      self.specials.insert(&changed.key, node);
    }
    self.special_changed.insert(changed.key.clone(), changed.at);
    self
      .special_history
      .insert(changed.key.clone(), vec![(changed.at, changed.value)]);
  }

  fn restore_xattr(&mut self, changed: &crate::checkpoint::Changed<(String, String), Vec<u8>>) {
    let (path, name) = &changed.key;
    if let Some(old) = self.xattrs.remove(path, name) {
      self.identity.remove(&xattr_entry(path, name, &old));
    }
    if let Some(value) = &changed.value {
      self.identity.add(&xattr_entry(path, name, value));
      self.xattrs.insert(path, name, value.clone());
    }
    self.xattr_changed.insert(changed.key.clone(), changed.at);
    self.xattr_history.insert(
      changed.key.clone(),
      vec![(changed.at, changed.value.clone())],
    );
  }

  /// The head version: the checkpoint's version, if restored from one, plus the deltas committed since.
  pub fn head(&self) -> u64 {
    self.deltas_from.saturating_add(self.deltas.len() as u64)
  }

  /// The oldest base the green still judges and maps: the version it was restored at from a checkpoint, else 0.
  pub fn mappable_from(&self) -> u64 {
    self.deltas_from
  }

  /// The identity of the green's state at the head (§4.16 "the manifest identity": what a holder
  /// "recomputes … before serving the version" and "compares … with the record at every version",
  /// D-27 "mismatch fatal-and-loud"): a hash of a canonical encoding of every entry of every
  /// dimension — every field length-prefixed and tagged by kind — so two engines that
  /// applied the same increments to the same origin hash to the same identity, and a single
  /// differing byte anywhere (a file's content, a mode, a link target, an attribute) changes it.
  /// A function of the state alone; it reads no clock. Since A-202 it is the set hash of every
  /// entry (`crate::identity`), brought up to date here by the keys changed since it was last read,
  /// so a read costs what changed since the last one, not what the green holds, and a commit pays
  /// nothing for it; it equals [`Green::head_identity_from_scratch`] in every state (it hashed every
  /// byte the green holds on each call before). Mutable only to keep that running sum.
  pub fn head_identity(&mut self) -> [u8; 32] {
    self.fold_identity();
    self.identity.identity()
  }

  /// Brings the set hash from `identity_version` to the head: for each key a commit changed since, its entry at
  /// `identity_version` (read from the histories, which hold every version at or above the fold floor, kept at or
  /// below `identity_version` by [`Green::fold_history_before`]) is subtracted and its current entry added. O(the keys
  /// changed since the identity was last read), each once however often it changed. A copy made since adds its own
  /// entry, which stands for every entry it brought, and a key it brought that changed since also gives back the entry
  /// the copy gave it (A-586, [`Green::identity_moves`]).
  fn fold_identity(&mut self) {
    let since = self.identity_version;
    for key in std::mem::take(&mut self.identity_dirty) {
      let (out, into) = self.identity_moves(&key, since);
      for entry in &out {
        self.identity.remove(entry);
      }
      for entry in &into {
        self.identity.add(entry);
      }
    }
    self.identity_version = self.head();
  }

  /// What `key` moves the set hash by from `since` to the head (A-202, A-586): the entries out (its own at `since`, and
  /// the one each copy made since gave it, which the copy's entry stands for) and in (its own at the head; for a copy's
  /// destination, the entries of the copies made there since).
  fn identity_moves(
    &self,
    key: &IdentityKey,
    since: u64,
  ) -> (Vec<crate::identity::Entry>, Vec<crate::identity::Entry>) {
    if let IdentityKey::Copy(dst) = key {
      return (Vec::new(), self.copy_entries_after(dst, since));
    }
    let (old, new) = self.entries_of(key, since);
    let mut out: Vec<crate::identity::Entry> = old.into_iter().collect();
    out.extend(self.copied_entries(key, since));
    (out, new.into_iter().collect())
  }

  /// `key`'s identity entry at version `since` and at the head (`None` where it was absent).
  fn entries_of(
    &self,
    key: &IdentityKey,
    since: u64,
  ) -> (
    Option<crate::identity::Entry>,
    Option<crate::identity::Entry>,
  ) {
    match key {
      IdentityKey::File(path) => (
        self
          .content_ref_at(path, since)
          .map(|bytes| file_entry(path, &bytes)),
        self.content.get(path).map(|bytes| file_entry(path, bytes)),
      ),
      IdentityKey::Special(path) => (
        self
          .special_at(path, since)
          .map(|node| special_entry(path, &node)),
        self
          .specials
          .get(path)
          .map(|node| special_entry(path, node)),
      ),
      IdentityKey::Dir(path) => (
        self.dir_at(path, since).then(|| dir_entry(path)),
        self.dirs.contains_key(path).then(|| dir_entry(path)),
      ),
      IdentityKey::Mode(path) => (
        self.mode_at(path, since).map(|mode| mode_entry(path, mode)),
        self.modes.get(path).map(|mode| mode_entry(path, *mode)),
      ),
      IdentityKey::Symlink(path) => (
        self
          .symlink_at(path, since)
          .map(|target| link_entry(IDENTITY_TAG_SYMLINK, path, &target)),
        self
          .symlinks
          .get(path)
          .map(|target| link_entry(IDENTITY_TAG_SYMLINK, path, target)),
      ),
      IdentityKey::Hardlink(path) => (
        self
          .hardlink_at(path, since)
          .map(|target| link_entry(IDENTITY_TAG_HARDLINK, path, &target)),
        self
          .hardlinks
          .get(path)
          .map(|target| link_entry(IDENTITY_TAG_HARDLINK, path, target)),
      ),
      IdentityKey::Xattr(path, name) => (
        self
          .xattr_at(path, name, since)
          .map(|value| xattr_entry(path, name, &value)),
        self
          .xattrs
          .get(path, name)
          .map(|value| xattr_entry(path, name, value)),
      ),
      // A copy's entries are no key's: `Green::identity_moves` takes them from its records.
      IdentityKey::Copy(_) => (None, None),
    }
  }

  /// A key's value at `version` by the module's one history rule and its copies (A-534, A-586): while it holds what the
  /// latest copy above `path` gave it, its current value from that copy's version on and its own history before (absent
  /// where it has none); otherwise its history when it has one, else its current value.
  fn read_at<T: Clone>(
    &self,
    path: &str,
    history: Option<&Vec<(u64, Option<T>)>>,
    current: Option<&T>,
    version: u64,
  ) -> Option<T> {
    match self.covered_since(path, last_version(history)) {
      Some(since) if version >= since => current.cloned(),
      Some(_) => history.and_then(|history| history_at(history, version)),
      None => value_at(history, current, version),
    }
  }

  /// A path's IPC node at `version` ([`Green::read_at`]).
  fn special_at(&self, path: &str, version: u64) -> Option<SpecialNode> {
    self.read_at(
      path,
      self.special_history.get(path),
      self.specials.get(path),
      version,
    )
  }

  /// Whether a directory stood at `path` at `version`, by the module's one history rule and its copies (A-534, A-586).
  fn dir_at(&self, path: &str, version: u64) -> bool {
    let history = self.dir_history.get(path);
    match self.covered_since(path, last_version(history)) {
      Some(since) if version >= since => self.dirs.contains_key(path),
      Some(_) => history.is_some_and(|history| present_at(history, version)),
      None => match history {
        Some(history) => present_at(history, version),
        None => self.dirs.contains_key(path),
      },
    }
  }

  /// A path's mode at `version` ([`Green::read_at`]).
  fn mode_at(&self, path: &str, version: u64) -> Option<u32> {
    self.read_at(
      path,
      self.mode_history.get(path),
      self.modes.get(path),
      version,
    )
  }

  /// A symbolic link's target at `version` ([`Green::read_at`]).
  fn symlink_at(&self, path: &str, version: u64) -> Option<String> {
    self.read_at(
      path,
      self.symlink_history.get(path),
      self.symlinks.get(path),
      version,
    )
  }

  /// A hard link's target at `version` ([`Green::read_at`]).
  fn hardlink_at(&self, path: &str, version: u64) -> Option<String> {
    self.read_at(
      path,
      self.hardlink_history.get(path),
      self.hardlinks.get(path),
      version,
    )
  }

  /// An attribute's value at `version` ([`Green::read_at`], by the path it is on).
  fn xattr_at(&self, path: &str, name: &str, version: u64) -> Option<Vec<u8>> {
    self.read_at(
      path,
      self.xattr_history.get(&(path.to_owned(), name.to_owned())),
      self.xattrs.get(path, name),
      version,
    )
  }

  /// Whether `path` is or ever was an IPC node: a special now, or one whose node has a history (A-534: by the one
  /// history rule a special unchanged since version 0 has none, so the history alone no longer says it).
  fn was_special(&self, path: &str) -> bool {
    self.specials.contains_key(path) || self.special_history.contains_key(path)
  }

  /// [`Green::head_identity`] recounted from scratch over every current entry: the oracle the running set hash is
  /// held equal to (`crates/merge/tests/identity.rs`), O(everything the green holds).
  pub fn head_identity_from_scratch(&self) -> [u8; 32] {
    let mut set = crate::identity::SetHash::default();
    for (path, node) in self.specials.iter() {
      set.add(&special_entry(&path, node));
    }
    for (path, bytes) in self.content.iter() {
      set.add(&file_entry(&path, bytes));
    }
    for path in self.dirs.keys() {
      set.add(&dir_entry(&path));
    }
    for (path, mode) in self.modes.iter() {
      set.add(&mode_entry(&path, *mode));
    }
    for (path, target) in self.symlinks.iter() {
      set.add(&link_entry(IDENTITY_TAG_SYMLINK, &path, target));
    }
    for (path, target) in self.hardlinks.iter() {
      set.add(&link_entry(IDENTITY_TAG_HARDLINK, &path, target));
    }
    for ((path, name), value) in self.xattrs.iter() {
      set.add(&xattr_entry(&path, &name, value));
    }
    // Each copy's entry in, and the entries of the tree it copied out: the running sum adds the one and never the
    // others (A-586). The recount reads each source at its version, so it holds while the histories reach that far.
    for (dst, records) in &self.copies {
      for record in records {
        set.add(&copy_entry(dst, &record.source, record.source_version));
        for entry in self.copied_tree_entries(dst, record) {
          set.remove(&entry);
        }
      }
    }
    set.identity()
  }

  /// The paths any dimension of which changed at a version in `(from, to]` (§4.16 "`advance` …
  /// invalidates exactly the paths the manifest diff between the two versions names"): what an
  /// attachment moving from `from` to `to` must invalidate, read from the change index in the span's own entries, so
  /// it costs the changes in the span and never the green's tree (A-264). Every version a path changed at is an entry
  /// at or above the fold floor, so a path changed inside the span and again after it is named; below the floor each
  /// path keeps its last change before the floor, as the folded histories do. Sorted, each path once.
  pub fn changed_between(&self, from: u64, to: u64) -> Vec<String> {
    if from >= to {
      return Vec::new();
    }
    let changed: BTreeSet<&str> = self
      .changes
      .range(from.saturating_add(1)..=to)
      .flat_map(|(_, entries)| entries.iter().map(|entry| entry.path.as_str()))
      .collect();
    changed.into_iter().map(str::to_owned).collect()
  }

  /// The model of [`Green::changed_between`], for tests only (the `test-model` feature): the paths any dimension of
  /// which changed at a version in `(from, to]`, read off the per-dimension histories, a walk of every path. Product
  /// code reads the change index; this second computation stays out of it, as the oracle the index is checked against.
  #[cfg(feature = "test-model")]
  pub fn changed_between_by_histories(&self, from: u64, to: u64) -> Vec<String> {
    let mut changed: BTreeSet<String> = BTreeSet::new();
    changed.extend(paths_changed_in(&self.content_history, from, to));
    changed.extend(paths_changed_in(&self.special_history, from, to));
    changed.extend(paths_changed_in(&self.dir_history, from, to));
    changed.extend(paths_changed_in(&self.mode_history, from, to));
    changed.extend(paths_changed_in(&self.symlink_history, from, to));
    changed.extend(paths_changed_in(&self.hardlink_history, from, to));
    changed.extend(
      self
        .xattr_history
        .iter()
        .filter(|(_, history)| history_touches(history, from, to))
        .map(|((path, _), _)| path.clone()),
    );
    // A copy names its destination, which stands for its tree (A-586).
    changed.extend(
      self
        .copies
        .iter()
        .filter(|(_, records)| {
          records
            .iter()
            .any(|record| record.version > from && record.version <= to)
        })
        .map(|(dst, _)| dst.clone()),
    );
    // Every link at either end: by the one history rule a link unchanged since version 0 has no history (A-534).
    let aliases: Vec<String> =
      union_keys(self.hardlinks.keys(), self.hardlink_history.keys().cloned()).collect();
    for alias in aliases {
      let targets = [self.hardlink_at(&alias, from), self.hardlink_at(&alias, to)];
      if targets
        .iter()
        .flatten()
        .any(|target| self.was_special(target) && changed.contains(target))
      {
        changed.insert(alias);
      }
    }
    changed.into_iter().collect()
  }

  /// The green's current state as a deriver [`Base`](crate::increment::Base) — every file with its
  /// content length, and the directories, modes, symlinks, hard links and xattrs — so an increment
  /// based on the head can be composed against what the green actually holds. (An *older* version's
  /// base, for a work that lagged behind an intervening submit, is reconstructed by [`Green::base_at`], every
  /// dimension from its own history.)
  pub fn current_base(&self) -> crate::increment::Base {
    crate::increment::Base {
      specials: self
        .specials
        .iter()
        .map(|(path, node)| (path, *node))
        .collect(),
      files: self
        .content
        .iter()
        .map(|(path, bytes)| (path, bytes.len() as u64))
        .collect(),
      dirs: self.dirs.keys().collect(),
      modes: self
        .modes
        .iter()
        .map(|(path, mode)| (path, *mode))
        .collect(),
      symlinks: self
        .symlinks
        .iter()
        .map(|(path, target)| (path, target.clone()))
        .collect(),
      xattrs: self
        .xattrs
        .iter()
        .map(|((path, name), value)| (path, name, value.clone()))
        .collect(),
      hardlinks: self
        .hardlinks
        .iter()
        .map(|(path, target)| (path, target.clone()))
        .collect(),
    }
  }

  /// A FIFO/socket inode's captured metadata, or `None` for any other kind. A hard-link
  /// alias refers to its primary inode; it does not hold an independent metadata copy.
  pub fn special(&self, path: &str) -> Option<SpecialNode> {
    self
      .specials
      .get(path)
      .or_else(|| {
        self
          .hardlinks
          .get(path)
          .and_then(|target| self.specials.get(target))
      })
      .copied()
  }

  /// A file's current bytes, or `None` when it is absent.
  pub fn content(&self, path: &str) -> Option<&[u8]> {
    self.content.get(path)
  }

  /// Every file the green currently holds, with its bytes — so a new work volume can be seeded with
  /// the base content it inherits and edits splice against it. Each path is assembled by the walk (a persistent map's
  /// keys are its edges, [`crate::dims`]), so it is owned.
  pub fn files(&self) -> impl Iterator<Item = (String, &[u8])> {
    self.content.iter()
  }

  /// Whether a directory exists at the path.
  pub fn is_dir(&self, path: &str) -> bool {
    self.dirs.contains_key(path)
  }

  /// IPC aliases share the primary inode's metadata; no independent value is stored at an alias.
  fn metadata_path<'a>(&'a self, path: &'a str) -> &'a str {
    self
      .hardlinks
      .get(path)
      .filter(|target| self.specials.contains_key(*target))
      .map_or(path, String::as_str)
  }

  /// A path's current mode, or `None` when none has been set.
  pub fn mode(&self, path: &str) -> Option<u32> {
    self.modes.get(self.metadata_path(path)).copied()
  }

  /// A symbolic link's target at the path, or `None` when it is not a symlink.
  pub fn symlink(&self, path: &str) -> Option<&str> {
    self.symlinks.get(path).map(String::as_str)
  }

  /// A hard link's target file at the path, or `None` when it is not a hard link.
  pub fn hardlink(&self, path: &str) -> Option<&str> {
    self.hardlinks.get(path).map(String::as_str)
  }

  /// An xattr value at `(path, name)`, or `None` when unset.
  pub fn xattr(&self, path: &str, name: &str) -> Option<&[u8]> {
    self
      .xattrs
      .get(self.metadata_path(path), name)
      .map(Vec::as_slice)
  }

  /// The paths any dimension of which changed strictly after `version`, sorted, each once (§4.16 `changed_since`): the
  /// same paths as [`Green::changed_between`]`(version, head)`, read from the change index in the changes since
  /// `version`.
  pub fn changed_since(&self, version: u64) -> Vec<String> {
    let mut changed: Vec<String> = self
      .changes_after(version, None)
      .filter(|change| change.last)
      .map(|change| change.path.to_owned())
      .collect();
    changed.sort();
    changed
  }

  /// The change index from just past `version`, or from `cursor` (a version and how many of its paths a walk has
  /// passed), in version order and each version's paths in path order (§4.16 `changed_since`, paged; A-264). Every
  /// path changed after `version` is an entry with `last` set exactly once, at the last version it changed at: a walk
  /// answers those, and they are [`Green::changed_between`]`(version, head)`. A version's paths never change once it
  /// commits and an entry keeps its place when a fold drops others, so a cursor stays valid while the green moves on and
  /// folds; a path changed again after the walk passed it is answered again, at its new version. Reading costs the
  /// entries passed, never the green's tree.
  pub fn changes_after(
    &self,
    version: u64,
    cursor: Option<(u64, usize)>,
  ) -> impl Iterator<Item = ChangeAt<'_>> + '_ {
    let (start, skip) = match cursor {
      Some((at, passed)) if at > version => (at, passed),
      _ => (version.saturating_add(1), 0),
    };
    self.changes.range(start..).flat_map(move |(at, entries)| {
      let from = if *at == start {
        entries.partition_point(|entry| usize::try_from(entry.index).unwrap_or(usize::MAX) < skip)
      } else {
        0
      };
      entries
        .get(from..)
        .unwrap_or_default()
        .iter()
        .map(move |entry| ChangeAt {
          version: *at,
          index: usize::try_from(entry.index).unwrap_or(usize::MAX),
          path: &entry.path,
          last: entry.last,
        })
    })
  }

  /// The number of paths merged through the content fast path (the non-vacuity counter).
  pub fn fast_path_hits(&self) -> u64 {
    self.fast_path_hits
  }

  /// The deltas the verdict's position mapping has consulted ([`Green::fast_path_hits`]'s companion): one per delta
  /// that changed a path an increment touches since its base.
  pub fn deltas_visited(&self) -> u64 {
    self.deltas_visited.get()
  }

  /// The nodes walks of the green's maps visited: what a copy leaves unmoved (A-586's non-vacuity count, as a branch
  /// leaves [`Lineage::walked`]). The maps' arenas are the green's own unless a lineage lends them, which counts its own.
  pub fn walked(&self) -> u64 {
    [
      self.content.visited(),
      self.specials.visited(),
      self.dirs.visited(),
      self.modes.visited(),
      self.symlinks.visited(),
      self.hardlinks.visited(),
      self.xattrs.visited(),
      self.links_to.visited(),
    ]
    .into_iter()
    .fold(0u64, u64::saturating_add)
  }

  /// Copies that brought a tree (A-586): the count a copy's tests assert moved, so a copy that grafted nothing cannot pass
  /// as one that did.
  pub fn copies_grafted(&self) -> u64 {
    self.copies_grafted
  }

  /// Copies a map refused at commit after their verdict judged them possible (A-586): zero in every correct use.
  pub fn copies_refused(&self) -> u64 {
    self.copies_refused
  }

  /// The green's state at an older `version` as a deriver [`Base`](crate::increment::Base), for a
  /// work whose base lagged behind an intervening submit, or a listing of the green at that version. Every
  /// dimension comes from its own `(version, value)` history, the last entry at or before `version`: files from the
  /// content history (renames and deletes included), directories from their presence history, modes, symlinks, hard
  /// links, xattrs and special nodes from their value histories. (Until 2026-11-09 this said symlinks, hard links and
  /// xattrs were reconstructed empty; they have been read from their histories since those were kept.)
  pub fn base_at(&self, version: u64) -> crate::increment::Base {
    if version >= self.head() {
      return self.current_base();
    }
    // Each dimension is reconstructed from its own `(version, value)` history: the last entry at or
    // before `version` is that dimension's state then. Files come from the content history (a path
    // absent or removed by then yields no entry); directories from a presence history; modes,
    // symlinks, hard links and xattrs from a value history (a removal is a `None` entry). A key with
    // no history has held its current value since version 0, so each dimension's keys are its current
    // keys and its history's together (the one history rule, A-534).
    let files = union_keys(self.content.keys(), self.content_history.keys().cloned())
      .filter_map(|path| {
        let len = self.content_len_at(&path, version)?;
        Some((path, len as u64))
      })
      .collect();
    let dirs = union_keys(self.dirs.keys(), self.dir_history.keys().cloned())
      .filter(|path| self.dir_at(path, version))
      .collect();
    let modes = union_keys(self.modes.keys(), self.mode_history.keys().cloned())
      .filter_map(|path| self.mode_at(&path, version).map(|mode| (path, mode)))
      .collect();
    let symlinks = union_keys(self.symlinks.keys(), self.symlink_history.keys().cloned())
      .filter_map(|path| self.symlink_at(&path, version).map(|target| (path, target)))
      .collect();
    let hardlinks = union_keys(self.hardlinks.keys(), self.hardlink_history.keys().cloned())
      .filter_map(|path| {
        self
          .hardlink_at(&path, version)
          .map(|target| (path, target))
      })
      .collect();
    let xattrs = union_keys(
      self.xattrs.iter().map(|(key, _)| key),
      self.xattr_history.keys().cloned(),
    )
    .filter_map(|(path, name)| {
      self
        .xattr_at(&path, &name, version)
        .map(|value| (path, name, value))
    })
    .collect();
    crate::increment::Base {
      specials: union_keys(self.specials.keys(), self.special_history.keys().cloned())
        .filter_map(|path| self.special_at(&path, version).map(|node| (path, node)))
        .collect(),
      files,
      dirs,
      modes,
      symlinks,
      xattrs,
      hardlinks,
    }
  }

  /// The base of `version` that composing `journal` reads (§4.16 "Composition at seal"): every
  /// dimension's entry at each path the journal names
  /// ([`VolumeOp::names`](crate::increment::VolumeOp::names)), and the hard links aimed at those
  /// paths, as [`Green::base_at`] would give them. The deriver reads a base only at the paths its
  /// journal names, so [`compose_volume`](crate::increment::compose_volume) over this base equals it
  /// over the whole one (the oracle in `crates/merge/tests/base_for.rs`), and a submit's cost follows
  /// what it changed rather than what the green holds: the whole base, every path's name cloned, was
  /// built on every submit before (2026-11-04).
  pub fn base_for(
    &self,
    version: u64,
    journal: &[crate::increment::VolumeOp],
  ) -> crate::increment::Base {
    let named: BTreeSet<&str> = journal.iter().flat_map(|op| op.names()).flatten().collect();
    let mut base = crate::increment::Base::of_files(Vec::new());
    let at_head = version >= self.head();
    for path in &named {
      if at_head {
        self.head_entries(path, &mut base);
      } else {
        self.entries_at(path, version, &mut base);
      }
    }
    // A link named by the journal, or one aimed at a path it names (an unlink of a primary path is refused while
    // aliases survive). Hard links are few, and no index by target exists yet.
    let named_link = |link: &str, target: &str| named.contains(link) || named.contains(target);
    base.hardlinks = self
      .links_at(version)
      .into_iter()
      .filter(|(link, target)| named_link(link, target))
      .collect();
    base
  }

  /// Every hard link at `version` and its target: the head's, or each link's value then by the module's one history
  /// rule (A-534). Hard links are few, and no index by target exists yet.
  fn links_at(&self, version: u64) -> Vec<(String, String)> {
    if version >= self.head() {
      return self
        .hardlinks
        .iter()
        .map(|(link, target)| (link, target.clone()))
        .collect();
    }
    union_keys(self.hardlinks.keys(), self.hardlink_history.keys().cloned())
      .filter_map(|link| {
        self
          .hardlink_at(&link, version)
          .map(|target| (link, target))
      })
      .collect()
  }

  /// The entries one path contributes to a restricted base at `version` ([`Green::base_for`]): every dimension's entry
  /// at `path`, and the hard links whose link or target it is. What a work's shard fetches with a path it touches, so
  /// that [`Base::for_names`](crate::increment::Base::for_names) assembles its increment's base without the green
  /// (A-247).
  pub fn path_base(&self, version: u64, path: &str) -> crate::increment::Base {
    let mut base = crate::increment::Base::of_files(Vec::new());
    if version >= self.head() {
      self.head_entries(path, &mut base);
    } else {
      self.entries_at(path, version, &mut base);
    }
    base.hardlinks = self
      .links_at(version)
      .into_iter()
      .filter(|(link, target)| link == path || target == path)
      .collect();
    base
  }

  /// Every dimension's entry at `path` at the head but hard links, added to `base` ([`Green::base_for`]).
  fn head_entries(&self, path: &str, base: &mut crate::increment::Base) {
    if let Some(bytes) = self.content.get(path) {
      base.files.push((path.to_owned(), bytes.len() as u64));
    }
    if let Some(node) = self.specials.get(path) {
      base.specials.push((path.to_owned(), *node));
    }
    if self.dirs.contains_key(path) {
      base.dirs.push(path.to_owned());
    }
    if let Some(mode) = self.modes.get(path) {
      base.modes.push((path.to_owned(), *mode));
    }
    if let Some(target) = self.symlinks.get(path) {
      base.symlinks.push((path.to_owned(), target.clone()));
    }
    for (name, value) in self.xattrs.of(path) {
      base.xattrs.push((path.to_owned(), name, value.clone()));
    }
  }

  /// Every dimension's entry at `path` at the earlier `version` but hard links, from the histories by the module's one
  /// history rule (A-534), added to `base` ([`Green::base_for`]).
  fn entries_at(&self, path: &str, version: u64, base: &mut crate::increment::Base) {
    if let Some(len) = self.content_len_at(path, version) {
      base.files.push((path.to_owned(), len as u64));
    }
    if let Some(node) = self.special_at(path, version) {
      base.specials.push((path.to_owned(), node));
    }
    if self.dir_at(path, version) {
      base.dirs.push(path.to_owned());
    }
    if let Some(mode) = self.mode_at(path, version) {
      base.modes.push((path.to_owned(), mode));
    }
    if let Some(target) = self.symlink_at(path, version) {
      base.symlinks.push((path.to_owned(), target));
    }
    let from = (path.to_owned(), String::new());
    let recorded = self
      .xattr_history
      .range(from..)
      .take_while(|((attr_path, _), _)| attr_path == path)
      .map(|((_, name), _)| name.clone());
    let names: Vec<String> =
      union_keys(self.xattrs.of(path).map(|(name, _)| name), recorded).collect();
    for name in names {
      if let Some(value) = self.xattr_at(path, &name, version) {
        base.xattrs.push((path.to_owned(), name, value));
      }
    }
  }

  /// A file's bytes at `version`, reconstructed from its history — the value in effect at `version`, or `None` when
  /// the file was absent then. What a version-pinned attachment reads (§4.16 "Attachments and versions": a reader's
  /// view moves only by `advance`).
  pub fn content_at(&self, path: &str, version: u64) -> Option<Vec<u8>> {
    self.content_ref_at(path, version).map(Cow::into_owned)
  }

  /// Every file at `version` with its bytes in path order: the head's files, or each path's value at an earlier
  /// version. What a work based at `version` lists (its untouched files are the green's there). Borrowed where a
  /// file is unchanged since `version`, copied out of the history where it changed after ([`Green::content_ref_at`]).
  /// The paths are owned, as [`Green::files`]'s are.
  pub fn files_at(&self, version: u64) -> Vec<(String, Cow<'_, [u8]>)> {
    if version >= self.head() {
      return self
        .files()
        .map(|(path, bytes)| (path, Cow::Borrowed(bytes)))
        .collect();
    }
    union_keys(self.content.keys(), self.content_history.keys().cloned())
      .filter_map(|path| {
        let bytes = self.content_ref_at(&path, version)?;
        Some((path, bytes))
      })
      .collect()
  }

  /// A file's bytes at `version` ([`Green::content_at`] without the copy where it can): borrowed from the head when
  /// the file is unchanged since `version` (its value then is the current one), copied out of the history's chunks
  /// when it changed after (W2: the history holds what each version changed, not a whole copy of the file to lend).
  /// A file with no history has held its current bytes since version 0 (the one history rule, A-534).
  pub fn content_ref_at(&self, path: &str, version: u64) -> Option<Cow<'_, [u8]>> {
    match self.content_read(path, version) {
      ContentRead::Head => self.content.get(path).map(Cow::Borrowed),
      ContentRead::Value(value) => Some(Cow::Owned(value.materialize(&self.chunks))),
      ContentRead::Absent => None,
    }
  }

  /// Where `path`'s bytes at `version` are read from, by the module's one history rule and its copies (A-534, A-586):
  /// the head's while the file holds its current value (no history; its history's last entry; or the copy's value from
  /// the copy's version on), else the history's value in effect, else nowhere.
  fn content_read(&self, path: &str, version: u64) -> ContentRead<'_> {
    let history = self.content_history.get(path);
    let covered = self.covered_since(path, last_version(history));
    if covered.is_some_and(|since| version >= since) {
      return ContentRead::Head;
    }
    let Some(history) = history else {
      return if covered.is_some() {
        ContentRead::Absent
      } else {
        ContentRead::Head
      };
    };
    let Some(index) = history
      .iter()
      .rposition(|(recorded, _)| *recorded <= version)
    else {
      return ContentRead::Absent;
    };
    let latest = covered.is_none() && index.saturating_add(1) == history.len();
    match history.get(index) {
      Some((_, Some(_))) if latest => ContentRead::Head,
      Some((_, Some(value))) => ContentRead::Value(value),
      _ => ContentRead::Absent,
    }
  }

  /// A page of a file at `version`: its bytes in `[offset, offset + want)` (clamped to its length) and its length, or
  /// `None` when it was absent then. Sliced from the head when the file is unchanged since `version`, and copied out of
  /// only the history's pieces that hold the page when it changed after: a paged read never copies the whole file.
  pub fn content_page_at(
    &self,
    path: &str,
    version: u64,
    (offset, want): (u64, u64),
  ) -> Option<(Vec<u8>, u64)> {
    let page_of = |bytes: &[u8]| {
      let total = bytes.len();
      let start = usize::try_from(offset).unwrap_or(usize::MAX).min(total);
      let end = usize::try_from(offset.saturating_add(want))
        .unwrap_or(usize::MAX)
        .min(total);
      (
        bytes.get(start..end).unwrap_or_default().to_vec(),
        u64::try_from(total).unwrap_or(u64::MAX),
      )
    };
    let value = match self.content_read(path, version) {
      ContentRead::Head => return self.content.get(path).map(page_of),
      ContentRead::Value(value) => value,
      ContentRead::Absent => return None,
    };
    let total = value.len();
    let start = usize::try_from(offset).unwrap_or(usize::MAX).min(total);
    let end = usize::try_from(offset.saturating_add(want))
      .unwrap_or(usize::MAX)
      .min(total);
    Some((
      value.read(&self.chunks, start, end),
      u64::try_from(total).unwrap_or(u64::MAX),
    ))
  }

  /// Every file at `version` with its length, in path order: [`Green::files_at`] without the bytes.
  pub fn file_lens_at(&self, version: u64) -> Vec<(String, usize)> {
    if version >= self.head() {
      return self
        .files()
        .map(|(path, bytes)| (path, bytes.len()))
        .collect();
    }
    union_keys(self.content.keys(), self.content_history.keys().cloned())
      .filter_map(|path| {
        let len = self.content_len_at(&path, version)?;
        Some((path, len))
      })
      .collect()
  }

  /// A file's length at `version`, or `None` when it was absent then: read off the history, never copied, or the
  /// head's when the file has no history (the one history rule, A-534).
  pub fn content_len_at(&self, path: &str, version: u64) -> Option<usize> {
    match self.content_read(path, version) {
      ContentRead::Head => self.content.get(path).map(<[u8]>::len),
      ContentRead::Value(value) => Some(value.len()),
      ContentRead::Absent => None,
    }
  }

  /// Every file whose value in effect at `at` was set after the origin, with that version and its bytes: the
  /// checkpoint's files ([`Green::checkpoint`]).
  fn changed_content_at(&self, at: u64) -> Vec<crate::checkpoint::Changed<String, Vec<u8>>> {
    self
      .content_history
      .iter()
      .filter_map(|(path, entries)| {
        let (version, value) = entries.iter().rev().find(|(v, _)| *v <= at)?;
        (*version > 0).then(|| crate::checkpoint::Changed {
          key: path.clone(),
          at: *version,
          value: value.as_ref().map(|value| value.materialize(&self.chunks)),
        })
      })
      .collect()
  }

  /// Records `value` as `path`'s content at `version`, the running totals with it: the new latest value replaces the
  /// previous one among the current values, and its list joins the history's.
  fn push_content(&mut self, path: String, version: u64, value: Option<crate::history::Value>) {
    let previous = self
      .content_history
      .get(&path)
      .and_then(|history| history.last())
      .and_then(|(_, value)| value.as_ref())
      .map_or(0, current_bytes);
    self.history_current = self.history_current.saturating_sub(previous);
    if let Some(value) = &value {
      self.history_lists = self.history_lists.saturating_add(value.list_bytes());
      self.history_current = self.history_current.saturating_add(current_bytes(value));
    }
    self
      .content_history
      .entry(path)
      .or_default()
      .push((version, value));
  }

  /// Before `path`'s first recorded content change since it held bytes it did not record (a change at `version`),
  /// records them whole where they date from (`seed`): version 0 by the module's one history rule (A-534), or the
  /// version of the copy that gave them (A-586), so the change's value shares its runs as every later one does. A path
  /// whose history holds them, or with no bytes, records nothing, and neither does a change in its copy's own version.
  fn seed_content(&mut self, path: &str, version: u64) {
    let own_last = last_version(self.content_history.get(path));
    let Some(since) = self
      .seed_since(path, own_last)
      .filter(|since| *since != version)
    else {
      return;
    };
    let Green {
      content, chunks, ..
    } = self;
    let Some(bytes) = content.get(path) else {
      return;
    };
    let value = crate::history::Value::whole(chunks, bytes);
    self.push_content(path.to_owned(), since, Some(value));
  }

  /// The value `bytes` that `ops` make of `path`'s latest content (`crate::history::Value::spliced`), or `bytes`
  /// whole for a path with none.
  fn content_value(&mut self, path: &str, ops: &[Op], bytes: &[u8]) -> crate::history::Value {
    let Green {
      content_history,
      chunks,
      ..
    } = self;
    match content_history
      .get(path)
      .and_then(|history| history.last())
      .and_then(|(_, value)| value.as_ref())
    {
      Some(latest) => latest.spliced(chunks, ops, bytes),
      None => crate::history::Value::whole(chunks, bytes),
    }
  }

  /// Whether `ops`, applied to `path` as it was at `base`, make exactly the head's file: the whole-file identity check
  /// of a length-changing op that overlaps an intervening change. Compared through the content history's values
  /// ([`crate::history::Value::applied_equals`]): the base's value and the head's share every run no version between
  /// them changed, and those are equal unread, so the check reads what the ops added and what changed since the base,
  /// not the file. Until 2026-11-09 it materialized the base, applied the ops to a copy and compared the copy with the
  /// head, three passes over the file per verdict: the largest self time under the owner's `decide` (`memcmp`,
  /// `memmove`) at 3,000 clients over `kernel/`. A path absent at `base` is empty there, as before. The head's value is
  /// the history's last, which every writer of the content pushes with it (`commit`, `restore_file`). A path with no
  /// history has held the head's bytes since version 0 (the one history rule, A-534), so its value at `base` is the
  /// head's.
  fn lands_on_head(&self, path: &str, base: u64, ops_and_post: (&[Op], &[u8])) -> bool {
    let history = self.content_history.get(path);
    let Some(history) =
      history.filter(|_| self.covered_since(path, last_version(history)).is_none())
    else {
      // No history, or a copy's value (A-586), whose history's last value is no head's: the bytes at the base against
      // the head's (at the base they are the head's, unless a copy since replaced them).
      let head = self.content.get(path).unwrap_or_default();
      let at_base = self.content_ref_at(path, base).unwrap_or_default();
      let (ops, post_state) = ops_and_post;
      return apply(&at_base, ops, post_state) == head;
    };
    let empty = crate::history::Value::default();
    let at_base = history
      .iter()
      .rposition(|(recorded, _)| *recorded <= base)
      .and_then(|index| history.get(index))
      .and_then(|(_, value)| value.as_ref())
      .unwrap_or(&empty);
    let head = history
      .last()
      .and_then(|(_, value)| value.as_ref())
      .unwrap_or(&empty);
    at_base.applied_equals(&self.chunks, ops_and_post, head)
  }

  /// Drops `path`'s content history, its chunks released and the running totals with it.
  fn clear_content_history(&mut self, path: &str) {
    let Some(history) = self.content_history.remove(path) else {
      return;
    };
    if let Some((_, Some(latest))) = history.last() {
      self.history_current = self.history_current.saturating_sub(current_bytes(latest));
    }
    for (_, value) in history {
      self.release_content(value);
    }
  }

  /// Releases one content history value: its list leaves the running total and its chunks their references.
  fn release_content(&mut self, value: Option<crate::history::Value>) {
    if let Some(value) = value {
      self.history_lists = self.history_lists.saturating_sub(value.list_bytes());
      value.release(&mut self.chunks);
    }
  }

  /// The bytes the content and IPC histories retain beyond the current values — every earlier value still held for
  /// reconstruction (`content_at`, `base_at`) — read off running totals so the service charges retention without
  /// walking the histories: the content history's chunks and piece lists, less the current values they also hold,
  /// and the IPC history's earlier nodes.
  fn history_bytes(&self) -> usize {
    self
      .chunks
      .bytes()
      .saturating_add(self.history_lists)
      .saturating_sub(self.history_current)
      .saturating_add(self.special_history_bytes)
  }

  /// The ops each intervening delta in `(base, head]` applied to `path`, oldest first: read at the versions `path`'s
  /// content history names after `base`, the only deltas that hold ops for it (a delta records a path's ops exactly
  /// when the commit set its content, which pushes its history entry), so the walk costs the path's own changes since
  /// the base. Walking every delta since the base cost the versions since it, whatever they changed: at 10,000 clients
  /// over one green, whose works lag by thousands of versions, that walk on the green's owner shard was the rung's wall
  /// (2026-11-07). A base below the fold, whose history entries may be gone, walks every delta as before.
  /// Whether the file at `path` was removed after version `base` (`file_removed`): unlinked, renamed away or renamed
  /// onto after the base, whatever stands at the path now (A-612).
  fn file_removed_since(&self, path: &str, base: u64) -> bool {
    self
      .file_removed
      .get(path)
      .is_some_and(|version| *version > base)
  }

  fn intervening(&self, path: &str, base: u64) -> Vec<&[Op]> {
    if base < self.folded_below {
      return self.intervening_walk(path, base);
    }
    let Some(history) = self.content_history.get(path) else {
      return Vec::new();
    };
    let after = history.partition_point(|(version, _)| *version <= base);
    let ops: Vec<&[Op]> = history
      .get(after..)
      .unwrap_or_default()
      .iter()
      .filter_map(|(version, _)| {
        let index = version.checked_sub(self.deltas_from)?.checked_sub(1)?;
        self
          .deltas
          .get(usize::try_from(index).ok()?)?
          .get(path)
          .map(Vec::as_slice)
      })
      .collect();
    self.count_visited(history.len().saturating_sub(after));
    ops
  }

  /// [`Green::intervening`] by walking every delta in `(base, head]`: for a base below the fold.
  fn intervening_walk(&self, path: &str, base: u64) -> Vec<&[Op]> {
    // `submit` and `rebase` refuse a base below `deltas_from`, so the skip is the base's own place in the deltas.
    let skip = usize::try_from(base.saturating_sub(self.deltas_from)).unwrap_or(usize::MAX);
    self.count_visited(self.deltas.len().saturating_sub(skip));
    self
      .deltas
      .iter()
      .skip(skip)
      .filter_map(|delta| delta.get(path).map(Vec::as_slice))
      .collect()
  }

  /// Adds `deltas` to the verdict's visited count.
  fn count_visited(&self, deltas: usize) {
    let deltas = u64::try_from(deltas).unwrap_or(u64::MAX);
    self
      .deltas_visited
      .set(self.deltas_visited.get().saturating_add(deltas));
  }

  /// A whole-path namespace conflict window.
  fn window(path: &str, class: MergeConflictClass) -> ConflictWindow {
    ConflictWindow {
      path: path.to_owned(),
      range: Range::new(0, 0),
      class,
    }
  }

  /// Resolves an increment's ops document into per-dimension groups by path.
  fn resolve(inc: &Increment) -> Resolved {
    let mut r = Resolved::default();
    // Resolve a path-table reference (a 64-bit index, A-621) to its name; an index the table does not hold names nothing.
    let name = |index: u64| inc.doc.paths.path(index).unwrap_or_default().to_owned();
    for op in &inc.doc.ops {
      let path = name(op.path);
      match op.kind {
        _ if is_content(op.kind) => r.content.entry(path).or_default().push(*op),
        OpKind::Mknod => match special_payload(&inc.doc, &inc.post_state, op) {
          Ok(node) => {
            r.specials.insert(path, node);
          }
          Err(_) => {
            r.invalid.insert(path);
          }
        },
        OpKind::Create => {
          r.creates.insert(path);
        }
        OpKind::Unlink => {
          r.unlinks.insert(path);
        }
        OpKind::Mkdir => {
          r.mkdirs.insert(path);
        }
        OpKind::Rmdir => {
          r.rmdirs.insert(path);
        }
        OpKind::SetMode => {
          r.modes
            .insert(path, u32::try_from(op.len).unwrap_or(u32::MAX));
        }
        OpKind::Symlink => {
          r.symlinks.insert(path, name(op.src));
        }
        OpKind::Link => {
          r.hardlinks.insert(path, name(op.src));
        }
        OpKind::Rename => {
          r.renames.insert(path, name(op.src));
        }
        OpKind::SetXattr => {
          let value = post_slice(&inc.post_state, op.src, op.len);
          r.xattr_sets.push((path, name(op.at), value));
        }
        OpKind::RemoveXattr => r.xattr_removes.push((path, name(op.at))),
        OpKind::Copy => r.declare_copy(path, name(op.src), op.at),
        OpKind::Overwrite | OpKind::Extend | OpKind::Truncate | OpKind::Insert | OpKind::Delete => {
        }
      }
    }
    for path in r.specials.keys() {
      if r.creates.contains(path)
        || r.content.contains_key(path)
        || r.mkdirs.contains(path)
        || r.symlinks.contains_key(path)
        || r.hardlinks.contains_key(path)
        || r.renames.contains_key(path)
        || r.unlinks.contains(path)
        || r.rmdirs.contains(path)
      {
        r.invalid.insert(path.clone());
      }
    }
    r.split_copies();
    r
  }

  /// Whether any file, directory, symlink or hard link under `prefix` survives `cleared`: each map's keys under the
  /// prefix, one descent each (A-534; a scan of every key of every map before).
  fn has_live_child(&self, prefix: &str, cleared: &BTreeSet<String>) -> bool {
    let live = |path: &String| !cleared.contains(path);
    self.content.prefix(prefix).any(|(path, _)| live(&path))
      || self.dirs.prefix(prefix).any(|(path, _)| live(&path))
      || self.symlinks.prefix(prefix).any(|(path, _)| live(&path))
      || self.hardlinks.prefix(prefix).any(|(path, _)| live(&path))
      || self.specials.prefix(prefix).any(|(path, _)| live(&path))
  }

  /// Whether an intervening change occupied `path` with any non-file kind.
  fn occupied_by_nonfile(&self, path: &str) -> bool {
    self.dirs.contains_key(path)
      || self.symlinks.contains_key(path)
      || self.hardlinks.contains_key(path)
      || self.specials.contains_key(path)
  }

  /// Merges a file rename to `dst` from `from` (source content captured at merge time), collecting
  /// the destination set and the source removal. `ops` are the work's edits to `dst` after the rename, in the
  /// coordinates of the source's bytes at the work's base, adding from `post_state`: they are mapped through what changed
  /// the source since the base and spliced into the bytes it moves (A-547), so an edit after a rename is kept; one that
  /// meets an intervening change to the source is an overlap conflict, never dropped. Until A-547 the destination took
  /// the source's bytes alone, and the submit was accepted with the edit lost.
  fn merge_rename(
    &self,
    (dst, ops): (&str, &[Op]),
    (base, post_state): (u64, &[u8]),
    from: &str,
    effects: &mut Vec<Effect>,
  ) -> Result<(), ConflictWindow> {
    if from == dst {
      return Ok(());
    }
    // The source must still name something — a file, a symlink or a hard link. A source an
    // intervening change renamed away or removed is a rename/rename conflict (checked first, so a
    // vanished source is named even when the destination is also occupied).
    let source_is_file = self.content.contains_key(from);
    let source_exists =
      source_is_file || self.symlinks.contains_key(from) || self.hardlinks.contains_key(from);
    if !source_exists {
      return Err(Green::window(dst, MergeConflictClass::RenameRename));
    }
    // A source whose file was removed after the work's base is gone too, whatever stands at its path now (made again,
    // renamed onto): the rename names a file that no longer exists, never the one that took its place (A-612).
    if self.file_removed_since(from, base) {
      return Err(Green::window(dst, MergeConflictClass::RenameRename));
    }
    // The destination must be free: a non-file already there is a type change; a file the base did
    // not still hold (changed since base) is a rename/rename. An unchanged base file at the
    // destination is replaced (the design's rename-over-a-base-file case).
    if self.occupied_by_nonfile(dst) {
      return Err(Green::window(dst, MergeConflictClass::TypeChanged));
    }
    if self.changed_last(self.last_changed.get(dst), dst) > base {
      return Err(Green::window(dst, MergeConflictClass::RenameRename));
    }
    // Move whatever the source names, capturing its current value so an intervening change to the
    // source follows the move, and removing the source name.
    if let Some(source) = self.content.get(from) {
      let edits = self.edits_after_rename((dst, ops), base, from)?;
      let bytes = apply(source, &edits, post_state);
      let whole = vec![insert_whole(bytes.len() as u64)];
      effects.push(Effect::SetContent(
        dst.to_owned(),
        bytes,
        whole,
        ContentSource::Copied(from.to_owned(), edits),
      ));
      effects.push(Effect::RemoveContent(from.to_owned()));
    } else if let Some(target) = self.symlinks.get(from) {
      effects.push(Effect::Symlink(dst.to_owned(), target.clone()));
      effects.push(Effect::RemoveSymlink(from.to_owned()));
    } else if let Some(target) = self.hardlinks.get(from) {
      effects.push(Effect::Hardlink(dst.to_owned(), target.clone()));
      effects.push(Effect::RemoveHardlink(from.to_owned()));
    }
    Ok(())
  }

  /// The work's edits to a rename's destination `dst` (`ops`, in the coordinates of the source `from`'s bytes at `base`),
  /// moved to where they stand in the source's bytes now, past every change made to it since `base` (A-547); an edit that
  /// meets one of those changes is an overlap conflict on `dst`.
  fn edits_after_rename(
    &self,
    (dst, ops): (&str, &[Op]),
    base: u64,
    from: &str,
  ) -> Result<Vec<Op>, ConflictWindow> {
    // Bytes a copy brought whole after the base are not what the edits were declared against (A-586).
    if self.replaced_after(from, base) && !ops.is_empty() {
      return Err(Green::window(dst, MergeConflictClass::DeleteModify));
    }
    let unchanged = self.changed_last(self.last_changed.get(from), from) <= base;
    let intervening = if unchanged {
      Vec::new()
    } else {
      self.intervening(from, base)
    };
    let source_len = self.content.get(from).map_or(0, len_of);
    ops
      .iter()
      .map(
        |op| match crate::map::map_range(&intervening, touched_range(op)) {
          crate::map::Mapped::Shifted(shifted) => {
            let mut moved = *op;
            moved.at = shifted.start;
            Ok(at_head(moved, source_len))
          }
          crate::map::Mapped::Overlaps => Err(ConflictWindow {
            path: dst.to_owned(),
            range: touched_range(op),
            class: MergeConflictClass::Overlap,
          }),
        },
      )
      .collect()
  }

  /// Merges a create at `path` with `bytes`.
  fn merge_create(
    &self,
    path: &str,
    bytes: Vec<u8>,
    declared: &[Op],
  ) -> Result<Option<Effect>, ConflictWindow> {
    if self.occupied_by_nonfile(path) {
      return Err(Green::window(path, MergeConflictClass::TypeChanged));
    }
    match self.content.get(path) {
      Some(current) if current == bytes.as_slice() => Ok(None),
      Some(_) => Err(Green::window(path, MergeConflictClass::CreateCreate)),
      None => {
        let ops = vec![insert_whole(bytes.len() as u64)];
        Ok(Some(Effect::SetContent(
          path.to_owned(),
          bytes,
          ops,
          ContentSource::Created(declared.to_vec()),
        )))
      }
    }
  }

  /// Merges an unlink at `path`.
  /// Merges an unlink of whatever the path names — a regular file, a symlink, or a hard link
  /// (§4.16). Each is a delete/modify conflict if that dimension changed since the increment's base;
  /// a path that names nothing is a no-op (idempotent removal). A directory is not unlinked (rmdir
  /// removes directories); a path that is a directory falls through to the no-op.
  fn merge_unlink(&self, path: &str, base: u64) -> Result<Option<Effect>, ConflictWindow> {
    if self.specials.contains_key(path) {
      if self.changed_last(self.special_changed.get(path), path) > base
        || self.changed_last(self.mode_changed.get(path), path) > base
        || self
          .xattr_changed
          .iter()
          .any(|((changed_path, _), version)| changed_path == path && *version > base)
        || self.hardlinks.values().any(|target| target == path)
      {
        return Err(Green::window(path, MergeConflictClass::DeleteModify));
      }
      return Ok(Some(Effect::RemoveSpecial(path.to_owned())));
    }
    if self.content.contains_key(path) {
      if self.changed_last(self.last_changed.get(path), path) > base {
        return Err(Green::window(path, MergeConflictClass::DeleteModify));
      }
      return Ok(Some(Effect::RemoveContent(path.to_owned())));
    }
    if self.symlinks.contains_key(path) {
      if self.changed_last(self.symlink_changed.get(path), path) > base {
        return Err(Green::window(path, MergeConflictClass::DeleteModify));
      }
      return Ok(Some(Effect::RemoveSymlink(path.to_owned())));
    }
    if self.hardlinks.contains_key(path) {
      if self.changed_last(self.hardlink_changed.get(path), path) > base {
        return Err(Green::window(path, MergeConflictClass::DeleteModify));
      }
      return Ok(Some(Effect::RemoveHardlink(path.to_owned())));
    }
    Ok(None) // already gone (or never present)
  }

  /// Merges an edit to an existing file (the content path), by the two pure passes of §4.16 / D-27.
  /// Pass one maps each net op forward: an op no intervening change touched accepts at its shifted
  /// position; an op that meets an intervening change is a same-range candidate. Pass two is a
  /// memcmp **of that span alone** — the agent's declared bytes for the span against the green's
  /// current bytes there — so an identical (convergent) edit accepts as a no-op while a genuine
  /// divergence is a byte-exact conflict. The span-only check is the fix for the whole-file
  /// identity check's bug: a disjoint edit elsewhere in the file no longer poisons an identical
  /// overlap (T-6.x). A length-changing op (insert/delete/truncate) that overlaps is not yet decided
  /// per range — its coordinate mapping under a conflicting neighbour is owed — so the whole-file
  /// identity check still stands for the path in that case, exactly as before (never a regression).
  fn merge_content(
    &mut self,
    inc: &Increment,
    path: &str,
    ops: &[Op],
  ) -> Result<Option<Effect>, Vec<ConflictWindow>> {
    if !self.content.contains_key(path) {
      if self.occupied_by_nonfile(path) {
        return Err(vec![Green::window(path, MergeConflictClass::TypeChanged)]);
      }
      return Err(vec![Green::window(path, MergeConflictClass::DeleteModify)]);
    }
    // A copy after the base replaced the file whole, which no delta records: the edit was declared against other bytes
    // (A-586).
    if self.replaced_after(path, inc.base) {
      return Err(vec![Green::window(path, MergeConflictClass::DeleteModify)]);
    }
    // The file the work edited is gone when the path's file was removed after its base, whatever stands at the path
    // now: delete against modify (§4.16 "The verdict"), never an edit mapped into the file that took its place, whose
    // delta (one insert of its whole length) every op of the work would shift past (A-612).
    if self.file_removed_since(path, inc.base) {
      return Err(vec![Green::window(path, MergeConflictClass::DeleteModify)]);
    }
    let base_changed = self.changed_last(self.last_changed.get(path), path);
    let unchanged = base_changed <= inc.base;
    if unchanged {
      self.fast_path_hits = self.fast_path_hits.saturating_add(1);
    }
    let intervening = if unchanged {
      Vec::new()
    } else {
      self.intervening(path, inc.base)
    };
    // Pass one: classify each op. Disjoint ops go to `mapped` (accept at the shifted position);
    // overlapping overwrites become same-range candidates; a length-changing op that overlaps falls
    // the path back to the whole-file identity check (owed).
    let mut mapped = Vec::with_capacity(ops.len());
    let mut candidates: Vec<&Op> = Vec::new();
    let mut shifting_overlap: Option<Range> = None;
    let head_len = self.content.get(path).map_or(0, len_of);
    for op in ops {
      match crate::map::map_range(&intervening, touched_range(op)) {
        crate::map::Mapped::Shifted(shifted) => {
          let mut moved = *op;
          moved.at = shifted.start;
          mapped.push(at_head(moved, head_len));
        }
        crate::map::Mapped::Overlaps => {
          if op.kind == OpKind::Overwrite {
            candidates.push(op);
          } else {
            shifting_overlap.get_or_insert(touched_range(op));
          }
        }
      }
    }

    // A length-changing op overlapped: the whole-file identity check decides the path (owed: full
    // per-range identity for shifting ops), so a currently-accepted convergent case never regresses.
    if let Some(range) = shifting_overlap {
      if self.lands_on_head(path, inc.base, (ops, &inc.post_state)) {
        return Ok(None);
      }
      return Err(vec![ConflictWindow {
        path: path.to_owned(),
        range,
        class: MergeConflictClass::Overlap,
      }]);
    }

    // Pass two: each overlapping overwrite must reproduce, byte for byte, what the intervening
    // change already placed at that span; a differing span is a byte-exact conflict. An identical
    // span is a no-op (the green already holds those bytes), so it is not added to `mapped`. Every differing span of
    // the file is named (the reply is the windows the agent rebases against, §4.16), in the ops' order, which is the
    // file's: until 2026-11-04 the first one ended the verdict, so an agent learned of a file's conflicts one rebase at
    // a time.
    let mut differing: Vec<ConflictWindow> = Vec::new();
    for op in &candidates {
      let span = touched_range(op);
      let head_start = match crate::map::map_range(&intervening, Range::new(span.start, 0)) {
        crate::map::Mapped::Shifted(head) => head.start,
        // A zero-length point cannot overlap (the edge rule), so this arm is unreachable; take the
        // base offset rather than panic.
        crate::map::Mapped::Overlaps => span.start,
      };
      let agent_bytes = span_bytes(&inc.post_state, op.src, op.len);
      let current = self.content.get(path).unwrap_or_default();
      let start = usize::try_from(head_start).unwrap_or(usize::MAX);
      let end = start.saturating_add(agent_bytes.len());
      let identical = current.get(start..end) == Some(agent_bytes);
      if !identical {
        differing.push(ConflictWindow {
          path: path.to_owned(),
          range: span,
          class: MergeConflictClass::Overlap,
        });
      }
    }
    if !differing.is_empty() {
      return Err(differing);
    }

    // Every op accepted. When only identical overlaps remained (nothing disjoint to apply), the
    // green already holds the result: accept with no new effect, as an identical edit always has.
    if mapped.is_empty() {
      return Ok(None);
    }
    let current = self.content.get(path).unwrap_or_default();
    let next = apply(current, &mapped, &inc.post_state);
    Ok(Some(Effect::SetContent(
      path.to_owned(),
      next,
      mapped,
      ContentSource::Mapped,
    )))
  }

  /// Merges a directory creation.
  fn merge_mkdir(&self, path: &str) -> Result<Option<Effect>, ConflictWindow> {
    if self.content.contains_key(path)
      || self.symlinks.contains_key(path)
      || self.hardlinks.contains_key(path)
      || self.specials.contains_key(path)
    {
      return Err(Green::window(path, MergeConflictClass::TypeChanged));
    }
    if self.dirs.contains_key(path) {
      return Ok(None);
    }
    Ok(Some(Effect::MakeDir(path.to_owned())))
  }

  /// Merges a directory removal against the increment's own removals.
  fn merge_rmdir(
    &self,
    path: &str,
    cleared: &BTreeSet<String>,
  ) -> Result<Option<Effect>, ConflictWindow> {
    if self.content.contains_key(path)
      || self.symlinks.contains_key(path)
      || self.hardlinks.contains_key(path)
      || self.specials.contains_key(path)
    {
      return Err(Green::window(path, MergeConflictClass::TypeChanged));
    }
    if !self.dirs.contains_key(path) {
      return Ok(None);
    }
    let prefix = format!("{path}/");
    if self.has_live_child(&prefix, cleared) {
      return Err(Green::window(path, MergeConflictClass::DeleteModify));
    }
    Ok(Some(Effect::RemoveDir(path.to_owned())))
  }

  /// Metadata on a former IPC inode cannot silently apply to another kind that now owns its
  /// name, even when the replacement already has the requested value. An absent name is handled
  /// by each verb's existing delete/idempotence rule.
  fn special_kind_changed(&self, path: &str, base: u64) -> bool {
    let current = self.specials.get(path).map(|node| node.kind);
    if current.is_none()
      && !self.content.contains_key(path)
      && !self.dirs.contains_key(path)
      && !self.symlinks.contains_key(path)
      && !self.hardlinks.contains_key(path)
    {
      return false;
    }
    let previous = self.special_at(path, base).map(|node| node.kind);
    previous != current
  }

  /// Merges a mode change.
  fn merge_setmode(
    &self,
    path: &str,
    base: u64,
    mode: u32,
    established: bool,
  ) -> Result<Option<Effect>, ConflictWindow> {
    if !self.content.contains_key(path)
      && !self.dirs.contains_key(path)
      && !self.specials.contains_key(path)
      && !established
    {
      return Err(Green::window(path, MergeConflictClass::DeleteModify));
    }
    // A file removed after the work's base is gone for it, whatever stands at the path now: the
    // mode was set on a file that no longer exists (A-631, as A-612 for an edit).
    if !established && self.file_removed_since(path, base) {
      return Err(Green::window(path, MergeConflictClass::DeleteModify));
    }
    if !established && self.special_kind_changed(path, base) {
      return Err(Green::window(path, MergeConflictClass::TypeChanged));
    }
    if self.changed_last(self.mode_changed.get(path), path) > base {
      if self.modes.get(path).copied() == Some(mode) {
        return Ok(None);
      }
      return Err(Green::window(path, MergeConflictClass::MetaMeta));
    }
    Ok(Some(Effect::Mode(path.to_owned(), mode)))
  }

  /// Merges a symbolic link.
  fn merge_symlink(
    &self,
    path: &str,
    base: u64,
    target: &str,
  ) -> Result<Option<Effect>, ConflictWindow> {
    // A file, directory or hard link already at this path is a different kind — a type conflict. An
    // existing symlink is not: that is the retarget case decided just below.
    if self.content.contains_key(path)
      || self.dirs.contains_key(path)
      || self.hardlinks.contains_key(path)
      || self.specials.contains_key(path)
    {
      return Err(Green::window(path, MergeConflictClass::TypeChanged));
    }
    if self.changed_last(self.symlink_changed.get(path), path) > base {
      if self.symlinks.get(path).map(String::as_str) == Some(target) {
        return Ok(None);
      }
      return Err(Green::window(path, MergeConflictClass::CreateCreate));
    }
    Ok(Some(Effect::Symlink(path.to_owned(), target.to_owned())))
  }

  /// Merges a hard link (a namespace edge; a differing intervening link is a conflict).
  fn merge_hardlink(
    &self,
    path: &str,
    base: u64,
    target: &str,
  ) -> Result<Option<Effect>, ConflictWindow> {
    // A file, directory or symlink already at this path is a different kind — a type conflict. An
    // existing hard link is not: that is the retarget case decided just below.
    if self.content.contains_key(path)
      || self.dirs.contains_key(path)
      || self.symlinks.contains_key(path)
      || self.specials.contains_key(path)
    {
      return Err(Green::window(path, MergeConflictClass::TypeChanged));
    }
    if self.changed_last(self.hardlink_changed.get(path), path) > base {
      if self.hardlinks.get(path).map(String::as_str) == Some(target) {
        return Ok(None);
      }
      return Err(Green::window(path, MergeConflictClass::CreateCreate));
    }
    Ok(Some(Effect::Hardlink(path.to_owned(), target.to_owned())))
  }

  /// Merges an xattr set; a differing intervening value for the same `(path, name)` conflicts.
  fn merge_set_xattr(
    &self,
    path: &str,
    name: &str,
    base: u64,
    value: &[u8],
    established: bool,
  ) -> Result<Option<Effect>, ConflictWindow> {
    if !self.content.contains_key(path)
      && !self.dirs.contains_key(path)
      && !self.specials.contains_key(path)
      && !established
    {
      return Err(Green::window(path, MergeConflictClass::DeleteModify));
    }
    // An attribute set on a file removed after the work's base was set on a file that no longer
    // exists (A-631).
    if !established && self.file_removed_since(path, base) {
      return Err(Green::window(path, MergeConflictClass::DeleteModify));
    }
    if !established && self.special_kind_changed(path, base) {
      return Err(Green::window(path, MergeConflictClass::TypeChanged));
    }
    let key = (path.to_owned(), name.to_owned());
    if self.changed_last(self.xattr_changed.get(&key), path) > base {
      if self.xattrs.get(path, name).map(Vec::as_slice) == Some(value) {
        return Ok(None);
      }
      return Err(Green::window(path, MergeConflictClass::MetaMeta));
    }
    Ok(Some(Effect::SetXattr(
      path.to_owned(),
      name.to_owned(),
      value.to_vec(),
    )))
  }

  /// Merges an xattr removal.
  fn merge_remove_xattr(
    &self,
    path: &str,
    name: &str,
    base: u64,
    established: bool,
  ) -> Result<Option<Effect>, ConflictWindow> {
    // An attribute removed from a file removed after the work's base was removed from a file that
    // no longer exists (A-631), whatever the file at the path now holds.
    if !established && self.file_removed_since(path, base) {
      return Err(Green::window(path, MergeConflictClass::DeleteModify));
    }
    if self.special_kind_changed(path, base) {
      return Err(Green::window(path, MergeConflictClass::TypeChanged));
    }
    let key = (path.to_owned(), name.to_owned());
    if !self.xattrs.contains_key(path, name) {
      return Ok(None);
    }
    if self.changed_last(self.xattr_changed.get(&key), path) > base {
      return Err(Green::window(path, MergeConflictClass::MetaMeta));
    }
    Ok(Some(Effect::RemoveXattr(path.to_owned(), name.to_owned())))
  }

  /// An IPC creation compares the complete captured metadata, without ever comparing stream
  /// bytes. A different kind is a type conflict; a differing concurrent creation conflicts.
  fn merge_special(
    &self,
    path: &str,
    base: u64,
    node: SpecialNode,
  ) -> Result<Option<Effect>, ConflictWindow> {
    if self.content.contains_key(path)
      || self.dirs.contains_key(path)
      || self.symlinks.contains_key(path)
      || self.hardlinks.contains_key(path)
    {
      return Err(Green::window(path, MergeConflictClass::TypeChanged));
    }
    if let Some(current) = self.specials.get(path) {
      if current.kind != node.kind {
        return Err(Green::window(path, MergeConflictClass::TypeChanged));
      }
      if current == &node {
        return Ok(None);
      }
    }
    if self.changed_last(self.special_changed.get(path), path) > base {
      return Err(Green::window(path, MergeConflictClass::CreateCreate));
    }
    Ok(Some(Effect::Special(path.to_owned(), node)))
  }

  /// The paths whose content ops do not fit the content they edit at the increment's base ([`fits_base`]): a
  /// created path edits nothing, a renamed-into path edits its source.
  fn unfit_content(&self, inc: &Increment, r: &Resolved) -> Vec<String> {
    let post_len = u64::try_from(inc.post_state.len()).unwrap_or(u64::MAX);
    r.content
      .iter()
      .filter(|(path, ops)| {
        let edited = r.renames.get(*path).unwrap_or(path);
        let base_len = if r.creates.contains(*path) {
          0
        } else {
          self
            .content_len_at(edited, inc.base)
            .map_or(0, |len| u64::try_from(len).unwrap_or(u64::MAX))
        };
        !fits_base(ops, base_len, post_len)
      })
      .map(|(path, _)| path.clone())
      .collect()
  }

  /// Decides every dimension of an increment, collecting the accepted effects or the conflicts.
  fn decide(&mut self, inc: &Increment, r: &Resolved) -> Result<Vec<Effect>, Vec<ConflictWindow>> {
    let mut effects = Vec::new();
    let mut windows: Vec<_> = r
      .invalid
      .iter()
      .chain(self.unfit_content(inc, r).iter())
      .chain(self.unfit_copied(inc, r).iter())
      .map(|path| Green::window(path, MergeConflictClass::TypeChanged))
      .collect();
    // Copies first, and what lands on their trees after them (A-586): their effects lead, as commit applies them.
    self.decide_copies(inc, r, &mut effects, &mut windows);
    self.decide_copied(inc, r, &mut effects, &mut windows);
    for (path, node) in &r.specials {
      record(
        &mut effects,
        &mut windows,
        self.merge_special(path, inc.base, *node),
      );
    }
    self.decide_content(inc, r, &mut effects, &mut windows);
    self.decide_directories(r, &mut effects, &mut windows);
    self.decide_metadata(inc, r, &mut effects, &mut windows);
    self.decide_node_exclusive(inc, &mut windows);
    if windows.is_empty() {
      Ok(effects)
    } else {
      Err(windows)
    }
  }

  /// The node-exclusive rule (§4.17.4 item 3, rules 1 and 2; A-542): a path an operation marked node-exclusive changes,
  /// a move's source among them, that changed in any dimension after the increment's base is a conflict, however
  /// disjoint the bytes and however identical the change, as FSFS's commit-time merge judges a node (`tree.c`
  /// `merge()`: a file changed on both sides fails, "even an identical replacement", and two additions are "not
  /// auto-mergeable"). The dimension verdicts have run and added their own windows; this pass refuses only what they
  /// would accept. Each path is one lookup of its last change in the change index (`latest_change`), and is judged once.
  fn decide_node_exclusive(&self, inc: &Increment, windows: &mut Vec<ConflictWindow>) {
    let mut judged: BTreeSet<&str> = BTreeSet::new();
    for op in inc.doc.ops.iter().filter(|op| op.is_node_exclusive()) {
      for (index, class) in node_exclusive_paths(op) {
        let Some(path) = inc.doc.paths.path(index) else {
          continue;
        };
        if judged.insert(path) && self.changed_after(path, inc.base) {
          windows.push(Green::window(path, class));
        }
      }
    }
  }

  /// Whether `path` changed in any dimension after version `base`: its last change in the change index, or the latest
  /// copy above it (A-586), is past it.
  fn changed_after(&self, path: &str, base: u64) -> bool {
    self.changed_last(self.latest_change.get(path), path) > base
  }

  /// The content dimension: renames, creates, unlinks and edits.
  fn decide_content(
    &mut self,
    inc: &Increment,
    r: &Resolved,
    effects: &mut Vec<Effect>,
    windows: &mut Vec<ConflictWindow>,
  ) {
    for (dst, from) in &r.renames {
      let ops = r.content.get(dst).map(Vec::as_slice).unwrap_or(&[]);
      if let Err(window) = self.merge_rename((dst, ops), (inc.base, &inc.post_state), from, effects)
      {
        windows.push(window);
      }
    }
    for path in &r.creates {
      let content = r.content.get(path).map(Vec::as_slice).unwrap_or(&[]);
      let bytes = apply(&[], content, &inc.post_state);
      record(effects, windows, self.merge_create(path, bytes, content));
    }
    for path in &r.unlinks {
      record(effects, windows, self.merge_unlink(path, inc.base));
    }
    for (path, ops) in &r.content {
      if r.creates.contains(path) || r.renames.contains_key(path) {
        continue; // content of a created or renamed-into file is handled with that effect
      }
      match self.merge_content(inc, path, ops) {
        Ok(Some(effect)) => effects.push(effect),
        Ok(None) => {}
        Err(mut found) => windows.append(&mut found),
      }
    }
  }

  /// The directory dimension: creation and removal (removal against the increment's own removals).
  fn decide_directories(
    &self,
    r: &Resolved,
    effects: &mut Vec<Effect>,
    windows: &mut Vec<ConflictWindow>,
  ) {
    let cleared = r.cleared();
    for path in &r.mkdirs {
      record(effects, windows, self.merge_mkdir(path));
    }
    for path in &r.rmdirs {
      record(effects, windows, self.merge_rmdir(path, &cleared));
    }
  }

  /// The metadata dimensions: mode, symlink, hard link and xattrs.
  fn decide_metadata(
    &self,
    inc: &Increment,
    r: &Resolved,
    effects: &mut Vec<Effect>,
    windows: &mut Vec<ConflictWindow>,
  ) {
    for (path, mode) in &r.modes {
      record(
        effects,
        windows,
        self.merge_setmode(path, inc.base, *mode, r.establishes(path)),
      );
    }
    for (path, target) in &r.symlinks {
      record(effects, windows, self.merge_symlink(path, inc.base, target));
    }
    for (path, target) in &r.hardlinks {
      record(
        effects,
        windows,
        self.merge_hardlink(path, inc.base, target),
      );
    }
    for (path, name, value) in &r.xattr_sets {
      record(
        effects,
        windows,
        self.merge_set_xattr(path, name, inc.base, value, r.establishes(path)),
      );
    }
    for (path, name) in &r.xattr_removes {
      record(
        effects,
        windows,
        self.merge_remove_xattr(path, name, inc.base, r.establishes(path)),
      );
    }
  }

  /// Keep the superseded IPC value for pinned readers before setting the new value.
  fn commit_special(&mut self, path: String, node: SpecialNode, version: u64) {
    let at = self.seed_of(&self.special_history, &path, &path, version);
    seed(
      &mut self.special_history,
      &path,
      held(self.specials.get(&path)),
      at,
    );
    if self.specials.insert(&path, node).is_some() {
      self.special_history_bytes = self
        .special_history_bytes
        .saturating_add(SPECIAL_NODE_BYTES);
    }
    self.special_changed.insert(path.clone(), version);
    self
      .special_history
      .entry(path)
      .or_default()
      .push((version, Some(node)));
  }

  /// Remove the live inode and its metadata while retaining their historical values.
  fn remove_special(&mut self, path: String, version: u64) {
    self.file_removed.insert(path.clone(), version);
    // The inode's own key is marked by its effect; its mode and attributes go with it and are marked here.
    let at = self.seed_of(&self.special_history, &path, &path, version);
    seed(
      &mut self.special_history,
      &path,
      held(self.specials.get(&path)),
      at,
    );
    if self.specials.remove(&path).is_some() {
      self.special_history_bytes = self
        .special_history_bytes
        .saturating_add(SPECIAL_NODE_BYTES);
    }
    self.special_changed.insert(path.clone(), version);
    let at = self.seed_of(&self.mode_history, &path, &path, version);
    seed(
      &mut self.mode_history,
      &path,
      held(self.modes.get(&path)),
      at,
    );
    if self.modes.remove(&path).is_some() {
      self.identity_dirty.insert(IdentityKey::Mode(path.clone()));
      self.mode_changed.insert(path.clone(), version);
      self
        .mode_history
        .entry(path.clone())
        .or_default()
        .push((version, None));
    }
    let attributes: Vec<(String, Vec<u8>)> = self
      .xattrs
      .of(&path)
      .map(|(name, value)| (name, value.clone()))
      .collect();
    for (name, value) in attributes {
      self.xattrs.remove(&path, &name);
      let key = (path.clone(), name);
      let at = self.seed_of(&self.xattr_history, &key, &path, version);
      seed(&mut self.xattr_history, &key, Some(Some(value)), at);
      self
        .identity_dirty
        .insert(IdentityKey::Xattr(key.0.clone(), key.1.clone()));
      self.xattr_changed.insert(key.clone(), version);
      self
        .xattr_history
        .entry(key)
        .or_default()
        .push((version, None));
    }
    self
      .special_history
      .entry(path)
      .or_default()
      .push((version, None));
  }

  /// The [`Seed`] of a change at `version` to `key` of `history`, on `path`: the version the key's held value dates
  /// from, where its history does not hold it already (A-534's one history rule, A-586's copies).
  fn seed_of<K: Ord, T>(
    &self,
    history: &BTreeMap<K, Vec<(u64, T)>>,
    key: &K,
    path: &str,
    version: u64,
  ) -> Seed {
    Seed {
      since: self.seed_since(path, last_version(history.get(key))),
      version,
    }
  }

  /// Applies the copies among the accepted effects (A-586), before any other effect, so what the increment does to a
  /// copied tree lands on what the copy brought.
  fn commit_copies(&mut self, effects: &[Effect], version: u64) {
    for effect in effects {
      if let Effect::CopyTree(dst, src, source_version) = effect {
        self.commit_copy(dst, src, *source_version, version);
      }
    }
  }

  /// Applies the accepted effects, committing version `version`.
  fn commit(&mut self, effects: Vec<Effect>, version: u64) {
    let changed: BTreeSet<String> = effects.iter().map(|e| e.path().to_owned()).collect();
    let mut delta: BTreeMap<String, Vec<Op>> = BTreeMap::new();
    let mut removed_content: Vec<String> = Vec::new();
    // Each set carries whether it is a rename's, which removes the file it replaces (`file_removed`, A-612).
    let mut set_content: Vec<(String, Vec<u8>, Vec<Op>, bool)> = Vec::new();
    self.commit_copies(&effects, version);
    // Content removes are staged before sets so a rename's source removal never deletes a file the
    // same increment recreates or renames into at that path.
    for effect in effects {
      // Every key an effect touches is marked once; the identity is brought up to date only when it is read (A-202).
      self.identity_dirty.insert(effect.identity_key());
      match effect {
        Effect::Special(path, node) => self.commit_special(path, node, version),
        Effect::RemoveSpecial(path) => self.remove_special(path, version),
        Effect::SetContent(path, bytes, ops, source) => {
          let renamed = matches!(source, ContentSource::Copied(..));
          set_content.push((path, bytes, ops, renamed));
        }
        Effect::RemoveContent(path) => removed_content.push(path),
        // Each key's history first records the value it held when its history does not hold it (`seed`: since version
        // 0 by the one history rule, A-534; since a copy, A-586), then its change.
        Effect::MakeDir(path) => {
          let stood = self.dirs.contains_key(&path).then_some(true);
          let at = self.seed_of(&self.dir_history, &path, &path, version);
          seed(&mut self.dir_history, &path, stood, at);
          self.dirs.insert(&path, ());
          self
            .dir_history
            .entry(path)
            .or_default()
            .push((version, true));
        }
        Effect::RemoveDir(path) => {
          self.file_removed.insert(path.clone(), version);
          let stood = self.dirs.contains_key(&path).then_some(true);
          let at = self.seed_of(&self.dir_history, &path, &path, version);
          seed(&mut self.dir_history, &path, stood, at);
          self.dirs.remove(&path);
          self
            .dir_history
            .entry(path)
            .or_default()
            .push((version, false));
        }
        Effect::Mode(path, mode) => {
          let at = self.seed_of(&self.mode_history, &path, &path, version);
          seed(
            &mut self.mode_history,
            &path,
            held(self.modes.get(&path)),
            at,
          );
          self.modes.insert(&path, mode);
          self.mode_changed.insert(path.clone(), version);
          self
            .mode_history
            .entry(path)
            .or_default()
            .push((version, Some(mode)));
        }
        Effect::Symlink(path, target) => {
          let at = self.seed_of(&self.symlink_history, &path, &path, version);
          seed(
            &mut self.symlink_history,
            &path,
            held(self.symlinks.get(&path)),
            at,
          );
          self.symlinks.insert(&path, target.clone());
          self.symlink_changed.insert(path.clone(), version);
          self
            .symlink_history
            .entry(path)
            .or_default()
            .push((version, Some(target)));
        }
        Effect::RemoveSymlink(path) => {
          self.file_removed.insert(path.clone(), version);
          let at = self.seed_of(&self.symlink_history, &path, &path, version);
          seed(
            &mut self.symlink_history,
            &path,
            held(self.symlinks.get(&path)),
            at,
          );
          self.symlinks.remove(&path);
          self.symlink_changed.insert(path.clone(), version);
          self
            .symlink_history
            .entry(path)
            .or_default()
            .push((version, None));
        }
        Effect::Hardlink(path, target) => {
          let at = self.seed_of(&self.hardlink_history, &path, &path, version);
          seed(
            &mut self.hardlink_history,
            &path,
            held(self.hardlinks.get(&path)),
            at,
          );
          let old = self.hardlinks.insert(&path, target.clone());
          relink(&mut self.links_to, &path, old.as_deref(), Some(&target));
          self.hardlink_changed.insert(path.clone(), version);
          self
            .hardlink_history
            .entry(path)
            .or_default()
            .push((version, Some(target)));
        }
        Effect::RemoveHardlink(path) => {
          self.file_removed.insert(path.clone(), version);
          let at = self.seed_of(&self.hardlink_history, &path, &path, version);
          seed(
            &mut self.hardlink_history,
            &path,
            held(self.hardlinks.get(&path)),
            at,
          );
          let old = self.hardlinks.remove(&path);
          relink(&mut self.links_to, &path, old.as_deref(), None);
          self.hardlink_changed.insert(path.clone(), version);
          self
            .hardlink_history
            .entry(path)
            .or_default()
            .push((version, None));
        }
        Effect::SetXattr(path, name, value) => {
          let before = held(self.xattrs.get(&path, &name));
          self.xattrs.insert(&path, &name, value.clone());
          let key = (path, name);
          let at = self.seed_of(&self.xattr_history, &key, &key.0, version);
          seed(&mut self.xattr_history, &key, before, at);
          self.xattr_changed.insert(key.clone(), version);
          self
            .xattr_history
            .entry(key)
            .or_default()
            .push((version, Some(value)));
        }
        Effect::RemoveXattr(path, name) => {
          let before = self.xattrs.remove(&path, &name).map(Some);
          let key = (path, name);
          let at = self.seed_of(&self.xattr_history, &key, &key.0, version);
          seed(&mut self.xattr_history, &key, before, at);
          self.xattr_changed.insert(key.clone(), version);
          self
            .xattr_history
            .entry(key)
            .or_default()
            .push((version, None));
        }
        // Applied before every other effect (`commit_copies`), so the effects on its tree land on what it brought.
        Effect::CopyTree(..) => {}
      }
    }
    for path in removed_content {
      // A removed file's last value stays in the history for reconstruction: no longer a current value, so retained
      // and charged (`history_bytes`).
      self.seed_content(&path, version);
      self.content.remove(&path);
      self.last_changed.insert(path.clone(), version);
      self.file_removed.insert(path.clone(), version);
      self.push_content(path, version, None);
    }
    for (path, bytes, ops, renamed) in set_content {
      // A rename onto a file removes it (A-612); onto a path this commit emptied, the removal is recorded already. A file
      // this version's own copy brought and its increment edited (A-586) was no other file: nothing was removed.
      if renamed && self.content.contains_key(&path) && !self.copied_in(&path, version) {
        self.file_removed.insert(path.clone(), version);
      }
      // The value this one supersedes stays in the history, sharing every run the new one kept (W2): what the
      // history gains is the bytes the new value added, its piece list, and whatever the old one held that the new
      // one does not, the retention charged to the running totals.
      self.seed_content(&path, version);
      let value = self.content_value(&path, &ops, &bytes);
      self.content.insert(&path, bytes);
      self.last_changed.insert(path.clone(), version);
      self.push_content(path.clone(), version, Some(value));
      delta.insert(path, ops);
    }
    self.deltas.push(delta);
    self.record_changes(version, changed);
  }

  /// Records what version `version` changed in the change index (`changes`): every path an effect changed, and every
  /// hard link naming one of them that is or was a special, as [`Green::changed_between`] counts a link changed with
  /// the special it names. The cost of the version's own changes and their links.
  fn record_changes(&mut self, version: u64, mut changed: BTreeSet<String>) {
    let links: Vec<String> = changed
      .iter()
      .filter(|path| self.was_special(path))
      .filter_map(|path| self.links_to.get(path))
      .flatten()
      .cloned()
      .collect();
    changed.extend(links);
    if changed.is_empty() {
      return;
    }
    for path in &changed {
      let bytes = change_bytes(path);
      match self.latest_change.insert(path.clone(), version) {
        None => self.changes_bytes = self.changes_bytes.saturating_add(bytes),
        // The earlier version's entry is no longer the path's last: a seek in that version's sorted list.
        Some(earlier) => {
          if let Some(entries) = self.changes.get_mut(&earlier)
            && let Ok(at) = entries.binary_search_by(|entry| entry.path.as_str().cmp(path))
            && let Some(entry) = entries.get_mut(at)
          {
            entry.last = false;
          }
        }
      }
      self.changes_bytes = self.changes_bytes.saturating_add(bytes);
    }
    self.changes.insert(version, numbered(changed));
  }

  /// Each path's last change after the origin in any dimension, read off the histories' last entries.
  fn last_changes_after_origin(&self) -> BTreeMap<String, u64> {
    let mut last: BTreeMap<String, u64> = BTreeMap::new();
    for (path, history) in &self.content_history {
      note_last(&mut last, path, history.last().map(|(v, _)| *v));
    }
    for (path, history) in &self.special_history {
      note_last(&mut last, path, history.last().map(|(v, _)| *v));
    }
    for (path, history) in &self.dir_history {
      note_last(&mut last, path, history.last().map(|(v, _)| *v));
    }
    for (path, history) in &self.mode_history {
      note_last(&mut last, path, history.last().map(|(v, _)| *v));
    }
    for (path, history) in &self.symlink_history {
      note_last(&mut last, path, history.last().map(|(v, _)| *v));
    }
    for (path, history) in &self.hardlink_history {
      note_last(&mut last, path, history.last().map(|(v, _)| *v));
    }
    for ((path, _), history) in &self.xattr_history {
      note_last(&mut last, path, history.last().map(|(v, _)| *v));
    }
    last
  }

  /// Rebuilds the change index from the histories, each path at its last change after the origin: a green restored
  /// from a checkpoint ([`Green::restore`]) holds one entry a key, at the version its value in effect was set, and a hard
  /// link naming a special is changed when the special last changed, if later. The cost of the restore's own keys.
  fn index_restored_changes(&mut self) {
    let mut last = self.last_changes_after_origin();
    for (alias, target) in self.hardlinks.iter() {
      if self.was_special(target)
        && let Some(special) = last.get(target).copied()
      {
        note_last(&mut last, &alias, Some(special));
      }
    }
    let mut by_version: BTreeMap<u64, BTreeSet<String>> = BTreeMap::new();
    for (path, version) in &last {
      by_version.entry(*version).or_default().insert(path.clone());
    }
    // Every key restored is at or below the checkpoint, the restored green's floor: its last change is the one below it.
    self.changes_bytes = last
      .keys()
      .map(|path| change_bytes(path).saturating_mul(RESTORED_INDEX_ENTRIES))
      .sum();
    self.below_floor_last = last.clone();
    self.changes = by_version
      .into_iter()
      .map(|(version, paths)| (version, numbered(paths)))
      .collect();
    self.latest_change = last;
  }

  /// Folds the change index below `floor` (from `previous`, the floor before), as the histories fold, each key keeping
  /// its value in effect at the floor: of a path's entries below the floor only its last stays, the others go, so a span
  /// that starts below the floor and ends at or above it is answered as the folded histories answer it. A version
  /// emptied goes too; an entry keeps its place in its version, so a walk's cursor holds. The cost of the entries the
  /// floor passes.
  fn fold_changes_before(&mut self, previous: u64, floor: u64) {
    let passed: Vec<u64> = self
      .changes
      .range(previous..floor)
      .map(|(version, _)| *version)
      .collect();
    for version in passed {
      let paths: Vec<String> = self
        .changes
        .get(&version)
        .map(|entries| entries.iter().map(|entry| entry.path.clone()).collect())
        .unwrap_or_default();
      for path in paths {
        match self.below_floor_last.insert(path.clone(), version) {
          None => self.changes_bytes = self.changes_bytes.saturating_add(change_bytes(&path)),
          Some(earlier) => self.drop_change_entry(earlier, &path),
        }
      }
    }
  }

  /// Drops `path`'s entry at `version` from the change index, and the version when it is left empty.
  fn drop_change_entry(&mut self, version: u64, path: &str) {
    let Some(entries) = self.changes.get_mut(&version) else {
      return;
    };
    if let Ok(at) = entries.binary_search_by(|entry| entry.path.as_str().cmp(path)) {
      entries.remove(at);
      self.changes_bytes = self.changes_bytes.saturating_sub(change_bytes(path));
    }
    if entries.is_empty() {
      self.changes.remove(&version);
    }
  }

  /// The bytes the change index holds (`changes`, `latest_change`), a running total: charged with the histories'
  /// ([`Green::retention_charge`]) and folded under the same budget.
  pub fn changes_bytes(&self) -> usize {
    self.changes_bytes
  }

  /// The change index's bytes, recounted from it: the oracle [`Green::changes_bytes`] is checked against.
  pub fn changes_recounted(&self) -> usize {
    let indexed: usize = self
      .changes
      .values()
      .flatten()
      .map(|entry| &entry.path)
      .chain(self.latest_change.keys())
      .chain(self.below_floor_last.keys())
      .map(|path| change_bytes(path))
      .sum();
    let copies: usize = self
      .copies
      .iter()
      .flat_map(|(dst, records)| {
        records
          .iter()
          .map(move |record| copy::appearance_bytes(dst, &record.source))
      })
      .sum();
    indexed.saturating_add(copies)
  }

  /// Submits an increment: idempotent by identity, decides every dimension of its ops document, and
  /// commits a new version when all accept, or returns the conflict windows and changes nothing.
  /// An accepted increment's outcome is remembered for good (it is in the chain); a rejected one's
  /// windows are kept in the bounded rejected-result cache, from which a retry is answered while the
  /// entry survives and judged again once it was evicted.
  pub fn submit(&mut self, inc: &Increment) -> Outcome {
    self.submit_resolved(inc).0
  }

  /// [`Green::submit`], and what its verdict resolved the increment to when it committed a new version: what the
  /// durable chain records (`crate::chain_entry`). `None` beside every other outcome, an idempotent accept included,
  /// whose version the chain already holds.
  pub fn submit_resolved(
    &mut self,
    inc: &Increment,
  ) -> (Outcome, Option<Vec<crate::chain_entry::Applied>>) {
    if let Some(version) = self.accepted.get(&inc.id) {
      return (Outcome::Accepted { version: *version }, None);
    }
    if let Some(windows) = self.rejected.get(&inc.id) {
      return (
        Outcome::Conflict {
          windows: windows.clone(),
        },
        None,
      );
    }
    if inc.base < self.deltas_from {
      return (
        Outcome::BaseUnavailable {
          base: inc.base,
          floor: self.deltas_from,
        },
        None,
      );
    }
    let resolved = Green::resolve(inc);
    match self.decide(inc, &resolved) {
      Ok(effects) => {
        let applied = effects.iter().map(Effect::applied).collect();
        // A u64 version: 2^64 merges into one green cannot happen; saturating keeps the arithmetic total.
        let version = self.head().saturating_add(1);
        self.commit(effects, version);
        self.accepted.insert(inc.id, version);
        (Outcome::Accepted { version }, Some(applied))
      }
      Err(windows) => {
        self.retain_rejected(inc.id, &windows);
        (Outcome::Conflict { windows }, None)
      }
    }
  }

  /// Commits an accepted version as its chain entry recorded it, the next version after the head (§4.16, §4.8 recovery;
  /// `crate::chain_entry`): nothing is judged, so what a checkpoint restored of the green below its version does not
  /// matter. The state after it is a function of the entry and the state before: the same effects reach
  /// [`Green::commit`] as reached it live, so the deltas, the histories and every last-changed index come out the same,
  /// and the increment's identity is remembered, so a retry of it is answered its version. Returns the version; an
  /// increment already accepted commits nothing and returns the version it was accepted as.
  pub fn apply_recorded(
    &mut self,
    inc: &Increment,
    applied: &[crate::chain_entry::Applied],
  ) -> u64 {
    // An increment already accepted is answered its version and commits nothing, as a submit would: a chain holding one
    // increment twice is corrupt, and its caller sees the version differ from the one the entry was recorded as.
    if let Some(version) = self.accepted.get(&inc.id) {
      return *version;
    }
    let effects = applied
      .iter()
      .map(|effect| self.effect_of(effect, &inc.post_state))
      .collect();
    let version = self.head().saturating_add(1);
    self.commit(effects, version);
    self.accepted.insert(inc.id, version);
    version
  }

  /// Records how the signature of the submit that committed `version` was judged (signed work, A-321): what the service
  /// does once the version's chain entry is durable, and recovery as it replays the entry. A version past the head is
  /// never committed, so it is not recorded, and its answer is `false`.
  pub fn record_signature(
    &mut self,
    version: u64,
    signature: crate::chain_entry::Signature,
  ) -> bool {
    if version == 0 || version > self.head() {
      return false;
    }
    self.signatures.insert(version, signature);
    true
  }

  /// How the signature of the submit that committed `version` was judged, if that submit carried one (A-321).
  pub fn signature(&self, version: u64) -> Option<crate::chain_entry::Signature> {
    self.signatures.get(&version).copied()
  }

  /// The effect a chain entry's resolved form stands for, its bytes read from the head and the increment's post-state.
  fn effect_of(&self, applied: &crate::chain_entry::Applied, post_state: &[u8]) -> Effect {
    use crate::chain_entry::Applied;
    match applied {
      Applied::Special(path, node) => Effect::Special(path.clone(), *node),
      Applied::RemoveSpecial(path) => Effect::RemoveSpecial(path.clone()),
      Applied::Spliced(path, ops) => {
        let current = self.content.get(path).unwrap_or_default();
        Effect::SetContent(
          path.clone(),
          apply(current, ops, post_state),
          ops.clone(),
          ContentSource::Mapped,
        )
      }
      Applied::Created(path, declared) => {
        let bytes = apply(&[], declared, post_state);
        let ops = vec![insert_whole(bytes.len() as u64)];
        Effect::SetContent(
          path.clone(),
          bytes,
          ops,
          ContentSource::Created(declared.clone()),
        )
      }
      Applied::Copied { dst, from, ops } => {
        let source = self.content.get(from).unwrap_or_default();
        let bytes = apply(source, ops, post_state);
        let whole = vec![insert_whole(bytes.len() as u64)];
        Effect::SetContent(
          dst.clone(),
          bytes,
          whole,
          ContentSource::Copied(from.clone(), ops.clone()),
        )
      }
      Applied::RemoveContent(path) => Effect::RemoveContent(path.clone()),
      Applied::MakeDir(path) => Effect::MakeDir(path.clone()),
      Applied::RemoveDir(path) => Effect::RemoveDir(path.clone()),
      Applied::Mode(path, mode) => Effect::Mode(path.clone(), *mode),
      Applied::Symlink(path, target) => Effect::Symlink(path.clone(), target.clone()),
      Applied::RemoveSymlink(path) => Effect::RemoveSymlink(path.clone()),
      Applied::Hardlink(path, target) => Effect::Hardlink(path.clone(), target.clone()),
      Applied::RemoveHardlink(path) => Effect::RemoveHardlink(path.clone()),
      Applied::SetXattr(path, name, value) => {
        Effect::SetXattr(path.clone(), name.clone(), value.clone())
      }
      Applied::RemoveXattr(path, name) => Effect::RemoveXattr(path.clone(), name.clone()),
      Applied::CopyTree { dst, from, version } => {
        Effect::CopyTree(dst.clone(), from.clone(), *version)
      }
    }
  }

  /// Keeps a rejected result within the cache's byte budget: entries are evicted oldest-first until
  /// this one fits; one that cannot fit even an empty cache is not retained. Every eviction and every
  /// unretained result is counted (`rejected_evicted`), and the running byte total moves with each.
  fn retain_rejected(&mut self, id: [u8; 32], windows: &[ConflictWindow]) {
    let needed = rejected_entry_bytes(windows);
    if needed > self.rejected_budget {
      self.rejected_evicted = self.rejected_evicted.saturating_add(1);
      return;
    }
    while self.rejected_bytes.saturating_add(needed) > self.rejected_budget {
      let Some(oldest) = self.rejected_order.pop_front() else {
        break;
      };
      if let Some(evicted) = self.rejected.remove(&oldest) {
        self.rejected_bytes = self
          .rejected_bytes
          .saturating_sub(rejected_entry_bytes(&evicted));
        self.rejected_evicted = self.rejected_evicted.saturating_add(1);
      }
    }
    self.rejected.insert(id, windows.to_vec());
    self.rejected_order.push_back(id);
    self.rejected_bytes = self.rejected_bytes.saturating_add(needed);
  }

  /// Sets the rejected-result cache's byte budget (the service derives it from the merge budget of
  /// §4.2), evicting oldest-first whatever no longer fits. Zero keeps no rejected result.
  pub fn set_rejected_budget(&mut self, bytes: usize) {
    self.rejected_budget = bytes;
    while self.rejected_bytes > self.rejected_budget {
      let Some(oldest) = self.rejected_order.pop_front() else {
        break;
      };
      if let Some(evicted) = self.rejected.remove(&oldest) {
        self.rejected_bytes = self
          .rejected_bytes
          .saturating_sub(rejected_entry_bytes(&evicted));
        self.rejected_evicted = self.rejected_evicted.saturating_add(1);
      }
    }
  }

  /// Drops one rejected result from the cache (the service does this when the shard's budget cannot
  /// cover its retention: the verdict was still returned; a retry is judged again), returning the
  /// bytes released and counting it as evicted. Zero when the identity was not retained.
  pub fn forget_rejected(&mut self, id: &[u8; 32]) -> usize {
    let Some(windows) = self.rejected.remove(id) else {
      return 0;
    };
    self.rejected_order.retain(|kept| kept != id);
    let released = rejected_entry_bytes(&windows);
    self.rejected_bytes = self.rejected_bytes.saturating_sub(released);
    self.rejected_evicted = self.rejected_evicted.saturating_add(1);
    released
  }

  /// The rejected-result cache: entries held, bytes held, and results evicted or never retained.
  pub fn rejected_cache(&self) -> (usize, usize, u64) {
    (
      self.rejected.len(),
      self.rejected_bytes,
      self.rejected_evicted,
    )
  }

  /// What the change index grows by when `inc` commits (§4.2 all-cost admission): each path its ops name, recorded at
  /// the new version and as its last change, and the hard links naming any of them that is a special. Secured before
  /// the verdict with [`Green::history_reservation`]; the settle after it trues the charge up to what the index holds,
  /// crediting a path already recorded and charging the paths a directory's move derives for its children.
  /// Nothing for an increment already accepted.
  pub fn changes_reservation(&self, inc: &Increment) -> usize {
    if self.accepted.contains_key(&inc.id) {
      return 0;
    }
    let mut paths: BTreeSet<&str> = BTreeSet::new();
    for op in &inc.doc.ops {
      paths.extend(inc.doc.paths.path(op.path));
      if op.kind == OpKind::Rename {
        paths.extend(inc.doc.paths.path(op.src));
      }
    }
    let links = paths
      .iter()
      .filter(|path| self.was_special(path))
      .filter_map(|path| self.links_to.get(*path))
      .flatten()
      .map(String::as_str);
    let indexed: usize = paths
      .iter()
      .copied()
      .chain(links)
      .map(|path| change_bytes(path).saturating_mul(2))
      .sum();
    indexed.saturating_add(Green::copy_records_reservation(inc))
  }

  /// The bytes the copies `inc` declares add to the change index's charge: one record each (A-586).
  fn copy_records_reservation(inc: &Increment) -> usize {
    // Each index is 64 bits (A-621); one the table does not hold names nothing.
    let name = |index: u64| inc.doc.paths.path(index).unwrap_or_default();
    inc
      .doc
      .ops
      .iter()
      .filter(|op| op.kind == OpKind::Copy)
      .map(|op| copy::appearance_bytes(name(op.path), name(op.src)))
      .fold(0usize, usize::saturating_add)
  }

  /// The most this increment can add to the retained history ([`Green::retention_charge`]), charged before the
  /// verdict (A-16): what the history would gain if every op were applied. Per path (W2: the history shares what a
  /// version kept, `crate::history`):
  /// - a content edit gains the bytes it adds, the bytes it deletes or truncates away (they leave the current value
  ///   but stay in the history), and its piece list; or, when the list would outweigh the value and it is stored
  ///   whole, the old value;
  /// - a path replaced whole (created over, renamed onto, removed, or renamed away) keeps its old value and its list
  ///   (a new value, whole, costs no more than it holds);
  /// - an IPC node replaced or removed keeps the node.
  ///
  /// A post-state shorter than its ops declare is stored whole after the verdict (`crate::history::Value::spliced`);
  /// the settle after it charges what that costs.
  pub fn history_reservation(&self, inc: &Increment) -> usize {
    if self.accepted.contains_key(&inc.id) {
      return 0;
    }
    let mut edits: BTreeMap<&str, Vec<&Op>> = BTreeMap::new();
    let mut replaced: BTreeSet<&str> = BTreeSet::new();
    for op in &inc.doc.ops {
      let Some(path) = inc.doc.paths.path(op.path) else {
        continue;
      };
      if is_content(op.kind) {
        edits.entry(path).or_default().push(op);
      } else if matches!(
        op.kind,
        OpKind::Create | OpKind::Unlink | OpKind::Rename | OpKind::Mknod
      ) {
        replaced.insert(path);
      }
      if op.kind == OpKind::Rename
        && let Some(source) = inc.doc.paths.path(op.src)
      {
        replaced.insert(source);
      }
    }
    let content: usize = edits
      .iter()
      .filter(|(path, _)| !replaced.contains(*path))
      .map(|(path, ops)| self.edit_reservation(path, ops))
      .sum();
    replaced.into_iter().fold(content, |total, path| {
      total
        .saturating_add(self.latest_bytes(path))
        .saturating_add(if self.specials.contains_key(path) {
          SPECIAL_NODE_BYTES
        } else {
          0
        })
    })
  }

  /// [`Green::history_reservation`] for one path's content ops. The history gains the new value's chunk and list
  /// less what the current value (bytes and list) grew by: the overwritten and the deleted or truncated bytes, and the
  /// old value's list (the bytes an insert or extension adds are current). Stored whole, the old value and its list.
  fn edit_reservation(&self, path: &str, ops: &[&Op]) -> usize {
    let old = self.content.get(path).map_or(0, <[u8]>::len);
    let old_len = u64::try_from(old).unwrap_or(u64::MAX);
    let (overwritten, removed, inserted) =
      ops
        .iter()
        .fold((0u64, 0u64, 0u64), |(over, removed, ins), op| {
          match op.kind {
            OpKind::Overwrite => (over.saturating_add(op.len), removed, ins),
            OpKind::Insert | OpKind::Extend => (over, removed, ins.saturating_add(op.len)),
            OpKind::Delete => (over, removed.saturating_add(op.len), ins),
            OpKind::Truncate => (
              over,
              removed.saturating_add(old_len.saturating_sub(op.at)),
              ins,
            ),
            _ => (over, removed, ins),
          }
        });
    let to_usize = |bytes: u64| usize::try_from(bytes).unwrap_or(usize::MAX);
    let latest = self.latest_bytes(path);
    let old_list = latest.saturating_sub(old);
    let spliced = to_usize(overwritten)
      .saturating_add(to_usize(removed))
      .saturating_add(old_list);
    // Stored whole when the new list would outweigh the new value: at most the old pieces, each op splitting one
    // and adding one.
    let list = old_list.saturating_add(
      ops
        .len()
        .saturating_mul(2)
        .saturating_add(1)
        .saturating_mul(crate::history::PIECE_BYTES),
    );
    let next = old
      .saturating_sub(to_usize(removed))
      .saturating_add(to_usize(inserted));
    if list > next {
      latest.max(spliced)
    } else {
      spliced
    }
  }

  /// What `path`'s latest content value counts among the current values: its bytes and its list (none for no value).
  /// A path with no history has none yet; its first change records its bytes whole (`seed_content`, A-534), and that
  /// is what it counts.
  fn latest_bytes(&self, path: &str) -> usize {
    let history = self.content_history.get(path);
    // A path holding a copy's value (A-586) records it whole at its first change too.
    let covered = self.covered_since(path, last_version(history)).is_some();
    match history.filter(|_| !covered) {
      Some(history) => history
        .last()
        .and_then(|(_, value)| value.as_ref())
        .map_or(0, current_bytes),
      None => self
        .content
        .get(path)
        .map_or(0, |bytes| crate::history::Value::whole_cost(bytes.len())),
    }
  }

  /// What the green charges as retention (AUD-16): the content and IPC histories' bytes and the
  /// rejected-result cache's, both running totals, so the settle after every submit costs nothing that
  /// follows the green's size. [`Green::retained_bytes`] also sums the current files, a report's figure.
  pub fn retention_charge(&self) -> usize {
    self
      .history_bytes()
      .saturating_add(self.changes_bytes)
      .saturating_add(self.rejected_bytes)
  }

  /// What the green holds in memory beyond its durable chain ([`RetainedBytes`]): the history and
  /// rejected figures read off running totals; the current files' figure is a walk of them.
  pub fn retained_bytes(&self) -> RetainedBytes {
    RetainedBytes {
      content: self
        .content
        .values()
        .map(<[u8]>::len)
        .sum::<usize>()
        .saturating_add(self.specials.len().saturating_mul(SPECIAL_NODE_BYTES)),
      history: self.history_bytes(),
      changes: self.changes_bytes,
      rejected: self.rejected_bytes,
    }
  }

  /// The content and IPC histories' retained bytes **recounted** from the histories themselves — the oracle
  /// the running totals are checked against (balanced accounting): the distinct chunks the content values name and
  /// their piece lists, less the current values, and every earlier IPC node.
  pub fn history_bytes_recounted(&self) -> usize {
    let values = || {
      self
        .content_history
        .values()
        .flat_map(|history| history.iter().filter_map(|(_, value)| value.as_ref()))
    };
    let current: usize = self
      .content_history
      .values()
      .filter_map(|history| history.last())
      .filter_map(|(_, value)| value.as_ref())
      .map(current_bytes)
      .sum();
    self
      .chunks
      .referenced_bytes(values())
      .saturating_add(values().map(crate::history::Value::list_bytes).sum())
      .saturating_sub(current)
      .saturating_add(self.special_history_recounted())
  }

  /// The IPC history's earlier nodes, recounted: every node but each path's latest.
  fn special_history_recounted(&self) -> usize {
    self
      .special_history
      .values()
      .map(|history| {
        history
          .iter()
          .rev()
          .skip(1)
          .filter(|(_, node)| node.is_some())
          .count()
          .saturating_mul(SPECIAL_NODE_BYTES)
      })
      .sum()
  }

  /// The version below which the histories are folded.
  pub fn folded_below(&self) -> u64 {
    self.folded_below
  }

  /// Folds the histories **oldest-first, only as far as the retention budget needs** (§4.16 "delta
  /// retention before folding … capped by the delta memory budget"; AUD-16): while the retained
  /// history exceeds `budget` bytes, the floor rises one version at a time — never past
  /// `reachable_floor`, the oldest version a live reader still names — folding what lies below it
  /// ([`fold_history_before`](Self::fold_history_before)). Under an ample budget nothing folds, so a
  /// reader may still re-pin any earlier version; under pressure the oldest unreachable history goes
  /// first, and a version below the floor is thereafter refused rather than reconstructed wrongly.
  /// Returns the bytes released. A reachable floor at or below the current one is a no-op.
  pub fn fold_history_to_budget(&mut self, reachable_floor: u64, budget: usize) -> usize {
    let mut released = 0usize;
    while self.history_bytes().saturating_add(self.changes_bytes) > budget
      && self.folded_below < reachable_floor
    {
      let next = self.folded_below.saturating_add(1);
      released = released.saturating_add(self.fold_history_before(next));
    }
    released
  }

  /// **Folds** every history below `floor` (§4.16 "delta retention before folding"; AUD-16): of the
  /// entries recorded before `floor`, only the last — the value in effect at `floor` — survives, so
  /// `content_at`, `base_at` and `changed_between` answer exactly as before for every version at or
  /// above the floor, and a version below it is no longer reconstructible. The service calls this
  /// with the oldest version a live reader can still name (a work's base, a pinned attachment), so
  /// nothing reachable is lost, and a file rewritten many times by small edits no longer keeps a
  /// full copy per edit past what any reader needs. Returns the content and IPC metadata bytes released. Idempotent;
  /// a floor below the current one is a no-op.
  pub fn fold_history_before(&mut self, floor: u64) -> usize {
    if floor <= self.folded_below {
      return 0;
    }
    // The identity reads its keys' values at the version it was last brought to, so it is brought to the head
    // before the history below the floor goes (A-202).
    if self.identity_version < floor {
      self.fold_identity();
    }
    let previous = self.folded_below;
    self.folded_below = floor;
    let before = self.history_bytes().saturating_add(self.changes_bytes);
    self.fold_changes_before(previous, floor);
    self.fold_content_before(floor);
    self.fold_specials_before(floor);
    for history in self.mode_history.values_mut() {
      fold_entries(history, floor);
    }
    for history in self.symlink_history.values_mut() {
      fold_entries(history, floor);
    }
    for history in self.hardlink_history.values_mut() {
      fold_entries(history, floor);
    }
    for history in self.xattr_history.values_mut() {
      fold_entries(history, floor);
    }
    for history in self.dir_history.values_mut() {
      fold_entries(history, floor);
    }
    before.saturating_sub(self.history_bytes().saturating_add(self.changes_bytes))
  }

  /// Folds the content history below `floor`: the latest value is never folded, so the current values stand, and each
  /// dropped value releases its chunks.
  fn fold_content_before(&mut self, floor: u64) {
    let mut dropped = Vec::new();
    for history in self.content_history.values_mut() {
      dropped.extend(fold_entries(history, floor));
    }
    for (_, value) in dropped {
      self.release_content(value);
    }
  }

  /// Folds the IPC history below `floor`, its earlier nodes leaving the running total.
  fn fold_specials_before(&mut self, floor: u64) {
    let mut specials = 0usize;
    for history in self.special_history.values_mut() {
      specials = specials.saturating_add(
        fold_entries(history, floor)
          .iter()
          .filter(|(_, node)| node.is_some())
          .count(),
      );
    }
    self.special_history_bytes = self
      .special_history_bytes
      .saturating_sub(specials.saturating_mul(SPECIAL_NODE_BYTES));
  }

  /// Rebases a work's increment onto the green's head (§4.16 "Rebase, the only corrective path"):
  /// runs the same verdict `submit` would, but commits nothing to the green — instead, when every
  /// operation maps cleanly, it returns the work's new base (the head), its full rebased content
  /// (the head's files with the work's mapped edits re-applied), and its journal restated in head
  /// coordinates, so the agent can keep editing on a fresh base and submit without further mapping.
  /// A conflict returns the windows and changes nothing; the agent resolves each window and rebases
  /// again. The green — content, deltas and counters — is left exactly as it was.
  pub fn rebase(&mut self, inc: &Increment) -> Rebased {
    if inc.base < self.deltas_from {
      return Rebased::BaseUnavailable {
        base: inc.base,
        floor: self.deltas_from,
      };
    }
    let to = self.head();
    let resolved = Green::resolve(inc);
    // A journal has no copy to restate one as (A-586; owed with the deriver's copy): each copy is named, typed, and the
    // green is not judged.
    if !resolved.copies.is_empty() {
      return Rebased::Conflict {
        windows: resolved
          .copies
          .keys()
          .map(|dst| Green::window(dst, MergeConflictClass::TypeChanged))
          .collect(),
      };
    }
    // The verdict shares `submit`'s decision but must not perturb the green; the only field `decide`
    // writes is the fast-path counter (through `merge_content`), which a rebase restores.
    let fast_path_before = self.fast_path_hits;
    let decision = self.decide(inc, &resolved);
    self.fast_path_hits = fast_path_before;
    match decision {
      Err(windows) => Rebased::Conflict { windows },
      Ok(effects) => self.rebased_from(effects, to),
    }
  }

  /// Restates the accepted effects of a clean rebase as the work's changes over the head and its
  /// journal: each content effect's file, kept only where it ends different from the head's; the
  /// journal is the effects as declared operations in head coordinates (a content op keeps its mapped
  /// range, a file new to the head is created first). The green is not touched.
  fn rebased_from(&self, effects: Vec<Effect>, to: u64) -> Rebased {
    use crate::increment::VolumeOp;
    let mut changed: BTreeMap<String, Option<Vec<u8>>> = BTreeMap::new();
    let mut journal: Vec<VolumeOp> = Vec::new();
    // The sources of the renames restated: each one's removal is the rename's, not an unlink of its own.
    let mut moved_from: BTreeSet<String> = BTreeSet::new();
    for effect in effects {
      match effect {
        Effect::Special(path, node) => {
          if self.specials.contains_key(&path) {
            journal.push(VolumeOp::Unlink { path: path.clone() });
          }
          let mode = self.mode(&path).unwrap_or(0);
          journal.push(VolumeOp::Mknod { path, node, mode });
        }
        Effect::RemoveSpecial(path) => journal.push(VolumeOp::Unlink { path }),
        // A rename is restated as the rename it is, in head coordinates, as the composer states one (one `Rename`,
        // replacing whatever the head holds there): its whole-file insert stands for bytes supplied by the move, and
        // replayed over a head that holds the destination it would add them to that file's own.
        Effect::SetContent(path, bytes, _, ContentSource::Copied(from, edits)) => {
          journal.push(VolumeOp::Rename {
            from: from.clone(),
            to: path.clone(),
          });
          // The edits the work made after the rename follow it, over the bytes it moved (A-547).
          let moved_len = self
            .content
            .get(&from)
            .map_or(0, |moved| moved.len() as u64);
          journal.extend(content_volume_ops(&path, &edits, moved_len));
          moved_from.insert(from);
          changed.insert(path, Some(bytes));
        }
        Effect::SetContent(path, bytes, ops, _) => {
          if !self.content.contains_key(&path) {
            journal.push(VolumeOp::Create { path: path.clone() });
          }
          let head_len = self.content.get(&path).map_or(0, |head| head.len() as u64);
          journal.extend(content_volume_ops(&path, &ops, head_len));
          changed.insert(path, Some(bytes));
        }
        Effect::RemoveContent(path) => {
          if !moved_from.remove(&path) {
            journal.push(VolumeOp::Unlink { path: path.clone() });
          }
          changed.insert(path, None);
        }
        Effect::MakeDir(path) => journal.push(VolumeOp::Mkdir { path }),
        Effect::RemoveDir(path) => journal.push(VolumeOp::Rmdir { path }),
        Effect::Mode(path, mode) => journal.push(VolumeOp::SetMode { path, mode }),
        Effect::Symlink(path, target) => journal.push(VolumeOp::Symlink { path, target }),
        Effect::RemoveSymlink(path) | Effect::RemoveHardlink(path) => {
          journal.push(VolumeOp::Unlink { path });
        }
        Effect::Hardlink(path, target) => journal.push(VolumeOp::Link { path, target }),
        Effect::SetXattr(path, name, value) => {
          journal.push(VolumeOp::SetXattr { path, name, value });
        }
        Effect::RemoveXattr(path, name) => journal.push(VolumeOp::RemoveXattr { path, name }),
        // `Green::rebase` answers a work holding a copy before it restates anything (A-586).
        Effect::CopyTree(..) => {}
      }
    }
    // Only what ends different from the head is the work's own: bytes equal to the head's, and the
    // removal of a file the head does not hold, are no change.
    changed.retain(|path, after| after.as_deref() != self.content.get(path));
    Rebased::Rebased {
      version: to,
      changed,
      journal,
    }
  }
}

/// Restates a file's mapped content ops as declared operations, in head coordinates. The ops are one pass over the
/// head's `head_len` bytes ([`apply`]: each at its offset in the head, in order), while a journal is replayed one
/// operation at a time over the file as the operations before it left it. So each op is restated where [`apply`] stands
/// when it reaches it: past the output so far, with the head left from its cursor still after it. Restated at their
/// head offsets as they stand, an insert followed by a delete at the same offset deleted the inserted byte. The
/// post-state offset is re-derived when the rebased journal is next composed, so it is dropped here; an op of another
/// kind restates as nothing.
fn content_volume_ops(path: &str, ops: &[Op], head_len: u64) -> Vec<crate::increment::VolumeOp> {
  use crate::increment::VolumeOp;
  let mut restated = Vec::with_capacity(ops.len());
  // `pos`: the cursor in the head; `out`: the length [`apply`] has written, where the next op stands in the journal.
  let (mut pos, mut out) = (0u64, 0u64);
  for op in ops {
    if op.at > pos {
      out = out.saturating_add(op.at.min(head_len).saturating_sub(pos));
      pos = op.at;
    }
    let path = path.to_owned();
    let (at, len) = (out, op.len);
    match op.kind {
      OpKind::Overwrite => {
        restated.push(VolumeOp::Overwrite { path, at, len });
        out = out.saturating_add(len);
        pos = pos.saturating_add(len);
      }
      OpKind::Insert => {
        restated.push(VolumeOp::Insert { path, at, len });
        out = out.saturating_add(len);
      }
      OpKind::Extend => {
        restated.push(VolumeOp::Extend { path, at, len });
        out = out.saturating_add(len);
      }
      OpKind::Delete => {
        restated.push(VolumeOp::Delete {
          path,
          at,
          len: len.min(head_len.saturating_sub(pos)),
        });
        pos = pos.saturating_add(len);
      }
      // [`apply`] keeps nothing of the head past a truncate: the file is cut where its output stands.
      OpKind::Truncate => {
        restated.push(VolumeOp::Truncate { path, len: out });
        pos = head_len;
      }
      _ => {}
    }
  }
  restated
}

/// Files one per-dimension merge result into the accepted effects or the conflict windows.
fn record(
  effects: &mut Vec<Effect>,
  windows: &mut Vec<ConflictWindow>,
  result: Result<Option<Effect>, ConflictWindow>,
) {
  match result {
    Ok(Some(effect)) => effects.push(effect),
    Ok(None) => {}
    Err(window) => windows.push(window),
  }
}

/// A file's length, as an op's coordinates count it.
fn len_of(bytes: &[u8]) -> u64 {
  u64::try_from(bytes.len()).unwrap_or(u64::MAX)
}

/// An op mapped to the head (`head_len` bytes), in the head's canonical form. A truncate removes the bytes it saw, its
/// span `[at, at + len)`, a run of its base's bytes as the interval algebra composes a removal (`slates_vfs::algebra`),
/// and is judged by its span with the edge rule as every removal is. Where the head grew past that span after the base
/// (an append at the end the work never saw) it no longer runs to the file's end, so it is the delete of its span,
/// which `apply`, the version's delta and every reader of the delta take as exactly those bytes: cut to the head's end,
/// it removed the append (A-612). Any other op, and a truncate that still runs to the end, is unchanged.
fn at_head(op: Op, head_len: u64) -> Op {
  if op.kind == OpKind::Truncate && op.at.saturating_add(op.len) < head_len {
    return Op {
      kind: OpKind::Delete,
      ..op
    };
  }
  op
}

/// An `Insert` op that adds a whole file's bytes at offset zero from post-state offset zero (used
/// for a create or a rename's destination, whose bytes are supplied directly, not by `src`).
fn insert_whole(len: u64) -> Op {
  Op {
    kind: OpKind::Insert,
    flags: 0,
    path: 0,
    at: 0,
    len,
    src: 0,
  }
}

/// `post_state[src .. src + len]`, clamped to bounds.
fn post_slice(post_state: &[u8], src: u64, len: u64) -> Vec<u8> {
  let start = usize::try_from(src)
    .unwrap_or(usize::MAX)
    .min(post_state.len());
  let end = start
    .saturating_add(usize::try_from(len).unwrap_or(0))
    .min(post_state.len());
  post_state.get(start..end).unwrap_or_default().to_vec()
}

/// Read an IPC payload exactly. A saturated or truncated range must never become valid metadata.
fn special_payload(
  doc: &OpsDoc,
  post: &[u8],
  op: &Op,
) -> Result<SpecialNode, crate::ops_doc::DocDecodeError> {
  use crate::ops_doc::DocDecodeError;
  if doc.paths.path(op.path).is_none() {
    return Err(DocDecodeError::BadPath);
  }
  let start = usize::try_from(op.src).map_err(|_| DocDecodeError::Truncated)?;
  let len = usize::try_from(op.len).map_err(|_| DocDecodeError::Truncated)?;
  let end = start.checked_add(len).ok_or(DocDecodeError::Truncated)?;
  SpecialNode::decode(post.get(start..end).ok_or(DocDecodeError::Truncated)?)
}

mod branch;
pub use branch::{Lent, Lineage, LineageGreen, LineageRefused, Release};

mod copy;

#[cfg(test)]
mod copy_tests;

mod reanchor;
pub use reanchor::{ReanchorRefused, Reanchored};

#[cfg(test)]
mod node_exclusive_tests;

#[cfg(test)]
mod unseen_bytes_tests;

#[cfg(test)]
mod replaced_node_tests;

#[cfg(test)]
mod tests {
  use super::*;

  /// An increment under construction: its ops document and the post-state its content ops add from.
  #[derive(Default)]
  struct Steps {
    doc: OpsDoc,
    post: Vec<u8>,
  }

  impl Steps {
    /// `bytes` inserted into `path` at `at`.
    fn insert(mut self, path: &str, at: u64, bytes: &[u8]) -> Steps {
      let path = self.doc.paths.intern(path);
      let src = self.post.len() as u64;
      self.post.extend_from_slice(bytes);
      self.doc.ops.push(Op {
        kind: OpKind::Insert,
        flags: 0,
        path,
        at,
        len: bytes.len() as u64,
        src,
      });
      self
    }

    /// `path` created holding `bytes`.
    fn create(mut self, path: &str, bytes: &[u8]) -> Steps {
      let index = self.doc.paths.intern(path);
      self.doc.ops.push(Op {
        kind: OpKind::Create,
        flags: 0,
        path: index,
        at: 0,
        len: 0,
        src: u64::MAX,
      });
      self.insert(path, 0, bytes)
    }

    /// `from` renamed to `to` (the op is keyed at its destination, its source the `src` path index).
    fn rename(mut self, from: &str, to: &str) -> Steps {
      let to = self.doc.paths.intern(to);
      let from = self.doc.paths.intern(from);
      self.doc.ops.push(Op {
        kind: OpKind::Rename,
        flags: 0,
        path: to,
        at: 0,
        len: 0,
        src: u64::from(from),
      });
      self
    }

    /// The increment `id` based on version `base`.
    fn at(self, id: u8, base: u64) -> Increment {
      Increment {
        id: [id; 32],
        base,
        doc: self.doc,
        post_state: self.post,
        evidence: Vec::new(),
      }
    }
  }

  /// A-547 (GAPS "an edit after a rename is accepted and lost", 2026-11-09; §4.16 "Composition at seal"). Do: make a
  /// green holding files `a` and `b`; submit a work based on that version that renames `a` over `b` and then appends
  /// `XY` at `b`'s end; replay the version's chain entry, encoded and decoded, on a green that holds the version before
  /// it. Expect: the work accepted, and `b` holding `a`'s bytes and the edit, `0123456789XY`, with `a` gone, live and
  /// replayed alike. Before, the verdict copied `a`'s head bytes to `b` and dropped `b`'s own ops, so the submit was
  /// accepted and the edit lost.
  #[test]
  fn an_edit_after_a_rename_over_a_file_is_merged_with_the_renamed_bytes() {
    let first = Steps::default()
      .create("a", b"0123456789")
      .create("b", b"bbbbbbbbbbbbbbbb")
      .at(1, 0);
    let mut green = Green::new();
    let mut replayed = Green::new();
    assert_eq!(green.submit(&first), Outcome::Accepted { version: 1 });
    assert_eq!(replayed.submit(&first), Outcome::Accepted { version: 1 });
    let edited = Steps::default()
      .rename("a", "b")
      .insert("b", 10, b"XY")
      .at(2, 1);
    let (outcome, applied) = green.submit_resolved(&edited);
    assert_eq!(outcome, Outcome::Accepted { version: 2 });
    assert_eq!(
      green.content("b"),
      Some(b"0123456789XY".as_slice()),
      "the edit after the rename is lost"
    );
    assert_eq!(green.content("a"), None, "the rename's source is gone");
    let applied = applied.expect("an accepted version's effects");
    let entry = crate::chain_entry::encode(&edited.encode(), &edited, &applied, None, None, None);
    let decoded = crate::chain_entry::decode(&entry).expect("the entry decodes");
    assert_eq!(
      decoded.applied, applied,
      "the entry carries what the verdict resolved"
    );
    assert_eq!(
      replayed.apply_recorded(&decoded.increment, &decoded.applied),
      2
    );
    assert_eq!(
      replayed.content("b"),
      Some(b"0123456789XY".as_slice()),
      "the replayed version lost the edit"
    );
  }
}
