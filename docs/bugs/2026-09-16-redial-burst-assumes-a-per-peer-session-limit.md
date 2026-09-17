# The re-dial burst assumes a per-peer limit that the demultiplexer does not enforce

Date: 2026-09-16 (America/Chicago).
Status: investigated; fix designed, not implemented.
Contracts: §4.3 (bounded tasks), §4.8 (fleet membership and reconnection), §4.10a
(session demultiplexing), R5 (tests prove observable behaviour), R8 (one deployment path).

## Finding

The isolated re-dial test fails because it expects three dials to exhaust a pool sized for
future enrollment. It does not demonstrate a session-slot overfill. In the diagnostic run,
the target had **2,824 slots per plane**, while the burst made **three dials**.

The test also assumes a per-peer quota that has never been implemented by `Demux`:
`SESSIONS_PER_PEER` is multiplied by the peer capacity to size a shared pool. A slot is
allocated before the TLS handshake authenticates the peer. The peer certificate is used
later to replace the previous established session, not to allocate the initial slot.

## Reproduction and evidence

Test:
`a_peers_re_dial_burst_is_held_to_its_session_slots_and_never_refuses_a_client`.

Measured on the local macOS arm64 host on 2026-09-16. The load average immediately before
the clean build was 4.81 / 5.13 / 5.19 (`uptime`). No load generator or concurrent test
suite was started by this investigation.

| Source | Test time | Dials established | Client Status replies | Client refusals | Record replacements | Slot refusals | Live tasks before / after |
|---|---:|---:|---:|---:|---:|---:|---:|
| Existing workspace binary; source provenance not established | 22.89 s | 3 | 20 | 0 | 1 → 4 | 0 → 0 | 15 / 15 |
| Fresh, untouched archive of `04d2ea0` | 23.03 s | 3 | 16 | 0 | 1 → 4 | 0 → 0 | 15 / 15 |
| Same isolated source, with one capacity-printing diagnostic added to the fixture | 23.01 s | 3 | 15 | 0 | 1 → 4 | 0 → 0 | 15 / 15 |

All three runs failed at `assert_burst_bounded`'s `sessions_refused >= 1` assertion.
The clean trace's refusal observation was present and contained only
`fleet.accept.handshake: 4`; `fleet.serve_spawn` was absent. Its runtime pulses reported
zero refused admissions. The later assertions in the test are not reached after the
first assertion panics; the task and refusal values above come from its trace.

The diagnostic printed A's `fleet_peer_capacity=1412`, `session_limit_per_plane=2824`,
and `tasks_per_shard=16954`. B's values were 706, 1412, and 8482. These are measured
derivations for this run, not constants to put in a test. Capacity was not printed by
the untouched first run.

The clean source was extracted with `git archive` at full revision
`04d2ea0750207975d3b682aa7b91046dbd1a7511`. Its target directory was new and belonged
only to that source tree. The build took 23.28 s. Build and test had separate supervisor
limits of 180 s and 60 s; neither limit fired. The diagnostic repeat rebuilt only the
test fixture, in 1.06 s, and did not change production code.

Commands, from that isolated source directory, with `audit_root` naming its scratch parent:

```sh
env RUSTUP_TOOLCHAIN=1.98.0 CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0 \
  CARGO_TARGET_DIR="$audit_root/target" \
  cargo test --offline --locked -p slates-server --test fleet --no-run

env SLATES_FLEET_TRACE="$audit_root/fleet.trace" \
  "$audit_root/target/debug/deps/fleet-60877685d42923fa" \
  a_peers_re_dial_burst_is_held_to_its_session_slots_and_never_refuses_a_client \
  --exact --nocapture
```

Local evidence is in
`/private/var/folders/1s/ldpdh04d7d7219qts5t19d7h0000gn/T/slates-redial-clean-47696/`:
`revision`, `build.log`, `test.log`, `fleet.trace`, `capacity.log`, and `capacity.trace`.
The first binary was not used to attribute the failure to a commit; its trace contains
the uncommitted scheduler diagnostic. The clean archive excludes all workspace edits.

## Root cause and history

1. `crates/server/src/config.rs`, `DaemonConfig::with_fleet`, derives peer capacity as
   `C = max(seed_peers, base_tasks_per_shard / 6)`. Six is the existing task shape:
   two outgoing peer loops and two incoming session slots on each of two planes.
2. `crates/server/src/fleet.rs`, `run_membership`, passes `2 × C` to each `Demux`.
3. `crates/transport/src/demux.rs`, `Inner::open`, allocates from one global free list.
   `Demux::bind` replaces a previous session only after authentication identifies its
   certificate. There is no per-certificate slot quota in `open`.
4. `crates/server/tests/fleet.rs` still chooses `SESSIONS_PER_PEER + 1`, or three dials,
   and unconditionally asserts that this exceeds the record plane's capacity.

`git log -S fleet_peer_capacity -- crates/server/src/config.rs crates/server/src/fleet.rs`
identifies `f50e939` (2026-09-15) as the capacity expansion. Its diff replaces the seed
count with the enrollment capacity in both the task reserve and the demultiplexer size.
The old two-node fixture happened to have a global pool of two slots. The test retained
that assumption when the production pool grew.

This source history explains the invalid assumption; it is not a measured pass/fail
bracket around `f50e939`. No historical parent binary was run here. `git diff HEAD^ HEAD`
shows that the merge-identity commit changes neither the burst fixture nor the capacity
and demultiplexer paths. The merge failure and this assertion have separate causes.

The older per-peer wording is wrong even before enrollment: in a larger seeded fleet,
one peer could use the shared pool's available slots. The total bound still limits the
number of accepted endpoints and their serve tasks. These runs establish successful
replacement and reclamation; they do not prove fairness between competing peers or
the full-pool admission invariant.

## Recommended fix

### 1. State the actual admission contract

Retain the enrollment-derived capacity. Name the two-slot multiplier as a reservation
per peer-capacity unit, for example `SESSION_RESERVE_PER_PEER`, and document that it
sizes the shared pool. The invariant is:

```text
accepted endpoints per plane <= S = 2 × C
fleet task reserve = 2 × C + 2 × S + 5
```

An endpoint counts from allocation through final drop, including a pending handshake
and an endpoint whose routing has been closed. Closing a replaced endpoint must not
return its slot while its owning serve task still holds it. Keep the existing
generational handle and release-on-drop ownership.

Expose the actual session capacity and occupied/high-water counts in a shard-local
demultiplexer observation. Counters require no cross-thread synchronization. This lets
tests and operators distinguish successful replacement, saturation, and reclamation.
Use the same derived capacity for transport admission and task/timer budgeting.

### 2. Give the live fleet test an achievable, synchronized contract

Keep the small re-dial burst as the integration proof of authenticated replacement,
client availability, reclaimed serve tasks, and fleet recovery. Remove its unconditional
slot-exhaustion assertion only together with the saturation proof below. Rename the
test and its comments to describe replacement and bounded resource use.

Use an explicit test barrier to keep the burst in progress while the client performs a
Status request. Concurrent spawns alone do not guarantee overlap. Preserve the existing
replacement counter and successful-dial assertions, and observe task/slot reclamation.
Require `fleet_refusals()` to return an observation before treating a missing counter
as zero; an unavailable observation must fail with its own diagnosis.

### 3. Prove exhaustion at the transport seam

Add a by-use test in `crates/transport/tests/session.rs`, using the existing simulated
runtime and an explicitly small `Demux::start` capacity derived from the fixture's
participants. Drive real endpoints and datagrams:

1. Open the entire pool and hold the accepted endpoint owners behind a test barrier.
   Keep the demultiplexer's receive loop running.
2. Send a valid initial handshake from one additional source. Require the capacity
   refusal counter to move, occupied slots to remain at the bound, and the extra dial
   to remain unadmitted.
3. Release exactly one owner. Retry the extra dial, require it to establish and exchange
   an application request, then require all slots to return after their owners drop.
4. Cover replacement separately: closing the old session does not free its slot until
   the old endpoint is dropped, and dropping that stale owner does not remove the new
   session's routes. The new endpoint must continue answering requests.
5. Include a larger-pool case in which the same small burst is admitted without any
   capacity refusal. This pins the distinction that the enrollment change exposed.

Use barrier acknowledgements and simulated progress, not sleeps or a larger retry
budget, to establish occupancy. Simply changing the live burst to `capacity + 1` would
still not guarantee simultaneous occupancy and would make this host create thousands
of unnecessary handshake tasks.

### 4. Make the saturation counter unambiguous

`Inner::open` currently returns `None` both for an empty free list and for a TLS server
construction error; `route` counts both as `sessions_refused`. Return distinct typed
outcomes, count capacity exhaustion only for the former, and preserve the setup error
under a separate transport refusal category. Otherwise a TLS setup failure can make a
saturation test pass without exhausting anything. This is a sibling diagnostic defect,
not the cause of the observed zero counter.

### Exact edit scope for implementation

- `crates/server/src/config.rs` and `crates/server/src/fleet.rs`: clarify the pooled
  reservation and use its derived capacity consistently; retain enrollment headroom.
- `crates/transport/src/demux.rs`: expose bounded occupancy and distinguish setup refusal
  from capacity refusal; preserve slot ownership and authenticated replacement.
- `crates/server/tests/fleet.rs`: synchronize the replacement/client test and require
  successful observations.
- `crates/transport/tests/session.rs`: add controlled saturation, release, and replacement
  histories over the transport.
- `docs/wip/SLATES_DESIGN.md` §4.3's task-share status and §4.8's transport description,
  `docs/wip/GAPS.md`, and the 2026-09-14 task-budget bug record: correct the per-peer
  claim and distinguish replacement evidence from capacity-exhaustion evidence. Preserve
  the historical measurements and append their corrected interpretation.

Per-peer fairness is a separate admission-policy question. Enforcing it requires a
bounded pre-authentication handshake pool and enforcement after peer authentication;
source IP is not a peer identity, especially across NAT or a pod replacement. This
investigation does not claim that the current shared pool provides that guarantee.
No production admission-policy change is justified merely to force this test's counter
to increase.

## Validation still required after implementation

Run the new simulated saturation/replacement histories first, then the exact live fleet
test, the existing transport re-dial test, and the unlisted-node enrollment regression.
The latter guards against accidentally shrinking capacity back to the manifest's seeds.
Run the repository's required formatting, lint and structural checks for the resulting
change. Linux and KIND were not run in this investigation. No fix or passing-fix claim
is part of this report.
