use super::WaitOnce;

/// Goal 93 item 2b, the readers' one rule (main's order of 2026-11-26). Do: a reader stops at a let-go window, then
/// meets the same window again with no read answered between. Expect: it waits the first time and not the second, so a
/// window that cannot be brought back ends in the reader's typed refusal rather than holding the push.
#[test]
fn a_reader_waits_once_for_a_window_and_refuses_when_it_meets_it_again_with_nothing_read() {
  let rule = WaitOnce::<u64>::default();
  assert!(rule.waits(Some(7)), "the first stop at a window waits");
  assert!(!rule.waits(Some(7)), "the same window again ends typed");
  assert!(
    !rule.waits(Some(7)),
    "and stays ended while nothing is read"
  );
}

/// The same rule across reads. Do: stop at a window, answer a read, stop at it again; then stop at another window.
/// Expect: each waits: an answered read means the window was brought back, so meeting it let go again later is a new
/// stop, and a different window is never the one waited for.
#[test]
fn a_reader_waits_again_after_a_read_answered_and_for_each_other_window() {
  let rule = WaitOnce::<u64>::default();
  assert!(rule.waits(Some(7)));
  rule.answered();
  assert!(rule.waits(Some(7)), "let go again after a read: a new stop");
  assert!(rule.waits(Some(9)), "another window is its own stop");
  assert!(!rule.waits(Some(9)));
}

/// A read that met no let-go window. Do: ask the rule with no window named. Expect: no wait, the reader refuses at once,
/// as it does for an object nothing holds.
#[test]
fn a_read_that_met_no_let_go_window_never_waits() {
  let rule = WaitOnce::<u64>::default();
  assert!(!rule.waits(None));
  assert_eq!(rule.last(), None);
}

/// A reader made again each turn (a push's verdicts). Do: stop at a window, carry the stop into a rule made again from
/// it, and meet the window there. Expect: no wait: the bound holds across the turns that make the reader again.
#[test]
fn a_reader_made_again_each_turn_carries_its_last_stop() {
  let first = WaitOnce::<u64>::default();
  assert!(first.waits(Some(3)));
  let again = WaitOnce::after(first.last());
  assert!(!again.waits(Some(3)), "the carried stop ends typed");
}
