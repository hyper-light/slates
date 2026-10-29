# A route claim gave up on a live daemon at the reply deadline (2026-10-29)

**Status:** fixed the same day, failing test first.

## Description

CI's macOS lane (run 40822078142, slates 0b7f377d) failed `a_long_writable_map_is_waited_for_while_the_daemon_lives`:
the client's `map_write` answered `Ipc(DeadlineExceeded)` (`crates/client/tests/client.rs:1062`). That test gives its
client a reply deadline of 1 ms, so it proves the client waits for a large map's fill and write-backs while the daemon
lives (A-186). On Linux and Windows it passed. This Mac passed it 3 of 3 the day before.

## Root cause

On macOS a direct map's copy is not in the map's reply. The reply carries an offer, and the client claims the
descriptor on the daemon's socket route (A-173, `slates_ipc::route::claim`). The claim set `SO_RCVTIMEO` and
`SO_SNDTIMEO` to the client's reply deadline and refused the claim `DeadlineExceeded` at the first expiry of any step:
the hello, the daemon's challenge and nonce, the proof, or the confirmation with the descriptor. The daemon answers its
route on a shard, and on a loaded runner that answer took longer than 1 ms. So the claim gave up on a live daemon,
while the map's own reply, which `defers_reply` already waits for while the daemon lives (0b7f377d), would have been
waited for. The Shards form's stream-end claim (`claim_offer`, A-161) used the same bound.

## Impact

On macOS, a direct map (read or writable) or a Shards stream end failed `DeadlineExceeded` whenever the daemon's route
answered later than one reply deadline. In the field the deadline is derived from the liveness budget, so it took a
daemon starved for that long. The test's 1 ms deadline made it likely on a loaded runner. The daemon kept the offer
until it expired, and nothing it held leaked.

## Reproduction

The same test with its deadline at 1 µs, which no route answer can beat (`IMPATIENT_REPLY_NS`). Before the fix it fails
at the map on this Mac, `Ipc(DeadlineExceeded)`, 1 run of 1 (2026-10-29).

## Exact edits

- `crates/ipc/src/route.rs` `claim` takes `alive`. When a step's wait runs out, the step asks whether the daemon still
  lives. While it does, the step waits again. Once it does not, the claim is refused `DeadlineExceeded`. This covers
  `write_all`, `read_exact` and `receive_confirmation`. The confirmation's receive now reads an expired wait as one, so
  it no longer surfaces as `OsRefused { call: "recvmsg confirmation", code: EAGAIN }`.
- `crates/client/src/client.rs` `claim_map` and `claim_offer` pass `!daemon_gone()`, the liveness the client already
  asks on its cold path.
- The tests that claim directly (`crates/client/tests/client.rs`, `crates/server/tests/shards.rs`) pass `|| false`.
  They keep the old bound, since each expects an answer or a typed refusal at once.
- `IMPATIENT_REPLY_NS` is now 1 µs, so the test is red on any machine without the fix.

Verified on this Mac (M5 Max, 2026-10-29): the test green 1 of 1, the client's 20 map tests, the server's 5 Shards route
tests and the ipc crate's 4 route unit tests green.
