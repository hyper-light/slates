# A Windows daemon's first create met its extents made but not yet held (2026-11-23)

## Description
On CI's Windows runners, every Node SDK sync test's first create, an 8 MiB bounded volume on a one-shard daemon
started the moment before, was refused `BudgetExceeded { available: 5722144768 }` (windows-latest; 5,720,506,368 on
windows-11-arm). This was main's SDK lanes from run 45217887083 on, and w-interop's diag run 45230459274. The same
creates through the CLI passed. A second case was found by reading and ruled in by main: a create sent at once after a
destroy, needing the room the destroyed volume gave back, was refused the same way.

## Root cause
On Windows each content pool extent is a section of its own, and a claim takes only an extent that is made and held by
the anchor (A-789), so that data never lands where a daemon's death would lose it. Measured with logging on both
runners (diag run 45238179939):
- Each shard's start spawned its slice's holds as a detached task, one anchor exchange per extent, and the control
  shard answered claims at once. The holds took 117.5 ms for 25 extents (windows-latest) and 141.9 ms for 22
  (windows-11-arm). Every Node daemon's first create came 20 to 63 ms after its holds began, and met "free but unready
  23, free and ready 2 (131072 bytes)" (arm64: 21 unready, 1 ready). The CLI's first create came 133 ms after the
  daemon served, when every hold had been answered. No hold was ever refused.
- The pool's `available()` counted every free extent, ready or not, while its claim passed over the unready ones. So
  `make_room` saw room, grew 131,072 of 11,010,048 bytes, and gave them back, and the refusal named the whole pool
  (claimable 5,724,766,208, admittable 5,722,144,768) as available.
- A released extent is closed, so its commit returns, then made again and held: two anchor exchanges. A destroy's
  publication gives its volume's wholly free extents back before the destroy answers, so a create right after it met
  them unready.

## Impact
On Windows a client creating right after its daemon started, or right after destroying a volume whose room it needed,
was refused `BudgetExceeded` with a number that said there was room. Every Node SDK sync suite failed on both Windows
runners, which kept those lanes red. A multi-shard daemon's first creates could fall in the same window (inferred from
the code: its holds raced the control shard's start the same way).

## Exact edits
- `crates/server/src/daemon.rs`: the control loop, which answers every claim, has the anchor hold every extent
  (`content_extents::hold_before_serving`) before its first, after its boot's steps (`boot_before_claims`, the client
  ids restored and each partition's greens and works brought whole, moved out of the loop so its cognitive complexity
  stays within the wall's ten on Windows). Each hold is bounded by the hold channel's learned answer bound (A-552).
  Each shard's own spawned hold task is gone.
- `crates/server/src/content_extents_windows.rs`: `hold_every_slice` holds every made, unheld extent and logs how many
  it holds and how long the holds took. A slot marks an extent being made again from its release
  (`take_for_remake`) to the end of its remake (`end_remake`). `remaking`, `remakes_in_flight` and `remake_bound_ns`
  (its two exchanges at the learned answer bound) are new, and a remake's end tells the waiting admissions
  (`room_wait::remade`).
- `crates/server/src/content_pool.rs`: `available()` counts only extents a claim can take now (`claimable_now`, the
  same rule `best_fit` keeps), and `pending()` counts the free extents being made again.
- `crates/mem/src/arena.rs` (`ExtentSource::pending`, `ChunkArena::pending_claimable`) and `crates/vfs/src/volume.rs`
  (`Store::pending_room`) carry that pending room up.
- `crates/server/src/room_wait.rs` (new, on `lease_wait`'s pattern): a bounded create refused for room that the
  remakes in flight would make is parked, not refused. It runs again as each remake ends, told across shards while any
  waits. Its deadline is one remake's bound for each remake in flight when it parked. A create that would not fit
  even then is refused at once with the room there is. `crates/server/src/verbs.rs` notes the refusal in `admit_over`,
  parks in `dispatch`, and joins a retry of a parked create; `lease_wait::bound` is shared.
- `crates/cli/tests/content_extents_windows.rs`: `a_create_the_moment_the_daemon_serves_is_admitted_on_windows` and
  `a_create_during_the_released_extents_remakes_waits_for_them_on_windows`, on a real one-shard anchor, each under a
  watchdog that ends its anchor past its bound so a blocked step fails rather than holds the run; the wait for the
  daemon's serving line is bounded too.
- `.github/workflows/ci.yml`: the extents file runs on both Windows lanes (its commit give-back test skipped as
  A-878's red, below).

## Evidence
- Red, with the fix reversed (diag run 45260798132, both runners): the first create refused
  `BudgetExceeded { available: 5722144768 }` (windows-latest) and `{ available: 5720506368 }` (windows-11-arm); the
  create during the remakes refused `{ available: 5713756160 }` and `{ available: 5712117760 }`, the old count of every
  free extent, ready or not.
- Green, with it (the same run): both tests admitted, and the restart test passed; the daemon logged "the anchor holds
  25 of the 25 content extents made, held in 119.2207ms, before the control shard answers a claim" (22 of 22 in
  111.6 to 122.1 ms on windows-11-arm). The Node SDK suites passed 8 of 8 on both runners, where every sync suite had
  failed at its first create.
- Windows clippy of the changed crates and the CLI test, clean on both runners (the same run).
- The first build of the fix awaited the holds inside the control shard's start, which runs alone
  (`futures::spawn_alone`), so the holds' exchanges, tasks of the same shard, never ran and the daemon never served:
  the extents tests hung on both runners until cancelled (diag run 45251289894). The holds run at the control loop's
  head since.
- The file's commit give-back test, run here for the first time, fails apart from this: its 256 MiB write through a
  CLI attachment is refused `AttachmentExpired`. Skipped in CI by name, a tracked red (GAPS, A-878).

## Siblings
- A dynamic volume's growth by a write and the other admissions (a clone, a copy-up) still meet a released extent's
  remake window unwaited: refused with the honest room now, not a count of the whole pool. Only a bounded create
  parks.
- A create sent at once after a destroy meets the destroy's teardown, which holds the volume's reservation past the
  destroy's answer, and is refused `BudgetExceeded` on every platform (diag run 45251289894: `{ available: 1430536192 }`,
  the room less the destroyed volume's): item 3b, ruled by main to wait as a remake does.
- The pressure hold's claims (`ContentPool::claim_from_hold`) do not check that an extent is held. Every made extent is
  held before the daemon serves now, and a remaking one has no handle to map, so none is taken unheld today.
- `install`'s comment said the anchor's duplicate of an extent handed twice "goes when the slice's holds are made
  again"; nothing did that, and the anchor refuses a hold of an id it already holds. The comment now says only what
  happens.
