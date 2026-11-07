# A destroy was answered before its tombstone was placed, and a takeover undid it

**A safety defect: a destroyed volume was served again after its owner's loss.** A client was told its destroy was
done; the owner was then lost; a survivor took the volume over and listed it and served its file, all 4,096 bytes.

**Found** 2026-11-06, raft-chaos, by two flows of `crates/cli/tests/live_chaos.rs` (macOS arm64, debug, load 45–68).

**Live volume, writes waiting.** In `a_live_volume_destroyed_with_writes_waiting_refuses_them_and_keeps_nothing`, both
candidates were stopped (SIGSTOP) with a write waiting, and the destroy then exited 1, `refused: NotFound`, although it
had happened: afterwards no node held anything of the volume (1 of 1). `fleet.live.awaited` 50 and `resolved` 50 are 48
acknowledged writes, the 1 refused write, and the destroy itself.

**Any volume, owner lost.** In `a_live_volumes_unheard_destroy_then_its_owners_loss`, every candidate held the log and
then both were stopped. The destroy exited 0. Then the owner was SIGKILLed and the candidates resumed. A survivor listed
the volume again and served its file, all 4,096 bytes (1 of 1). The plain volume's run was muddied by the test's own
wrong verb: it asked for `slates snapshot`, and the verb is `volume snapshot`. Even so, a survivor listed the destroyed
plain volume again.

**Root cause.** Two faults, one place (`crates/server/src/verbs.rs` `run_recorded`):
1. **The destroy waited on the live log.** A recorded verb on a live-shipped volume answers only once the log is
   durable where its publication put it (A-200). The destroy publishes no content, but while an earlier write was still
   undurable it waited at that write's position. Its teardown then ends with `live_ship::forget`, which refuses every
   reply waiting on the volume `NotFound`, the destroy's own included.
2. **No destroy waited for its tombstone.** A destroy closes the volume's registers with a tombstone committed at
   `f + 1` (AUD-29-43, `crate::tombstone`). Once it commits, "no successor can adopt an older live head". But the destroy
   was answered as soon as its own partition committed it, so an owner lost between the answer and the tombstone's
   placement left a takeover that adopted the last live head (or seal) and served the volume again: a client was told
   an outcome the fleet then undid.

**Impact.** Every fleet volume, live-shipped or not, at `f > 0`:
- a destroy answered done can be undone by an owner's loss inside the tombstone's placement window (the record period,
  up to 100 ms, then its round, or unbounded with the candidates unreachable);
- on a live-shipped volume, a destroy answered while an earlier write waited was told `NotFound`.

A laptop (`f = 0`) is unaffected: its tombstone retires with its destroy.

**Edits.**
- `crates/server/src/live_ship.rs`:
  - `tombstone_target` names the tombstone sequence a destroy that just committed must see region-placed (`None` at
    `f = 0`).
  - `defer_destroy` holds the destroy's completion and reply, bounded with the live waits (`held_replies`, `Overloaded`
    past the bound), and a retry joins the wait (`join`).
  - `resolve_destroys` answers it once `placed_heads` shows the tombstone at f + 1.
  - A fenced owner refuses a waiting destroy `StaleEpoch`, as it refuses its writes.
  - Counter: `fleet.destroy.awaited`.
- `crates/server/src/fleet.rs` `record_acks_in` resolves the destroys its acknowledgements complete.
- `crates/server/src/verbs.rs` `run_recorded`: a `Destroyed` reply is never deferred on the live log. It is deferred on
  its tombstone instead.

**Tests**, in `crates/cli/tests/live_chaos.rs`, failing first:
- `a_live_volume_destroyed_with_writes_waiting_refuses_them_and_keeps_nothing` (candidates stopped): requires the
  destroy answered done. Red on the tree before the fix (1 of 1, run of 2026-11-06 05:29-05:49 UTC): exit 1 `refused: NotFound`.
- `a_live_volumes_unheard_destroy_then_its_owners_loss` (the owner lost before the tombstone is durable): requires that
  a destroy answered done is never undone. Red on the tree before the fix (1 of 1, 2026-11-06 06:06 UTC): the destroy
  exited 0 with both candidates stopped, and after the owner's SIGKILL survivor `a` listed the volume and read its
  file (4,096 bytes).
- The same flow plain, and both heard (candidates up): `a_plain_volumes_unheard_destroy_then_its_owners_loss`,
  `a_live_volumes_destroy_then_its_owners_loss`, `a_plain_volumes_destroy_then_its_owners_loss`. A heard destroy must
  be answered done; no destroy may be told `NotFound`; an unheard one must never be answered done.

**What is now proven** (macOS arm64, debug, load 30-50, 2026-11-06 06:21-06:23 UTC, 1 run of each
flow):
- with both candidates stopped, the destroy is not answered; once they resume it exits 0, 0.52 s later;
- an unheard destroy is never answered done, live or plain: its client is told the daemon is gone (exit 3) when the
  owner is killed;
- a heard destroy exits 0 before the owner's loss, live or plain, and neither survivor lists or serves the volume
  afterwards;
- no destroy is told `NotFound`.

**Red-check** (2026-11-06 06:41-06:46 UTC, the fix disabled by making `verbs::replicated_waits` skip the destroy's
branch, so a destroy takes the old path): 3 of the 5 flows fail.
- The destroy-wait flow: the destroy is told `NotFound` (exit 1).
- The live unheard destroy and the plain unheard destroy: each exits 0 with both candidates stopped, "a destroy no
  candidate could hold is never answered done".

The two heard flows pass either way. In this run neither survivor served a destroyed volume within its 8 s window,
so the red is on the answer. The resurrection itself was seen once, at 06:06.

Not yet proven: whether a takeover serves the acknowledged writes of a live volume whose destroy was never answered.
The flow now checks NoLoss there (8 s after the loss neither survivor served it: one `NotFound`, one `Overloaded`); its
result is owed.
