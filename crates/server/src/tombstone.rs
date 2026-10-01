//! A destroyed volume's register tombstone (§4.4 destroy: "release quota, tombstone the id"; §4.8 mechanism 1;
//! §4.10 "a late copy is redundant and reclaimed"; AUD-29-43). Before 2026-09-30 a destroy ended on the owner:
//! the volume's candidate holders kept its head, catalog and content for good, and when the owner later died a
//! takeover adopted the last live head and served the destroyed volume again.
//!
//! A destroy now closes the volume's registers in two stages, each an ordinary register write shipped to
//! every candidate holder and committed at `f + 1`:
//!
//! 1. **The tombstone** at the database's tombstone sequence (`Partition::tombstone_sequence`, one past every
//!    sequence the volume's registers used). A holder that accepts it releases everything it held for the
//!    volume's content. A takeover adopts it like any newest record and never materializes the volume: phase
//!    one meets any `f + 1` commit, so once the tombstone committed no successor can adopt an older live head.
//! 2. **The retirement** at the next sequence, shipped only once *every* candidate holds the tombstone. A
//!    holder that accepts it drops the object's and its catalog's records entirely. It must wait for every
//!    candidate: a holder that dropped its records while another still held only the live head could let a
//!    later takeover meet that live head alone.
//!
//! When every candidate holds the retirement the owner records `Op::TombstoneRetired` and the tombstone gives
//! back the volume slot it kept (the tombstone table is bounded by the volume capacity). A successor that
//! adopts either stage records `Op::TombstoneAdopted` and continues from that stage. On a laptop there are no
//! remote candidates, so both stages are complete at once and the tombstone retires with the destroy — the
//! same code with nothing to ship (R8).

use slates_db::register::ObjectId;
use slates_wire::Wire;

use crate::catalog::{TOMBSTONE_CLASS, tagged, untagged};
use crate::state::ShardState;

/// The status count under which a holder records a destroyed volume's content it released on accepting the
/// volume's tombstone (AUD-29-43).
/// Format: a refusal name in the daemon's status report, alongside the verbs' refusal kinds.
pub const TOMBSTONE_RELEASED: &str = "fleet.tombstone.released";
/// The status count under which a holder records a destroyed volume's records it dropped on accepting the
/// volume's retirement (AUD-29-43).
/// Format: a refusal name in the daemon's status report, alongside the verbs' refusal kinds.
pub const TOMBSTONE_RETIRED: &str = "fleet.tombstone.retired";

/// What a destroyed volume's register holds at its tombstone sequence (stage one) or the one after it (stage
/// two, `retired`).
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub struct TombstoneValue {
  /// Whether this is the retirement (every candidate holds the tombstone; drop the records).
  pub retired: bool,
}

impl TombstoneValue {
  /// The value's canonical bytes, behind the tombstone class byte.
  pub fn to_record_bytes(&self) -> Vec<u8> {
    tagged(TOMBSTONE_CLASS, self.to_bytes())
  }

  /// Parses a tombstone record's value; `None` for another class or for bytes that are not exactly one value.
  pub fn from_record_bytes(bytes: &[u8]) -> Option<TombstoneValue> {
    let mut input = untagged(TOMBSTONE_CLASS, bytes)?;
    let value = <TombstoneValue as Wire>::decode(&mut input).ok()?;
    if input.is_empty() { Some(value) } else { None }
  }

  /// The tombstone sequence a record of this value at `sequence` names: the retirement is written one past it.
  pub fn tombstone_sequence(&self, sequence: u64) -> u64 {
    if self.retired {
      sequence.saturating_sub(1)
    } else {
      sequence
    }
  }
}

/// A holder accepted `value` for `object` (the volume's register): releases the content it held for the
/// volume, drops what was waiting to materialize it, and — at the retirement — drops the volume's and its
/// catalog's records. Called after the acceptance is stored, so the acknowledgement it answers is durable.
pub(crate) fn on_held(state: &mut ShardState, object: ObjectId, value: TombstoneValue) {
  let released = state.held_content.forget_object(
    &mut crate::content_holder::hold_space(&mut state.store),
    object,
  );
  if released > 0 {
    let count = state.refusals.entry(TOMBSTONE_RELEASED).or_insert(0);
    *count = count.saturating_add(1);
  }
  state.merge.replicas.remove(&object);
  state.pending_materializations.remove(&object);
  state.pending_green_materializations.remove(&object);
  state.pending_catalogs.remove(&object);
  if value.retired {
    for register in [object, object.catalog()] {
      state.holder_records.remove(&register);
      state.fleet.forget_object(register);
      state.departed_owners.remove(&register);
    }
    let count = state.refusals.entry(TOMBSTONE_RETIRED).or_insert(0);
    *count = count.saturating_add(1);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// AUD-29-43: do: encode both stages twice and decode them; expect the same bytes both times and each
  /// value back, with the retirement naming the tombstone one sequence below it.
  #[test]
  fn both_stages_round_trip_and_name_their_tombstone_sequence() {
    for retired in [false, true] {
      let value = TombstoneValue { retired };
      let bytes = value.to_record_bytes();
      assert_eq!(bytes, value.to_record_bytes());
      assert_eq!(TombstoneValue::from_record_bytes(&bytes), Some(value));
    }
    assert_eq!(TombstoneValue { retired: false }.tombstone_sequence(9), 9);
    assert_eq!(TombstoneValue { retired: true }.tombstone_sequence(10), 9);
  }

  /// Hostile input (§4.9): do: decode every truncation, a trailing byte, a non-canonical flag and the value
  /// under another class byte; expect each refused, never read as a tombstone.
  #[test]
  fn truncated_padded_non_canonical_and_foreign_class_values_are_refused() {
    let bytes = TombstoneValue { retired: true }.to_record_bytes();
    for cut in 0..bytes.len() {
      assert!(
        TombstoneValue::from_record_bytes(&bytes[..cut]).is_none(),
        "cut {cut}"
      );
    }
    let mut padded = bytes.clone();
    padded.push(0);
    assert!(TombstoneValue::from_record_bytes(&padded).is_none());
    let mut flag = bytes.clone();
    *flag.last_mut().unwrap() = 2;
    assert!(TombstoneValue::from_record_bytes(&flag).is_none());
    let mut foreign = bytes;
    foreign[0] = crate::catalog::HEAD_CLASS;
    assert!(TombstoneValue::from_record_bytes(&foreign).is_none());
  }
}
