//! A git push's names kept as one content, for record version 3 (A-724 piece 8c; §4.17.5 "The partition's snapshot,
//! sized by its data"; the shape agreed with w-perf for A-727 (b), 2026-11-20).
//!
//! **Why.** Version 2 of a push's record (`super::record`) names every object the push brought: its type, its content
//! identity and its length, 41 bytes a name, about 410 MiB for the Linux kernel's ten million objects against a
//! 746,962,928-byte snapshot slot on the Studio. Version 3 keeps the names as one content instead, put whole beside the
//! pack under the same barrier, and the record holds the names' root identity and their count: about 40 bytes, whatever
//! the push's size. A replay reads them back by identity and checks them against the pack as it checks version 2's.
//!
//! **The form** ([`names_to_content`]). Each name is one row of [`NAME_BYTES`]: its type's number as a pack names it (one
//! byte), its content identity, and its length little-endian (as `super::record` encodes a name), the rows in the order
//! the push admitted its objects, put whole through `slates_vcs::rows` (`ContentStore::put_object`: cut at the store's
//! window when longer than one, named by its root). [`names_from_content`] reads them back by root and count, checking
//! the length before it allocates anything and every type byte, each refusal typed ([`NamesRefused`]), never a panic:
//! the bytes are the store's, but a replay reads what a segment held.
//!
//! **Pure.** Both functions touch only the content store they are handed: no record, entry or barrier. A-727 (b) calls
//! them from `super::record`'s version 3 and owns the barrier and the release of the root when the record is dropped.

use slates_git::object_name::Kind;
use slates_git::store::kind_of;
use slates_vcs::content::IDENTITY_BYTES;
use slates_vcs::rows::{self, RowsRefused};
use slates_vcs::{ContentId, ContentStore};

/// Format: a name's row: its type's number (1 byte), its content identity, its length (8 bytes, little-endian).
pub(crate) const NAME_BYTES: usize = 1 + IDENTITY_BYTES + size_of::<u64>();

/// A name of a push's object: its type, its content identity, its length (`super::record::Record::names`).
pub(crate) type Name = (Kind, ContentId, u64);

/// Why names were not put or read (module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum NamesRefused {
  /// The rows' content was refused: its length is not the count's, the store does not hold it whole, or the store
  /// refused it.
  Rows(RowsRefused),
  /// The memory for the rows or the names read back was refused.
  Allocation {
    /// The bytes asked.
    bytes: u64,
  },
  /// The row at `at` names a type git does not number.
  Kind {
    /// The row's position.
    at: u64,
    /// The byte it holds.
    number: u8,
  },
}

/// Puts `names`, in their order, into `store` as one content (module docs): its root identity, held once more for the
/// caller, and the count of rows, which with the root is all a record keeps.
pub(crate) fn names_to_content(
  store: &mut ContentStore,
  names: &[Name],
) -> Result<(ContentId, u64), NamesRefused> {
  let mut bytes = Vec::new();
  bytes
    .try_reserve_exact(names.len().saturating_mul(NAME_BYTES))
    .map_err(|_| NamesRefused::Allocation {
      bytes: u64::try_from(names.len().saturating_mul(NAME_BYTES)).unwrap_or(u64::MAX),
    })?;
  for (kind, identity, len) in names {
    bytes.push(kind.pack_number());
    bytes.extend_from_slice(&identity.0);
    bytes.extend_from_slice(&len.to_le_bytes());
  }
  let root = rows::put(store, &bytes).map_err(NamesRefused::Rows)?;
  Ok((root, u64::try_from(names.len()).unwrap_or(u64::MAX)))
}

/// The `count` names the content `root` holds in `store`, in the order they were put (module docs): refused typed when
/// the content is missing, of another length than `count` rows, or a row names no type git numbers.
pub(crate) fn names_from_content(
  store: &ContentStore,
  root: &ContentId,
  count: u64,
) -> Result<Vec<Name>, NamesRefused> {
  let bytes = rows::read(store, root, count, NAME_BYTES).map_err(NamesRefused::Rows)?;
  let cut = rows::rows_of(&bytes, count, NAME_BYTES).map_err(NamesRefused::Rows)?;
  let mut names = Vec::new();
  names
    .try_reserve_exact(cut.len())
    .map_err(|_| NamesRefused::Allocation {
      bytes: u64::try_from(cut.len().saturating_mul(size_of::<Name>())).unwrap_or(u64::MAX),
    })?;
  for (at, row) in (0u64..).zip(cut) {
    let (number, rest) = row
      .split_first()
      .ok_or(NamesRefused::Kind { at, number: 0 })?;
    let kind = kind_of(*number).ok_or(NamesRefused::Kind {
      at,
      number: *number,
    })?;
    let (identity, len) = rest
      .split_at_checked(IDENTITY_BYTES)
      .ok_or(NamesRefused::Kind {
        at,
        number: *number,
      })?;
    let identity: [u8; IDENTITY_BYTES] = identity.try_into().map_err(|_| NamesRefused::Kind {
      at,
      number: *number,
    })?;
    let len: [u8; size_of::<u64>()] = len.try_into().map_err(|_| NamesRefused::Kind {
      at,
      number: *number,
    })?;
    names.push((kind, ContentId(identity), u64::from_le_bytes(len)));
  }
  Ok(names)
}

#[cfg(test)]
mod tests {
  use super::{NAME_BYTES, Name, NamesRefused, names_from_content, names_to_content};
  use proptest::prelude::*;
  use slates_git::object_name::Kind;
  use slates_vcs::rows::RowsRefused;
  use slates_vcs::{ContentId, ContentStore};

  /// Shape: a small window, so names span several windows and their root is a list.
  const WINDOW: usize = 512;
  /// Shape: a budget no test meets.
  const BUDGET: u64 = 1 << 30;

  /// Any kind alike. The union is built as `prop_oneof!` builds one of more than ten arms, from boxed strategies: its
  /// shorter forms hold their arms in `Arc`s (R2).
  fn kind_strategy() -> impl Strategy<Value = Kind> {
    proptest::strategy::Union::new([
      Just(Kind::Commit).boxed(),
      Just(Kind::Tree).boxed(),
      Just(Kind::Blob).boxed(),
      Just(Kind::Tag).boxed(),
    ])
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::with_cases(200)))]

    /// A-724 piece 8c, record v3's names (agreed with w-perf). Do: put any names (none, a window's, many windows') and
    /// read them back by root and count. Expect: the names as put, in order; one root for the same names put twice; the
    /// root's content exactly 41 bytes a name; another count refused before anything past it is read.
    #[test]
    fn a_pushs_names_put_as_one_content_read_back_in_their_order(
      names in proptest::collection::vec((kind_strategy(), any::<[u8; 32]>(), any::<u64>()), 0..150),
    ) {
      let names: Vec<Name> = names.into_iter().map(|(kind, id, len)| (kind, ContentId(id), len)).collect();
      let mut store = ContentStore::new(BUDGET, WINDOW).unwrap();
      let (root, count) = names_to_content(&mut store, &names).unwrap();
      prop_assert_eq!(count, names.len() as u64);
      prop_assert_eq!(&names_from_content(&store, &root, count).unwrap(), &names);
      prop_assert_eq!(names_to_content(&mut store, &names).unwrap(), (root, count));
      let whole = store.object_bytes(&root, count * NAME_BYTES as u64).unwrap();
      prop_assert_eq!(whole.len(), names.len() * NAME_BYTES);
      let refused = matches!(
        names_from_content(&store, &root, count + 1),
        Err(NamesRefused::Rows(RowsRefused::Length { .. } | RowsRefused::Missing))
      );
      prop_assert!(refused);
    }
  }

  /// A-724 piece 8c, hostile rows. Do: read a content whose row names type 5 (git numbers 1 to 4 and the deltas 6 and 7),
  /// and a root the store does not hold. Expect: the type refused with its row and byte; the root refused `Missing`.
  #[test]
  fn a_row_naming_no_git_type_and_an_absent_root_are_refused_typed() {
    let mut store = ContentStore::new(BUDGET, WINDOW).unwrap();
    let mut bytes = vec![0u8; NAME_BYTES * 2];
    bytes[0] = 3;
    bytes[NAME_BYTES] = 5;
    let root = store.put_object(&bytes).unwrap();
    assert_eq!(
      names_from_content(&store, &root, 2),
      Err(NamesRefused::Kind { at: 1, number: 5 })
    );
    let absent = ContentId::of(b"never put");
    assert_eq!(
      names_from_content(&store, &absent, 1),
      Err(NamesRefused::Rows(RowsRefused::Missing))
    );
  }
}
