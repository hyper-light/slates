# A takeover's fetch copied the whole live log in one turn of a control shard (2026-11-18, A-386; §4.10)

Found by the coordinator's nemesis run diag-supervisor-live-2 (x86_64, seed 18189587465523915115), after A-374 had
sliced the successor's rebuild: a daemon's shard 1 ran 1,000 ms of CPU without a step and the watchdog ended it, and
just before, a control shard logged a single 557 ms poll (log `~/.cache/g229-scratch/live2-nemesis/nemesis-x86.log`).

## Description

A live takeover's second step fetches the log of the one holder phase one adopted (A-240). That fetch was one turn on
each end:

- The candidate answered in one turn of its control shard, where its record session is served (`fleet::serve_peer_records`):
  `live_ship::serve_fetch` cloned every entry it held, `slates_db::live::encode_fetched` copied them all into one
  answer grown as it went, and the shard published its fence.
- The transport copied that whole answer once more into the stream's send buffer (`StreamSender::write`).
- The successor copied every payload back out of the answer in one turn of its control shard
  (`slates_db::live::decode_fetched`); a successor that held the adopted head itself cloned its own copy whole there.

## Evidence (the nemesis log's counters)

- The dead daemon is c (host 7411529605045610882): the harness reads it `exit 3` (unreachable); its last line is the
  watchdog's `exit 4`, "shard 1 is spinning, 1000 ms of CPU without a step".
- A live takeover of b's volume ran with c as the successor: a's copy is fenced at epoch 2 while b's is at 1; a counted
  `fleet.live.reported 1` and `fleet.live.fetched 1` (it answered a successor's report and its whole-log fetch); b's
  shard 1 refused 129 writes `lease.refused.lost_to_successor`.
- a held 34,814 entries (`fleet.live.held`); b had appended 406,401,331 bytes of changes (`fleet.live.change_bytes`).
- c's shard-1 spin was the successor's one-turn rebuild, A-374's shape (that run's tree predates A-374). The 557 ms
  poll was on a control shard; the near-miss line names neither the daemon nor the task, so which whole-log copy took
  it (the candidate's answer or the successor's decode) is not known. Both were whole-log copies in one turn.

## Root cause

The fetch was written as one exchange whose answer is built, sent and read whole: its cost on each end grows with the
log, which is bounded only by twice the volume's size (`hold_bound`), so a large live volume's takeover held a control
shard for as long as copying its log took, with the council, the heads and every other session of that node waiting.

## Fix (A-386)

- `crates/server/src/live_ship.rs`: `serve_fetch` is async and runs on a task of the serve loop
  (`fleet::serve_peer_records` routes a live fetch to `ServedForwards::spawn`); its first turn authorizes, fences and
  publishes; `FetchAnswer` lays the entries out a copy slice a turn (`DaemonConfig::copy_slice_bytes_within` of
  `fleet::seal_quantum_ns`, an entry at least, counted `fleet.live.fetch_slices`) into a buffer reserved whole; a copy
  that moved meanwhile answers as a fetch arriving then would (`moved_answer`). `own_log` lays the successor's own
  copy out the same way. `fetch_log` keeps the answer and reads its reply alone; `Rebuilding` takes each payload in
  place as it applies it (`apply_payload`), and a broken framing stops it `fleet.live.takeover_refused.fetched`, which
  `note_rebuild` does not keep (the log is fetched again).
- `crates/db/src/live.rs`: `fetched_reply` and `push_fetched` in place of `encode_fetched`; `FetchedLog` in place of
  `decode_fetched`. The bytes on the wire are unchanged.
- `crates/transport`: `Endpoint::reply` and `Connection::reply` take the reply by value, and `StreamSender::write` keeps
  a buffer handed to an empty sender as it is, so no reply is copied whole.
- `crates/server/src/config.rs`: `DaemonConfig::copy_slice_bytes_within`.

## Tests

- `a_live_takeover_is_fetched_and_rebuilt_in_turns_and_joined_while_it_runs` (`crates/server/tests/fleet.rs`, A-374's
  test renamed): with the takeover's quantum cut on both survivors, the answer took at least as many turns as the log
  held payloads, wherever it was laid out; both survivors' longest steps stayed under the watchdog's window; and the
  successor's failover estimate timed the volume it served (A-363 through the live path).
- `a_fetch_answered_an_entry_a_turn_is_the_answer_laid_out_at_once` (`crates/server/src/live_ship.rs`): six entries laid
  out an entry a turn take six turns and give the bytes laid out at once, read back in place in order; a fence raised
  between turns answers its stale-epoch refusal, a let-go copy the empty head.
- `hostile_fetched_replies_are_refused` (`crates/db/tests/live.rs`): the payloads read in place; one over the cap and a
  length of `u32::MAX` refused when reached; no cut of the answer reads whole.

## Siblings (reported, not fixed here)

- The owner's install to a holder behind its tail (`live_ship::refresh_image`, `Install::encode`) and the live exchange's
  request copy (`slates_cluster::live::exchange` copies the message for each session, and `Connection::open_exchange`
  copies it again into the stream): whole-volume copies in one turn on the owner (g230's whole-entry item).
- The plain (unlogged) takeover's restore and population (`verbs::materialize_taken_over` inside `fleet::materialize`'s
  `call_within`, deadline a period): one owner-shard turn whatever the volume (g232's kernel base volume, 97,254 files
  and 1.99 GB), and the next period's attempt starts over. Next in g251's order.
