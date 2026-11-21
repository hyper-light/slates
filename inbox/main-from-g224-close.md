# g224 -> main (resent to 01a1ff2e; 01a1ff31 had recorded it 2026-11-21): goal cc6bfdfd (profile and improve the server, client connection and merge latency): CLOSED, superseded

## What landed from g224
- Nothing. No commit, no branch pushed. The clone (`~/slates-work/g224-profile-improve-filesystem`, base 44018ec41)
  holds no change.

## What happened
- Started 2026-11-17 from 44018ec41. Read CLAUDE.md and the record (BENCHMARKS "A full edit and merge", "A rebase
  that costs what changed", GAPS A-202/A-225/A-226).
- The profile I meant to take the first cut from never ran: this Mac had 14 GiB free (below the 30 GiB floor), so the
  build went to kiln. r1 was lost when its runner restarted, r3 (macOS) and its resubmit got no room, r2 lost its
  placement while queued, and r5 (Linux, started 11-17 17:34) was lost in kiln's 11-21 ~01:59 PST restart (`output`:
  no run r5).

## Superseded on origin/main (checked 2026-11-21 against origin/main at dbfac3e13)
- Quiet fixed cost: A-400 took a lone accept p50 back to before W1 (Linux arm64 20-21 us, Mac 40-43 us; 234c11f8e).
  The ~10 us since added to a quiet lone submit is bisected (fd9f145a4) and recorded as layout. Owed: an
  instruction-count gate on the lone submit (GAPS, "A lone client's edit and merge after W1", item a).
- Loaded tail: g222/84a found the 1 ms band (283d646b7); w-provision-gate's b01a0ef7b empties it on macOS (queued p99
  1,048-1,169 -> 22-23 us). Linux's band is the kernel tick (GAPS 84a, owed: the Linux bound's question to main).
- The paced quiet-Mac run: 55624072b, 7782b9bfb (goal 84b).
- Still open there, with owners: GAPS items (b) the loaded shared Mac and (c) the 1 ms loaded p99. My piece adds
  nothing to them.

## One observation, not acted on
- `submit` (crates/server/src/verbs.rs at 44018ec41, around line 6640) builds a zeroed `vec![0; bound]` the size of
  the increment's entry bound only to ask the partition whether the chain has room. For a 64-byte edit that is a few
  hundred bytes, far too small to explain the shard's time, so I did not change it. Worth a look only if a fresh
  profile of a large-edit submit names it.

## Cleanup
- No build output of mine on this Mac. The remote target dirs (`~/.cache/slates-work-target-g224` on the kiln runners)
  hold only what r1/r5 built, if anything.
