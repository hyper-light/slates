//! Tests for the NFSv3 file-handle codec (§4.6, Phase 4). A handle round-trips through its
//! opaque wire form; a reused inode with a new generation is a distinct handle (so a stale
//! handle is refused, not answered from whatever now holds the number); a known identity encodes
//! to exact bytes (golden); and a wrong length or an unknown version is a typed refusal, not a
//! misread. Pure, every host — no socket, no volume, no mount.

use slates_bridge_nfs::handle::{FileHandle, FileHandleError};
use slates_bridge_nfs::nfs::{MAX_FH, Nfsfh3};
use slates_db::catalog::VolumeId;

/// A sample identity with distinct, recognizable field values.
fn sample() -> FileHandle {
  FileHandle {
    volume: VolumeId { bytes: [0x11; 16] },
    inode: 0x0102_0304_0506_0708,
    generation: 0x1122_3344_5566_7788,
  }
}

/// A file handle round-trips through its opaque wire form and stays within the size cap.
#[test]
fn a_handle_round_trips_within_the_size_cap() {
  let handle = sample();
  let fh = handle.to_fh();
  // version(1) + volume(16) + inode(8) + generation(8).
  assert_eq!(fh.0.len(), 33, "a version-1 handle is 33 bytes");
  assert!(fh.0.len() <= MAX_FH, "a handle fits the NFS3_FHSIZE cap");
  assert_eq!(FileHandle::from_fh(&fh).unwrap(), handle);
}

/// The encoding is golden: a known identity produces exactly these bytes.
#[test]
fn a_handle_is_golden() {
  let fh = sample().to_fh();
  let mut expected = vec![1u8]; // version
  expected.extend_from_slice(&[0x11; 16]); // volume
  expected.extend_from_slice(&[0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]); // inode, big-endian
  expected.extend_from_slice(&[0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88]); // generation
  assert_eq!(fh.0, expected);
}

/// A reused inode number with a new generation is a different handle (the design's staleness
/// property): the bytes differ and each decodes back to its own identity.
#[test]
fn a_reused_inode_with_a_new_generation_is_a_different_handle() {
  let first = sample();
  let mut second = first;
  second.generation += 1;
  assert_ne!(first.to_fh().0, second.to_fh().0);
  assert_ne!(
    FileHandle::from_fh(&first.to_fh()).unwrap(),
    FileHandle::from_fh(&second.to_fh()).unwrap()
  );
}

/// A handle of the wrong length is refused, not misread.
#[test]
fn a_wrong_length_handle_is_refused() {
  let short = Nfsfh3(vec![1u8; 10]);
  assert_eq!(FileHandle::from_fh(&short), Err(FileHandleError::Malformed));
  let empty = Nfsfh3(Vec::new());
  assert_eq!(FileHandle::from_fh(&empty), Err(FileHandleError::Malformed));
}

/// A handle of the right length but an unknown format version is refused as such, so a build
/// that changes the format cannot route a call to the wrong object.
#[test]
fn an_unknown_version_handle_is_refused() {
  let mut bytes = sample().to_fh().0;
  bytes[0] = 2; // a version this daemon does not write
  assert_eq!(
    FileHandle::from_fh(&Nfsfh3(bytes)),
    Err(FileHandleError::UnknownVersion)
  );
}
