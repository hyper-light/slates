# An unserialized fleet test took another test's ports

Date: 2026-09-25. Scope: the fleet test harness (`crates/server/tests/fleet.rs`), not the product.
Found by CI runs 36191789379 (`e339bd8`) and 36199796152 (`04b7142`), both failing the Linux gates.

## Symptom

`a_cross_region_client_finds_the_copyset_successor_instead_of_an_unrelated_live_peer` failed in phases
that have nothing to do with what it tests:

- **CI, `e339bd8`:** the successor never adopted within the audit wait.
- **CI, `04b7142`:** "the root admits the new region before its regional bootstrap" (`fleet.rs:2611`).
- **Two local full-suite runs** (Linux container, four CPUs): "initial membership did not commit"
  (`fleet.rs:2664`). One region-0 member was never admitted to its council.

In all four, the test that finished just before it was
`a_fleet_node_under_a_containers_memory_bound_still_admits_a_client`. Run alone, the copyset test never
failed this way in about 100 runs.

## Evidence

The formation failure's message now carries each daemon's state. The member left out showed:

```
refusals={"fleet.bind": 1}                         its fleet failed to bind a serve port
record_links=[]  meshed=false
alive=[1094…, 5036…, 7078…, 9874… (itself), 16064…]   hosts from outside this test
```

Its four peers each saw an intruder alive, `14626965562813648463`, where it should have been. So the
port assigned to it was held by a socket of another test's daemons. The port's traffic went there, and
this test's node was cut off.

The differential makes it a cause, not a coincidence:

| Run (Linux container, four CPUs, 2026-09-25) | Result |
|---|---|
| The two tests together, before the fix | 1 of 6 failed, at the formation assertion `fleet.rs:2611` |
| The two tests together, after the fix | 8 of 8 passed |
| The full suite, before the fix | 2 of 2 runs failed formation |
| The full suite, after the fix | 50 of 50 passed (267 s) |

## Root cause

Every fleet test takes `serialize_fleet_tests()` so each runs against a quiet machine and its own
loopback ports. The memory-bound test did not. It is also the heaviest port user in the suite:

- `free_ports(2 × client_only_budget)` binds that many UDP sockets on port 0, reads the ports and
  releases them.
- Its node then dials every silent peer from ephemeral ports and re-dials from new ones.

Every test's serve ports come from the same bind-then-release (`mesh_serve_ports` → `free_ports`), so a
port is free only until someone binds it. Run beside another fleet test, this one's dialers took the
other test's just-released serve ports before its daemons could bind them.

The budget, and so the churn, grew on 2026-09-22 (`4a09b1d`): a pod-sized node now seats 330 clients
where it seated 1–2. That is why the collision became frequent enough to reach CI.

## Fix

The memory-bound test takes the fleet-test lock like every other fleet test. Its comment carries the
reason and this record.

## Found on the way (not changed here; reported)

1. **Ports are still allocated by bind-then-release.** Serialized, no two fleet tests overlap. But a
   test's own dialers could still take its own next node's serve port between allocation and bind. No
   failure has been traced to that. Allocating serve ports below the kernel's ephemeral range would
   close the whole class, because a bind to port 0 never returns such a port.
2. **The stalled first placement** recorded in
   `2026-09-25-a-forward-refused-while-the-owners-session-was-out.md` (found 3) is probably this same
   collision. That run's test order was not captured, so it is not confirmed.

## Edits

- `crates/server/tests/fleet.rs`:
  - The memory-bound test serializes.
  - The formation assertion prints each daemon's alive set, mesh, record links, council state and
    refusals (the diagnostics that found this).
- `docs/wip/GAPS.md`, `TBD_FIXES.md`.
