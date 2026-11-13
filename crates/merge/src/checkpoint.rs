//! A green's checkpoint (§4.16 "Deltas older than the derived retention fold into checkpoint deltas by the same
//! composition, which is exact"; §4.8 recovery): the green's state at a version `F` no live reader can be below, kept
//! durably in place of every increment up to `F`, so the partition holds what the green is, not what it has been.
//!
//! **What it holds.** Only what differs from the green's origin, which the service holds already (as bytes, or as a
//! reference to its base snapshot, A-217): for every key of every dimension whose value in effect at `F` was set by a
//! version after 0, that version and that value (absent when the key was removed). With it, the identities of every
//! increment accepted up to `F` and their versions, the idempotency record a retried submit is answered from. A key
//! untouched since the origin is not in it. So a checkpoint costs what the green's versions changed, once each, and an
//! increment folded into it is never read again: the green restored from the origin and the checkpoint
//! ([`crate::engine::Green::restore`]) answers exactly as the green did for every version from `F` on, and a base below
//! `F` is refused as unknown rather than judged against deltas that are gone.
//!
//! **Why.** Before it, every accepted increment stayed in the partition's chain for good (`green_chains`): each version
//! cost its increment, every snapshot taken when the log filled re-encoded the whole chain (9.4 GB of large allocations
//! in one 900-version level, 2026-11-06), recovery replayed every version, and a green whose readers all kept up was
//! refused `green_chains at capacity` at version 31 under a 256 KiB cap (`crates/server/tests/green_checkpoint.rs`).
//!
//! **The encoding** follows the origin's: magic and version, then each table as a count and its entries, every string
//! and byte run length-prefixed, little-endian, tables sorted by key, so one checkpoint encodes to one byte sequence on
//! every host. `decode` checks every count against the bytes that remain before it allocates and every length before
//! it reads, and refuses any malformation typed ([`DocDecodeError`]), never a panic.

use crate::ops_doc::{DocDecodeError, Reader};
use crate::origin::{put_bytes, put_count, reader_bytes, take_count, take_str};
use crate::special::SpecialNode;

/// Format: the checkpoint encoding's magic, "GCKP" little-endian, so a chain entry of another kind (an increment, an
/// origin) is refused at the first word rather than misread.
const MAGIC: u32 = u32::from_le_bytes(*b"GCKP");
/// Format: the checkpoint encoding's version; a decoder refuses any other.
const VERSION: u32 = 2;
/// Format: an accepted increment's identity, a BLAKE3 hash.
const IDENTITY_BYTES: usize = 32;
/// Format: the byte that marks a value present (a key whose value at the checkpoint is set).
const PRESENT: u8 = 1;
/// Format: the byte that marks a value absent (a key removed by the version that last changed it).
const ABSENT: u8 = 0;

/// One key's value in effect at the checkpoint: the version that set it, and the value (`None`: removed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Changed<K, V> {
  /// The key.
  pub key: K,
  /// The version whose change is in effect at the checkpoint (after 0, at or before the checkpoint's version).
  pub at: u64,
  /// The value it left: `None` when it removed the key.
  pub value: Option<V>,
}

/// A green's state at [`Checkpoint::version`] beyond its origin (module docs).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Checkpoint {
  /// The version the checkpoint is the green's state at: the head of the green it restores.
  pub version: u64,
  /// Every increment accepted at or before `version`, by identity, with the version it committed.
  pub accepted: Vec<([u8; 32], u64)>,
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
  /// The green's head when this checkpoint was taken and its identity there (A-257): a recovery's replay must reach that
  /// identity when it reaches that version, or the green is fenced. `None` where the taker did not record one.
  pub head_check: Option<(u64, [u8; 32])>,
}

impl Checkpoint {
  /// Sorts every table by its key, so the encoding is one sequence whatever order the engine walked its histories in.
  pub fn canonicalize(&mut self) {
    self.accepted.sort();
    self.files.sort_by(|a, b| a.key.cmp(&b.key));
    self.dirs.sort_by(|a, b| a.key.cmp(&b.key));
    self.modes.sort_by(|a, b| a.key.cmp(&b.key));
    self.symlinks.sort_by(|a, b| a.key.cmp(&b.key));
    self.hardlinks.sort_by(|a, b| a.key.cmp(&b.key));
    self.specials.sort_by(|a, b| a.key.cmp(&b.key));
    self.xattrs.sort_by(|a, b| a.key.cmp(&b.key));
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
    put_table(
      &mut out,
      &canonical.files,
      |out, path| put_bytes(out, path.as_bytes()),
      |out, bytes| {
        put_bytes(out, bytes);
      },
    );
    put_table(
      &mut out,
      &canonical.dirs,
      |out, path| put_bytes(out, path.as_bytes()),
      |_, ()| {},
    );
    put_table(
      &mut out,
      &canonical.modes,
      |out, path| put_bytes(out, path.as_bytes()),
      |out, mode| {
        out.extend_from_slice(&mode.to_le_bytes());
      },
    );
    put_table(
      &mut out,
      &canonical.symlinks,
      |out, path| put_bytes(out, path.as_bytes()),
      |out, target| {
        put_bytes(out, target.as_bytes());
      },
    );
    put_table(
      &mut out,
      &canonical.hardlinks,
      |out, path| put_bytes(out, path.as_bytes()),
      |out, target| {
        put_bytes(out, target.as_bytes());
      },
    );
    put_table(
      &mut out,
      &canonical.specials,
      |out, path| put_bytes(out, path.as_bytes()),
      |out, node| {
        out.extend_from_slice(&node.encode());
      },
    );
    put_table(
      &mut out,
      &canonical.xattrs,
      |out, (path, name)| {
        put_bytes(out, path.as_bytes());
        put_bytes(out, name.as_bytes());
      },
      |out, value| put_bytes(out, value),
    );
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
    let mut checkpoint = Checkpoint {
      version: reader.u64()?,
      ..Checkpoint::default()
    };
    for _ in 0..take_count(&mut reader)? {
      let mut identity = [0u8; IDENTITY_BYTES];
      identity.copy_from_slice(reader.bytes(IDENTITY_BYTES)?);
      checkpoint.accepted.push((identity, reader.u64()?));
    }
    checkpoint.files = take_table(&mut reader, take_str, |r| Ok(reader_bytes(r)?.to_vec()))?;
    checkpoint.dirs = take_table(&mut reader, take_str, |_| Ok(()))?;
    checkpoint.modes = take_table(&mut reader, take_str, Reader::u32)?;
    checkpoint.symlinks = take_table(&mut reader, take_str, take_str)?;
    checkpoint.hardlinks = take_table(&mut reader, take_str, take_str)?;
    checkpoint.specials = take_table(&mut reader, take_str, |r| {
      SpecialNode::decode(r.bytes(crate::special::ENCODED_BYTES)?)
    })?;
    checkpoint.xattrs = take_table(
      &mut reader,
      |r| Ok((take_str(r)?, take_str(r)?)),
      |r| Ok(reader_bytes(r)?.to_vec()),
    )?;
    checkpoint.head_check = match reader.u8()? {
      PRESENT => {
        let head = reader.u64()?;
        let mut identity = [0u8; IDENTITY_BYTES];
        identity.copy_from_slice(reader.bytes(IDENTITY_BYTES)?);
        Some((head, identity))
      }
      ABSENT => None,
      _ => return Err(DocDecodeError::BadKind),
    };
    if !reader.is_empty() {
      return Err(DocDecodeError::TrailingBytes);
    }
    Ok(checkpoint)
  }

  /// The checkpoint's identity: the BLAKE3 of its canonical bytes.
  pub fn identity(&self) -> [u8; 32] {
    *blake3::hash(&self.encode()).as_bytes()
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

/// The inverse of [`put_table`]: a presence byte other than [`PRESENT`] or [`ABSENT`] is refused.
fn take_table<'a, K, V>(
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
  Ok(table)
}
