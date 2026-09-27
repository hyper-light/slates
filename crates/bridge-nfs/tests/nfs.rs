//! Tests for the NFSv3 core data types (RFC 1813; §4.6, Phase 4). Attributes, times, handles and
//! optional attributes round-trip through XDR; a file handle past the size cap and an unknown file
//! type are refused; a status and an attribute structure encode to the exact bytes the protocol
//! defines (golden vectors). Pure, every host — no socket, no mount.

use slates_bridge_nfs::nfs::{Fattr3, Ftype3, Nfsfh3, Nfsstat3, Nfstime3, PostOpAttr, Specdata3};
use slates_bridge_nfs::xdr::{XdrError, XdrReader, XdrWriter};

/// A sample regular-file attribute set with distinct field values.
fn sample_attr() -> Fattr3 {
  Fattr3 {
    kind: Ftype3::Reg,
    mode: 0o644,
    nlink: 1,
    uid: 501,
    gid: 20,
    size: 4096,
    used: 4096,
    rdev: Specdata3::default(),
    fsid: 0x1122_3344,
    fileid: 42,
    atime: Nfstime3 {
      seconds: 1_700_000_000,
      nseconds: 1,
    },
    mtime: Nfstime3 {
      seconds: 1_700_000_001,
      nseconds: 2,
    },
    ctime: Nfstime3 {
      seconds: 1_700_000_002,
      nseconds: 3,
    },
    v4: None,
  }
}

/// `fattr3` round-trips through XDR.
#[test]
fn fattr3_round_trips() {
  let attr = sample_attr();
  let mut w = XdrWriter::new();
  attr.encode(&mut w);
  let bytes = w.into_bytes();
  assert_eq!(bytes.len(), 84, "fattr3 is a fixed 84-byte structure");
  let decoded = Fattr3::decode(&mut XdrReader::new(&bytes)).unwrap();
  assert_eq!(decoded, attr);
}

/// `post_op_attr` round-trips both present and absent.
#[test]
fn post_op_attr_round_trips() {
  for present in [Some(sample_attr()), None] {
    let value = PostOpAttr(present);
    let mut w = XdrWriter::new();
    value.encode(&mut w);
    let decoded = PostOpAttr::decode(&mut XdrReader::new(&w.into_bytes())).unwrap();
    assert_eq!(decoded, value);
  }
}

/// A file handle round-trips and one past the size cap is refused.
#[test]
fn file_handle_round_trips_and_caps_size() {
  let fh = Nfsfh3(vec![0xab; 32]);
  let mut w = XdrWriter::new();
  fh.encode(&mut w);
  assert_eq!(
    Nfsfh3::decode(&mut XdrReader::new(&w.into_bytes())).unwrap(),
    fh
  );

  // A handle claiming 128 bytes (over the 64-byte cap) is refused.
  let mut over = XdrWriter::new();
  over.opaque(&[0u8; 128]);
  assert_eq!(
    Nfsfh3::decode(&mut XdrReader::new(&over.into_bytes())),
    Err(XdrError::BadLength)
  );
}

/// An unknown file type in `fattr3` is refused, not decoded to a wrong kind.
#[test]
fn an_unknown_file_type_is_refused() {
  let mut w = XdrWriter::new();
  w.u32(99); // not a valid ftype3
  // enough trailing bytes for the rest of fattr3 so the failure is the type, not truncation
  for _ in 0..24 {
    w.u32(0);
  }
  assert_eq!(
    Fattr3::decode(&mut XdrReader::new(&w.into_bytes())),
    Err(XdrError::BadLength)
  );
  assert_eq!(Ftype3::from_wire(99), None);
  assert_eq!(Ftype3::from_wire(2), Some(Ftype3::Dir));
}

/// The status and time wire values are the protocol's (golden).
#[test]
fn status_and_time_are_golden() {
  assert_eq!(Nfsstat3::Ok.wire(), 0);
  assert_eq!(Nfsstat3::Noent.wire(), 2);
  assert_eq!(Nfsstat3::Notempty.wire(), 66);
  assert_eq!(Nfsstat3::Badhandle.wire(), 10001);

  let time = Nfstime3 {
    seconds: 0x0102_0304,
    nseconds: 0x0506_0708,
  };
  let mut w = XdrWriter::new();
  time.encode(&mut w);
  assert_eq!(w.into_bytes(), [1, 2, 3, 4, 5, 6, 7, 8]);
}

/// A-38: an NFSv3 `fattr3` is exactly RFC 1813's 84 bytes; in the NFSv4 front end's dialect the change
/// counter and the full-range times follow them and round-trip, and a `wcc_data` carries it in both halves; the v3 reader of a
/// v3 reply is unchanged.
#[test]
fn the_front_end_dialect_carries_the_change_counter_and_nfsv3_is_unchanged() {
  use slates_bridge_nfs::nfs::{V4Attrs, Wcc, WccAttr};
  /// Format: the size of an RFC 1813 `fattr3`.
  const FATTR3_BYTES: usize = 84;
  let plain = sample_attr();
  let mut writer = XdrWriter::new();
  plain.encode(&mut writer);
  assert_eq!(
    writer.as_slice().len(),
    FATTR3_BYTES,
    "a v3 fattr3 is RFC 1813's structure"
  );
  assert_eq!(
    Fattr3::decode(&mut XdrReader::new(writer.as_slice())).unwrap(),
    plain
  );

  let counted = Fattr3 {
    v4: Some(V4Attrs {
      change: 0x0102_0304_0506_0708,
      atime_ns: -1,
      mtime_ns: 1 << 62,
      ctime_ns: 0,
    }),
    ..plain
  };
  let mut writer = XdrWriter::new();
  counted.encode(&mut writer);
  assert_eq!(writer.as_slice().len(), FATTR3_BYTES + 4 * 8);
  assert_eq!(
    &writer.as_slice()[FATTR3_BYTES..],
    &[
      1, 2, 3, 4, 5, 6, 7, 8, // the change counter
      0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
      0xff, // atime: one nanosecond before the epoch
      0x40, 0, 0, 0, 0, 0, 0, 0, // mtime: 2^62 nanoseconds
      0, 0, 0, 0, 0, 0, 0, 0, // ctime: the epoch
    ],
    "the counter and the full-range times follow the fattr3 fields, big-endian"
  );
  assert_eq!(
    Fattr3::decode_with_change(&mut XdrReader::new(writer.as_slice())).unwrap(),
    counted
  );

  let wcc = Wcc {
    pre: Some(WccAttr {
      size: 4096,
      mtime: plain.mtime,
      ctime: plain.ctime,
      change: Some(7),
    }),
    post: Some(Fattr3 {
      v4: Some(V4Attrs {
        change: 8,
        atime_ns: 1,
        mtime_ns: 2,
        ctime_ns: 3,
      }),
      ..plain
    }),
  };
  let mut writer = XdrWriter::new();
  wcc.encode(&mut writer);
  assert_eq!(
    Wcc::decode_with_change(&mut XdrReader::new(writer.as_slice())).unwrap(),
    wcc
  );
}
