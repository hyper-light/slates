# The in-process fleet suite fails a late test only under ~3.5× CPU oversubscription — accumulated per-test leaks, not the load regime

- **Date:** 2026-09-14
- **Area:** the in-process fleet integration suite (`crates/server/tests/fleet.rs`), run under sustained CPU
  oversubscription. Not a product code path — the leaked resources are process-lifetime allocations a real
  one-daemon-per-process deployment never accumulates.
- **Severity:** test reliability at loads **beyond** the charter's acceptance bar (one CPU burner per
  hardware thread). At the acceptance load and at normal load the suite is 34/34 (see
  `docs/wip/fleet-under-load.md`); this record is the honest account of what breaks past that bar and why.

## Symptom

On this 18-thread box, under **36** `yes` CPU burners (2× per hardware thread, load 64–94), the full
serialized fleet suite failed twice, at two different tests, in two different ways:

- **loadC (10:42):** a hang at test 32, `three_daemons_form_a_fleet_and_the_survivors_retire_a_dead_node`.
  Survivors never retired the killed node; my 420 s no-output stall detector killed it. Thread sample: both
  survivor shards **parked in `kevent`** (1397/1462, 1404/1462), healthy idle duty cycle, steps climbing to
  851,159 — not a wedge, not starvation. `poll_until`'s `min_progress` climbed 1 → 3765 record-plane
  periods; it would have failed the test cleanly at 4000 periods.
- **loadD (10:23):** a clean assertion at test 23, `a_stale_or_forged_announcement_is_refused_and_counted`
  (`fleet.rs:1585` `stale_counted`), after the poll ran a full **4000 coordinator periods over 409 s**.

## What the enriched pulse ruled out (loadD)

The per-shard pulse (`Daemon::shard_pulses`, this change) at the loadD failure:

```
A (polled):      periods=4060 steps=1,138,214 spawns=565,076 done=565,062 adm_refused=0 longest_step=39ms parked=true
B:               periods=4000 steps=8,878      spawns=23      done=11      adm_refused=0 longest_step=39ms
stale announcer: periods=4000 steps=8,794      spawns=20      done=8       adm_refused=0 longest_step=37ms
forged announcer:periods=4003 steps=8,704      spawns=18      done=6       adm_refused=0 longest_step=35ms
```

So the failure is **not**:
- **arena saturation** — `adm_refused=0` on every shard; A completed every task it admitted (done ≈ spawns);
- **a blocking step** — `longest_step` ≤ 39 ms everywhere;
- **a failed observe path / the harness clock** — A's shard was healthy and answered **565,076**
  `fleet_refusals()` observe queries (128× the other daemons' work); `poll_until` correctly judged 4000
  periods of non-convergence in daemon time, not wall-clock.

The two announcer daemons (spawns ~20) simply never got their stale/forged announcements **served by A**, so
A never counted the refusals. The missing step is a fresh daemon's SWIM handshake/announcement converging.

## Root cause — accumulated per-test leaks, proven by discriminators

The decisive experiments: run each failing test **alone** under the same 36 burners.

- `three_daemons_form_a_fleet_and_the_survivors_retire_a_dead_node` alone @36: **passes in 12.01 s**
  (detection at 47 coordinator periods).
- `a_stale_or_forged_announcement_is_refused_and_counted` alone @36: **passes in 11.21 s** (the stale count
  held at period 1).

Same tests, same 36-burner load, same poll rate — the only difference is the accumulated static state of the
~22–31 tests that ran before them in the same process. So both failures are **accumulated suite-state under
load**, not the load regime and not the poll rate.

The in-process suite runs 34 tests × 3–5 daemons in **one process**, and each daemon leaks its
process-lifetime resources: the runtime `ShardContext` and its kqueue driver fd (`Box::leak`, never
dropped), the fleet demux UDP sockets (leaked because an in-process daemon cannot rebind them on restart —
`crates/server/tests/fleet.rs` docstrings and `[[fleet-phase8]]` record this), the NFS listener, the anchor
segment and IPC shm, the `&'static` identity and progress atom. Threads are **not** leaked (`Daemon::stop`
joins the doorbell and every shard via `runtime.shutdown()`; the loadC thread sample confirms only the
current test's threads are live). By test 23–32 the process holds ~22–31 tests' worth of leaked fds and
memory; under ~3.5× CPU oversubscription that extra fd/allocator pressure slows a fresh daemon's transport
enough that one SWIM handshake or announcement does not complete within the 4000-period budget.

## Impact

None at or below the charter's acceptance load: the full suite is 34/34 under one burner per hardware thread
(214.82 s) and at normal load (190.75 s / 198.63 s on the OCI tree), with no wait near its bound in the
trace. The failures appear only at ~2× burners per hardware thread (~3.5× total oversubscription), a regime
past the acceptance bar, and only for a late-in-suite test on the whole suite's prior state.

## Why it is not fixed here

- It is a **test-harness artifact**, not a product defect: a real deployment runs one daemon per process
  and never accumulates this state. Freeing the leaks would mean not `Box::leak`-ing the runtime context and
  rebinding leaked sockets — a large in-process-suite refactor with no product benefit.
- The remaining hypothesis (fd vs allocator pressure) would need fd-count/allocator instrumentation and
  several more 36-burner runs to separate; R4 forbids touching the consensus-adjacent transport/timing paths
  speculatively for a regime beyond the acceptance bar.
- The charter's acceptance is 18 burners, which passes. A quiet box, or one burner per hardware thread,
  shows 34/34.

The enriched pulse (`adm_refused`, `longest_step`, spawns/completions per shard, read directly off the
runtime registry) is left in place so a future over-spec run names the exact resource without new work.

## Sibling note — the WAN tree's over-spec signature

On the WAN tree (main a0ef0ee, RTT-derived election timing), the same over-spec regime surfaces with a
different, benign signature: `adm_refused` climbs (4554 in one 18-burner-hot-box run) because the
accept-side handshake tasks each hold an arena slot for their bounded 32 × PTO retransmit budget while
their peer is starved past its turn; under 2.5–3× oversubscription those budgets overlap and fill the
shard's admission bound. No task leaks (a re-dial closes the previous session; the serve task ends
promptly). It does not occur at normal load (WAN tree 35/35, `adm_refused=0`). Design question for later
(the WAN author's): the accept-side task budget is the shard's admission bound, not a value derived from
the handshake budget × expected re-dials. Detail in `docs/wip/fleet-under-load.md`.

## Sibling note

This is the class the churned-`target/` record already named
(`docs/bugs/2026-09-13-fleet-suite-wedges-in-a-churned-target-dir.md`: "leaked demux sockets, leaked
identities") and the class an integrator saw as a normal-load stall of `a_stale_or_forged` on a pre-fix OCI
tree; on the fixed OCI tree at normal load the traced suite is 34/34 (`ociNormal`, 198.63 s), so that
instance is cleared.
