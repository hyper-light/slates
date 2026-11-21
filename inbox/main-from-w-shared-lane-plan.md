# w-shared -> main: the lane's liveness and bounded-work plan (your 23:50 ruling), 2026-11-21 ~01:40 PST

Base: origin/main f656820d3. Worktree ~/slates-work/w-shared-lane-live (agents/w-shared-lane-live).

## What the code does today (read, not yet measured)
- A holder's submit sends its whole increment inside one verb: `work_home::computed_request` builds `SubmitComputed {
  increment, computed, ... }` and `verdict_over_fleet` forwards it under `xshard::task_within(.., park_bound_ns)`. The
  bound is the node's failover delay (about 3 s): size-independent, against a transfer that grows with size.
- `computed_request` runs in one control-shard step on the holder: it encodes the increment (a copy of all of it),
  builds its inputs archive (`hold_composed_inputs`: chunked, BLAKE3-hashed and held, another copy), then encodes the
  request (a third copy). At 320 MiB that is one step of several hundred ms: the beat's lateness on the holder.
- The owner's `run_forwarded -> submit_computed` decodes the 320 MiB request and judges and appends in one step: the
  beat's lateness on the owner.
- A retry runs the same `computed_request` and re-sends all 320 MiB into the same bound.

## Steps, each landed as it goes green
1. **Measure** (counters kept): holder `merge.submit.forward_build` (the control-shard step that builds the request, ns,
   and its bytes), the forward's wall (`merge.submit.forward`, exists), the owner's served-forward wall and its step
   (`merge.submit.served_forward`, `merge.submit.owner_step`), and the control shard's longest poll and the beat's
   lateness as counters (today a log line only). Red test first: a two-node fleet test, B's failover bound injected
   short (`Daemon::inject_failover`), a holder submit whose transfer outlasts it while bytes still move: refused
   `Overloaded` today.
2. **Bound judged on progress (3a).** The forward's outer bound stops being a fixed deadline: it is extended while the
   lane reports this exchange's bytes still moving (the lane's own stall rule), and judged stalled after a stall window
   with none, refused typed as now. The red test goes green.
3. **Bounded work on the control shard (your item 2).** `computed_request`'s encode, archive and request build sliced
   over turns under the step quantum (or made unnecessary by step 4: the content is held once, the verb names it).
   The owner's decode and append likewise. Proof: the beat's lateness and longest-poll counters stay under the window
   at 320 and 512 MiB.
4. **Content ahead, stored once by identity (your 3b; the larger change).** The holder already holds its composed
   increment as an inputs archive under the green and the next sequence (A-697 (1)). The verb will carry the archive's
   manifest identity and the computed verdict, not the bytes; the owner fetches what it lacks by identity from the
   holder over the content plane (the hedged fetch `fetch_manifest_into_hold` uses), stores it once, then judges from
   what it holds. A retry sends only the ask: the owner already holds the content, so it fetches nothing again. The
   push rule's bounded working set follows: the fetch is chunked, and each chunk is checked against its identity as it
   arrives.
5. **Datagram batching**, if the profile still puts `sendto` first.

## Proof
Release, interleaved A/B (before = main at the start of this series, after = each step's tree), 64/128/256/320/512 MiB,
all N shown, on a loaded machine (this MacBook at its usual load 40-60, and Studio-1 as the quiet control); 320 and 512
MiB must pass 10 of 10. BENCHMARKS gets the tables.

## Update ~03:00 PST (machine clock): step 1 landed; 3a written but not landable alone; item 2 is the prerequisite
- **Landed: step 1 (measurement)**, 47b87f28d (main 948711996). Counters: merge.submit.forward_build,
  merge.submit.owner_step, daemon.beat.late (+ _ns). The bench prints them.
- **Written, on agents/w-shared-lane-live, not landed:** the holder awaits its forward to its end
  (`xshard::task_to_its_end`) instead of a failover-delay deadline; the connection reports send streams whose bytes
  were acknowledged, and the endpoint counts them as the exchange's progress, so the lane's "never given up while its
  own bytes move" holds for uploads. Tests: slates-transport 149 of 149 with a new unit test and its red;
  slates-server lib 28 of 28; fleet a_holder/a_forwarded 30 of 30 apart from the new test.
- **Why it can't land alone (measured, debug, this Mac at load ~40):** the red test (32 MiB holder submit, B's bound
  injected short) passes 1 of 2 to 3 of 4. The rest fail later, measured: the owner's step for that submit runs
  4.2 to 6.9 s as one step of its control shard (decode, verdict, append with the log record's checksum, all on the
  whole increment). While that shard is stuck it reads none of its sessions, so the holder sees no acknowledgements,
  and its lane rightly gives the forward up as silent (fleet.forward.given_up_silent 8 to 12), resends it, then gives
  up at the bound: Overloaded. The holder's build step is 0.6 to 1.9 s the same way.
- **So the order inside your list:** item 2 (bounded work) is the prerequisite for item 3's liveness, not only for
  the beat. The owner's step is dominated by work on the whole increment: its decode, the chain entry that holds all
  its bytes, and that record's checksum. That is where 3b (content stored once by identity, the chain entry naming it
  rather than holding a copy, the by-reference item GAPS already owes) and the slicing meet. I'm starting there: (i)
  the owner's step measured by part (decode, judge, append, record checksum) to pick what to slice first; (ii) the
  sliced or by-reference form of the biggest part; landing 3a with it once the red test passes 10 of 10.
