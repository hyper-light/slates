//! A-629: a repository's LFS objects in `slates-vcs`'s store, named by SHA-256 and checked as they
//! arrive.

use slates_vcs::{Admitted, ContentRefused, ContentStore};

use super::{LfsNaming, LfsObjects, ObjectRefused};
use crate::lfs::Oid;
use crate::lfs::tests::EXAMPLE_OID;

/// Shape: a store's budget, room for every object these tests keep.
const BUDGET: u64 = 1 << 22;

/// Shape: a store's window, small so an object of a few windows is cut into them.
const WINDOW: usize = 4_096;

/// A store with [`BUDGET`] and [`WINDOW`].
fn store() -> ContentStore {
  ContentStore::new(BUDGET, WINDOW).unwrap()
}

/// A-629: do name the bytes `hello world\n` and no bytes. Expect SHA-256's names for them, the
/// first the one the A-610 tests use and the second the empty pointer's.
#[test]
fn an_object_is_named_by_the_sha_256_of_its_bytes() {
  assert_eq!(LfsNaming::oid(b"hello world\n").to_string(), EXAMPLE_OID);
  let empty = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
  assert_eq!(LfsNaming::oid(b"").to_string(), empty);
}

/// A-629: do keep an object under its oid, read it back, and keep it once more. Expect it held at
/// its size with its bytes, kept again as the same object, and one row.
#[test]
fn an_object_is_kept_under_its_oid_and_read_back() {
  let mut store = store();
  let mut objects = LfsObjects::new();
  let bytes = b"large file one\n";
  let oid = LfsNaming::oid(bytes);
  assert_eq!(objects.admit(&mut store, &oid, bytes), Ok(Admitted::New));
  let held = (objects.held(&store, &oid), objects.bytes(&store, &oid));
  assert_eq!(held, (Some(15), Some(bytes.to_vec())));
  assert_eq!(objects.admit(&mut store, &oid, bytes), Ok(Admitted::Again));
  assert_eq!(objects.len(), 1);
}

/// A-629: do keep an object of three windows and a half. Expect its bytes read back whole.
#[test]
fn an_object_longer_than_a_window_is_kept_and_read_back_whole() {
  let mut store = store();
  let mut objects = LfsObjects::new();
  let bytes: Vec<u8> = (0..WINDOW * 7 / 2)
    .map(|at| u8::try_from(at % 251).unwrap())
    .collect();
  let oid = LfsNaming::oid(&bytes);
  assert_eq!(objects.admit(&mut store, &oid, &bytes), Ok(Admitted::New));
  assert_eq!(objects.bytes(&store, &oid), Some(bytes));
}

/// A-629, hostile input: do upload bytes under another object's oid. Expect them refused as not
/// the oid's, nothing held under it, and the store holding no content at all.
#[test]
fn bytes_that_are_not_their_oid_are_refused_and_nothing_is_kept() {
  let mut store = store();
  let mut objects = LfsObjects::new();
  let oid = Oid::from_hex(EXAMPLE_OID.as_bytes()).unwrap();
  let refused = objects.admit(&mut store, &oid, b"not hello world\n");
  assert_eq!(refused, Err(ObjectRefused::Mismatch));
  assert_eq!(
    (objects.held(&store, &oid), objects.is_empty()),
    (None, true)
  );
  assert_eq!((store.len(), store.used()), (0, 0));
}

/// A-629: do upload an object to a store with room for less than its bytes. Expect it refused by
/// the store's budget, typed, and nothing held.
#[test]
fn an_object_past_the_store_s_budget_is_refused() {
  let mut store = ContentStore::new(64, WINDOW).unwrap();
  let mut objects = LfsObjects::new();
  let bytes = vec![7u8; 1_000];
  let oid = LfsNaming::oid(&bytes);
  let refused = objects.admit(&mut store, &oid, &bytes);
  assert!(matches!(
    refused,
    Err(ObjectRefused::Store(ContentRefused::OverBudget { .. }))
  ));
  assert_eq!(objects.held(&store, &oid), None);
}

/// A-629 part 3: do keep two objects, one longer than a window, then give the index back. Expect
/// the store to hold no content and no bytes afterwards: an index given back with its repository
/// keeps nothing behind.
#[test]
fn an_index_given_back_leaves_the_store_holding_nothing() {
  let mut store = store();
  let mut objects = LfsObjects::new();
  let long: Vec<u8> = (0..WINDOW * 3)
    .map(|at| u8::try_from(at % 241).unwrap())
    .collect();
  for bytes in [&b"small object\n"[..], &long[..]] {
    let oid = LfsNaming::oid(bytes);
    assert_eq!(objects.admit(&mut store, &oid, bytes), Ok(Admitted::New));
  }
  assert_ne!(store.len(), 0);
  assert_eq!(objects.release(&mut store), Ok(()));
  assert_eq!((store.len(), store.used()), (0, 0));
}
