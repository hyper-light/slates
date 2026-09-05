//! The NFSv3 file handle codec (§4.6): slates' private encoding of a volume object's durable
//! identity. The design fixes the contents — "file handles encode `(volume, inode no, gen)`" —
//! and NFS is stateless: the client caches a handle and presents it on a later, unrelated call,
//! possibly after a daemon restart, so the handle must name the same object with no server-side
//! table. This module is the pure, transport-free encode and decode over [`crate::nfs::Nfsfh3`]
//! (the opaque wire container): round-trip, golden-vector and hostile-input tested with no
//! socket and no volume, exactly as the XDR, RPC and NFSv3-type layers are.
//!
//! A version byte leads the handle so a handle minted by an incompatible daemon build is refused
//! (a typed [`FileHandleError`], mapped to `NFS3ERR_BADHANDLE` by the procedures), never misread
//! as a different object — the same discipline slates applies to its Wire types, where a tag
//! "travels in front of it so a changed definition is refused, never misread"
//! (`crates/db/src/catalog.rs`), and the Linux in-kernel NFS server applies by versioning its
//! own handles (evidence C: `fs/nfsd/nfsfh.h`).

use slates_db::catalog::VolumeId;

use crate::nfs::Nfsfh3;

/// Format: the file-handle format version this daemon writes and accepts. A handle whose leading
/// byte is not this value is refused, not decoded, so a format change across builds cannot route
/// a call to the wrong object.
const FH_VERSION: u8 = 1;

/// Format: the length in bytes of a version-1 file handle — the version byte, the volume id, the
/// inode number and the generation, in that order. Derived from the field sizes so it cannot
/// drift from the layout; comfortably within [`crate::nfs::MAX_FH`] (the 64-byte `NFS3_FHSIZE`).
const FH_LEN: usize = size_of::<u8>() + size_of::<VolumeId>() + size_of::<u64>() + size_of::<u64>();

/// The decoded contents of an NFSv3 file handle: the durable identity of a volume object, exactly
/// the `(volume, inode no, gen)` every bridge request carries (§4.6). The generation makes a
/// reused inode number a distinct handle, so a stale handle to a freed inode is refused rather
/// than answered from whatever now occupies that number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileHandle {
  /// The volume the object lives in.
  pub volume: VolumeId,
  /// The object's inode number within the volume.
  pub inode: u64,
  /// The inode's generation: a reused number with a new generation is a different handle.
  pub generation: u64,
}

/// Why a byte string is not a file handle this daemon minted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileHandleError {
  /// The handle is not the length this format produces (a corrupt, truncated or foreign handle).
  Malformed,
  /// The handle's format version is not the one this daemon writes (an incompatible build).
  UnknownVersion,
}

impl FileHandle {
  /// Encodes the identity into an opaque wire handle: the version byte, the volume id, then the
  /// inode number and generation big-endian (the handle is opaque to the client, so the byte
  /// order is only this server's own convention; big-endian keeps it uniform with the XDR wire).
  pub fn to_fh(&self) -> Nfsfh3 {
    let mut bytes = Vec::with_capacity(FH_LEN);
    bytes.push(FH_VERSION);
    bytes.extend_from_slice(&self.volume.bytes);
    bytes.extend_from_slice(&self.inode.to_be_bytes());
    bytes.extend_from_slice(&self.generation.to_be_bytes());
    Nfsfh3(bytes)
  }

  /// Decodes an opaque wire handle, refusing one of the wrong length or an unknown version rather
  /// than misreading it.
  pub fn from_fh(fh: &Nfsfh3) -> Result<FileHandle, FileHandleError> {
    let bytes = &fh.0;
    if bytes.len() != FH_LEN {
      return Err(FileHandleError::Malformed);
    }
    let mut at = 0;
    if bytes[at] != FH_VERSION {
      return Err(FileHandleError::UnknownVersion);
    }
    at += size_of::<u8>();
    let mut volume = VolumeId::default();
    volume
      .bytes
      .copy_from_slice(&bytes[at..at + size_of::<VolumeId>()]);
    at += size_of::<VolumeId>();
    let inode = read_u64(&bytes[at..at + size_of::<u64>()])?;
    at += size_of::<u64>();
    let generation = read_u64(&bytes[at..at + size_of::<u64>()])?;
    Ok(FileHandle {
      volume,
      inode,
      generation,
    })
  }
}

/// Reads a big-endian `u64` from exactly eight bytes; the length is checked by the caller, so a
/// short slice is the malformed refusal rather than a panic.
fn read_u64(bytes: &[u8]) -> Result<u64, FileHandleError> {
  let word: [u8; size_of::<u64>()] = bytes.try_into().map_err(|_| FileHandleError::Malformed)?;
  Ok(u64::from_be_bytes(word))
}
