//! A presented landing's full entry list, on demand (§4.15 step 2: "the confirmation surface shows the summary, the full
//! entry list on demand, and the conflicts found by a preliminary verdict pass"). Until 2026-11-17 a presentation carried
//! its summary's counts and its conflicts alone, so a person could not see what a grant would write path by path, and a
//! reviewer with no host mount of the volume (a guest's volume served over virtio-fs, kiln's microVM sessions) could not
//! learn the change it was asked to review.
//!
//! A presentation keeps its manifest's entries, moved from the plan when it is presented (never copied), with the
//! conflict class the preliminary verdict found at each conflicting path; they go when the presentation goes (A-46: its
//! landing consumes it, its client's next presentation of the volume and target replaces it, its client's retirement
//! abandons it), and a shard holds at most one presentation a client seat. `LandingEntries` reads them a page at a time
//! as the bytes of their wire encodings (`PlannedEntry`), one after another in the manifest's order. A page's cursor
//! names an entry and a byte within its encoding, so an entry larger than one reply chunk (4 KiB: a path and a rename's
//! origin or a symlink's target near the OS limit each) is carried across pages rather than refused, each page encodes
//! only the entries it carries (work proportional to the page, never to the list), and the server keeps nothing of its
//! readers. The list is answered to the principal it was presented to and to no one else: to anyone else, and once the
//! presentation has gone, the landing is `NotFound`, so a landing id tells nobody else of a plan. The canonical example
//! of the shape is `crate::listing` (a directory's pages, sized by the same reply chunk).
//!
//! What a reader does with it: each create and replace names its bytes' BLAKE3 and length, so a reader of the volume's
//! file checks that what it read is what the grant would land, and a landing of the same snapshot later presents the
//! same manifest hash, or the plan changed.

use std::collections::BTreeMap;

use slates_db::catalog::Principal;
use slates_ipc::protocol::{
  ConflictClass, EntriesCursor, PlannedAction, PlannedContent, PlannedEntry, Refusal, ReplyBody,
  framed_len,
};
use slates_land::manifest::{Action, LandingEntry};
use slates_land::verdict::ConflictClass as LandConflict;
use slates_wire::Wire as _;

use crate::state::ShardState;
use crate::verbs::refused;

/// The conflict classes a presentation's preliminary verdict pass found (§4.15 step 4 run without writing), by path:
/// what each entry of its list names. Only the conflicting entries, so its size is the conflicts'. Unix's, where a
/// landing is presented (the Windows writer is owed, `crate::landing::land_verb`).
#[cfg(unix)]
pub(crate) fn conflicts_of(
  preliminary: &[slates_land::engine::EntryReport],
) -> BTreeMap<Box<str>, LandConflict> {
  preliminary
    .iter()
    .filter_map(|report| match report.verdict {
      Some(slates_land::verdict::Verdict::Conflict(class)) => Some((report.path.clone(), class)),
      _ => None,
    })
    .collect()
}

/// One manifest entry as its list names it: the action, a file's bytes (their BLAKE3 and length, as the manifest binds
/// them), the mode a file or a directory lands with, whether it replaces or removes what the volume witnessed, and
/// `conflict`.
fn planned(entry: &LandingEntry, conflict: Option<&LandConflict>) -> PlannedEntry {
  let action = match &entry.action {
    Action::Create => PlannedAction::Create,
    Action::Replace => PlannedAction::Replace,
    Action::Delete => PlannedAction::Delete,
    Action::Rename { from } => PlannedAction::Rename {
      from: from.to_string(),
    },
    Action::Mkdir => PlannedAction::Mkdir,
    Action::Rmdir => PlannedAction::Rmdir,
    Action::Symlink { target } => PlannedAction::Symlink {
      target: target.to_string(),
    },
    Action::Clear => PlannedAction::Clear,
  };
  let content = match entry.action {
    Action::Create | Action::Replace => entry.overlay.map(|overlay| PlannedContent {
      hash: overlay.hash,
      size: overlay.size,
    }),
    _ => None,
  };
  PlannedEntry {
    path: entry.path.to_string(),
    action,
    content,
    mode: entry.overlay.map(|overlay| overlay.mode),
    witnessed: entry.witnessed.is_some(),
    conflict: conflict.map(|class| wire_class(*class)),
  }
}

/// A conflict class as the wire names it: one for one.
fn wire_class(class: LandConflict) -> ConflictClass {
  match class {
    LandConflict::ModifyModify => ConflictClass::ModifyModify,
    LandConflict::ModifyDelete => ConflictClass::ModifyDelete,
    LandConflict::DeleteModify => ConflictClass::DeleteModify,
    LandConflict::RenameRename => ConflictClass::RenameRename,
    LandConflict::CreateCreate => ConflictClass::CreateCreate,
    LandConflict::TypeChanged => ConflictClass::TypeChanged,
    LandConflict::TargetInUse => ConflictClass::TargetInUse,
  }
}

/// The bytes one page may carry: one reply chunk less the page's own framing with a next cursor, as a directory
/// listing's page is sized (`crate::listing`). The wire's integers are fixed-width and its lengths four bytes
/// (`slates_wire::codec`), so this is exact for every page.
fn page_room() -> usize {
  let chunk = usize::try_from(crate::config::bulk_chunk_bytes()).unwrap_or(usize::MAX);
  let empty = ReplyBody::LandingEntries {
    bytes: Vec::new(),
    next: Some(EntriesCursor::default()),
    total: 0,
    manifest: [0; 32],
  };
  chunk.saturating_sub(framed_len(&empty))
}

/// `LandingEntries` (§4.15 step 2): one page of presented landing `landing`'s entry list from `from`, as many bytes of
/// their encodings as one reply chunk holds. Answered to the principal the landing was presented to; to anyone else,
/// and for a presentation consumed by its landing, replaced by its client's next or abandoned with its client, as a
/// landing that isn't there (`NotFound`). A read: nothing changes.
pub(crate) fn landing_entries_verb(
  state: &ShardState,
  principal: &Principal,
  landing: u64,
  from: EntriesCursor,
) -> ReplyBody {
  let Some(awaiting) = state
    .landing
    .awaiting
    .get(&landing)
    .filter(|awaiting| awaiting.principal == *principal)
  else {
    return refused(Refusal::NotFound);
  };
  let count = awaiting.entries.len();
  let encode = |index: usize, out: &mut Vec<u8>| {
    if let Some(entry) = awaiting.entries.get(index) {
      planned(entry, awaiting.conflicts.get(&entry.path)).encode(out);
    }
  };
  match page_of(count, encode, from, page_room()) {
    Ok((bytes, next)) => ReplyBody::LandingEntries {
      bytes,
      next,
      total: u64::try_from(count).unwrap_or(u64::MAX),
      manifest: awaiting.manifest,
    },
    Err(refusal) => refused(refusal),
  }
}

/// The page of a list of `count` entries that starts at `from` and holds at most `room` bytes: the entries' encodings
/// (`encode` writes entry `index`'s onto the end of its buffer), one after another, the first from `from.byte` on and
/// the last possibly cut where the room ends; and the cursor of the page after it, or `None` when this one ends the
/// list. Every page but the last holds exactly `room` bytes, so a reader always progresses. Refused `BadRequest` for a
/// cursor that names no place in the list (an entry past its end, or a byte past its entry's), and `Unsupported` when
/// a page holds no byte at all.
fn page_of(
  count: usize,
  mut encode: impl FnMut(usize, &mut Vec<u8>),
  from: EntriesCursor,
  room: usize,
) -> Result<(Vec<u8>, Option<EntriesCursor>), Refusal> {
  let nowhere = |what: &str| Refusal::BadRequest {
    reason: format!(
      "entries cursor {}.{}: {what} (the list holds {count} entries)",
      from.entry, from.byte
    ),
  };
  if room == 0 {
    return Err(Refusal::Unsupported {
      feature: "an entry list page that holds no byte".to_owned(),
    });
  }
  let mut index = usize::try_from(from.entry).map_err(|_| nowhere("past the list's end"))?;
  let mut skip = usize::try_from(from.byte).map_err(|_| nowhere("past its entry's end"))?;
  if index > count || (index == count && skip > 0) {
    return Err(nowhere("past the list's end"));
  }
  let mut bytes = Vec::new();
  let mut encoded = Vec::new();
  while index < count {
    encoded.clear();
    encode(index, &mut encoded);
    // An entry's encoding is never empty, so a cursor at its end is no place in it: the next entry's start is.
    let Some(tail) = encoded.get(skip..).filter(|tail| !tail.is_empty()) else {
      return Err(nowhere("past its entry's end"));
    };
    let space = room.saturating_sub(bytes.len());
    if tail.len() > space {
      bytes.extend_from_slice(tail.get(..space).unwrap_or_default());
      let next = EntriesCursor {
        entry: u64::try_from(index).unwrap_or(u64::MAX),
        byte: u64::try_from(skip.saturating_add(space)).unwrap_or(u64::MAX),
      };
      return Ok((bytes, Some(next)));
    }
    bytes.extend_from_slice(tail);
    index = index.saturating_add(1);
    skip = 0;
    if bytes.len() == room && index < count {
      let next = EntriesCursor {
        entry: u64::try_from(index).unwrap_or(u64::MAX),
        byte: 0,
      };
      return Ok((bytes, Some(next)));
    }
  }
  Ok((bytes, None))
}

#[cfg(test)]
mod tests {
  use super::{EntriesCursor, page_of};
  use proptest::prelude::*;
  use slates_ipc::protocol::Refusal;

  /// Reads every page of `entries` (each an entry's encoding) from the list's start in pages of `room` bytes: the
  /// pages, and the cursor each was asked from.
  fn read_all(entries: &[Vec<u8>], room: usize) -> Vec<(EntriesCursor, Vec<u8>)> {
    let mut pages = Vec::new();
    let mut from = EntriesCursor::default();
    loop {
      let encode = |index: usize, out: &mut Vec<u8>| {
        out.extend_from_slice(entries.get(index).map_or(&[][..], Vec::as_slice))
      };
      let (bytes, next) = page_of(entries.len(), encode, from, room).unwrap();
      pages.push((from, bytes));
      match next {
        Some(next) => {
          assert!(next > from, "the cursor moves on: {from:?} then {next:?}");
          from = next;
        }
        None => return pages,
      }
    }
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::default()))]

    /// §4.15 step 2 (the full entry list on demand). Do: page generated lists of entries (each a non-empty encoding,
    /// some longer than a page) from the list's start in pages of a generated room. Expect: the pages joined are the
    /// entries' encodings joined, in order; every page but the last holds exactly the room, and the last at most it;
    /// so the pages are as few as the bytes allow, and an entry larger than a page is carried across pages whole.
    #[test]
    fn pages_join_to_the_list_and_every_page_but_the_last_is_full(
      entries in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 1..64), 0..24),
      room in 1usize..48,
    ) {
      let pages = read_all(&entries, room);
      let joined: Vec<u8> = pages.iter().flat_map(|(_, bytes)| bytes.iter().copied()).collect();
      let whole: Vec<u8> = entries.iter().flatten().copied().collect();
      prop_assert_eq!(&joined, &whole);
      let (last, full) = pages.split_last().unwrap();
      prop_assert!(full.iter().all(|(_, bytes)| bytes.len() == room));
      prop_assert!(last.1.len() <= room);
      prop_assert_eq!(pages.len(), whole.len().div_ceil(room).max(1));
    }
  }

  /// §4.15 step 2. Do: ask pages from cursors that name no place in a list of two entries of three bytes: an entry
  /// past the end, a byte into the end, a byte at an entry's end and past it, and a page room of nothing. Expect: each
  /// refused, `BadRequest` naming the cursor or `Unsupported`, and the list's end itself an empty last page.
  #[test]
  fn a_cursor_that_names_no_place_in_the_list_is_refused() {
    let entries = [vec![1u8, 2, 3], vec![4u8, 5, 6]];
    let encode = |index: usize, out: &mut Vec<u8>| {
      out.extend_from_slice(entries.get(index).map_or(&[][..], Vec::as_slice))
    };
    let at = |entry: u64, byte: u64| EntriesCursor { entry, byte };
    for (cursor, why) in [
      (at(3, 0), "an entry past the end"),
      (at(2, 1), "a byte into the end"),
      (at(1, 3), "a byte at its entry's end"),
      (at(0, 9), "a byte past its entry's end"),
      (at(u64::MAX, u64::MAX), "the farthest cursor"),
    ] {
      let refused = page_of(entries.len(), encode, cursor, 4);
      assert!(
        matches!(&refused, Err(Refusal::BadRequest { reason }) if reason.contains("entries cursor")),
        "{why}: {refused:?}"
      );
    }
    assert!(matches!(
      page_of(entries.len(), encode, at(0, 0), 0),
      Err(Refusal::Unsupported { .. })
    ));
    assert_eq!(
      page_of(entries.len(), encode, at(2, 0), 4),
      Ok((Vec::new(), None))
    );
    assert_eq!(
      page_of(entries.len(), encode, at(1, 2), 4),
      Ok((vec![6], None)),
      "a page from within an entry carries the rest of it"
    );
  }
}
