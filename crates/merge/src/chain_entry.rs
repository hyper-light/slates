//! A green's durable chain entry (§4.16 "Commit", §4.8 recovery, A-257): the increment as the service accepted it, and
//! what its verdict resolved it to, so recovery applies what the version changed and never judges it again.
//!
//! **Why.** A recovered green's chain was replayed by submitting every increment again. A verdict reads the green at the
//! increment's base (the deltas since, the content's length and, for an overlapping length change, its bytes), and a
//! checkpoint (W3) keeps none of that below its version. An increment accepted late over an old base stays in the chain
//! above a checkpoint that passed its base, so its replay was refused `BaseUnavailable` and the whole green fenced: a
//! decision inside replay, taken from state a checkpoint restores differently. The project's rule for a replayed log
//! (`crates/db/src/partition.rs`, guard-then-apply) is that a decision belongs before the append; the verdict is that
//! decision, and this entry records its result. The position of a checkpoint no longer matters to replay.
//!
//! **What it holds.** The increment's own encoding, byte for byte, since a merge record's inputs are those bytes
//! (`merge_service::inputs_of`) and a holder and a successor judge them afresh (§4.16 "Apply on holders"). Then each
//! [`Applied`] effect: content by reference to the increment's post-state (the mapped ops of a splice, the declared ops
//! of a create, the source path of a rename), a copied tree by its two paths (A-586), so no file's bytes are recorded
//! twice; every other dimension by value.
//! Then the completion key of the submit that made the version, where one did, with the work it was submitted from when
//! its client was on another node (A-255: its merge record carries both, so a green's successor answers the submit's
//! resend after a takeover and keeps the work's route only while it holds such a completion, A-288; and a restarted owner
//! rebuilds that record byte for byte from this entry), the green's head identity at this version where one was
//! computed, how the submit's signature was judged where it carried one (signed work, A-321: the version's provenance,
//! which replay hands the green's accepted record, [`crate::engine::Green::record_signature`], so a checkpoint keeps it
//! once the entry is folded), and a digest of everything before it.
//!
//! **The encoding** follows the origin's and the checkpoint's: magic and version, little-endian, every count checked
//! against the bytes that remain before anything is allocated, any malformation refused typed ([`DocDecodeError`]).

use crate::engine::Increment;
use crate::ops_doc::{DocDecodeError, Op, OpKind, Reader};
use crate::origin::{put_bytes, put_count, reader_bytes, take_str};
use crate::special::SpecialNode;

/// Format: the chain entry encoding's magic, "GCHN" little-endian, so bytes of another kind (a bare increment written by
/// an earlier layout) are refused at the first word rather than misread.
const MAGIC: u32 = u32::from_le_bytes(*b"GCHN");
/// Format: the chain entry encoding's version; a decoder refuses any other. 2 since A-255 added the completion key, 3
/// since A-288 added the submitting work beside it, 4 since A-321 added the signature's verdict.
const VERSION: u32 = 4;
/// Format: a head identity and the entry's digest, each a BLAKE3 hash.
const HASH_BYTES: usize = 32;
/// Format: the byte that marks a head identity, a completion key or a submitting work present.
const PRESENT: u8 = 1;
/// Format: the byte that marks a head identity, a completion key or a submitting work absent.
const ABSENT: u8 = 0;
/// Format: an op's fixed record, as [`crate::ops_doc::OpsDoc::encode`] writes it.
const OP_BYTES: usize = 28;
/// Format: a work volume's id.
const WORK_BYTES: usize = 16;
/// Format: a completion key and its work: the origin host, the client, the sequence, and the submitting work's marker
/// and id.
const COMPLETION_BYTES: usize = 8 + 4 + 4 + 1 + WORK_BYTES;
/// Format: a signature's verdict at its largest: its marker, the verdict's kind and detail, and the signer's fingerprint.
const SIGNATURE_BYTES: usize = 1 + 1 + 1 + HASH_BYTES;
/// Format: the fixed bytes of an entry around its parts: magic, version, the increment's length, the effect count, the
/// completion's marker and key, the identity's marker and hash, the signature's section, and the digest.
const ENTRY_OVERHEAD_BYTES: usize =
  4 + 4 + 4 + 4 + 1 + COMPLETION_BYTES + 1 + HASH_BYTES + SIGNATURE_BYTES + HASH_BYTES;
/// Format: an effect's largest fixed part: its tag and three length prefixes or counts.
const EFFECT_OVERHEAD_BYTES: usize = 1 + 3 * 4;
/// Format: the smallest an effect can encode to: its tag and one length prefix.
const EFFECT_MIN_BYTES: usize = 1 + 4;
/// Format: the effects one declared op can resolve to, at most: a rename's write and its source's removal.
const EFFECTS_PER_OP: usize = 2;
/// Format: how many times an effect can name the strings of the op it came from: a path and a link's target or xattr's
/// name.
const STRINGS_PER_EFFECT: usize = 2;

/// One thing an accepted version changed, resolved by its verdict into the head's coordinates (module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Applied {
  /// Set an IPC inode.
  Special(String, SpecialNode),
  /// Remove an IPC inode.
  RemoveSpecial(String),
  /// Splice these ops, in the head's coordinates, into the file.
  Spliced(String, Vec<Op>),
  /// Create the file from these declared ops over nothing.
  Created(String, Vec<Op>),
  /// Set `dst` to what `from` holds at this version, with `ops` spliced into those bytes (A-547).
  Copied {
    /// The file written.
    dst: String,
    /// The file it takes its bytes from.
    from: String,
    /// The edits the work made to `dst` after the rename, in the coordinates of the bytes it takes: the work's own ops
    /// mapped through what changed `from` since the work's base. Empty for a rename alone, which encodes as before.
    ops: Vec<Op>,
  },
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
  /// Remove a symlink.
  RemoveSymlink(String),
  /// Create or retarget a hard link.
  Hardlink(String, String),
  /// Remove a hard link's name.
  RemoveHardlink(String),
  /// Set an xattr: path, name, value.
  SetXattr(String, String, Vec<u8>),
  /// Remove an xattr: path, name.
  RemoveXattr(String, String),
  /// Copy `from`'s tree to `dst` by reference (A-586): what the head holds of it, which the verdict found unchanged
  /// since `version`, the source's version the increment declared. Encoded as its two paths; `version` is read back
  /// from the increment's own `Copy` operation, so the entry costs no more than its bound's two strings.
  CopyTree {
    /// The destination.
    dst: String,
    /// The source.
    from: String,
    /// The version of the source the increment copies.
    version: u64,
  },
}

/// A decoded chain entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainEntry {
  /// The increment.
  pub increment: Increment,
  /// What its verdict resolved it to.
  pub applied: Vec<Applied>,
  /// The completion key of the submit that made this version, where a request made it (module docs).
  pub completion: Option<Completion>,
  /// The green's head identity at this version, where one was computed.
  pub identity: Option<[u8; 32]>,
  /// How the submit's signature was judged, where it carried one (module docs).
  pub signature: Option<Signature>,
}

/// How the signature of the submit that made a version was judged (signed work, A-321): the version's provenance.
/// Opaque here: the merge plane keeps what the server's verifier decided (`slates_server::signed_work`), so a restarted
/// owner answers it as it was judged, never judging again under signers set since.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Signature {
  /// The verdict's kind (verified or invalid), as the server numbers it.
  pub kind: u8,
  /// The verdict's detail (the signer's key type, or why it did not verify), as the server numbers it.
  pub detail: u8,
  /// The signer's key fingerprint (zero for a signature that did not verify).
  pub fingerprint: [u8; 32],
}

/// A submit's completion key (§4.9 RIFL): the host its client is on, the client and the request's sequence, with the
/// work the submit was made from when that host was not the work's owner. Opaque here: the merge plane carries it so the
/// request's completion moves with the green it changed (RIFL §3, A-255), and the green's successor keeps the work's route
/// only while it holds a completion a resend can reach only through that route (A-288).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Completion {
  /// The client's host (`HostId.0`).
  pub origin: u64,
  /// The client.
  pub client: u32,
  /// The request's sequence within the client.
  pub sequence: u32,
  /// The work the submit was made from (its volume id), when the client was on another node than the work's owner: not
  /// part of the key, the route a resend under it takes. `None` for a client of the owner's own node, whose resend the
  /// owner's own records answer.
  pub work: Option<[u8; WORK_BYTES]>,
}

/// Format: an [`Applied`] variant's tag.
mod tag {
  /// Format: the tag of an IPC inode set.
  pub(super) const SPECIAL: u8 = 1;
  /// Format: the tag of an IPC inode removed.
  pub(super) const REMOVE_SPECIAL: u8 = 2;
  /// Format: the tag of a file spliced by mapped ops.
  pub(super) const SPLICED: u8 = 3;
  /// Format: the tag of a file created.
  pub(super) const CREATED: u8 = 4;
  /// Format: the tag of a file taking another's bytes.
  pub(super) const COPIED: u8 = 5;
  /// Format: the tag of a file removed.
  pub(super) const REMOVE_CONTENT: u8 = 6;
  /// Format: the tag of a directory created.
  pub(super) const MAKE_DIR: u8 = 7;
  /// Format: the tag of a directory removed.
  pub(super) const REMOVE_DIR: u8 = 8;
  /// Format: the tag of a mode set.
  pub(super) const MODE: u8 = 9;
  /// Format: the tag of a symlink set.
  pub(super) const SYMLINK: u8 = 10;
  /// Format: the tag of a symlink removed.
  pub(super) const REMOVE_SYMLINK: u8 = 11;
  /// Format: the tag of a hard link set.
  pub(super) const HARDLINK: u8 = 12;
  /// Format: the tag of a hard link removed.
  pub(super) const REMOVE_HARDLINK: u8 = 13;
  /// Format: the tag of an xattr set.
  pub(super) const SET_XATTR: u8 = 14;
  /// Format: the tag of an xattr removed.
  pub(super) const REMOVE_XATTR: u8 = 15;
  /// Format: the tag of a file spliced by exactly the ops it declared.
  pub(super) const SPLICED_AS_DECLARED: u8 = 16;
  /// Format: the tag of a file taking another's bytes with edits spliced into them (A-547): a rename's destination the
  /// work edited after the rename. A rename alone keeps [`COPIED`].
  pub(super) const COPIED_SPLICED: u8 = 17;
  /// Format: the tag of a tree copied by reference (A-586), its source's version the increment's `Copy` operation's.
  pub(super) const COPY_TREE: u8 = 18;
}

/// The largest an entry for `increment` can encode to, whatever its verdict resolves it to: what the service charges
/// the chain before the verdict, so an accepted version is always recorded. Every effect comes from a declared op and
/// names at most two of the document's strings; mapped ops are at most the declared ones; values are in the post-state.
pub fn entry_bound(increment_bytes: usize, increment: &Increment) -> usize {
  let ops = increment.doc.ops.len();
  let longest_string = increment
    .doc
    .paths
    .paths()
    .iter()
    .map(String::len)
    .max()
    .unwrap_or(0);
  let per_effect =
    EFFECT_OVERHEAD_BYTES.saturating_add(longest_string.saturating_mul(STRINGS_PER_EFFECT));
  ENTRY_OVERHEAD_BYTES
    .saturating_add(increment_bytes)
    .saturating_add(
      ops
        .saturating_mul(EFFECTS_PER_OP)
        .saturating_mul(per_effect),
    )
    .saturating_add(ops.saturating_mul(OP_BYTES))
    .saturating_add(increment.post_state.len())
}

/// Encodes a chain entry from the increment's own encoding.
///
/// A splice whose ops are the ones the increment declared for that file (nothing was mapped: the common case, an increment
/// based on the head) is recorded by its path alone.
pub fn encode(
  increment_bytes: &[u8],
  increment: &Increment,
  applied: &[Applied],
  completion: Option<Completion>,
  identity: Option<[u8; 32]>,
  signature: Option<Signature>,
) -> Vec<u8> {
  let mut out = Vec::with_capacity(increment_bytes.len().saturating_add(ENTRY_OVERHEAD_BYTES));
  out.extend_from_slice(&MAGIC.to_le_bytes());
  out.extend_from_slice(&VERSION.to_le_bytes());
  put_bytes(&mut out, increment_bytes);
  put_count(&mut out, applied.len());
  for effect in applied {
    put_applied(&mut out, increment, effect);
  }
  match completion {
    Some(key) => {
      out.push(PRESENT);
      out.extend_from_slice(&key.origin.to_le_bytes());
      out.extend_from_slice(&key.client.to_le_bytes());
      out.extend_from_slice(&key.sequence.to_le_bytes());
      match key.work {
        Some(work) => {
          out.push(PRESENT);
          out.extend_from_slice(&work);
        }
        None => out.push(ABSENT),
      }
    }
    None => out.push(ABSENT),
  }
  match identity {
    Some(hash) => {
      out.push(PRESENT);
      out.extend_from_slice(&hash);
    }
    None => out.push(ABSENT),
  }
  put_signature(&mut out, signature);
  let digest = *blake3::hash(&out).as_bytes();
  out.extend_from_slice(&digest);
  out
}

/// Appends a signature's verdict (module docs): a present marker and the verdict, or an absent marker alone.
pub(crate) fn put_signature(out: &mut Vec<u8>, signature: Option<Signature>) {
  match signature {
    Some(verdict) => {
      out.push(PRESENT);
      out.push(verdict.kind);
      out.push(verdict.detail);
      out.extend_from_slice(&verdict.fingerprint);
    }
    None => out.push(ABSENT),
  }
}

/// Reads what [`put_signature`] wrote.
pub(crate) fn take_signature(reader: &mut Reader<'_>) -> Result<Option<Signature>, DocDecodeError> {
  match reader.u8()? {
    PRESENT => {
      let kind = reader.u8()?;
      let detail = reader.u8()?;
      let fingerprint = <[u8; HASH_BYTES]>::try_from(reader.bytes(HASH_BYTES)?)
        .map_err(|_| DocDecodeError::Truncated)?;
      Ok(Some(Signature {
        kind,
        detail,
        fingerprint,
      }))
    }
    ABSENT => Ok(None),
    _ => Err(DocDecodeError::BadKind),
  }
}

/// The increment's own encoding within an entry, checking the entry's digest: what a merge record's inputs are.
pub fn increment_bytes(entry: &[u8]) -> Result<&[u8], DocDecodeError> {
  let mut reader = checked(entry)?;
  reader_bytes(&mut reader)
}

/// Decodes an entry: typed refusals, never a panic.
pub fn decode(entry: &[u8]) -> Result<ChainEntry, DocDecodeError> {
  let mut reader = checked(entry)?;
  let increment = Increment::decode(reader_bytes(&mut reader)?)?;
  let count = reader.u32()? as usize;
  // An effect is at least its tag and one length prefix, so a wild count is refused before anything is reserved.
  if count > reader.remaining() / EFFECT_MIN_BYTES {
    return Err(DocDecodeError::Truncated);
  }
  let mut applied = Vec::with_capacity(count);
  for _ in 0..count {
    applied.push(take_applied(&mut reader, &increment)?);
  }
  let completion = match reader.u8()? {
    PRESENT => {
      let origin = reader.u64()?;
      let client = reader.u32()?;
      let sequence = reader.u32()?;
      let work = match reader.u8()? {
        PRESENT => Some(
          <[u8; WORK_BYTES]>::try_from(reader.bytes(WORK_BYTES)?)
            .map_err(|_| DocDecodeError::Truncated)?,
        ),
        ABSENT => None,
        _ => return Err(DocDecodeError::BadKind),
      };
      Some(Completion {
        origin,
        client,
        sequence,
        work,
      })
    }
    ABSENT => None,
    _ => return Err(DocDecodeError::BadKind),
  };
  let identity = match reader.u8()? {
    PRESENT => {
      let mut hash = [0u8; HASH_BYTES];
      hash.copy_from_slice(reader.bytes(HASH_BYTES)?);
      Some(hash)
    }
    ABSENT => None,
    _ => return Err(DocDecodeError::BadKind),
  };
  let signature = take_signature(&mut reader)?;
  if !reader.is_empty() {
    return Err(DocDecodeError::TrailingBytes);
  }
  Ok(ChainEntry {
    increment,
    applied,
    completion,
    identity,
    signature,
  })
}

/// A reader over the entry's body past its magic and version, the trailing digest verified and left out.
fn checked(entry: &[u8]) -> Result<Reader<'_>, DocDecodeError> {
  let body_len = entry
    .len()
    .checked_sub(HASH_BYTES)
    .ok_or(DocDecodeError::Truncated)?;
  let (body, digest) = entry
    .split_at_checked(body_len)
    .ok_or(DocDecodeError::Truncated)?;
  let mut reader = Reader::new(body);
  if reader.u32()? != MAGIC {
    return Err(DocDecodeError::BadMagic);
  }
  if reader.u32()? != VERSION {
    return Err(DocDecodeError::BadVersion);
  }
  if blake3::hash(body).as_bytes().as_slice() != digest {
    return Err(DocDecodeError::Truncated);
  }
  Ok(reader)
}

fn put_str(out: &mut Vec<u8>, text: &str) {
  put_bytes(out, text.as_bytes());
}

fn put_ops(out: &mut Vec<u8>, ops: &[Op]) {
  put_count(out, ops.len());
  for op in ops {
    out.push(op.kind.to_wire());
    out.push(op.flags);
    out.extend_from_slice(&op.path.to_le_bytes());
    out.extend_from_slice(&op.at.to_le_bytes());
    out.extend_from_slice(&op.len.to_le_bytes());
    out.extend_from_slice(&op.src.to_le_bytes());
  }
}

fn take_ops(reader: &mut Reader<'_>) -> Result<Vec<Op>, DocDecodeError> {
  let count = reader.u32()? as usize;
  if count > reader.remaining() / OP_BYTES {
    return Err(DocDecodeError::Truncated);
  }
  let mut ops = Vec::with_capacity(count);
  for _ in 0..count {
    let kind = OpKind::from_wire(reader.u8()?).ok_or(DocDecodeError::BadKind)?;
    let flags = reader.u8()?;
    let path = reader.u16()?;
    let at = reader.u64()?;
    let len = reader.u64()?;
    let src = reader.u64()?;
    ops.push(Op {
      kind,
      flags,
      path,
      at,
      len,
      src,
    });
  }
  Ok(ops)
}

fn put_tagged_paths(out: &mut Vec<u8>, tag: u8, paths: &[&str]) {
  out.push(tag);
  for path in paths {
    put_str(out, path);
  }
}

/// The content ops `increment` declares for `path`, in order.
fn declared_ops(increment: &Increment, path: &str) -> Vec<Op> {
  increment
    .doc
    .ops
    .iter()
    .filter(|op| {
      crate::engine::is_content(op.kind) && increment.doc.paths.path(op.path) == Some(path)
    })
    .copied()
    .collect()
}

fn put_applied(out: &mut Vec<u8>, increment: &Increment, applied: &Applied) {
  match applied {
    Applied::Special(path, node) => {
      put_tagged_paths(out, tag::SPECIAL, &[path]);
      put_bytes(out, &node.encode());
    }
    Applied::RemoveSpecial(path) => put_tagged_paths(out, tag::REMOVE_SPECIAL, &[path]),
    Applied::Spliced(path, ops) if *ops == declared_ops(increment, path) => {
      put_tagged_paths(out, tag::SPLICED_AS_DECLARED, &[path]);
    }
    Applied::Spliced(path, ops) => {
      put_tagged_paths(out, tag::SPLICED, &[path]);
      put_ops(out, ops);
    }
    Applied::Created(path, ops) => {
      put_tagged_paths(out, tag::CREATED, &[path]);
      put_ops(out, ops);
    }
    Applied::Copied { dst, from, ops } if ops.is_empty() => {
      put_tagged_paths(out, tag::COPIED, &[dst, from]);
    }
    Applied::Copied { dst, from, ops } => {
      put_tagged_paths(out, tag::COPIED_SPLICED, &[dst, from]);
      put_ops(out, ops);
    }
    Applied::RemoveContent(path) => put_tagged_paths(out, tag::REMOVE_CONTENT, &[path]),
    Applied::MakeDir(path) => put_tagged_paths(out, tag::MAKE_DIR, &[path]),
    Applied::RemoveDir(path) => put_tagged_paths(out, tag::REMOVE_DIR, &[path]),
    Applied::Mode(path, mode) => {
      put_tagged_paths(out, tag::MODE, &[path]);
      out.extend_from_slice(&mode.to_le_bytes());
    }
    Applied::Symlink(path, target) => put_tagged_paths(out, tag::SYMLINK, &[path, target]),
    Applied::RemoveSymlink(path) => put_tagged_paths(out, tag::REMOVE_SYMLINK, &[path]),
    Applied::Hardlink(path, target) => put_tagged_paths(out, tag::HARDLINK, &[path, target]),
    Applied::RemoveHardlink(path) => put_tagged_paths(out, tag::REMOVE_HARDLINK, &[path]),
    Applied::SetXattr(path, name, value) => {
      put_tagged_paths(out, tag::SET_XATTR, &[path, name]);
      put_bytes(out, value);
    }
    Applied::RemoveXattr(path, name) => put_tagged_paths(out, tag::REMOVE_XATTR, &[path, name]),
    Applied::CopyTree { dst, from, .. } => put_tagged_paths(out, tag::COPY_TREE, &[dst, from]),
  }
}

fn take_applied(reader: &mut Reader<'_>, increment: &Increment) -> Result<Applied, DocDecodeError> {
  Ok(match reader.u8()? {
    tag::SPECIAL => {
      let path = take_str(reader)?;
      Applied::Special(path, SpecialNode::decode(reader_bytes(reader)?)?)
    }
    tag::REMOVE_SPECIAL => Applied::RemoveSpecial(take_str(reader)?),
    tag::SPLICED => {
      let path = take_str(reader)?;
      Applied::Spliced(path, take_ops(reader)?)
    }
    tag::SPLICED_AS_DECLARED => {
      let path = take_str(reader)?;
      let ops = declared_ops(increment, &path);
      Applied::Spliced(path, ops)
    }
    tag::CREATED => {
      let path = take_str(reader)?;
      Applied::Created(path, take_ops(reader)?)
    }
    tag::COPIED => {
      let dst = take_str(reader)?;
      let from = take_str(reader)?;
      Applied::Copied {
        dst,
        from,
        ops: Vec::new(),
      }
    }
    tag::COPIED_SPLICED => {
      let dst = take_str(reader)?;
      let from = take_str(reader)?;
      Applied::Copied {
        dst,
        from,
        ops: take_ops(reader)?,
      }
    }
    tag::REMOVE_CONTENT => Applied::RemoveContent(take_str(reader)?),
    tag::MAKE_DIR => Applied::MakeDir(take_str(reader)?),
    tag::REMOVE_DIR => Applied::RemoveDir(take_str(reader)?),
    tag::MODE => {
      let path = take_str(reader)?;
      Applied::Mode(path, reader.u32()?)
    }
    tag::SYMLINK => {
      let path = take_str(reader)?;
      Applied::Symlink(path, take_str(reader)?)
    }
    tag::REMOVE_SYMLINK => Applied::RemoveSymlink(take_str(reader)?),
    tag::HARDLINK => {
      let path = take_str(reader)?;
      Applied::Hardlink(path, take_str(reader)?)
    }
    tag::REMOVE_HARDLINK => Applied::RemoveHardlink(take_str(reader)?),
    tag::SET_XATTR => {
      let path = take_str(reader)?;
      let name = take_str(reader)?;
      Applied::SetXattr(path, name, reader_bytes(reader)?.to_vec())
    }
    tag::REMOVE_XATTR => {
      let path = take_str(reader)?;
      Applied::RemoveXattr(path, take_str(reader)?)
    }
    tag::COPY_TREE => {
      let dst = take_str(reader)?;
      let from = take_str(reader)?;
      let version = declared_copy(increment, &dst, &from).ok_or(DocDecodeError::BadKind)?;
      Applied::CopyTree { dst, from, version }
    }
    _ => return Err(DocDecodeError::BadKind),
  })
}

/// The source version of the `Copy` operation `increment` declares from `from` to `dst` (A-586); `None` when it declares
/// none, which no entry the engine recorded holds.
fn declared_copy(increment: &Increment, dst: &str, from: &str) -> Option<u64> {
  let path = |index: u64| {
    u16::try_from(index)
      .ok()
      .and_then(|index| increment.doc.paths.path(index))
  };
  increment
    .doc
    .ops
    .iter()
    .find(|op| {
      op.kind == OpKind::Copy && path(u64::from(op.path)) == Some(dst) && path(op.src) == Some(from)
    })
    .map(|op| op.at)
}
