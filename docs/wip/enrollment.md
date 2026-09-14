# §4.13 enrollment: consumers, attestation, revocation, sharing

Date: 2026-09-13. Branch `agent/enrollment` (`596bfb0` the grant issuer; `c9f1415` a paused snapshot;
this change completes it). Design: §4.13 "Security specification (A-8)" — *Principals*, *Access lists*,
*Grants*, *Refusals added*. Ledger: GAP-A9-9 ("Same-uid agents share ambient authority; channel class is
not evidence of a human approval").

## What the design says, in the words the code realizes

- "Host credentials establish `AccountId`; a trusted enrollment establishes `ConsumerId` and scoped
  rights for a workload under that account."
- "A consumer channel is bound at rendezvous using a capability delivered and retained outside other
  agents' reach … Per-request identity strings and peer uid alone cannot establish consumer identity."
- "Rights are checked before resource admission, namespace lookup that reveals protected content, queue
  creation or any VFS/device effect."
- "Only an authenticated human confirmation surface holds grant-issuer authority … running a CLI
  executable, claiming a control header class or sharing the human's uid is insufficient."
- Refusals: `ConsumerNotEnrolled`, `ConsumerRevoked`, `GrantIssuerUnverified` — "closed variants carried
  through §4.4's operation refusal taxonomy."

## What is built

**The issuer secret and grants (`596bfb0`).** The daemon mints a 256-bit issuer secret at every start
(`transport::handshake::secure_random`) and publishes it into the anchor segment's supervision block
(`SUP_ISSUER`, RAM only — mapped by the supervisor, the daemon and a `slates` command running as the
anchor's user; never by a ring, MCP or SDK client). A `Grant` carries `BLAKE3_keyed(secret, landing ‖
manifest ‖ scope ‖ term)`; the daemon recomputes it (constant-time compare) and refuses
`GrantIssuerUnverified` on a forged, replayed, retargeted or modified-plan approval. `slates grant` is the
human surface. Record in that commit's message.

**Consumers (this change, completing the paused snapshot).**

- *Wire* (`crates/ipc/src/protocol.rs`, appended for append-only evolution): `Enroll { account, proof }`,
  `Attest { consumer, proof }`, `Revoke { consumer, proof }`, `Share { volume, principal, rights }`;
  replies `Enrolled { consumer, secret }`, `Attested`, `Revoked`, `Shared`; `Principal::{Uid, Consumer}`
  and `Rights { read, write, admin }` as a `share` names them.
- *Durable record* (`crates/db`): `ConsumerRecord { consumer, account, secret, revoked }`;
  `Op::ConsumerEnrolled`, `Op::ConsumerRevoked` (guard-then-apply, replayed with the log, in the
  snapshot). The catalog's `Principal::Consumer { account, consumer }` (`KIND_CONSUMER = 4`).
- *Proofs* (`crates/server/src/landing.rs`, pure): `enroll_proof = BLAKE3_keyed(issuer, "slates.enroll" ‖
  account)`, `revoke_proof = BLAKE3_keyed(issuer, "slates.revoke" ‖ consumer)`, `attest_proof =
  BLAKE3_keyed(capability, client_id)`. Domain tags separate the issuer secret's uses; the attestation is
  bound to the channel's client id so a proof captured from one session cannot bind another. Golden
  vectors and the separation are unit-tested (`the_enrollment_proofs_are_domain_separated_and_pinned`).
- *Enroll* (`verbs::enroll`): the issuer proof verifies on the channel's shard, a consumer id is minted
  owner-tagged like a landing id (`landing_id(partition, next_seq)`), the record is written durably, the
  capability is returned once to the human. A forged proof refuses `GrantIssuerUnverified`, counted — an
  agent cannot enroll itself.
- *Attest* (`verbs::attest_on_channel` + the pure `verify_attestation`): served as a task on the
  channel's shard that reads the record from the partition the id names (`call_within`, liveness-budget
  bounded, **mapped through `shard_of_partition`** — the id names a partition, `call_within` addresses a
  shard), decides — enrolled, not revoked, the channel's own account, this channel's proof — binds the
  slot's principal to `Consumer { account, consumer }` and delivers. Refusals `ConsumerNotEnrolled` /
  `ConsumerRevoked`, counted where decided. After the bind no cross-shard call is on any verb's path: the
  principal *is* the consumer and rights are checked locally.
- *Revoke* (`verbs::revoke_on_channel` + `revoke_everywhere` + `mark_revoked`): the issuer proof verifies
  on the channel's shard (forged → `GrantIssuerUnverified`, counted, no cross-shard work); then a task
  records the revocation on the owner partition's shard and marks every shard's slots bound to the
  consumer — each a bounded `call_within` — and **only then** delivers `Revoked`. The per-verb gate in
  `serve` is one local read of `slot.revoked`, before any lookup or mutation, whatever the verb.
- *Share* (`verbs::share`): `admin` on the volume sets a principal's rights; all-false removes the entry.
  The owner's rights are not an entry and cannot be reduced.

## Failing tests first

| Test | Before | After |
|---|---|---|
| `distinct_consumers_under_one_uid_hold_only_the_rights_shared_with_them_until_revoked` (`crates/server/tests/daemon.rs`; a 2-shard daemon, each client on its own shard) | did not compile at the snapshot (`attest_on_channel` undefined); with the snapshot's local-partition read stubbed in: FAILED 1.05 s at "the genuine capability binds the channel, got `ConsumerNotEnrolled`" — the record was on another shard's partition | ok 1.10 / 0.97 / 0.95 s (three runs) |
| the same test after the revoke fan-out was a `run_on` message | FAILED at the `Status` after `Revoked` (the workload's next request overtook the mark) | ok, three runs — `Revoked` now waits for every mark |
| `verbs::tests::an_attestation_verifies_only_the_enrolled_unrevoked_consumer_of_the_channels_own_account` (pure) | new | ok |
| `landing::tests::the_enrollment_proofs_are_domain_separated_and_pinned` (pure, golden) | new | ok |

## Two defects found while finishing, both in this change's own first cuts, both fixed

1. **A partition handed to a shard-addressed call.** `attest_on_channel` passed `owner_of_consumer(id)`
   (a partition) to `call_within` (which addresses runtime shards). On the two-shard test daemon shard 2
   holds partition 1, so the read went to shard 1 (partition 0) and found nothing — traced:
   `TRACE-ENROLL shard=2 partition=1 … owner_of=1` / `TRACE-ATTEST origin=1 owner=1 … call_within=Some(false)`.
   Fixed with the tree's own map, `shard_of_partition`. The `Grant`/`Revoke` routing table already
   mapped through it (`serve`'s `owner != state.partition` branch), so `596bfb0` was not affected.
2. **An acknowledged revocation not yet in force.** The first fan-out was `run_on` per shard — a sent
   control message — followed at once by `Revoked`; the workload's next `Status` arrived before its shard
   processed the mark and was served. The design's "every later effect refuses" is later than the
   *acknowledgement*, so the acknowledgement now waits for every mark (`call_within` per shard, bounded).

## Measured (2026-09-13, shared 18-core box, two other agents building; `cargo test -p …`)

- `slates-server --test daemon`: 3/3, 14.08 s (the grant test and the exactly-once lifecycle test unchanged
  with `Attest`/`Revoke` served as `Forwarded`).
- `slates-server --lib`: 22/22. `slates-ipc` 13, `slates-db` 74, `slates-client` 5, `slates-anchor` 7,
  `slates-cli` 22 (in-process).
- Gates: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo xtask check`
  (structural 26 crates, literals, unsafe) all clean.
- Not run (the integrator's): the whole fleet suite; `SLATES_TEST_CLI=1` process flows.

## Siblings swept

- `Grant` takes effect on the partition it is recorded on (the landing awaits there) — no fan-out, no
  ordering hole. The only remaining `run_on` calls in `verbs.rs` are `origin → origin` (run inline).
- The `Capability` width is named once (`[u8; ISSUER_SECRET_BYTES]`, the BLAKE3 key width) — the two
  `32` literals the gate flagged are gone.

## Owed (honest list)

- **The CLI surface for consumers**: `slates enroll ACCOUNT`, `slates revoke CONSUMER`, `slates share
  VOLUME PRINCIPAL RIGHTS` (the daemon side and the wire are complete; `slates grant` exists). A separable
  piece with its own `SLATES_TEST_CLI=1` by-use tests — not built here because those tests are the
  integrator's lane and an untested CLI verb would be surface without evidence.
- **Roster admission of an enrolled identity** (the fleet's certificate allow-list): the consumer model is
  per-host (a workload under an account on one daemon); the design's "between hosts, authenticated
  transport also binds the delegated consumer scope" is the fleet leg — the transport's `Keyring`
  (`crates/transport/src/enrollment.rs`) populates from the manifest today; carrying a consumer scope
  across the mutual-TLS session is not built.
- **MCP servable roots "enrolled by a human through the CLI"** — the MCP crate does not yet consult the
  access list for its roots.
- **`revoked` on resume**: a replayed session's slot is rebuilt from the durable record's `revoked` at
  attestation (a revoked consumer cannot attest), which covers it; a channel *already bound* when the
  daemon restarts does not survive the restart, so no stale bind can outlive a revocation.

## Decision for Ada (outward-facing)

The capability is delivered "once, to the human" as bytes in the `Enrolled` reply; how it reaches the
workload is the harness's (§4.13: "the harness owns process isolation and capability delivery"). Options:
(a) an environment variable the harness sets for the workload process (simplest; visible to same-uid
`ps -E` on some platforms); (b) an inherited descriptor (the design's own example — "an inherited
endpoint from the trusted harness"); (c) the anchor segment slot. Recommendation: (b) — build a
`SLATES_CONSUMER_FD` inherited pipe in the SDKs' connect path, since it is the one channel other same-uid
agents provably cannot read. Not decided here.

## Paragraphs for the integrator (GAPS.md and SLATES_DESIGN.md are not edited on this branch)

**Security (4.13) row — append to the status cell:**

> **Consumer enrollment built (2026-09-13, `596bfb0` + `agent/enrollment`):** the grant issuer is a
> capability the daemon verifies (a per-start 256-bit issuer secret in the anchor segment, keyed BLAKE3
> proofs with domain separation, constant-time compare; `slates grant`), and consumers are enrolled under
> a host account by the same authority — `Enroll` mints a consumer id and a capability shown once;
> `Attest` binds a channel to it by a proof keyed over that channel's client id, read from the partition
> the id names and verified pure (`verify_attestation`); `Revoke` records durably and marks every shard's
> bound slots **before** acknowledging, so every later verb refuses `ConsumerRevoked` at a local gate;
> `Share` sets `Rights { read, write, admin }` per principal. Two consumers under one uid hold only the
> rights shared with them (`distinct_consumers_under_one_uid_hold_only_the_rights_shared_with_them_until_revoked`,
> a two-shard daemon, ok 1.0 s; forged proofs refused and counted). Owed: the CLI verbs
> `enroll`/`revoke`/`share`, the fleet leg (consumer scope over the authenticated transport), MCP roots
> consulting the access list, and the capability-delivery channel (a decision: inherited descriptor
> recommended). Record: `docs/wip/enrollment.md`.

**GAP-A9-9 row — replace the "Gap and source finding" cell:**

> Enrollment and the grant issuer are built to the A-8 specification for one host: consumers are trusted
> enrollments under an account with scoped rights, channels bind by a delivered capability (never by uid
> or channel class), rights are checked before any lookup or effect, and grant authority is a verified
> capability. Still open: the CLI verbs for enrollment, the cross-host consumer scope on the transport, MCP
> roots against the access list, and the harness delivery channel.

**§4.13 status blockquote — add after "*Refusals added.*":**

> **Status (2026-09-13).** Built for one host: the issuer secret and verified grants (`596bfb0`); consumer
> enrollment, attestation, revocation and sharing (`agent/enrollment`). An attestation is decided by a pure
> function over the consumer's durable record — read from the partition its id names, mapped to the
> runtime shard that holds it — and binds the channel's principal; a revocation is acknowledged only after
> every shard has marked its bound slots, so "every later effect refuses" holds by construction rather
> than by scheduling (the first cut's `run_on` fan-out let the very next request through — measured, then
> fixed). Every refusal is typed and counted. Owed: the `slates enroll`/`revoke`/`share` verbs, the
> delegated consumer scope over the fleet transport, MCP servable roots against the access list, and the
> harness-owned delivery of the capability (an inherited descriptor is the recommendation). Record:
> `docs/wip/enrollment.md`.
