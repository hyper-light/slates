# A park a liveness look ended was counted as no kind (2026-11-18)

## Description

A-456 (main `ba179ff61`, fixed in `7113415eb`, 2026-11-18) made a synchronous call's reply wait end at the reply or at
its daemon's departure (`slates_ipc::endpoint::ClientEnd::wait_while_daemon_lives`). On macOS the park waits on the wake
word until the next look at the daemon's life, and on Windows on the reply's Event and the daemon's process handle with
the same timeout. A park whose timeout comes before any wake ends at that look: the look finds the daemon alive, and
the client parks again.

The client counts every park (`park_ratio`), the parks a wake ended with no reply (`unanswered_wakes`) and the replies.
It counted a park its look ended as none of these. The client suite checks that every park ended by a reply or an
unanswered wake, and on macOS it failed:
`the_typed_verbs_drive_the_lifecycle_and_refusals_are_typed` (`crates/client/tests/client.rs`), "every park is ended by
a reply or counted as an unanswered wake: 32 parks, 17 replies, 0 unanswered" (main's run r432, on f3f618f2, at
`7113415eb`).

## Root cause

`ClientEnd::after_park` sorted a park's end into a reply's wake, an unanswered wake, the departure, or a deadline. A
departure park that timed out fell through to "wait on" uncounted. The field doc of `unanswered_wakes` already named a
look as one of a park's ends, but no counter held it. Linux never shows it: its departure park has no timeout.

## Impact

The accounting only: no wait ended early or late, and no wake was lost. The wake estimate was not taught by such a park:
a park's wait is kept for the estimate only when a wake ended it (`after_wake`). Nothing held that, though, and a change
that kept every park's wait would have taught the estimate a look's timeout (up to the time waited) as a wake.

## Exact edits

- `crates/ipc/src/endpoint.rs`: the field `parks_ended_by_looks` and `ClientEnd::parks_ended_by_looks`; `park_once`
  passes whether the park waited until a look (`look_timeout_ns.is_some()`), and `after_park` counts a departure park
  that did and timed out. The unit test
  `departure::a_park_a_liveness_look_ends_is_counted_as_its_own_kind_and_teaches_the_estimate_nothing`, and the late
  reply's daemon thread shared as `reply_late`.
- `crates/client/src/client.rs`: `Client::parks_ended_by_looks`.
- `crates/client/tests/client.rs`: the suite's accounting counts a look's park.
- `docs/wip/SLATES_DESIGN.md` (§4.7's A-456 status; A-506), `docs/wip/GAPS.md` (class C, C-b).

## Sibling sweep

Every reader of the park counters (`park_ratio`, `unanswered_wakes`, `liveness_looks`) in the tree: `crates/ipc/tests/
rings.rs` holds the same accounting over a deadline's wait (`ClientEnd::wait`), which never looks, so it holds as it
was; the others (`crates/api/tests/api.rs`, `crates/sdk-node/tests/sdk.test.mjs`, `crates/ipc/src/endpoint.rs`'s
bridge test, `ipc_bench`, `provision_bench`) bound parks from below, which more counted parks cannot break.

## Evidence (Mac Studio, macOS arm64, debug, 2026-11-18, load 11 to 14)

- Red on main (`3d174a1`): `cargo test --locked -p slates-client --test client
  the_typed_verbs_drive_the_lifecycle_and_refusals_are_typed` failed, "every park is ended by a reply or counted as an
  unanswered wake: 63 parks, 17 replies, 0 unanswered". Green with the fix: passed (0.85 s).
- The unit test: a reply held 1.5 s made 12 parks, 1 reply, 0 unanswered wakes, 11 parks ended by looks, 11 looks, and
  1 sample taught the estimate. Red with the count removed: "every park is counted: 12 parks, 1 replies, 0 unanswered,
  0 ended by looks".
