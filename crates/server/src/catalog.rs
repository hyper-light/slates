//! A volume's catalog register (§4.8 "volume heads, chain versions, landing leases and catalog entries are
//! registers the owner writes under that epoch"; AUD-29-17). The catalog register holds what a successor needs
//! to serve a taken-over volume *as it was*: its mount name, size class, name policy, owner, whether it must
//! live in locked RAM, and the access list. Its object is the volume's id with the register-class bit set
//! (`ObjectId::catalog`), placed and taken over exactly as the volume is, and its sequence is the volume
//! record's `catalog_version`, which every catalog-changing op raises — so each change ships as a newer record
//! and a takeover adopts the newest.
//!
//! Until 2026-09-30 these fields rode the head value, which ships only when a seal places: a grant or a resize
//! made after the last seal never reached a successor, and a takeover rebuilt the volume with no grants and
//! no locked policy.
//!
//! Every register value this daemon writes leads with a **class byte** ([`HEAD_CLASS`], [`MERGE_CLASS`],
//! [`CATALOG_CLASS`]), so a holder or a successor reads a record's class from the record, never by trying
//! decoders in turn.

use slates_db::catalog::{AccessEntry, NamePolicy, Principal, SizeClass, VolumeRecord};
use slates_wire::Wire;

/// Format: the class byte of a volume head register's value (`crate::head::HeadValue`).
pub const HEAD_CLASS: u8 = 1;
/// Format: the class byte of a green's merge register value (`crate::merge_service::MergeRecordValue`).
pub const MERGE_CLASS: u8 = 2;
/// Format: the class byte of a volume catalog register's value ([`CatalogValue`]).
pub const CATALOG_CLASS: u8 = 3;
/// Format: the class byte of a destroyed volume's tombstone value (`crate::tombstone::TombstoneValue`).
pub const TOMBSTONE_CLASS: u8 = 4;

/// Whether a register value of this class is a **single value** a holder keeps only at its newest position
/// (a head, a catalog, a tombstone: phase one adopts only the highest), as opposed to a **ledger** it keeps
/// whole (a green's merge chain, which a successor replays) — AUD-29-43's compaction rule, read from the
/// value's own class byte.
pub(crate) fn is_single_value(bytes: &[u8]) -> bool {
  bytes.first().is_some_and(|class| *class != MERGE_CLASS)
}

/// `body` behind its class byte: a register value's canonical bytes.
pub(crate) fn tagged(class: u8, body: Vec<u8>) -> Vec<u8> {
  let mut bytes = Vec::with_capacity(body.len().saturating_add(1));
  bytes.push(class);
  bytes.extend(body);
  bytes
}

/// The body of a register value of `class`, or `None` when its class byte names another class.
pub(crate) fn untagged(class: u8, bytes: &[u8]) -> Option<&[u8]> {
  let (&found, body) = bytes.split_first()?;
  (found == class).then_some(body)
}

/// What a volume's catalog register holds at one sequence.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct CatalogValue {
  /// The mount name (unique per host), so a successor serves the volume under the same name.
  pub name: String,
  /// The size class, so a successor reserves the same admission.
  pub size: SizeClass,
  /// The name-equivalence policy.
  pub names: NamePolicy,
  /// The owning principal.
  pub owner: Principal,
  /// Whether the content must live in locked RAM: a successor that cannot lock refuses the takeover.
  pub require_locked: bool,
  /// The principals granted rights beyond the owner.
  pub access: Vec<AccessEntry>,
}

impl CatalogValue {
  /// The catalog of the volume `record` describes.
  pub fn of(record: &VolumeRecord) -> CatalogValue {
    CatalogValue {
      name: record.name.clone(),
      size: record.policy.size,
      names: record.policy.names,
      owner: record.owner.clone(),
      require_locked: record.policy.require_locked,
      access: record.access.clone(),
    }
  }

  /// The value's canonical bytes, the catalog record's `value`.
  pub fn to_record_bytes(&self) -> Vec<u8> {
    tagged(CATALOG_CLASS, self.to_bytes())
  }

  /// Parses a catalog record's value; `None` for another class or for bytes that are not exactly one value.
  pub fn from_record_bytes(bytes: &[u8]) -> Option<CatalogValue> {
    let mut input = untagged(CATALOG_CLASS, bytes)?;
    let value = <CatalogValue as Wire>::decode(&mut input).ok()?;
    if input.is_empty() { Some(value) } else { None }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use slates_db::catalog::Rights;

  fn value() -> CatalogValue {
    CatalogValue {
      name: "catalogued".to_owned(),
      size: SizeClass::Bounded { limit: 1 << 20 },
      names: NamePolicy::Exact,
      owner: Principal::Uid { uid: 501 },
      require_locked: true,
      access: vec![AccessEntry {
        principal: Principal::Uid { uid: 502 },
        rights: Rights {
          read: true,
          write: false,
          admin: false,
        },
      }],
    }
  }

  /// AUD-29-17: do: encode a catalog value twice and decode it; expect the same bytes both times and the same
  /// value back.
  #[test]
  fn a_catalog_value_round_trips_and_is_deterministic() {
    let bytes = value().to_record_bytes();
    assert_eq!(bytes, value().to_record_bytes());
    assert_eq!(CatalogValue::from_record_bytes(&bytes), Some(value()));
  }

  /// Hostile input (§4.9): do: decode every truncation, the value with a trailing byte, and the value under
  /// another class byte; expect each refused, never a panic and never read as a catalog.
  #[test]
  fn truncated_padded_and_foreign_class_values_are_refused() {
    let bytes = value().to_record_bytes();
    for cut in 0..bytes.len() {
      assert!(
        CatalogValue::from_record_bytes(&bytes[..cut]).is_none(),
        "cut {cut}"
      );
    }
    let mut padded = bytes.clone();
    padded.push(0);
    assert!(CatalogValue::from_record_bytes(&padded).is_none());
    let mut foreign = bytes;
    foreign[0] = HEAD_CLASS;
    assert!(CatalogValue::from_record_bytes(&foreign).is_none());
  }
}
