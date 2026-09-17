# The fleet's tasks were admitted against the clients' task budget, and a refused fleet spawn was silent

Date: 2026-09-14
Area: `crates/server/src/config.rs` (`DaemonConfig::with_fleet`), `crates/server/src/fleet.rs` (the boot's
loop spawns, `accept_probes` / `accept_records`, `spawn_detached`), `crates/server/src/daemon.rs`
(`init_shard`, `control_loop`, the mount listener; the `fleet_demux_counters` and `live_tasks`
observations), `crates/server/src/nfs.rs` (`serve`)
Severity: banned item 8 (a structure without a derived bound: the fleet's task population had no share of
the shard's task arena, so under load it consumed the clients') and banned item 9 (a swallowed error: eight
`if let Ok(task) = futures::spawn(..)` sites dropped a refused spawn in silence — a plane's receive loop,
an accept loop, a peer's probe loop or record link, the coordinator, a peer session's serve task, the
shard's serve, reap and heartbeat loops, the mount listener and each of its connections).

## Symptom

`docs/wip/fleet-under-load.md` (2026-09-14): at ~2.5–3× CPU oversubscription on the WAN tree a
late-in-suite test stalled with the control shard's enriched pulse reading `adm_refused=4554`,
`spawns=34, done=13` — the shard's admission bound full of accept-side handshakes
(`serve_peer_records` / `serve_peer_probes` → `establish`), each held for its bounded retransmit budget
while its peer was starved — so the harness's observation spawn was refused and its poll timed out at the
10 s observe budget. The record classified this as a bounded wait and left the design question it raised:
"the accept-side task budget is the shard's admission bound, not a value derived from the handshake budget
× the expected re-dials".

## Root cause

`DaemonConfig::derive` sizes the task arena as `clients_per_shard × TASKS_PER_CLIENT +
LOOP_TASKS_PER_SHARD` — the clients' cross-shard traffic and the shard's own five loops — and
`with_fleet` added nothing to it. Every fleet task was therefore admitted out of the clients' share: per
peer the probe loop and the record link; per plane the demultiplexer's receive loop and the accept loop;
the coordinator; and, on the accept side, one serve task per session the demultiplexer holds. The
demultiplexer *does* bound those sessions — `SESSIONS_PER_PEER` (2) per peer per plane, a session's slot
held until its serve task drops the session (`Demux::release`), so serve tasks alive per peer per plane can
never exceed two — but nothing in the budget accounted for them, and under starvation each is held for a
whole handshake budget (32 retransmits at the probe timeout). That is the population that filled the arena.

Independently, every one of those spawns was written `if let Ok(task) = futures::spawn(..) { detach }`:
a refused spawn left the node silently without a plane, a peer link, its coordinator, or a session's
server; the daemon's own serve, reap and heartbeat loops and the mount listener had the same shape.

## Fix

- **The fleet's task share is derived** (`DaemonConfig::with_fleet`): `fleet_tasks_per_shard = peers ×
  FLEET_LOOPS_PER_PEER + peers × SESSIONS_PER_PEER × FLEET_PLANES + FLEET_LOOPS_PER_SHARD` (2, 2, 2 and
  `FLEET_PLANES × 2 + 1 = 5`, each a documented shape constant naming its loops), added to the shard's task
  **and timer** budgets (every fleet task may hold a timer) and logged in the boot derivations with its
  input. Five peers add 35 tasks: 4,241 → 4,276 on this box's profile. A burst of re-dials under load now
  fills the fleet's share and never a client's. Every shard receives the share (one number, a few dozen
  slots on shards that hold no peer session; R8 — one code path).
- **Every refused spawn is typed.** `fleet::spawn_detached(future, refused)` replaces the eight fleet
  sites: a refused boot loop counts `fleet.loop_spawn`, a refused session serve task counts
  `fleet.serve_spawn` (the accepted session is dropped explicitly — its slot returns to the demultiplexer
  and the peer's next re-dial takes a fresh one). The shard's serve and reap loops are a typed
  initialization failure (`init_shard` → `ServerError::Runtime`, surfaced as `INIT_FAILURES`): a shard
  without them serves nothing. The anchor heartbeat and the mount listener count `daemon.loop_spawn`; a
  mount connection whose serve task is refused counts `nfs.serve_spawn` (the kernel client reconnects).
- **Two observations** (§4.14): `Daemon::fleet_demux_counters` (per plane: opened, replaced, refused for a
  slot, dropped) and `Daemon::live_tasks` (the control shard's arena occupancy).

## Tests

Failing test first, `crates/server/src/config.rs`:
`a_fleet_configuration_derives_its_own_task_share_from_the_peer_count` — with the share not applied:
`left: 4241, right: 4276`; with it: ok (0.78 s).

By use, `crates/server/tests/fleet.rs`:
`a_peers_re_dial_burst_is_held_to_its_session_slots_and_never_refuses_a_client` — a two-node fleet; three
concurrent dials of A's record socket presenting B's certificate (one more than the two slots one peer is
allotted), driven from a runtime of the test's own as the daemon's own dialer drives a handshake (a budget
run out with the peer silent retried on the same socket, its pending flight resent); a client of A running
`Status` verbs throughout. Measured on this box (2026-09-14 13:52, load average 9.9): 3 dials all
established in turn; **301** client verbs ran during the burst, **0** refused; the record plane's
`sessions_refused` 0 → **9** and `replaced` 0 → **3** (`opened` 1 → 4); A's live tasks **16** before,
**15** after (one serve task per live session, never one per dial); `fleet.serve_spawn` absent; the fleet
still meshed; 7.50 s. Honestly stated: this test passes on the tree before the fix too — its arena had
thousands of slots of headroom — so it is not the failing-first proof of the share (the configuration test
is); it pins the **premise the share's derivation rests on**: the demultiplexer refuses a dial past a
peer's slots (typed, counted) rather than opening a session, and a replaced session's serve task ends,
so the accept-side population is bounded by `SESSIONS_PER_PEER × planes` per peer. It fails if either
breaks.

Not proven directly: the `fleet.serve_spawn` count under a *full* arena. Filling a daemon's arena from
outside is exactly what admission prevents, so the proof needs a test-facing arena cap; owed. The counting
path itself (`count_refusal` → `Daemon::fleet_refusals`) is the one
`a_stale_or_forged_announcement_is_refused_and_counted` drives.

## Verification

`cargo test -p slates-server --lib a_fleet_configuration_derives_its_own_task_share_from_the_peer_count`
→ ok; `cargo test -p slates-server --test fleet a_peers_re_dial_burst_is_held_to_its_session_slots_and_never_refuses_a_client -- --exact`
→ ok (7.50 s); `cargo clippy --workspace --all-targets -- -D warnings` clean; `cargo xtask check` ok
(unsafe budgets unchanged). Validated on a wiped `target/` (2026-09-14 13:56–14:03, load average ~10, the
KIND lane's cluster alive on the box): 20/20 fast suites, the three gates, and the fleet suite **36/36 in
195.89 s** under the 300 s stall detector (`validate.sh accept-budget`).

## Sibling sweep

- `xshard::call_on` / `call_within` (the record plane reaching owner shards each period) spawn no task on
  the target shard — a pending-call table and a foreign wake — so owner shards need no share of their own.
- `serve_peer_probes` / `serve_peer_records` spawn no per-request child tasks (each request kind is a
  stream on the session, served inside the task), so the per-session count is the whole accept-side
  population.
- The eight silent spawn sites are listed above and fixed in this change; `grep -rn 'if let Ok(task) =
  futures::spawn' crates/*/src` is now empty.
- `Daemon::observe` itself spawns a task per observation (its own admission is retried under
  `spawn_admitted`); a test's `live_tasks` reading includes that task, on both sides of a comparison.

## Correction (2026-09-16): the session bound is a shared pool, not a per-peer quota

The root cause and the by-use test above describe `SESSIONS_PER_PEER` as a bound the demultiplexer
enforces "per peer per plane", and read the burst's `sessions_refused 0 → 9` as "the demultiplexer refuses
a dial past a peer's slots". The demultiplexer never enforced that: `Demux` allots a slot to a *source*
before the handshake authenticates it, from one free list, and the certificate learned at establishment
only replaces that peer's previous session. The constant is a **reservation per unit of peer capacity** that
sizes a shared pool (`fleet_sessions_per_plane = SESSION_RESERVE_PER_PEER × fleet_peer_capacity`,
`DaemonConfig::with_fleet`, the one derivation the transport's admission and this task share both read);
the accept-side population is bounded by the pool, `S = 2 × C` per plane, not by `2 × planes` per peer.
The 2026-09-14 measurements stand: the fixture's pool was `peers × 2 = 2` slots (one peer), so three dials
did overflow it and the counter moved. Once `f50e939` grew `C` to the enrollment capacity (2,824 slots per
plane on this box), the same burst is admitted whole and the assertion failed on every host. The live test
now proves replacement, client availability and reclamation with no capacity refusal; exhaustion, release
and replaced-slot retention are proven at the transport seam over the simulated fabric
(`crates/transport/tests/session.rs`). `docs/bugs/2026-09-16-redial-burst-assumes-a-per-peer-session-limit.md`.
