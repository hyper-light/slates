# Ephemeral member id — learn-on-contact (task #22)

Status 2026-09-13. Design: §4.8 "Recovery" — *"the node rejoins with a new ephemeral id (a restart is a
join) and holds nothing for others until re-replication fills it; its owned objects are taken over by its
neighbours"* (line 1851) and *"a restarted host rejoins as a new member and holds nothing until its generation
and retained state are validated"* (line 1765); the failure matrix, *"on heal, C rejoins with a new node id,
its volumes have been taken over by its neighbours"* (line 3854).

## The two identities, as built

- **Stable anchor** — what a node's operator-provisioned certificate stands for across restarts. In a
  deployment it is the certificate's hash (`deploy::host_id_of_certificate`), the one fact about a node every
  peer already pins; on an in-process test fleet it is the machine identity's hash. The roster carries it
  explicitly per peer (`FleetPeer::anchor`), so the two sides of a session agree on it by construction. It
  keys authentication and the RIFL completion-record origin (a forwarded write stays exactly-once across the
  forwarding node's restart).
- **Ephemeral member id** — `deploy::member_id(anchor, generation)`: the leading eight bytes of
  `BLAKE3(anchor ‖ generation)`. It keys membership, ownership, rendezvous, `ObjectId::creator` and takeover.
  The **generation** is the anchor segment's start count (`SUP_GENERATION`, which the anchor's
  `record_start` increments before every daemon start); a fresh or anchorless segment is generation 0, so the
  manifest's precomputable seeds (`member_id(anchor, 0)`) form a first fleet with no exchange. An anchored
  daemon's very first boot is therefore already generation 1: the seed is a **placeholder** the peer's first
  contact replaces, exactly as a restart does.

## What this change adds (over the foundation in `ac40b7b`/`484768a`)

1. **The generation rides the wire.** `SwimMessage::Ping` and `Ack` carry `generation` after the nonce
   (`crates/cluster/src/swim.rs`; golden vector and hostile-input tests updated; a truncated generation is
   `Truncated`). `serve_probe` announces the server's; `ProbeOutcome::Acked` returns the acker's.
2. **One validating fold on both sides** (`crates/server/src/fleet.rs`). `classify_announced` is the pure
   decision: an announced id is **forged** unless it is `member_id(anchor, generation)` for the anchor the
   authenticated certificate stands for; **stale** if the generation is below the one learned; **current**
   if equal; a **restart** if above. `learn_member` applies it against `ShardState::learned_members` (one
   entry per rostered anchor, seeded at boot with the generation-0 seeds — bounded by the roster): a restart
   folds the old id **dead** at its current incarnation and the new id **alive** in the shared membership
   and carries the peer's region over to the new id; stale and forged announcements are counted
   (`fleet.member_generation_stale`, `fleet.member_id_forged`, in the status report's refusals) and fold
   nothing. The serve side (`serve_peer_probes`) runs it on every rostered prober's ping and answers a stale
   or forged prober with **no acknowledgement**; the probe side (`probe_and_apply`) runs it when a peer
   answers under a different id.
3. **The probe task follows the peer's current id** (`follow_current_id`): whichever side learned the
   restart, the task moves to the new id next period — the old id is dead in its detector (never probed
   again; its death handed to the other shards), the new joins fresh, the mesh record moves, the round-trip
   law restarts. So a restarted node is **probed** under its new id, not merely believed alive from its own
   pings — which is what the pre-change tree lacked (below).
4. **The record link and the record serve resolve the peer's current id** (a restart drops the dead session
   and re-dials once the council admits the new member; a record is accepted under the id the peer now
   writes as).
5. The council then commits `TakeOver(old)` and `Admit(new)` through its existing `reconcile_alive`, which
   installs the configuration, bumps the old id's fencing epoch, and takes its objects over.

## Tests and numbers (2026-09-13, 18-core shared box, load averages as noted)

- Pure: `fleet::tests::an_announced_identity_is_current_restarted_stale_or_forged` (server);
  `deploy::tests::the_member_id_is_ephemeral_per_generation_over_a_stable_certificate` (existing); the SWIM
  wire round-trip, golden and hostile tests (cluster, 115 unit tests green; live `swim` 4/4,
  `membership_takeover` 1/1).
- By use, over the wire: `a_restarted_peer_is_learned_on_contact_under_its_new_generation`
  (`crates/server/tests/fleet.rs`). Three nodes form at `f = 1`; B seals a volume A and C hold; B's process
  ends; a second daemon presents B's certificate at generation one — attaching an anchor segment on which
  the test, playing the anchor, recorded the start — on the address A and C dial for B. Asserted: the new
  id admitted, the old retired, both committed into the regional configuration, the probe mesh formed to
  the new incarnation on every side, the volume taken over by the rendezvous-first survivor and its file read
  back over that survivor's NFS port. **After:** passed 5.24 s (load 5.2), then 4.29, 4.98, 5.04, 5.25,
  4.54 s across the re-runs (loads 7–10). **Before** (`f3ced8b`, the same scenario with 30 s wall-clock
  polls, in a throwaway worktree): `admitted_new=true retired_old=true committed=true meshed_to_new=false`
  — FAILED in 34.17 s (load 9.0). The old tree converged its membership only because A's probe task, dialing
  the restart's address, aged the old id out of the acknowledgements it did not credit; it never probed the
  new id (the restarted node's next death would have been undetectable by the survivors' own probes), and it
  validated nothing (any announced id was folded). The wrong prediction is recorded: I expected the old tree
  to fail on `retired_old`; it failed on `meshed_to_new`.
- Siblings kept and green: `a_restarted_peer_rejoins_under_a_new_member_id_and_the_old_is_retired` (the
  injected fold, isolating it from the wire; its doc now points at the wire-level proof) and
  `a_falsely_retired_peer_rejoins_by_refutation` (A-15: the same generation refutes and keeps its id — the
  contrast), 5.79 s.
- Gates: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo xtask check`
  clean.

## Owed, reported (not done here)

- **The Raft voter sets do not follow a restart.** The new id is admitted as a *member*, but the council's
  and the root group's voter sets are Raft's `all_voters`, fixed at formation to the seeds — a restarted
  voter's new id is a learner until Raft membership change lands (the parallel item), and a dead seed keeps
  counting toward every majority.
- **A restarted node's failure domain.** The council's domain map is keyed by member id at formation, so a
  new id has no declared domain (unique-per-host, the safe default — copysets never repeat it — but the
  operator's declaration is lost). Carrying the domain on `Admit` (a council log change) closes it.
- **No wire-level test of a stale or forged refusal yet** — the pure classifier covers the decision; the
  serve side's "no acknowledgement" and the counters need an in-process accessor for the status report's
  refusals to assert by use.
- **The gated three-process deployment test** (`SLATES_TEST_CLI=1 … three_daemon_processes_deploy…`) was not
  run here (the integrator's, on a quiet box). It is the one that exercises real anchored daemons announcing
  generation 1 on their first boot, so the seeds are replaced on first contact; the in-process suite's
  daemons are generation 0 and match their seeds.
- `verbs::serve_forward`'s RIFL origin stays `host_id_of_certificate(presented)`: in a deployment that equals
  the rostered anchor; on an in-process fleet it differs from the machine-identity anchor, harmlessly (both
  ends of that path use the certificate hash).
