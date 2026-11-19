//! Golden tests of a store's names (A-630): the two stores stock Mercurial 7.2 wrote of one history, a dotencode store
//! and a hybrid one (`fixtures/store`), every name in their fncaches encoded to exactly the files 7.2 stored.

use std::collections::BTreeSet;

use crate::Admission;
use crate::fixtures::store::{file, find};
use crate::store::{encode_path, read_fncache, read_layout};

/// The lines of a fixture text file.
fn listed(name: &str) -> Vec<Vec<u8>> {
  crate::lines::lines(file(name)).map(<[u8]>::to_vec).collect()
}

/// Bytes from hexadecimal.
fn unhex(hex: &[u8]) -> Vec<u8> {
  let (pairs, _) = hex.as_chunks::<2>();
  pairs.iter().map(|pair| crate::quote::hex_pair(pair).unwrap()).collect()
}

/// The data revlogs a store's file list holds: under `data/`, or hashed under `dh/`.
fn data_files(repository: &str) -> BTreeSet<Vec<u8>> {
  listed(&format!("{repository}-files.txt"))
    .into_iter()
    .filter(|name| name.starts_with(b"data/") || name.starts_with(b"dh/"))
    .collect()
}

/// A-630 (golden): do read each store's requirements and fncache and encode every entry as its store encodes it.
/// Expect the dotencode store's layout and the hybrid one's, every entry's name exactly a data revlog 7.2 stored (the
/// encoded set equal to the stored set, each name a fixture file), every path the history tracked listed, and the
/// names the fixtures were made for among them: three hashed, and `FOO` beside `foo`.
#[test]
fn every_fncache_entry_names_a_file_7_2_stored() {
  for (repository, dotencode) in [("dot", true), ("hybrid", false)] {
    let layout = read_layout(file(&format!("{repository}-requires.txt")));
    assert_eq!((layout.store, layout.fncache, layout.dotencode), (true, true, dotencode));
    let entries = read_fncache(file(&format!("{repository}/fncache")), &mut Admission::new(1 << 20)).unwrap();
    let encoded: BTreeSet<Vec<u8>> = entries.iter().map(|entry| encode_path(entry, dotencode).unwrap()).collect();
    let stored = data_files(repository);
    assert_eq!(encoded, stored, "{repository}");
    for name in &stored {
      let name = core::str::from_utf8(name).unwrap();
      assert!(find(&format!("{repository}/{name}")).is_some(), "{repository}/{name}");
    }
    for path in listed(&format!("{repository}-paths.txt")) {
      let revlog = [b"data/".as_slice(), &unhex(&path), b".i"].concat();
      assert!(entries.contains(&revlog), "{repository}: {}", String::from_utf8_lossy(&revlog));
    }
    assert_eq!(stored.iter().filter(|name| name.starts_with(b"dh/")).count(), 3);
    assert!(stored.contains(b"data/_f_o_o.i".as_slice()) && stored.contains(b"data/foo.i".as_slice()));
  }
}
