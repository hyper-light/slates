# The re-dial burst assumes a per-peer limit that the demultiplexer does not enforce

Date: 2026-09-16 (America/Chicago).
Status: implemented (2026-09-16, see "Implemented" and "Validation performed" below).
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

## Implemented (2026-09-16)

1. **The admission contract.** `config::SESSION_RESERVE_PER_PEER` is the reservation per unit of peer
   capacity; `DaemonConfig::with_fleet` derives `fleet_sessions_per_plane = SESSION_RESERVE_PER_PEER ×
   fleet_peer_capacity` once (logged with its input) and `fleet::run_membership` sizes each plane's
   `Demux` from it, so admission and the serve-task reserve read one number; `fleet::SESSIONS_PER_PEER`
   is gone. The invariant (`S = 2 × C` accepted endpoints per plane; reserve `2C + 2S + 5`) is stated at
   `with_fleet` and in §4.3's correction. `Demux::capacity()` and `DemuxCounters::high_water` expose the
   pool's size and its peak occupancy (counted from a slot's allotment to its endpoint's drop).
2. **The live test, synchronized.** `a_peers_re_dial_burst_replaces_its_sessions_and_never_refuses_a_client`:
   each dialer holds its established session until released (`dial_record_socket`'s `release`), and the
   client is served once more while every dial holds — the overlap by construction (`verbs_while_held`);
   capacity and setup refusals are asserted **zero**; the refusal map must be **observed** (an absent kind
   in an observed map is the only zero). Measured alone (load 5.9): ok in 22.92 s — 3 dials all
   established; 16 verbs in flight and 1 while held, 0 refused; record plane `replaced 1 → 4`, `opened
   2 → 7`, `sessions_refused 0 → 0`, `setup_refused 0 → 0`, `high_water 2 → 4`; live tasks 15 → 15;
   refusals `{fleet.accept.handshake: 4}` (the replaced sessions' serve tasks ending, as on 2026-09-14).
3. **Exhaustion at the seam.** `crates/transport/tests/session.rs`, over the simulated fabric with pools
   the fixture sizes (17/17 in 0.44 s): `a_full_pool_refuses_the_next_dial_until_a_slot_is_released`
   (`SMALL_POOL` = 2 held → the extra dial refused for capacity with the pool at its bound and nothing
   opened, its handshake budget spent → one slot released → the retry admitted and served → every slot
   back, high-water 2); `a_replaced_session_holds_its_slot_until_its_stale_owner_drops` (`replaced` = 1,
   the pool holds both while the closed session is undropped, a third peer refused for capacity, the
   stale drop returns the slot, the third admitted, X's second request still answered over the
   replacement); `a_burst_within_the_pool_is_admitted_with_no_capacity_refusal` (`ROOMY_POOL` = 8, the
   same three dials admitted whole, high-water 3); and
   `a_roster_the_server_cannot_build_from_is_a_setup_refusal_not_capacity` (item 4's category).
4. **The counter, unambiguous.** `Inner::open` returns `Result<Slot, OpenRefusal>` — `Exhausted` or
   `Setup(EndpointError)` — and `route` counts each under its own category (`sessions_refused`,
   `setup_refused`); the last setup refusal's error is kept (`Demux::last_setup_refusal`). A roster entry
   that is not a certificate now counts `setup_refused ≥ 1`, `sessions_refused` 0, nothing opened.

Docs: §4.3 and §4.8 corrections (the measurements kept, their interpretation corrected), the two
2026-09-14 task-budget records, GAPS (the shared pool; per-peer fairness open).

## Validation performed (2026-09-16)

In the order prescribed: the four simulated histories (17/17, 0.44 s); the live fleet test (ok, 22.92 s,
alone); the existing transport re-dial test (`a_peer_that_redials_replaces_its_old_session`, in the
17); the unlisted-node enrollment regression (`an_unlisted_node_enrolls_through_one_seed_and_joins_the_existing_quorum`,
ok, 3.22 s, alone — the capacity was not shrunk back to the seeds); the config derivation test
(`a_fleet_configuration_derives_its_own_task_share_from_the_peer_count`, ok, 0.75 s). `cargo xtask
check` ok; the formatting and lint gates' final run is recorded in the commit. Linux and KIND run on
the push.
