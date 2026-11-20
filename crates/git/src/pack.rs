//! A pack's header and trailer (A-591; §4.17.2 "Pieces (git)", pieces 2 and 3: "checking the trailer"), and its
//! index, [`idx`].
//!
//! **The format** [B: git's `gitformat-pack.adoc`, "pack-*.pack files have the following format"; C: `pack.h`,
//! `index-pack.c`'s `parse_pack_header`, `packfile.c`'s `open_packed_git_1`; git 2.50.1, read 2026-11-19]. A pack opens
//! with a header of twelve bytes: the signature `PACK`, a version, and the number of objects that follow, the last two
//! 4-byte numbers in network order. git "currently accepts version number 2 or 3 but generates version 2 only", and
//! reads version 3 as version 2. The objects follow back to back. The trailer is the checksum of every byte before it,
//! in the repository's `object-format`: "In a repository using the traditional SHA-1, pack checksums, index checksums,
//! and object IDs (object names) mentioned below are all computed using SHA-1. Similarly, in SHA-256 repositories,
//! these values are computed using SHA-256" [B: gitformat-pack, "Checksums and object IDs"]. The objects themselves
//! (their entry headers, inflation and deltas) are the pack reader's (piece 3): nothing here reads one.
//!
//! **What it does.**
//! - [`read_header`] reads the header from the bytes a stream has delivered so far, as a push's pack arrives
//!   (§4.17.2 "A push", item 3). A signature byte is refused as soon as it arrives wrong, the version as soon as its four
//!   bytes have, and the answer is `None` until all twelve have.
//! - [`check`] checks a whole pack held in memory, such as a fragment kept as content (§4.17.2 "The repository in
//!   git's terms", item 4). Its trailer is verified before any byte it guards is read, then its header is read.
//! - [`Writer`] writes a pack as git does: the header at version 2, the caller's objects, then the trailer, computed
//!   as the bytes stream out (§4.17.2 "A fetch", item 3).
//!
//! **Checksums** are taken through [`Hasher`] (A-600), the hash the crate names objects with, so a SHA-1 checksum runs
//! git's collision detection too. A checksum whose input carries a known attack's disturbance is refused by kind
//! ([`Refusal::Collision`]). git 2.50.1 hashes its files' checksums with `unsafe_hash_algo` (`csum-file.c`), which
//! `hash.h` makes the collision-detecting SHA-1 unless a build names a separate fast one, so a default build of git
//! dies on such a checksum too. No published collision begins with a pack's first bytes, so no golden vector reaches
//! that refusal (GAPS, the §4.17 entry).
//!
//! **Hostile input (§4.9).** Every byte is untrusted. Each refusal is a typed variant naming where it stopped. Nothing
//! here allocates beyond the hash's own state.

pub mod idx;

use crate::entry::PACK_HEADER_LEN;
use crate::object_name::{CollisionAttack, Hasher, ObjectFormat, ObjectId};
use crate::out::Out;
use crate::to_u64;

/// Format: the bytes a pack opens with, git's `PACK_SIGNATURE` [B: gitformat-pack: "The signature is: {'P', 'A',
/// 'C', 'K'}"].
pub const SIGNATURE: [u8; 4] = *b"PACK";

/// Format: the version git writes, `PACK_VERSION` [B: gitformat-pack: "generates version 2 only"].
const VERSION_TWO: u32 = 2;

/// Format: the other version git reads, as version 2 (`pack.h`'s `pack_version_ok`) [B: gitformat-pack: "accepts
/// version number 2 or 3"].
const VERSION_THREE: u32 = 3;

/// Format: the bytes of the header's version and of its count, each a 4-byte number in network order [B:
/// gitformat-pack].
const WORD: usize = core::mem::size_of::<u32>();

/// Derived: where the version begins, after the signature.
const VERSION_AT: usize = SIGNATURE.len();

/// Derived: where the count begins, after the version. The header ends with the count, at A-582's
/// [`PACK_HEADER_LEN`], one number for the pack's header and for its entries' bases: the first object begins there, so
/// no object of a pack begins before it.
const COUNT_AT: usize = VERSION_AT.saturating_add(WORD);

/// A pack's version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Version {
  /// Version 2, the one git writes.
  Two,
  /// Version 3, which git reads as version 2 and never writes.
  Three,
}

/// A pack's header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
  /// Its version.
  pub version: Version,
  /// The number of objects it says follow.
  pub objects: u32,
}

/// Why bytes are not a pack. Each names where it stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
  /// A signature byte other than `PACK`'s (git's "pack signature mismatch").
  Signature {
    /// Where the byte is.
    at: u64,
  },
  /// A version other than 2 and 3 (git's "pack version %u unsupported").
  Version {
    /// The version the header names.
    version: u32,
  },
  /// A whole pack shorter than its header and its trailer.
  Truncated {
    /// The bytes a pack of no objects takes.
    needed: u64,
    /// The bytes there were.
    left: u64,
  },
  /// The trailer is not the checksum of the bytes before it (git's "pack is corrupted (SHA1 mismatch)").
  Checksum,
  /// The bytes the trailer guards carry a known SHA-1 collision attack's disturbance: refused before the trailer is
  /// compared, as git's collision detection refuses them.
  Collision(CollisionAttack),
}

/// A whole pack whose trailer and header are checked, lent in place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pack<'a> {
  /// Its header.
  header: Header,
  /// Its trailer, the checksum of every byte before it.
  checksum: ObjectId,
  /// The whole pack, header and trailer included, so an offset into it is the offset an index names.
  bytes: &'a [u8],
}

impl<'a> Pack<'a> {
  /// The hash its trailer is in, its repository's `object-format`.
  pub fn format(&self) -> ObjectFormat {
    self.checksum.format()
  }

  /// Its header.
  pub fn header(&self) -> Header {
    self.header
  }

  /// Its trailer: the checksum of every byte before it, which its index keeps a copy of.
  pub fn checksum(&self) -> ObjectId {
    self.checksum
  }

  /// The whole pack.
  pub fn bytes(&self) -> &'a [u8] {
    self.bytes
  }

  /// Where its trailer begins: the end of its last object.
  pub fn trailer_at(&self) -> usize {
    self.bytes.len().saturating_sub(self.format().raw_len())
  }

  /// Its objects, back to back: every byte between the header and the trailer.
  pub fn objects(&self) -> &'a [u8] {
    usize::try_from(PACK_HEADER_LEN)
      .ok()
      .and_then(|start| self.bytes.get(start..self.trailer_at()))
      .unwrap_or_default()
  }
}

/// The 4-byte number at `at` in `bytes`, if all four have arrived.
fn word(bytes: &[u8], at: usize) -> Option<u32> {
  let end = at.checked_add(WORD)?;
  let digits: [u8; WORD] = bytes.get(at..end)?.try_into().ok()?;
  Some(u32::from_be_bytes(digits))
}

/// The checksum `format` takes of `bytes`: what a pack's trailer and an index's last two hashes hold.
pub(crate) fn checksum_of(format: ObjectFormat, bytes: &[u8]) -> Result<ObjectId, CollisionAttack> {
  let mut hasher = Hasher::raw(format);
  hasher.update(bytes);
  hasher.finish()
}

/// Reads a pack's header from the first bytes of a stream: `None` until its last byte has arrived, a refusal as soon
/// as a signature byte or the version is wrong.
pub fn read_header(bytes: &[u8]) -> Result<Option<Header>, Refusal> {
  for (at, (byte, expected)) in bytes.iter().zip(SIGNATURE.iter()).enumerate() {
    if byte != expected {
      return Err(Refusal::Signature { at: to_u64(at) });
    }
  }
  let Some(version) = word(bytes, VERSION_AT) else {
    return Ok(None);
  };
  let version = match version {
    VERSION_TWO => Version::Two,
    VERSION_THREE => Version::Three,
    other => return Err(Refusal::Version { version: other }),
  };
  let Some(objects) = word(bytes, COUNT_AT) else {
    return Ok(None);
  };
  Ok(Some(Header { version, objects }))
}

/// Checks a whole pack held in memory, of `format`'s hash: long enough for a header and a trailer, its trailer the
/// checksum of the bytes before it, verified before any of them is read, then its header.
pub fn check(bytes: &[u8], format: ObjectFormat) -> Result<Pack<'_>, Refusal> {
  let truncated = Refusal::Truncated {
    needed: PACK_HEADER_LEN.saturating_add(to_u64(format.raw_len())),
    left: to_u64(bytes.len()),
  };
  let trailer_at = bytes
    .len()
    .checked_sub(format.raw_len())
    .filter(|at| to_u64(*at) >= PACK_HEADER_LEN)
    .ok_or(truncated)?;
  let (guarded, trailer) = bytes.split_at_checked(trailer_at).ok_or(truncated)?;
  let checksum = checksum_of(format, guarded).map_err(Refusal::Collision)?;
  if checksum.as_bytes() != trailer {
    return Err(Refusal::Checksum);
  }
  let header = read_header(guarded)?.ok_or(truncated)?;
  Ok(Pack {
    header,
    checksum,
    bytes,
  })
}

/// Bytes put to an output and into a checksum both, the checksum then written after them: how a pack's trailer and an
/// index's own checksum are made, as git's `hashwrite` and `finalize_hashfile` make them (`csum-file.c`).
pub(crate) struct Checksummed<'o, O: Out> {
  /// Where the bytes go.
  out: &'o mut O,
  /// Their checksum so far.
  hasher: Hasher,
}

impl<'o, O: Out> Checksummed<'o, O> {
  /// Puts nothing yet, in `format`'s hash.
  pub(crate) fn new(format: ObjectFormat, out: &'o mut O) -> Self {
    Self {
      out,
      hasher: Hasher::raw(format),
    }
  }

  /// Writes the checksum of every byte put so far after them, and answers it. On a collision refusal the checksum is
  /// not written, so what was written is no whole file and the caller lets it go.
  pub(crate) fn finish(self) -> Result<ObjectId, CollisionAttack> {
    let Self { out, hasher } = self;
    let checksum = hasher.finish()?;
    out.put(checksum.as_bytes());
    Ok(checksum)
  }
}

impl<O: Out> Out for Checksummed<'_, O> {
  fn put(&mut self, bytes: &[u8]) {
    self.hasher.update(bytes);
    self.out.put(bytes);
  }
}

/// Puts the header of a pack of `objects` objects at version 2: what [`Writer::new`] writes, and what a writer that
/// streams a pack across turns and keeps its own checksum writes first (A-639's upload-pack stream).
pub(crate) fn put_header(objects: u32, out: &mut impl Out) {
  out.put(&SIGNATURE);
  out.put(&VERSION_TWO.to_be_bytes());
  out.put(&objects.to_be_bytes());
}

/// Writes a pack as git does: [`Writer::new`] writes the header at version 2, the caller puts the objects through
/// [`Out`], and [`Writer::finish`] writes the trailer, the checksum of every byte before it, computed as they passed.
pub struct Writer<'o, O: Out>(Checksummed<'o, O>);

impl<'o, O: Out> Writer<'o, O> {
  /// Begins a pack of `objects` objects in `format`'s hash: writes its header.
  pub fn new(format: ObjectFormat, objects: u32, out: &'o mut O) -> Self {
    let mut checksummed = Checksummed::new(format, out);
    put_header(objects, &mut checksummed);
    Self(checksummed)
  }

  /// Ends the pack: writes its trailer and answers it. On a collision refusal no trailer is written, and the caller
  /// lets what was written go.
  pub fn finish(self) -> Result<ObjectId, CollisionAttack> {
    self.0.finish()
  }
}

impl<O: Out> Out for Writer<'_, O> {
  fn put(&mut self, bytes: &[u8]) {
    self.0.put(bytes);
  }
}

#[cfg(test)]
#[path = "tests/pack.rs"]
pub(crate) mod tests;
