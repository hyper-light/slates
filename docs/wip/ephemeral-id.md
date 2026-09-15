# Ephemeral member id — learn-on-contact (task #22)

> Superseded on 2026-09-14 by [AUD-07](../bugs/2026-09-14-raft-voter-state-loss.md): the stable
> anchor still authenticates contact, but a random per-start nonce replaces the supervision counter.
> Manifest seeds never vote. Fresh members import the common prefix once, then join by consensus.
> The text below records the earlier implementation and its evidence.

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

## The follow-up (Ada: "Then fix these?"), same day

6. **An admission carries the node's failure domain.** `ConfigCommand::Admit { host, domain }` — the
   council log entry gained a presence byte and, when present, the little-endian domain id
   (`crates/cluster/src/config_group.rs`; round-trip test covers both forms). `reconcile_alive` takes each
   alive host with the domain its node declares; the leader resolves it (`fleet::declared_domain`) from the
   deployment's declaration, which is keyed by the node's generation-0 seed: a member at a later generation
   is mapped to its node through the anchor it was learned under (this node's own through its origin anchor)
   and looked up by `member_id(anchor, 0)`. `RegionalConfiguration::admit` inserts the domain;
   `retire` now **drops** it, so the map stays bounded to the members while every restart admits a new id
   (banned item 8 — before, admissions never touched the map, so it could not grow; now it can, and it is
   pruned). The epoch is still kept on retire (fencing).
7. **The refusals are observable and proven over the wire.** `Daemon::fleet_refusals` (the status report's
   counts) and `Daemon::council_domains` (the committed domain map) are one-shot control-shard observations
   like the other accessors.
8. **The RIFL completion origin is the rostered anchor.** `serve_peer_records` no longer hashes the presented
   certificate a second time: the anchor the roster holds for that certificate — the same one the member id
   derives from and announcements are validated against — is both the learned-id key and the forwarded
   write's origin. In a deployment the two were equal; on an in-process fleet they now are too (one id per
   node on every plane).

Tests and numbers for the follow-up (2026-09-13, same box, loads 7–8):

- `config_group::tests::an_admission_carries_the_members_declared_domain` (leader admits B with a domain
  and C without; both voters agree) and the reconcile test now carries B's domain to the follower; cluster
  unit 116/116, `config_group_live` 1/1.
- `register::tests::an_admission_carries_the_members_domain_and_a_retirement_drops_it` (db).
- By use: `a_restarted_peer_is_learned_on_contact_under_its_new_generation` now declares B's node in a
  failure domain and asserts the restart's new id is committed under it on every survivor
  (`council_domains`). **Before** (the same tree with the admission carrying no domain — `declared_domain`
  neutralized for the run, then reverted): FAILED on `domain_carried` after 445.46 s (the poll ran out its
  4000-period daemon-time budget; every earlier step passed). **After:** passed 4.49 s (load 7.2).
- By use, new: `a_stale_or_forged_announcement_is_refused_and_counted`. A and B form, B at generation one;
  then a daemon on a fresh segment (generation zero, B's seed) and a daemon configured with a flipped anchor
  (an id B's certificate cannot derive) both present **B's certificate** to A. Asserted: A's
  `fleet.member_generation_stale` and `fleet.member_id_forged` both ≥ 1; A's alive membership held exactly
  {A, B₁} over a settle window once both were counted; each refused announcer, never acknowledged, aged A to
  death in its own view. Passed 8.91 s (load 7.5). Its before-state is "unobservable", not "failing": the
  counters existed since `ca0577a` but no accessor exposed them. Design note found on the way: a same-cert
  dialer **replaces** the peer's live session at the serve demultiplexer (`Demux::bind`, by certificate), so
  a stale or replayed boot holding B's key can bounce B's real session at A; A's own belief about B is
  unaffected (it rides A's dial to B), and the announcer still gets no acknowledgement.
- Siblings re-run one at a time, green: `a_restarted_peer_rejoins_under_a_new_member_id_and_the_old_is_retired`
  3.77 s; `a_falsely_retired_peer_rejoins_by_refutation` 5.78 s. Server lib 21/21, db register 32/32.
- Gates clean again (fmt, clippy `-D warnings` — one test split for cognitive complexity — `xtask check`).

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
  voter's new id is a learner until Raft membership change lands (the parallel raft-membership item), and a
  dead seed keeps counting toward every majority. Left to that branch deliberately: both touch
  `config_group.rs`, and the `Admit { host, domain }` log change here will need a merge against it.
- **The gated three-process deployment test** (`SLATES_TEST_CLI=1 … three_daemon_processes_deploy…`) was not
  run here (the integrator's, on a quiet box). It is the one that exercises real anchored daemons announcing
  generation 1 on their first boot, so the seeds are replaced on first contact; the in-process suite's
  daemons are generation 0 and match their seeds. It now also covers the manifest's domains riding an
  admission (`declared_domain` over a manifest-keyed map).
- The three items closed above (domain on `Admit`, the refusal accessor and wire-level test, the RIFL origin)
  were owed here in the first report; they are done.
