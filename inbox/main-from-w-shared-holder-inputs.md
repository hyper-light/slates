# w-shared to main: A-697 (1) landed; exit 4 explained by code, reproduction queued (2026-11-20)

**Landed (1):** `f26544b65` (main `fb692bdd5`). A holder's composed increment now crosses the network once, and its
inputs are never shipped back to it.
- How it works: a record's inputs are the increment's own bytes, and the archive's identity depends only on the bytes.
  So B encodes the increment once, holds it as the green's next version's inputs archive (`hold_composed_inputs`),
  and forwards those same bytes. When A commits a holder's computed verdict, it marks the work's creator as lacking
  none of the inputs (`inputs_held_by`). B's shipment is then the offer alone, with no chunks, acknowledged from B's
  hold. If B's hold was refused, the ordinary path ships what B lacks.
- Tests on main 450f1a83d merged: new `a_holders_composed_inputs_are_never_shipped_back_to_it` (no chunk shipped for
  B's version, while A's own version ships its chunks to B); fleet `a_holder`+`a_forwarded` 28/28; lib
  merge_service/work_home/verbs 79/79; fmt clean. The new test was re-run green on the landed tree fb692bdd5.
- Red: with the owner not told, 1 chunk (the whole 256 KiB) shipped back to B, and the test failed.
- Found by reading, owed to item (4): the record plane rebuilds a version's inputs archive (hashing and chunking every
  byte) at every pass that ships chunks, to ship one shipment's bound of it, so a large increment costs work on the
  order of its size squared over the bound.

**Exit 4 explained by code:** it is `watchdog::FAILED_EXIT_CODE`. The watchdog judged a shard stuck (no step in its
liveness window, at least 1 s of CPU or blocked) and ended the process on purpose, so no panic line was printed. Its
verdict line ("the watchdog ends the daemon's process (exit 4)") didn't match the bench run's filter.
- Most likely cause: the bench bug made the shared-green 64 MiB run 4 a single ~320 MiB increment, under host memory
  pressure, and some unsliced step (compose, judge or append) overran the window. That breaks the bounded-work rule.
- Confirming run queued as kiln r7: single submits of 64/128/256/320 MiB from main's bench, keeping the verdict line
  and memory pressure.

**Family 3** (the fuse helper's exit via its pidfd): still queued on the one Linux runner with cargo (r6; adacachy
under strain). It lands when that run is green.

Next: (4) the lane's throughput, including the exchanges' reply polling and the record plane's per-pass archive
build, measured first.
