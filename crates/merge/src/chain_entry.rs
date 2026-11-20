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

use std::collections::{BTreeMap, BTreeSet};

use crate::engine::Increment;
use crate::ops_doc::{DocDecodeError, Op, OpKind, Reader};
use crate::origin::{put_bytes, put_count, reader_bytes, take_str};
use crate::special::SpecialNode;

/// Format: the chain entry encoding's magic, "GCHN" little-endian, so bytes of another kind (a bare increment written by
/// an earlier layout) are refused at the first word rather than misread.
const MAGIC: u32 = u32::from_le_bytes(*b"GCHN");
/// Format: the chain entry encoding's version; a decoder refuses any other. 2 since A-255 added the completion key, 3
/// since A-288 added the submitting work beside it, 4 since A-321 added the signature's verdict, 5 since A-621 widened
/// an op's path index to 64 bits (its record is the ops document's, version 2).
const VERSION: u32 = 5;
/// Format: a head identity and the entry's digest, each a BLAKE3 hash.
const HASH_BYTES: usize = 32;
/// Format: the byte that marks a head identity, a completion key or a submitting work present.
const PRESENT: u8 = 1;
/// Format: the byte that marks a head identity, a completion key or a submitting work absent.
const ABSENT: u8 = 0;
/// Format: an op's fixed record, as [`crate::ops_doc::OpsDoc::encode`] writes it: one definition, the ops document's.
const OP_BYTES: usize = crate::ops_doc::OP_BYTES;
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
  /// since `version`, the source's version the increment declared, replacing what `dst` holds when `replace` (A-626).
  /// Encoded as its two paths; `version` and `replace` are read back from the increment's own `Copy` operation, so the
  /// entry costs no more than its bound's two strings.
  CopyTree {
    /// The destination.
    dst: String,
    /// The source.
    from: String,
    /// The version of the source the increment copies.
    version: u64,
    /// Whether the copy replaces what the destination holds ([`crate::ops_doc::REPLACE`]).
    replace: bool,
    /// What the copy changes in the head's tree it grafts, when its source changed after `version` (A-626 part 2):
    /// recorded whole, so a replay reads no history.
    fix: Option<Box<CopyFix>>,
  },
}

/// What a copy from a version its source changed after does to the head's tree it grafts, so the copy brings the
/// source's tree as it stood at that version (A-626 part 2: FSFS copies from any committed revision). Recorded in the
/// entry whole, so a replay reads no history, whatever its holder folded or restored. Paths are relative to the source,
/// which is what they are relative to at the destination too.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CopyFix {
  /// A copy made after the version at or above the source replaced its tree: nothing of the head's is grafted.
  pub start_empty: bool,
  /// The trees of copies made inside the source after the version, cut from what is grafted.
  pub cuts: BTreeSet<String>,
  /// Each file's bytes at the version, or none, where they differ from what is grafted.
  pub files: BTreeMap<String, Option<Vec<u8>>>,
  /// Each IPC node at the version, or none, where it differs from what is grafted.
  pub specials: BTreeMap<String, Option<SpecialNode>>,
  /// Whether each directory stood at the version, where that differs from what is grafted.
  pub dirs: BTreeMap<String, bool>,
  /// Each mode at the version, or none, where it differs from what is grafted.
  pub modes: BTreeMap<String, Option<u32>>,
  /// Each symbolic link's target at the version, or none, where it differs from what is grafted.
  pub symlinks: BTreeMap<String, Option<String>>,
  /// Each attribute's value at the version, or none, where it differs from what is grafted: by `(path, name)`.
  pub xattrs: BTreeMap<(String, String), Option<Vec<u8>>>,
}

/// Format: the least bytes one record of a [`CopyFix`] table holds, a path's length word and a presence byte: what a
/// decoder checks a table's count against before it allocates.
const FIX_RECORD_MIN_BYTES: usize = size_of::<u32>() + size_of::<u8>();

/// Format: the bytes a length or count word takes.
const WORD_BYTES: usize = size_of::<u32>();

/// The bytes [`put_fix`] writes for `fix`, without writing them: what a fixed copy's record adds to its entry, which the
/// service secures before the verdict (`Green::copy_entry_room`).
pub fn fix_bytes(fix: &CopyFix) -> usize {
  let string = |text: &str| WORD_BYTES.saturating_add(text.len());
  let maybe = |len: Option<usize>| len.map_or(1, |len| len.saturating_add(1));
  let sum = |sizes: &mut dyn Iterator<Item = usize>| sizes.fold(0usize, usize::saturating_add);
  let tables = [
    sum(&mut fix.cuts.iter().map(|cut| string(cut))),
    sum(&mut fix.files.iter().map(|(path, bytes)| {
      string(path).saturating_add(maybe(bytes.as_ref().map(|bytes| string_len(bytes))))
    })),
    sum(&mut fix.specials.iter().map(|(path, node)| {
      string(path).saturating_add(maybe(node.map(|node| string_len(&node.encode()))))
    })),
    sum(&mut fix.dirs.keys().map(|path| string(path).saturating_add(1))),
    sum(
      &mut fix
        .modes
        .iter()
        .map(|(path, mode)| string(path).saturating_add(maybe(mode.map(|_| WORD_BYTES)))),
    ),
    sum(
      &mut fix
        .symlinks
        .iter()
        .map(|(path, target)| string(path).saturating_add(maybe(target.as_deref().map(string)))),
    ),
    sum(&mut fix.xattrs.iter().map(|((path, name), value)| {
      string(path)
        .saturating_add(string(name))
        .saturating_add(maybe(value.as_ref().map(|value| string_len(value))))
    })),
  ];
  let counts = WORD_BYTES.saturating_mul(tables.len());
  sum(&mut tables.into_iter())
    .saturating_add(counts)
    .saturating_add(1)
}

/// The bytes a length word and `bytes` take.
fn string_len(bytes: &[u8]) -> usize {
  WORD_BYTES.saturating_add(bytes.len())
}

/// Writes `value`'s presence byte, then the value through `put` when present.
fn put_maybe<T: ?Sized>(out: &mut Vec<u8>, value: Option<&T>, put: impl Fn(&mut Vec<u8>, &T)) {
  out.push(u8::from(value.is_some()));
  if let Some(value) = value {
    put(out, value);
  }
}

/// Writes a [`CopyFix`]: whether it starts empty, then its tables in key order, each a count and its records.
fn put_fix(out: &mut Vec<u8>, fix: &CopyFix) {
  out.push(u8::from(fix.start_empty));
  put_count(out, fix.cuts.len());
  for cut in &fix.cuts {
    put_str(out, cut);
  }
  put_count(out, fix.files.len());
  for (path, bytes) in &fix.files {
    put_str(out, path);
    put_maybe(out, bytes.as_deref(), put_bytes);
  }
  put_count(out, fix.specials.len());
  for (path, node) in &fix.specials {
    put_str(out, path);
    put_maybe(out, node.as_ref(), |out, node| {
      put_bytes(out, &node.encode())
    });
  }
  put_count(out, fix.dirs.len());
  for (path, stood) in &fix.dirs {
    put_str(out, path);
    out.push(u8::from(*stood));
  }
  put_fix_rest(out, fix);
}

/// [`put_fix`]'s modes, symbolic links and attributes.
fn put_fix_rest(out: &mut Vec<u8>, fix: &CopyFix) {
  put_count(out, fix.modes.len());
  for (path, mode) in &fix.modes {
    put_str(out, path);
    put_maybe(out, mode.as_ref(), |out, mode| {
      out.extend_from_slice(&mode.to_le_bytes());
    });
  }
  put_count(out, fix.symlinks.len());
  for (path, target) in &fix.symlinks {
    put_str(out, path);
    put_maybe(out, target.as_deref(), put_str);
  }
  put_count(out, fix.xattrs.len());
  for ((path, name), value) in &fix.xattrs {
    put_str(out, path);
    put_str(out, name);
    put_maybe(out, value.as_deref(), put_bytes);
  }
}

/// A presence or truth byte: 0 or 1, anything else refused.
fn take_flag(reader: &mut Reader<'_>) -> Result<bool, DocDecodeError> {
  match reader.u8()? {
    0 => Ok(false),
    1 => Ok(true),
    _ => Err(DocDecodeError::BadKind),
  }
}

/// A [`CopyFix`] table's count, refused before anything is allocated when the bytes that remain could not hold that
/// many records of `least` bytes each.
fn take_table(reader: &mut Reader<'_>, least: usize) -> Result<usize, DocDecodeError> {
  let count = reader.u32()? as usize;
  // A record takes at least one byte, so the divisor is never zero; the checked division keeps the decoder free of
  // panics, and its impossible None refuses rather than admits.
  let room = reader.remaining().checked_div(least.max(1)).unwrap_or(0);
  if count > room {
    return Err(DocDecodeError::Truncated);
  }
  Ok(count)
}

/// A present value read by `take`, or none.
fn take_maybe<T>(
  reader: &mut Reader<'_>,
  take: impl Fn(&mut Reader<'_>) -> Result<T, DocDecodeError>,
) -> Result<Option<T>, DocDecodeError> {
  if take_flag(reader)? {
    take(reader).map(Some)
  } else {
    Ok(None)
  }
}

/// Inserts one record of a [`CopyFix`] table; a key written twice is refused (an encoder writes each once, in order).
fn insert_once<K: Ord, V>(
  table: &mut BTreeMap<K, V>,
  key: K,
  value: V,
) -> Result<(), DocDecodeError> {
  if table.insert(key, value).is_some() {
    return Err(DocDecodeError::DuplicatePath);
  }
  Ok(())
}

/// Reads a [`CopyFix`] as [`put_fix`] writes it.
fn take_fix(reader: &mut Reader<'_>) -> Result<CopyFix, DocDecodeError> {
  let mut fix = CopyFix {
    start_empty: take_flag(reader)?,
    ..CopyFix::default()
  };
  for _ in 0..take_table(reader, WORD_BYTES)? {
    if !fix.cuts.insert(take_str(reader)?) {
      return Err(DocDecodeError::DuplicatePath);
    }
  }
  for _ in 0..take_table(reader, FIX_RECORD_MIN_BYTES)? {
    let path = take_str(reader)?;
    let bytes = take_maybe(reader, |reader| reader_bytes(reader).map(<[u8]>::to_vec))?;
    insert_once(&mut fix.files, path, bytes)?;
  }
  for _ in 0..take_table(reader, FIX_RECORD_MIN_BYTES)? {
    let path = take_str(reader)?;
    let node = take_maybe(reader, |reader| SpecialNode::decode(reader_bytes(reader)?))?;
    insert_once(&mut fix.specials, path, node)?;
  }
  for _ in 0..take_table(reader, FIX_RECORD_MIN_BYTES)? {
    let path = take_str(reader)?;
    let stood = take_flag(reader)?;
    insert_once(&mut fix.dirs, path, stood)?;
  }
  take_fix_rest(reader, &mut fix)?;
  Ok(fix)
}

/// [`take_fix`]'s modes, symbolic links and attributes.
fn take_fix_rest(reader: &mut Reader<'_>, fix: &mut CopyFix) -> Result<(), DocDecodeError> {
  for _ in 0..take_table(reader, FIX_RECORD_MIN_BYTES)? {
    let path = take_str(reader)?;
    let mode = take_maybe(reader, |reader| reader.u32())?;
    insert_once(&mut fix.modes, path, mode)?;
  }
  for _ in 0..take_table(reader, FIX_RECORD_MIN_BYTES)? {
    let path = take_str(reader)?;
    let target = take_maybe(reader, take_str)?;
    insert_once(&mut fix.symlinks, path, target)?;
  }
  for _ in 0..take_table(reader, FIX_RECORD_MIN_BYTES)? {
    let key = (take_str(reader)?, take_str(reader)?);
    let value = take_maybe(reader, |reader| reader_bytes(reader).map(<[u8]>::to_vec))?;
    insert_once(&mut fix.xattrs, key, value)?;
  }
  Ok(())
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
  /// Format: the tag of a tree copied from a version its source changed after (A-626 part 2): [`COPY_TREE`]'s record,
  /// then its `CopyFix`.
  pub(super) const COPY_TREE_FIXED: u8 = 19;
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
  // Reserved at the entry's bound once, never grown: the effects follow the increment's bytes, and growing past a
  // revision-sized buffer copied it whole (A-742: 250 MiB of such copies in a 1,000-revision import).
  let mut out = Vec::with_capacity(entry_bound(increment_bytes.len(), increment));
  out.extend_from_slice(&MAGIC.to_le_bytes());
  out.extend_from_slice(&VERSION.to_le_bytes());
  put_bytes(&mut out, increment_bytes);
  put_count(&mut out, applied.len());
  let declared = Declared::of(increment);
  for effect in applied {
    put_applied(&mut out, &declared, effect);
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
  let declared = Declared::of(&increment);
  for _ in 0..count {
    applied.push(take_applied(&mut reader, &declared)?);
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
    let path = reader.u64()?;
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

/// What an increment declares, indexed for its entry's effects (A-632): each path's content ops in
/// order, and each copy's source version and whether it replaces, by its destination and source
/// (A-586, A-626). Built in one pass
/// over the increment's ops, once per entry encoded or decoded; finding them by scanning every op
/// for each splice and each copy cost O(effects × ops), quadratic for a version that splices many
/// files.
pub(crate) struct Declared<'a> {
  /// Each path's content ops, in the increment's order.
  ops: BTreeMap<&'a str, Vec<Op>>,
  /// Each copy's source version and whether it replaces, by destination, then source; the first a
  /// pair declares.
  copies: BTreeMap<&'a str, BTreeMap<&'a str, (u64, bool)>>,
}

impl<'a> Declared<'a> {
  /// The index of what `increment` declares: every op visited once.
  pub(crate) fn of(increment: &'a Increment) -> Declared<'a> {
    let mut declared = Declared {
      ops: BTreeMap::new(),
      copies: BTreeMap::new(),
    };
    let paths = &increment.doc.paths;
    for op in &increment.doc.ops {
      #[cfg(test)]
      tests::count_visit();
      let Some(path) = paths.path(op.path) else {
        continue;
      };
      if crate::engine::is_content(op.kind) {
        declared.ops.entry(path).or_default().push(*op);
      } else if op.kind == OpKind::Copy
        && let Some(from) = paths.path(op.src)
      {
        declared
          .copies
          .entry(path)
          .or_default()
          .entry(from)
          .or_insert((op.at, op.replaces()));
      }
    }
    declared
  }

  /// The content ops the increment declares for `path`, in order.
  fn ops_for(&self, path: &str) -> &[Op] {
    self.ops.get(path).map_or(&[][..], Vec::as_slice)
  }

  /// The source version of the copy the increment declares from `from` to `dst`, and whether it
  /// replaces; `None` when it declares none, which no entry the engine recorded holds.
  fn copy(&self, dst: &str, from: &str) -> Option<(u64, bool)> {
    self.copies.get(dst)?.get(from).copied()
  }
}

/// Appends one effect as an entry records it (module docs); a computed verdict's wire form writes its effects so too
/// (`crate::engine`'s `computed_wire`, A-658 part 3a).
pub(crate) fn put_applied(out: &mut Vec<u8>, declared: &Declared<'_>, applied: &Applied) {
  match applied {
    Applied::Special(path, node) => {
      put_tagged_paths(out, tag::SPECIAL, &[path]);
      put_bytes(out, &node.encode());
    }
    Applied::RemoveSpecial(path) => put_tagged_paths(out, tag::REMOVE_SPECIAL, &[path]),
    Applied::Spliced(path, ops) if ops.as_slice() == declared.ops_for(path) => {
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
    Applied::CopyTree {
      dst,
      from,
      fix: None,
      ..
    } => put_tagged_paths(out, tag::COPY_TREE, &[dst, from]),
    Applied::CopyTree {
      dst,
      from,
      fix: Some(fix),
      ..
    } => {
      put_tagged_paths(out, tag::COPY_TREE_FIXED, &[dst, from]);
      put_fix(out, fix);
    }
  }
}

/// Reads one effect [`put_applied`] wrote, refused typed.
pub(crate) fn take_applied(
  reader: &mut Reader<'_>,
  declared: &Declared<'_>,
) -> Result<Applied, DocDecodeError> {
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
      let ops = declared.ops_for(&path).to_vec();
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
    kind @ (tag::COPY_TREE | tag::COPY_TREE_FIXED) => {
      let dst = take_str(reader)?;
      let from = take_str(reader)?;
      let (version, replace) = declared.copy(&dst, &from).ok_or(DocDecodeError::BadKind)?;
      let fix = if kind == tag::COPY_TREE_FIXED {
        Some(Box::new(take_fix(reader)?))
      } else {
        None
      };
      Applied::CopyTree {
        dst,
        from,
        version,
        replace,
        fix,
      }
    }
    _ => return Err(DocDecodeError::BadKind),
  })
}

#[cfg(test)]
mod tests {
  use std::cell::Cell;

  use super::*;
  use crate::ops_doc::OpsDoc;

  /// A fix with a record of every kind it holds, present and absent.
  fn every_kind_fix() -> CopyFix {
    let mut fix = CopyFix::default();
    fix.cuts.insert("t".to_owned());
    fix.files.insert("a".to_owned(), Some(b"alpha".to_vec()));
    fix.files.insert("c".to_owned(), None);
    fix.specials.insert("p".to_owned(), None);
    fix.dirs.insert("d".to_owned(), true);
    fix.dirs.insert("e".to_owned(), false);
    fix.modes.insert("a".to_owned(), Some(0o644));
    fix.modes.insert("c".to_owned(), None);
    fix.symlinks.insert("l".to_owned(), Some("a".to_owned()));
    fix
      .xattrs
      .insert(("d".to_owned(), "n".to_owned()), Some(b"v".to_vec()));
    fix.xattrs.insert(("d".to_owned(), "m".to_owned()), None);
    fix
  }

  /// A-626 part 2. Do: encode a fix holding a record of every kind and decode it. Expect: the same fix, nothing left
  /// over, and `fix_bytes` the encoding's length exactly: the room the service secures is the bytes the entry gains.
  #[test]
  fn a_copy_fix_round_trips_and_its_size_is_its_encoding() {
    let fix = every_kind_fix();
    let mut out = Vec::new();
    put_fix(&mut out, &fix);
    assert_eq!(fix_bytes(&fix), out.len());
    let mut reader = Reader::new(&out);
    assert_eq!(take_fix(&mut reader), Ok(fix));
    assert_eq!(reader.remaining(), 0);
  }

  /// A-626 part 2 (golden vector). Do: encode a fix that starts empty with one cut and one file. Expect: these bytes, the
  /// format's, byte for byte: the flag, then each table's count and records in order.
  #[test]
  fn a_copy_fix_encodes_to_its_golden_bytes() {
    let mut fix = CopyFix {
      start_empty: true,
      ..CopyFix::default()
    };
    fix.cuts.insert("x".to_owned());
    fix.files.insert("a".to_owned(), Some(b"z".to_vec()));
    let mut out = Vec::new();
    put_fix(&mut out, &fix);
    let mut golden = [
      // Starts empty; one cut, "x".
      &b"\x01"[..],
      b"\x01\x00\x00\x00\x01\x00\x00\x00x",
      // One file, "a", present: "z".
      b"\x01\x00\x00\x00\x01\x00\x00\x00a\x01\x01\x00\x00\x00z",
    ]
    .concat();
    // No IPC node, directory, mode, symbolic link or attribute: five empty counts.
    golden.extend_from_slice(&[0; 20]);
    assert_eq!(out, golden);
  }

  /// A-626 part 2 (hostile bytes). Do: decode a fix whose file table claims `u32::MAX` records, one naming a file twice,
  /// and one whose flag byte is 2. Expect: `Truncated` (before anything is allocated for the claim), `DuplicatePath` and
  /// `BadKind`.
  #[test]
  fn a_hostile_copy_fix_is_refused_typed() {
    let mut huge = vec![0u8];
    put_count(&mut huge, 0);
    huge.extend_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(
      take_fix(&mut Reader::new(&huge)),
      Err(DocDecodeError::Truncated)
    );
    let mut twice = vec![0u8];
    put_count(&mut twice, 0);
    put_count(&mut twice, 2);
    for _ in 0..2 {
      put_str(&mut twice, "a");
      twice.push(0);
    }
    for _ in 0..5 {
      put_count(&mut twice, 0);
    }
    assert_eq!(
      take_fix(&mut Reader::new(&twice)),
      Err(DocDecodeError::DuplicatePath)
    );
    assert_eq!(
      take_fix(&mut Reader::new(&[2u8])),
      Err(DocDecodeError::BadKind)
    );
  }

  thread_local! {
    /// The increment's ops an entry's index visited on this thread ([`Declared::of`]).
    static VISITS: Cell<u64> = const { Cell::new(0) };
  }

  /// Counts one op visited by [`Declared::of`] (test builds).
  pub(super) fn count_visit() {
    VISITS.with(|count| count.set(count.get().saturating_add(1)));
  }

  /// The ops visited on this thread since the last read, the count reset.
  fn visits() -> u64 {
    VISITS.with(|count| count.replace(0))
  }

  /// Shape: the files a version splices, each as its increment declared it.
  const FILES: usize = 1_000;

  /// A-632 (failing first: each splice's declared ops were found by scanning every op of the
  /// increment). Do: record a version splicing each of a thousand files as its increment declared
  /// it (so each splice is recorded by its path alone and its ops found again from the increment),
  /// counting the increment's ops the entry's encode and its decode visit. Expect each to visit
  /// every op once, a thousand, never once a splice (a million), and the entry to decode to what
  /// was recorded.
  #[test]
  fn an_entry_finds_each_splices_declared_ops_in_one_pass() {
    let names: Vec<String> = (0..FILES).map(|n| format!("src/{n:04}.c")).collect();
    let mut doc = OpsDoc::new();
    for name in &names {
      let path = doc.paths.intern(name);
      doc.ops.push(Op {
        kind: OpKind::Overwrite,
        flags: 0,
        path,
        at: 0,
        len: 1,
        src: 0,
      });
    }
    doc.canonicalize();
    let increment = Increment {
      id: [3; 32],
      base: 0,
      doc,
      post_state: b"x".to_vec(),
      evidence: Vec::new(),
      named: Default::default(),
    };
    let applied: Vec<Applied> = increment
      .doc
      .ops
      .iter()
      .zip(&names)
      .map(|(op, name)| Applied::Spliced(name.clone(), vec![*op]))
      .collect();
    let increment_bytes = increment.encode();
    visits();
    let bytes = encode(&increment_bytes, &increment, &applied, None, None, None);
    let encoded = visits();
    let entry = decode(&bytes).unwrap();
    let decoded = visits();
    assert_eq!(entry.applied, applied);
    assert_eq!((encoded, decoded), (FILES as u64, FILES as u64));
  }

  /// A-621 (the chain entry's version 5). Do: record a version whose increment names 65,537 paths, its splice on the
  /// last path an op the verdict mapped (so the entry writes the op whole, its path index with it) and its splice on
  /// the first path the op as declared (so the entry names the path alone), then decode the entry. Expect both effects
  /// back as recorded: the mapped op's 64-bit path index naming the last path, past what 16 bits held, and the declared
  /// one found again in the increment.
  #[test]
  fn an_entry_keeps_an_ops_path_index_past_sixteen_bits() {
    let names: Vec<String> = (0..(1usize << 16) + 1)
      .map(|n| format!("tree/{n:06}.c"))
      .collect();
    let mut doc = OpsDoc::new();
    for name in &names {
      doc.paths.intern(name);
    }
    let (first, last) = (
      doc.paths.intern(&names[0]),
      doc.paths.intern(&names[names.len() - 1]),
    );
    assert_eq!(last, 1 << 16, "the names were interned in their order");
    for path in [first, last] {
      doc.ops.push(Op {
        kind: OpKind::Overwrite,
        flags: 0,
        path,
        at: 0,
        len: 1,
        src: 0,
      });
    }
    doc.canonicalize();
    let increment = Increment {
      id: [7; 32],
      base: 0,
      doc,
      post_state: b"x".to_vec(),
      evidence: Vec::new(),
      named: Default::default(),
    };
    let mapped = Op {
      at: 5,
      ..increment.doc.ops[1]
    };
    let applied = vec![
      Applied::Spliced(names[0].clone(), vec![increment.doc.ops[0]]),
      Applied::Spliced(names[names.len() - 1].clone(), vec![mapped]),
    ];
    let bytes = encode(&increment.encode(), &increment, &applied, None, None, None);
    let entry = decode(&bytes).unwrap();
    assert_eq!(entry.applied, applied);
    let Applied::Spliced(path, ops) = &entry.applied[1] else {
      panic!("not a splice: {:?}", entry.applied[1]);
    };
    assert_eq!(ops[0].path, 1 << 16);
    assert_eq!(
      entry.increment.doc.paths.path(ops[0].path),
      Some(path.as_str())
    );
  }
}
