//! What a content candidate keeps of what it holds for other owners (§4.10 "a late copy is redundant and
//! reclaimed"; §4.8 mechanism 1; AUD-29-43). Before 2026-09-30 a holder kept every manifest ever put to it:
//! each seal of a volume added its content on every holder and nothing let the old content go, so a
//! holder's RAM grew with a volume's history rather than its current state.
//!
//! The rule is decided from the holder's own accepted records for the object and the placements it holds,
//! so it needs no message of its own and no clock. A manifest held for an object is kept only while one of
//! these holds:
//!
//! - **Named.** The object's newest accepted record names it: a head naming it and listing this holder among
//!   its content holders, or any position of a green's merge ledger naming it as its inputs. A head that names
//!   it without listing this holder is a **late copy** — hedged here after the head's acknowledging set was
//!   fixed — which a reader never fetches from this holder; it goes.
//! - **The newest placement ahead of the records.** It was placed for a sequence newer than any record
//!   accepted for the object, and no newer placement for the object is held. An owner has at most one
//!   placement in flight per object (one seal job per volume; a green's single lowest pending version), and
//!   its record may follow after any delay — a green's records ship strictly in version order — so ahead
//!   content is released by **events**, never by time: an accepted record at or past its sequence that does
//!   not name it, a newer placement, the object's tombstone, or the takeover's stale-copy reclaim.
//!
//! The first cut released ahead content after a fixed window (one coordinator period plus the round budget's
//! longest deadline). Measured 2026-09-30: under load a green's inputs were released 1.2 s after their put,
//! before their record arrived, and the submit waiting on that record never completed
//! (`a_submit_is_answered_only_once_its_record_commits_at_the_quorum`). The window assumed what the protocol
//! does not promise; it was replaced, not lengthened.
//!
//! A put the holder's records already supersede is refused at the door ([`admits`]), so the owner never
//! counts an acknowledgement for bytes the holder would drop. The rule runs at the door, after every
//! accepted record for the object, and after every put the hold accepts. Anything left behind is bounded by
//! one placement per object, and is charged like the rest of the hold.

use std::collections::BTreeMap;

use slates_cluster::content::Placed;
use slates_db::register::{Acceptor, HostId, ObjectId};

use crate::head::HeadValue;
use crate::merge_service::MergeRecordValue;
use crate::state::ShardState;
use crate::tombstone::TombstoneValue;

/// The status count under which a holder records the manifests its retention rule released (AUD-29-43).
/// Format: a refusal name in the daemon's status report, alongside the verbs' refusal kinds.
pub const CONTENT_RELEASED: &str = "fleet.content.released";

/// What the holder's newest accepted record for `object` says about its content.
enum Newest<'a> {
  /// Nothing accepted: the content is ahead of the object's first record.
  Nothing,
  /// A volume head at a sequence.
  Head(u64, HeadValue),
  /// A green's merge ledger, newest at a sequence.
  Ledger(u64, &'a Acceptor),
  /// A destroyed volume's tombstone or retirement.
  Tombstone,
  /// A value this rule does not read (a catalog is its own object and holds no content).
  Other(u64),
}

fn newest(records: &BTreeMap<ObjectId, Acceptor>, object: ObjectId) -> Newest<'_> {
  let Some(acceptor) = records.get(&object) else {
    return Newest::Nothing;
  };
  let Some((sequence, value)) = acceptor.highest(object) else {
    return Newest::Nothing;
  };
  if TombstoneValue::from_record_bytes(value).is_some() {
    return Newest::Tombstone;
  }
  if let Some(head) = HeadValue::from_record_bytes(value) {
    return Newest::Head(sequence, head);
  }
  if MergeRecordValue::from_record_bytes(value).is_some() {
    return Newest::Ledger(sequence, acceptor);
  }
  Newest::Other(sequence)
}

/// Whether the object's accepted records name `manifest` as content this holder (`local`) backs.
fn named(newest: &Newest<'_>, object: ObjectId, local: HostId, manifest: &[u8; 32]) -> bool {
  match newest {
    Newest::Head(_, head) => {
      head.manifest.as_ref() == Some(manifest) && head.content_holders.contains(&local.0)
    }
    Newest::Ledger(_, acceptor) => acceptor.values(object).any(|value| {
      MergeRecordValue::from_record_bytes(value)
        .is_some_and(|record| record.inputs.as_ref() == Some(manifest))
    }),
    Newest::Nothing | Newest::Tombstone | Newest::Other(_) => false,
  }
}

/// Whether content placed for `sequence` is ahead of every record accepted for the object.
fn ahead(newest: &Newest<'_>, sequence: u64) -> bool {
  match newest {
    Newest::Nothing => true,
    Newest::Tombstone => false,
    Newest::Head(accepted, _) | Newest::Ledger(accepted, _) | Newest::Other(accepted) => {
      sequence > *accepted
    }
  }
}

/// Whether a put of `manifest` for `object` at `sequence` may be held (the rule at the door): it is named by
/// the holder's records, or ahead of them. A put the records already supersede is refused before it is held.
pub(crate) fn admits(
  records: &BTreeMap<ObjectId, Acceptor>,
  local: HostId,
  object: ObjectId,
  sequence: u64,
  manifest: &[u8; 32],
) -> bool {
  let newest = newest(records, object);
  named(&newest, object, local, manifest) || ahead(&newest, sequence)
}

/// Whether a held manifest is kept: named by the holder's records, or ahead of them and placed for the newest
/// sequence this holder holds for the object (`newest_placed`).
fn keeps(
  newest: &Newest<'_>,
  object: ObjectId,
  local: HostId,
  (manifest, placed): (&[u8; 32], Placed),
  newest_placed: u64,
) -> bool {
  named(newest, object, local, manifest)
    || (ahead(newest, placed.sequence) && placed.sequence >= newest_placed)
}

/// Applies the retention rule to what this holder holds for `object` now; returns how many manifests went.
pub(crate) fn retain_object(state: &mut ShardState, object: ObjectId) -> usize {
  let local = state.fleet.host();
  let newest = newest(&state.holder_records, object);
  let newest_placed = state.held_content.newest_placed(object).unwrap_or(0);
  let space = &mut crate::content_holder::hold_space(&mut state.store);
  let released = state
    .held_content
    .retain(space, object, |manifest, placed| {
      keeps(&newest, object, local, (manifest, placed), newest_placed)
    });
  if released > 0 {
    let count = state.refusals.entry(CONTENT_RELEASED).or_insert(0);
    *count = count.saturating_add(u64::try_from(released).unwrap_or(u64::MAX));
  }
  released
}

#[cfg(test)]
mod tests {
  use super::*;
  use slates_db::register::{Authority, HostEpoch, Record};

  const LOCAL: HostId = HostId(7);
  const OWNER: HostId = HostId(1);

  fn object() -> ObjectId {
    ObjectId::new(OWNER, 1)
  }

  /// The records map with one accepted head at `sequence` naming `manifest` with `holders`.
  fn records_with_head(
    sequence: u64,
    manifest: [u8; 32],
    holders: &[HostId],
  ) -> BTreeMap<ObjectId, Acceptor> {
    let mut acceptor = Acceptor::new(
      LOCAL,
      Authority {
        generation: 0,
        owner: OWNER,
      },
    );
    let value = HeadValue {
      manifest: Some(manifest),
      content_holders: holders.iter().map(|host| host.0).collect(),
    };
    acceptor
      .accept(&Record {
        owner: OWNER,
        object: object(),
        sequence,
        epoch: HostEpoch(1),
        generation: 0,
        value: value.to_record_bytes(),
      })
      .unwrap();
    BTreeMap::from([(object(), acceptor)])
  }

  /// AUD-29-43, the rule at the door: do: ask whether puts are admitted against an accepted head at sequence
  /// 5 naming manifest A with this holder listed; expect A admitted at any sequence, another manifest refused
  /// at or below 5 (superseded) and admitted above it (ahead of its record), a late copy of A to an unlisted
  /// holder refused, and anything admitted for an object with no records.
  #[test]
  fn a_put_is_admitted_only_when_named_or_ahead_of_the_records() {
    let (named_manifest, other) = ([1; 32], [2; 32]);
    let records = records_with_head(5, named_manifest, &[OWNER, LOCAL]);
    assert!(admits(&records, LOCAL, object(), 5, &named_manifest));
    assert!(!admits(&records, LOCAL, object(), 5, &other));
    assert!(!admits(&records, LOCAL, object(), 4, &other));
    assert!(admits(&records, LOCAL, object(), 6, &other));
    let unlisted = records_with_head(5, named_manifest, &[OWNER]);
    assert!(!admits(&unlisted, LOCAL, object(), 5, &named_manifest));
    assert!(admits(&BTreeMap::new(), LOCAL, object(), 0, &other));
  }

  /// AUD-29-43, the rule by events: do: judge, against an accepted head at sequence 5 naming manifest A,
  /// content ahead of it at sequence 6 while it is the newest placement and once a newer one (7) is held,
  /// superseded content at 5, and A however its placement compares; expect the newest ahead placement kept
  /// with no clock involved, an older one released, superseded content released, and A kept.
  #[test]
  fn ahead_content_is_kept_until_an_event_supersedes_it_and_named_content_for_good() {
    let (named_manifest, other) = ([1; 32], [2; 32]);
    let records = records_with_head(5, named_manifest, &[LOCAL]);
    let newest = newest(&records, object());
    let at = |sequence| Placed { sequence };
    assert!(keeps(&newest, object(), LOCAL, (&other, at(6)), 6));
    assert!(!keeps(&newest, object(), LOCAL, (&other, at(6)), 7));
    assert!(!keeps(&newest, object(), LOCAL, (&other, at(5)), 5));
    assert!(keeps(&newest, object(), LOCAL, (&named_manifest, at(5)), 7));
  }
}
