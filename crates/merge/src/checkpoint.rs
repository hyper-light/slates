//! A green's checkpoint (§4.16 "Deltas older than the derived retention fold into checkpoint deltas by the same
//! composition, which is exact"; §4.8 recovery): the green's state at a version `F` no live reader can be below, kept
//! durably in place of every increment up to `F`, so the partition holds what the green is, not what it has been.
//!
//! **What it holds.** Only what differs from the green's origin, which the service holds already (as bytes, or as a
//! reference to its base snapshot, A-217): for every key of every dimension whose value in effect at `F` was set by a
//! version after 0, that version and that value (absent when the key was removed). With it, the identities of every
//! increment accepted up to `F` and their versions, the idempotency record a retried submit is answered from. A key
//! untouched since the origin is not in it. With the accepted identities, how each signed version's submit was judged
//! (signed work, A-321): the provenance its chain entry carried until the checkpoint folded it, kept as long as the
//! version's idempotency record is. So a checkpoint costs what the green's versions changed, once each, and an
//! increment folded into it is never read again: the green restored from the origin and the checkpoint
//! ([`crate::engine::Green::restore`]) answers exactly as the green did for every version from `F` on, and a base below
//! `F` is refused as unknown rather than judged against deltas that are gone.
//!
//! **Copies (A-626 part 3).** A copy (A-586) is recorded once, at its destination, and the keys of its tree hold what it
//! gave them with no history of their own, read by the copy's record; and the identity's set hash holds one entry for
//! the copy, standing for every entry its tree holds. So a checkpoint at or after a copy carries three things more:
//! - every copy's record made at or before `F` (its destination, its version, its source and the source's version),
//!   which the restored green keeps as the green kept it;
//! - the copied trees' keys, each that holds at `F` what the copy above it gave it (`copied`), with that copy's version
//!   and the value (absent where a replace took the key away and the copy gave nothing back): a restore sets them with
//!   no history, so each reads by its record again. A key of a copied tree its own change set after the copy is one
//!   of the changed keys, as any key is;
//! - the identity's running sum at `F` ([`Sum`]): a restore takes it as the green's, since what a copy's entry stands
//!   for (its source's tree at the source's version) is read from histories a restored green no longer holds.
//!
//! The copies' section (the records and the copied tables) and the sum each sit behind a presence byte, written only
//! beside a copy's record (A-626 part 3, continued (1)): without one, the restored state's own entries are the identity,
//! so a green that never copied pays two bytes for them. Version 4 wrote the sum's 2,048 bytes into every checkpoint,
//! and the service's doubling rule, which takes a checkpoint once the increments it would fold reach the size of the
//! one it holds, then waited on 2,048 bytes of increments for every green: a root written twice held its first version
//! where it had folded it (194 bytes of increment against a 2,233-byte checkpoint, Mac Studio, 2026-11-19).
//!
//! A copied tree's keys are listed key by key, so its checkpoint costs the tree, where the green holds it by reference:
//! a restore that grafts a tree whose source is unchanged since its version is owed, if a measurement shows it worth
//! its rules (GAPS). Until A-626 part 3 a green holding a copy at or before `F` was not checkpointed at all, and the
//! service kept its chain whole (`merge.checkpoint.unavailable`).
//!
//! **Why.** Before it, every accepted increment stayed in the partition's chain for good (`green_chains`): each version
//! cost its increment, every snapshot taken when the log filled re-encoded the whole chain (9.4 GB of large allocations
//! in one 900-version level, 2026-11-06), recovery replayed every version, and a green whose readers all kept up was
//! refused `green_chains at capacity` at version 31 under a 256 KiB cap (`crates/server/tests/green_checkpoint.rs`).
//!
//! **The encoding** follows the origin's: magic and version, then each table as a count and its entries, every string
//! and byte run length-prefixed, little-endian, tables sorted by key, so one checkpoint encodes to one byte sequence on
//! every host. `decode` checks every count against the bytes that remain before it allocates and every length before
//! it reads, refuses a key a table holds twice (`DuplicatePath`: which of two values stands would be a guess), and
//! refuses any malformation typed ([`DocDecodeError`]), never a panic.

use std::collections::BTreeSet;

use crate::ops_doc::{DocDecodeError, Reader};
use crate::origin::{put_bytes, put_count, reader_bytes, take_count, take_str};
use crate::special::SpecialNode;

/// Format: the checkpoint encoding's magic, "GCKP" little-endian, so a chain entry of another kind (an increment, an
/// origin) is refused at the first word rather than misread.
const MAGIC: u32 = u32::from_le_bytes(*b"GCKP");
/// The checkpoint encoding's version: 3 since A-321 added the signed versions' verdicts beside the accepted
/// identities; 4 since A-626 part 3 added the copies' records, their trees' keys and the identity's sum; 5 since the
/// copies' section and the sum each sit behind a presence byte, written beside a copy's record alone (A-626 part 3,
/// continued (1)). An older checkpoint is refused (`BadVersion`), never read under version 5's layout: replaced, not
/// layered; and a segment that holds one is refused at adoption (the anchor's layout 13), so none is ever met.
///
/// Format: the checkpoint encoding's version; a decoder refuses any other.
const VERSION: u32 = 5;
/// Format: a signature verdict's fingerprint, a SHA-256 hash (`crate::chain_entry::Signature::fingerprint`).
const FINGERPRINT_BYTES: usize = 32;
/// Format: an accepted increment's identity, a BLAKE3 hash.
const IDENTITY_BYTES: usize = 32;
/// Format: the byte that marks a value present (a key whose value at the checkpoint is set).
const PRESENT: u8 = 1;
/// Format: the byte that marks a value absent (a key removed by the version that last changed it).
const ABSENT: u8 = 0;
/// Format: the bytes of the identity's running sum ([`Sum`]): LtHash16's lanes, each little-endian (`crate::identity`).
pub const SUM_BYTES: usize = crate::identity::SUM_BYTES;

/// One key's value in effect at the checkpoint: the version that set it, and the value (`None`: removed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Changed<K, V> {
  /// The key.
  pub key: K,
  /// The version whose change is in effect at the checkpoint (after 0, at or before the checkpoint's version); for a
  /// copied tree's key, the version of the copy that gave it its value.
  pub at: u64,
  /// The value it left: `None` when it removed the key.
  pub value: Option<V>,
}

/// One copy made at or before the checkpoint's version (A-586): where it was made, when, and what it copied.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct CopyRecord {
  /// The copy's destination.
  pub destination: String,
  /// The version the copy was made at.
  pub version: u64,
  /// The copy's source.
  pub source: String,
  /// The version of the source it copied.
  pub source_version: u64,
}

/// One table a dimension: every key of a checkpoint's kind (changed since the origin, or held by a copy), with its
/// value.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tables {
  /// Regular files.
  pub files: Vec<Changed<String, Vec<u8>>>,
  /// Directories (present: `Some(())`).
  pub dirs: Vec<Changed<String, ()>>,
  /// Explicit modes.
  pub modes: Vec<Changed<String, u32>>,
  /// Symbolic links.
  pub symlinks: Vec<Changed<String, String>>,
  /// Hard links.
  pub hardlinks: Vec<Changed<String, String>>,
  /// FIFO and socket nodes.
  pub specials: Vec<Changed<String, SpecialNode>>,
  /// Extended attributes, keyed by `(path, name)`.
  pub xattrs: Vec<Changed<(String, String), Vec<u8>>>,
}

impl Tables {
  /// Sorts every table by its key.
  fn canonicalize(&mut self) {
    self.files.sort_by(|a, b| a.key.cmp(&b.key));
    self.dirs.sort_by(|a, b| a.key.cmp(&b.key));
    self.modes.sort_by(|a, b| a.key.cmp(&b.key));
    self.symlinks.sort_by(|a, b| a.key.cmp(&b.key));
    self.hardlinks.sort_by(|a, b| a.key.cmp(&b.key));
    self.specials.sort_by(|a, b| a.key.cmp(&b.key));
    self.xattrs.sort_by(|a, b| a.key.cmp(&b.key));
  }

  /// The tables' bytes, in their order: files, directories, modes, symbolic links, hard links, IPC nodes, attributes.
  fn encode_into(&self, out: &mut Vec<u8>) {
    let path = |out: &mut Vec<u8>, path: &String| put_bytes(out, path.as_bytes());
    put_table(out, &self.files, path, |out, bytes| put_bytes(out, bytes));
    put_table(out, &self.dirs, path, |_, ()| {});
    put_table(out, &self.modes, path, |out, mode| {
      out.extend_from_slice(&mode.to_le_bytes());
    });
    put_table(out, &self.symlinks, path, |out, target| {
      put_bytes(out, target.as_bytes());
    });
    put_table(out, &self.hardlinks, path, |out, target| {
      put_bytes(out, target.as_bytes());
    });
    put_table(out, &self.specials, path, |out, node| {
      out.extend_from_slice(&node.encode());
    });
    put_table(
      out,
      &self.xattrs,
      |out, (path, name)| {
        put_bytes(out, path.as_bytes());
        put_bytes(out, name.as_bytes());
      },
      |out, value| put_bytes(out, value),
    );
  }

  /// Reads [`Tables::encode_into`]'s bytes.
  fn decode_from(reader: &mut Reader<'_>) -> Result<Tables, DocDecodeError> {
    Ok(Tables {
      files: take_table(reader, take_str, |r| Ok(reader_bytes(r)?.to_vec()))?,
      dirs: take_table(reader, take_str, |_| Ok(()))?,
      modes: take_table(reader, take_str, Reader::u32)?,
      symlinks: take_table(reader, take_str, take_str)?,
      hardlinks: take_table(reader, take_str, take_str)?,
      specials: take_table(reader, take_str, SpecialNode::read)?,
      xattrs: take_table(
        reader,
        |r| Ok((take_str(r)?, take_str(r)?)),
        |r| Ok(reader_bytes(r)?.to_vec()),
      )?,
    })
  }
}

/// The identity's running sum at a checkpoint's version (A-202's set hash; A-626 part 3): its lanes' bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sum(Box<[u8; SUM_BYTES]>);

impl Default for Sum {
  /// The empty set's sum: every lane zero.
  fn default() -> Sum {
    Sum(Box::new([0; SUM_BYTES]))
  }
}

impl Sum {
  /// The sum of these lanes' bytes.
  pub fn new(bytes: Box<[u8; SUM_BYTES]>) -> Sum {
    Sum(bytes)
  }

  /// The lanes' bytes.
  pub fn bytes(&self) -> &[u8; SUM_BYTES] {
    &self.0
  }
}

/// A green's state at [`Checkpoint::version`] beyond its origin (module docs).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Checkpoint {
  /// The version the checkpoint is the green's state at: the head of the green it restores.
  pub version: u64,
  /// Every increment accepted at or before `version`, by identity, with the version it committed.
  pub accepted: Vec<([u8; 32], u64)>,
  /// How the signature of each version at or before `version` whose submit carried one was judged (A-321), by version.
  pub signatures: Vec<(u64, crate::chain_entry::Signature)>,
  /// Regular files changed since the origin.
  pub files: Vec<Changed<String, Vec<u8>>>,
  /// Directories created or removed since the origin (present: `Some(())`).
  pub dirs: Vec<Changed<String, ()>>,
  /// Explicit modes changed since the origin.
  pub modes: Vec<Changed<String, u32>>,
  /// Symbolic links changed since the origin.
  pub symlinks: Vec<Changed<String, String>>,
  /// Hard links changed since the origin.
  pub hardlinks: Vec<Changed<String, String>>,
  /// FIFO and socket nodes changed since the origin.
  pub specials: Vec<Changed<String, SpecialNode>>,
  /// Extended attributes changed since the origin, keyed by `(path, name)`.
  pub xattrs: Vec<Changed<(String, String), Vec<u8>>>,
  /// Every copy made at or before `version` (A-586; module docs), by destination, then version.
  pub copies: Vec<CopyRecord>,
  /// The copied trees' keys holding at `version` what the copy above them gave them, each with that copy's version
  /// (module docs): set with no history by a restore, so each reads by its copy's record.
  pub copied: Tables,
  /// The identity's running sum at `version` (module docs), which a restore takes as the green's: carried beside a
  /// copy's record alone (`None` without one, where the restored state's own entries are the identity).
  pub sum: Option<Sum>,
  /// The green's head when this checkpoint was taken and its identity there (A-257): a recovery's replay must reach that
  /// identity when it reaches that version, or the green is fenced. `None` where the taker did not record one.
  pub head_check: Option<(u64, [u8; 32])>,
}

impl Checkpoint {
  /// Sorts every table by its key, so the encoding is one sequence whatever order the engine walked its histories in.
  pub fn canonicalize(&mut self) {
    self.accepted.sort();
    self.signatures.sort_by_key(|(version, _)| *version);
    self.files.sort_by(|a, b| a.key.cmp(&b.key));
    self.dirs.sort_by(|a, b| a.key.cmp(&b.key));
    self.modes.sort_by(|a, b| a.key.cmp(&b.key));
    self.symlinks.sort_by(|a, b| a.key.cmp(&b.key));
    self.hardlinks.sort_by(|a, b| a.key.cmp(&b.key));
    self.specials.sort_by(|a, b| a.key.cmp(&b.key));
    self.xattrs.sort_by(|a, b| a.key.cmp(&b.key));
    self.copies.sort();
    self.copied.canonicalize();
  }

  /// The canonical bytes (module docs). Canonicalizes a copy first.
  pub fn encode(&self) -> Vec<u8> {
    let mut canonical = self.clone();
    canonical.canonicalize();
    let mut out = Vec::new();
    out.extend_from_slice(&MAGIC.to_le_bytes());
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&canonical.version.to_le_bytes());
    put_count(&mut out, canonical.accepted.len());
    for (identity, version) in &canonical.accepted {
      out.extend_from_slice(identity);
      out.extend_from_slice(&version.to_le_bytes());
    }
    put_count(&mut out, canonical.signatures.len());
    for (version, signature) in &canonical.signatures {
      out.extend_from_slice(&version.to_le_bytes());
      out.push(signature.kind);
      out.push(signature.detail);
      out.extend_from_slice(&signature.fingerprint);
    }
    canonical.changed().encode_into(&mut out);
    canonical.encode_copies_into(&mut out);
    match &canonical.sum {
      Some(sum) => {
        out.push(PRESENT);
        out.extend_from_slice(sum.bytes());
      }
      None => out.push(ABSENT),
    }
    match canonical.head_check {
      Some((head, identity)) => {
        out.push(PRESENT);
        out.extend_from_slice(&head.to_le_bytes());
        out.extend_from_slice(&identity);
      }
      None => out.push(ABSENT),
    }
    out
  }

  /// The copies' section (module docs): a presence byte and, present, the copies' records and the copied tables. Written
  /// only where the checkpoint holds either, so a green that never copied pays the one byte.
  fn encode_copies_into(&self, out: &mut Vec<u8>) {
    if self.copies.is_empty() && self.copied == Tables::default() {
      out.push(ABSENT);
      return;
    }
    out.push(PRESENT);
    put_count(out, self.copies.len());
    for record in &self.copies {
      put_bytes(out, record.destination.as_bytes());
      out.extend_from_slice(&record.version.to_le_bytes());
      put_bytes(out, record.source.as_bytes());
      out.extend_from_slice(&record.source_version.to_le_bytes());
    }
    self.copied.encode_into(out);
  }

  /// The changed keys' tables, moved out of the checkpoint (its own fields, emptied).
  fn changed(&mut self) -> Tables {
    Tables {
      files: std::mem::take(&mut self.files),
      dirs: std::mem::take(&mut self.dirs),
      modes: std::mem::take(&mut self.modes),
      symlinks: std::mem::take(&mut self.symlinks),
      hardlinks: std::mem::take(&mut self.hardlinks),
      specials: std::mem::take(&mut self.specials),
      xattrs: std::mem::take(&mut self.xattrs),
    }
  }

  /// Decodes [`Checkpoint::encode`]'s bytes (module docs): typed refusals, never a panic, and the bytes consumed
  /// exactly.
  pub fn decode(bytes: &[u8]) -> Result<Checkpoint, DocDecodeError> {
    let mut reader = Reader::new(bytes);
    if reader.u32()? != MAGIC {
      return Err(DocDecodeError::BadMagic);
    }
    if reader.u32()? != VERSION {
      return Err(DocDecodeError::BadVersion);
    }
    let version = reader.u64()?;
    let accepted = take_accepted(&mut reader)?;
    let signatures = take_signatures(&mut reader)?;
    let changed = Tables::decode_from(&mut reader)?;
    let (copies, copied) = take_copies_section(&mut reader)?;
    let sum = take_sum(&mut reader)?;
    let head_check = take_head_check(&mut reader)?;
    if !reader.is_empty() {
      return Err(DocDecodeError::TrailingBytes);
    }
    Ok(Checkpoint {
      version,
      accepted,
      signatures,
      files: changed.files,
      dirs: changed.dirs,
      modes: changed.modes,
      symlinks: changed.symlinks,
      hardlinks: changed.hardlinks,
      specials: changed.specials,
      xattrs: changed.xattrs,
      copies,
      copied,
      sum,
      head_check,
    })
  }

  /// The checkpoint's identity: the BLAKE3 of its canonical bytes.
  pub fn identity(&self) -> [u8; 32] {
    *blake3::hash(&self.encode()).as_bytes()
  }
}

/// The accepted identities, as [`Checkpoint::encode`] writes them.
fn take_accepted(reader: &mut Reader<'_>) -> Result<Vec<([u8; 32], u64)>, DocDecodeError> {
  let mut accepted = Vec::new();
  for _ in 0..take_count(reader)? {
    let mut identity = [0u8; IDENTITY_BYTES];
    identity.copy_from_slice(reader.bytes(IDENTITY_BYTES)?);
    accepted.push((identity, reader.u64()?));
  }
  Ok(accepted)
}

/// The signed versions' verdicts, as [`Checkpoint::encode`] writes them.
fn take_signatures(
  reader: &mut Reader<'_>,
) -> Result<Vec<(u64, crate::chain_entry::Signature)>, DocDecodeError> {
  let mut signatures = Vec::new();
  for _ in 0..take_count(reader)? {
    let version = reader.u64()?;
    let kind = reader.u8()?;
    let detail = reader.u8()?;
    let mut fingerprint = [0u8; FINGERPRINT_BYTES];
    fingerprint.copy_from_slice(reader.bytes(FINGERPRINT_BYTES)?);
    signatures.push((
      version,
      crate::chain_entry::Signature {
        kind,
        detail,
        fingerprint,
      },
    ));
  }
  Ok(signatures)
}

/// The copies' section, as [`Checkpoint::encode`] writes it: a presence byte and, present, the copies' records and the
/// copied tables.
fn take_copies_section(
  reader: &mut Reader<'_>,
) -> Result<(Vec<CopyRecord>, Tables), DocDecodeError> {
  match reader.u8()? {
    PRESENT => {
      let copies = take_copies(reader)?;
      Ok((copies, Tables::decode_from(reader)?))
    }
    ABSENT => Ok((Vec::new(), Tables::default())),
    _ => Err(DocDecodeError::BadKind),
  }
}

/// The copies' records, as [`Checkpoint::encode`] writes them; one destination and version twice is refused.
fn take_copies(reader: &mut Reader<'_>) -> Result<Vec<CopyRecord>, DocDecodeError> {
  let mut copies = Vec::new();
  let mut seen = BTreeSet::new();
  for _ in 0..take_count(reader)? {
    let record = CopyRecord {
      destination: take_str(reader)?,
      version: reader.u64()?,
      source: take_str(reader)?,
      source_version: reader.u64()?,
    };
    if !seen.insert((record.destination.clone(), record.version)) {
      return Err(DocDecodeError::DuplicatePath);
    }
    copies.push(record);
  }
  Ok(copies)
}

/// The identity's running sum, as [`Checkpoint::encode`] writes it: a presence byte and, present, its [`SUM_BYTES`]
/// bytes.
fn take_sum(reader: &mut Reader<'_>) -> Result<Option<Sum>, DocDecodeError> {
  match reader.u8()? {
    PRESENT => {
      let bytes: Box<[u8; SUM_BYTES]> = reader
        .bytes(SUM_BYTES)?
        .to_vec()
        .into_boxed_slice()
        .try_into()
        .map_err(|_| DocDecodeError::Truncated)?;
      Ok(Some(Sum(bytes)))
    }
    ABSENT => Ok(None),
    _ => Err(DocDecodeError::BadKind),
  }
}

/// The head check, a presence byte and, present, the head and its identity.
fn take_head_check(reader: &mut Reader<'_>) -> Result<Option<(u64, [u8; 32])>, DocDecodeError> {
  match reader.u8()? {
    PRESENT => {
      let head = reader.u64()?;
      let mut identity = [0u8; IDENTITY_BYTES];
      identity.copy_from_slice(reader.bytes(IDENTITY_BYTES)?);
      Ok(Some((head, identity)))
    }
    ABSENT => Ok(None),
    _ => Err(DocDecodeError::BadKind),
  }
}

/// One table: its count, then per entry the key, the version, the presence byte and (present) the value.
fn put_table<K, V>(
  out: &mut Vec<u8>,
  table: &[Changed<K, V>],
  key: impl Fn(&mut Vec<u8>, &K),
  value: impl Fn(&mut Vec<u8>, &V),
) {
  put_count(out, table.len());
  for entry in table {
    key(out, &entry.key);
    out.extend_from_slice(&entry.at.to_le_bytes());
    match &entry.value {
      Some(present) => {
        out.push(PRESENT);
        value(out, present);
      }
      None => out.push(ABSENT),
    }
  }
}

/// The inverse of [`put_table`]: a presence byte other than [`PRESENT`] or [`ABSENT`] is refused, and so is a key the
/// table holds twice.
fn take_table<'a, K: Ord, V>(
  reader: &mut Reader<'a>,
  key: impl Fn(&mut Reader<'a>) -> Result<K, DocDecodeError>,
  value: impl Fn(&mut Reader<'a>) -> Result<V, DocDecodeError>,
) -> Result<Vec<Changed<K, V>>, DocDecodeError> {
  let count = take_count(reader)?;
  let mut table = Vec::with_capacity(count);
  for _ in 0..count {
    let key = key(reader)?;
    let at = reader.u64()?;
    let value = match reader.bytes(1)? {
      [PRESENT] => Some(value(reader)?),
      [ABSENT] => None,
      _ => return Err(DocDecodeError::BadKind),
    };
    table.push(Changed { key, at, value });
  }
  let mut seen = BTreeSet::new();
  if table.iter().any(|entry| !seen.insert(&entry.key)) {
    return Err(DocDecodeError::DuplicatePath);
  }
  Ok(table)
}

#[cfg(test)]
mod tests;
