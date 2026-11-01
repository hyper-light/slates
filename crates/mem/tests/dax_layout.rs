//! How a DAX object lays out its file (§4.6, the backed route's L1; A-193, A-194), as arithmetic every platform runs:
//! the pieces a VMM is told to map for a guest's range, and the pages a backing's filled ranges keep resident, which is
//! what its fill budget counts.
// Test harness code: an unwrap or an index here is a failed test.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use slates_mem::{DAX_OBJECT_BYTES, DAX_RANGE_BYTES, DaxLayout, DaxPiece};

/// Shape: the guard the tests lay out under, one chunk window of a 4 KiB-paged host (16 pages), as the arena's zones
/// hold one slot at each end of their page.
const GUARD: u64 = 64 * 1024;
/// Shape: the guest ranges the piece tests walk: past one cycle of fifteen, where a 64 KiB guard's pages line up again.
const RANGES: u64 = 40;

/// §4.6, the backed route's L1 (A-194). Do: lay out guest ranges 0 to 39 under a 64 KiB guard and list each range's
/// pieces. Expect: exactly two pieces a range, contiguous in the file and covering it whole, each inside one page's
/// interior (never within 64 KiB of the page's ends), the two in consecutive pages.
#[test]
fn a_guest_range_is_two_pieces_inside_page_interiors_under_a_guard() {
  let layout = DaxLayout::Guarded { guard: GUARD };
  for range in 0..RANGES {
    let start = range * DAX_RANGE_BYTES;
    let pieces: Vec<DaxPiece> = layout.pieces(start, DAX_RANGE_BYTES).collect();
    assert_eq!(pieces.len(), 2, "range {range}: {pieces:?}");
    assert_eq!(pieces[0].file_offset, start);
    assert_eq!(pieces[1].file_offset, start + pieces[0].len);
    assert_eq!(pieces[0].len + pieces[1].len, DAX_RANGE_BYTES);
    for piece in &pieces {
      let within = piece.object_offset % DAX_RANGE_BYTES;
      assert!(
        within >= GUARD && within + piece.len <= DAX_RANGE_BYTES - GUARD,
        "range {range}: {piece:?} reaches a guard"
      );
    }
    assert_eq!(
      pieces[1].object_offset / DAX_RANGE_BYTES,
      pieces[0].object_offset / DAX_RANGE_BYTES + 1,
      "range {range}: the two pieces lie in consecutive pages"
    );
  }
}

/// §4.6 (A-163's protocol, kept by A-194). Do: lay out guest ranges under the identity. Expect: one piece a range, at the
/// file's own offsets.
#[test]
fn the_identity_is_one_piece_a_range_at_the_files_own_offsets() {
  let layout = DaxLayout::Identity;
  for range in 0..RANGES {
    let start = range * DAX_RANGE_BYTES;
    let pieces: Vec<DaxPiece> = layout.pieces(start, DAX_RANGE_BYTES).collect();
    assert_eq!(
      pieces,
      vec![DaxPiece {
        file_offset: start,
        object_offset: start,
        len: DAX_RANGE_BYTES
      }]
    );
  }
}

/// §4.6, the backed route's L1 (A-194): what a backing's filled ranges keep resident, and so what its fill budget
/// counts. Do: count the pages holding sets of ranges under a 64 KiB guard and under the identity. Expect, guarded: two
/// for a range alone, three for two neighbours (they share the page between them), sixteen for fifteen consecutive
/// ranges, four for two far apart, none for none; under the identity, one a range.
#[test]
fn the_pages_holding_ranges_are_what_the_budget_counts() {
  let guarded = DaxLayout::Guarded { guard: GUARD };
  assert_eq!(guarded.pages_holding([0]), 2);
  assert_eq!(guarded.pages_holding([0, 1]), 3);
  assert_eq!(guarded.pages_holding(0..15), 16);
  assert_eq!(guarded.pages_holding([5, 100]), 4);
  assert_eq!(guarded.pages_holding(std::iter::empty()), 0);
  assert_eq!(DaxLayout::Identity.pages_holding([0, 1, 5]), 3);
}

/// §4.6, the backed route's L1 (A-194), hostile spans. Do: ask for the pieces and pages of an empty span, of the last
/// byte the guarded object holds, and of a span that overflows. Expect: none for the empty span; one piece, inside an
/// interior, for the last byte; and the overflowing span stops rather than wraps.
#[test]
fn empty_last_and_overflowing_spans_are_held_to_the_layout() {
  let layout = DaxLayout::Guarded { guard: GUARD };
  assert_eq!(layout.pieces(0, 0).count(), 0);
  assert_eq!(layout.pages_of(0, 0), None);
  let last = layout.file_bytes(DAX_OBJECT_BYTES) - 1;
  let pieces: Vec<DaxPiece> = layout.pieces(last, 1).collect();
  assert_eq!(pieces.len(), 1);
  assert_eq!(
    pieces[0].object_offset % DAX_RANGE_BYTES,
    DAX_RANGE_BYTES - GUARD - 1
  );
  assert!(pieces[0].object_offset < DAX_OBJECT_BYTES);
  assert!(
    layout
      .pieces(u64::MAX - 1, u64::MAX)
      .all(|piece| piece.len > 0),
    "pieces near the top of the offsets never wrap"
  );
  assert_eq!(layout.pages_of(u64::MAX, 2), None);
}

/// §4.6, the backed route's L1 (A-194). Do: ask for the backings' layout under a 64 KiB guard, a guard that is not whole
/// pages, and a guard that leaves no interior. Expect: guarded only where this host places a range in one huge page and
/// the guard fits; the identity everywhere else.
#[test]
fn a_guard_is_kept_only_where_a_page_can_be_huge_and_the_guard_fits() {
  let placeable =
    cfg!(target_os = "linux") && slates_machine::facts::huge_page_bytes() == Some(DAX_RANGE_BYTES);
  let page = slates_machine::facts::Facts::page_bytes();
  let expected = if placeable && GUARD.is_multiple_of(page) {
    DaxLayout::Guarded { guard: GUARD }
  } else {
    DaxLayout::Identity
  };
  assert_eq!(DaxLayout::for_backings(GUARD), expected);
  assert_eq!(DaxLayout::for_backings(GUARD + 1), DaxLayout::Identity);
  assert_eq!(
    DaxLayout::for_backings(DAX_RANGE_BYTES / 2),
    DaxLayout::Identity
  );
}
