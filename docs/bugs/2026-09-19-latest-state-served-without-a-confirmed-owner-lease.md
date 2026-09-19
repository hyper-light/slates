# Latest-state service was not fenced by a confirmed owner lease (AUD-08)

Date: 2026-09-19. Contracts: §4.8 "Leases and reads" and "Required persistence and protocol
invariants (A-9)"; GAP-A9-7; AUD-08 in `docs/bugs/2026-09-14_AUDIT.md`.

## Symptom

An owner served the **latest state** of its objects — a read at the live head, the head version,
a status, the mount's live tree — with no check that its authority was still current. Epoch
fencing stops a stale owner's *records* at the holders; it does not stop the owner answering its
own clients. So an owner isolated from its neighbourhood kept answering its local live view while
the surviving quorum took its objects over and advanced them: the isolated owner's clients read a
past. The control dispatcher gated certain writes on a durability shortfall, and the NFS export
served through a per-request attachment, but neither tested a confirmed read lease, and a Raft
CheckQuorum role is not one. Source-confirmed by the September 14 audit.

## Root cause

There was no owner lease. Nothing recorded, per object, that a quorum of its candidate holders had
recently confirmed this node as the live owner under the current configuration, and nothing gated
latest-state reads on that. Membership heartbeats arrived and were folded, but "a member believes
its peers alive" is exactly the signal the design forbids as a lease (§4.8: "Membership heartbeat
arrival is not a lease grant").

## Fix

A new module `crates/server/src/lease.rs` holds the rule and its safety argument; the daemon
collects the evidence on the SWIM probe plane and gates the service on it.

- **The evidence.** Every SWIM `Ping` and `Ack` now carries the sender's newest known
  **configuration version** (`crates/cluster/src/swim.rs`). When this node's probe of a peer is
  acknowledged (`fleet::probe_and_apply`), it records a **confirmation** — the probe's *send* time
  on the host clock and the version the peer announced — into `OwnerLease`. A peer announcing a
  version newer than this node's installed one marks the node **superseded**.
- **The lease** (`OwnerLease::holds`). This node may serve an object's latest state while it is not
  superseded and `f` of the object's *other* candidate holders confirmed it within `lease_bound_ns`
  under the installed version — with the owner itself that is `f + 1`, a quorum of the `2f + 1`
  copyset. The bound is the membership **horizon** (the detector's own death window: probe deadline
  plus the suspicion span, dilated to the Lifeguard local-health cap) less twice RFC 5905's 500 ppm
  clock-rate tolerance, measured from the probe's send time — so a lease measured on the owner's
  (possibly fast) clock ends before a holder's (possibly slow) clock opens the matching promotion.
- **The interlock** (`AnswersGiven::promotion_open`, gating `fleet::takeovers`). A holder answers a
  successor's promotion of a departed owner's object only once it has not answered that owner's
  probe for the horizon (so any lease it fed has lapsed) or the owner has announced it saw the
  retiring configuration. Any `f + 1` promotion quorum intersects any `f` fresh confirmations, so
  while an owner's lease holds no successor can adopt — the safety the design asks for.
- **The gate.** `verbs::dispatch` refuses `Refusal::LeaseUnconfirmed { version }` for a read of an
  owned object's latest state (`Read` at `Head`, `Versions`, `Status`, `ChangedSince`) when
  `verbs::lease_unconfirmed` holds; the mount (`crate::nfs`) answers `NFS3ERR_JUKEBOX` for every
  procedure the same way. An explicitly pinned immutable read — `Read` at a `Version` or an
  `Attachment` — is **not** gated: it keeps its separate contract (verified content and read
  rights, no latest-head lease).
- **Pauses, expiry, takeover.** The lease is read per request against the suspend-inclusive host
  monotonic clock (`slates_machine::clock`), never against a loop having run, so a paused owner's
  lease lapses by the clock while it is paused. The control shard's lease is fanned to every owner
  shard each period (`fleet::fan_configs_to_shards`) as absolute times, so a stale fan only shortens
  a lease. On installing a newer configuration (`fleet::sync_config_from_council`) a supersession at
  or below it is resolved and this node's held objects' routing is reassigned to their successors.
- **Bounded startup allowance.** For the membership horizon after installing a configuration, the
  lease holds without `f` fresh confirmations, so a just-formed or freshly-reconfigured **reachable**
  owner does not false-refuse while its first acks under the new version accumulate. Safe because a
  takeover cannot commit until the council has confirmed this node unreachable for its
  death-confirmation window, which exceeds the horizon: within a fresh configuration's first horizon
  no successor can exist. An owner cut off long ago installed its configuration long ago and gets no
  allowance.
- **Laptop (`f = 0`).** The only candidate is the owner; `needed = 0`, so the lease holds with no
  confirmations — the same arithmetic, no branch (R8).

## Failing test first, and regression

`crates/server/tests/fleet.rs::an_isolated_owner_refuses_latest_state_reads_while_the_successor_advances_the_green`
— three-node `f = 1`: a green advances three versions on owner A, both holders catch up, and A
serves its head version, a head read and a version-1 read on its client connection. A is then
**isolated** on the probe plane both directions (not stopped, the connection kept), and its control
shard is **paused** for two lease bounds — the pause across expiry the audit requires. When A
resumes its lease has lapsed by the clock; meanwhile the surviving quorum retires A, the successor
rendezvous ranks first materializes the green (AUD-14) and a new work advances it to version 4.
Then on A's original connection the head version and a head read refuse `LeaseUnconfirmed`, while
the pinned version-1 read still serves `hello`; the successor serves version 4. Before the fix A
served all three latest-state reads unconditionally. Unit tests in `lease.rs` pin the bound
arithmetic, the confirmation freshness/version/`f` logic, the supersession, the startup allowance
and the promotion interlock.

## Validation (this box, 18 cores, 2026-09-19)

- `cargo test -p slates-server --test fleet an_isolated_owner_refuses_latest_state_reads_while_the_successor_advances_the_green`:
  1 passed, 9.08 s.
- `cargo test -p slates-server --test fleet` alone: **48 passed, 0 failed, 320.66 s** (2026-09-19,
  12:05:26 → 12:10:49).
- `cargo test -p slates-server --lib lease`: 7 passed; `slates-cluster` lib 150 passed (SWIM wire
  golden vectors and hostile-input decoders updated for the new field); `slates-ipc`, `slates-bridge-nfs`
  suites pass; the server daemon suite 9 passed in 14.38 s.
- `cargo clippy` (server, cluster, ipc, bridge-nfs, all targets, `-D warnings`), `cargo fmt --check`,
  `cargo xtask check`: clean.

## Scope and siblings

- The lease is the deployed-service gate (AUD-08). The separate Raft-core `read_index` staleness
  (AUD-09) is a core method with no production caller and is not this change.
- Forwarding an **owned volume with no routing entry** (a node's own created volumes) to its
  successor after a same-id re-admission is the broader ledger/prefix-transfer contract (GAP-A9-7);
  the lease refuses the stale copy's latest state in the meantime, and the held-object routing yield
  covers what this node backs.
- Writes that commit a new head or seal are gated by durability (§4.8) and, at `f > 0`, do not
  publish acceptance until the record commits at the quorum (AUD-11), so they cannot return a stale
  success; the lease adds the missing **read/service** gate.
- The A-9 `FencedRegister` TLA+ revalidation the design still owes covers the register's
  `StaleNeverCommits`; the lease's own safety argument (the quorum intersection above) is stated
  here and checked by the promotion-interlock unit test, not by that model.
