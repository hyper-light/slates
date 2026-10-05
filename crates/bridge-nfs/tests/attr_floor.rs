//! A-90: the smallest an entry's attributes can encode to (`attr::encoded_floor`) is a floor for every object, so a
//! READDIR page sized from it never asks its v3 source for fewer entries than the reply can carry.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use proptest::prelude::*;
use slates_bridge_nfs::nfs::{Fattr3, Ftype3, Nfsfh3, Nfstime3, Specdata3, V4Attrs};
use slates_bridge_nfs::v4::attr::{self, FsFigures};
use slates_bridge_nfs::v4::types::Bitmap;
use slates_bridge_nfs::xdr::XdrWriter;

/// Format: the highest minor version the front end serves (4.2).
const MINOR: u32 = 2;
/// Format: `NFS3_FHSIZE`, the longest v3 handle.
const FH_MAX: usize = 64;

fn object() -> impl Strategy<Value = (Fattr3, Nfsfh3)> {
  (
    (any::<u32>(), any::<u32>(), any::<u32>(), any::<u32>()),
    (any::<u64>(), any::<u64>(), any::<u64>(), any::<u64>()),
    (any::<i64>(), any::<u64>()),
    prop::collection::vec(any::<u8>(), 0..=FH_MAX),
  )
    .prop_map(
      |((mode, nlink, uid, gid), (size, used, fsid, fileid), (time, change), handle)| {
        let attrs = Fattr3 {
          kind: Ftype3::Dir,
          mode,
          nlink,
          uid,
          gid,
          size,
          used,
          rdev: Specdata3 {
            specdata1: mode,
            specdata2: nlink,
          },
          fsid,
          fileid,
          atime: Nfstime3::default(),
          mtime: Nfstime3::default(),
          ctime: Nfstime3::default(),
          v4: Some(V4Attrs {
            change,
            atime_ns: time,
            mtime_ns: time,
            ctime_ns: time,
          }),
        };
        (attrs, Nfsfh3(handle))
      },
    )
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::default()))]

  /// A-90: do encode any readable subset of the supported attributes for any object (owners of every length, handles
  /// of every length up to `NFS3_FHSIZE`); expect the encoding never shorter than `encoded_floor` for that subset.
  #[test]
  fn no_object_encodes_below_the_floor(picks in prop::collection::vec(any::<prop::sample::Index>(), 0..12), (attrs, handle) in object()) {
    let supported: Vec<u32> = attr::supported(MINOR).bits().collect();
    let chosen: Vec<u32> = picks.iter().map(|pick| *pick.get(&supported)).collect();
    let requested = Bitmap::of(&chosen);
    prop_assume!(attr::check_readable(&requested).is_ok());
    let mut writer = XdrWriter::new();
    attr::encode((&requested, MINOR), &attrs, &handle, &FsFigures::default(), &mut writer).unwrap();
    prop_assert!(writer.len() >= attr::encoded_floor((&requested, MINOR)), "{chosen:?}");
  }
}
