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

---

# 2026-09-14: the harness delivery channel — the inherited descriptor

Branch `agent/consumer-capability` (`1e81737` the carrier, `2a9d2ac` the client, `762bc91` the CLI harness
verb; the SDK-test poll fix follows). Decided by Ada, 2026-09-14: of the three options above, **(b) the
inherited descriptor** — the one channel other same-uid processes provably cannot read (an environment
variable shows in `ps -E` and `/proc/<pid>/environ` to any same-uid reader; an anchor slot to any same-uid
client). Design words realized: §4.13 "a consumer channel is bound at rendezvous using a capability
delivered and retained outside other agents' reach, for example an inherited endpoint from the trusted
harness"; "the harness owns process isolation and capability delivery"; "unsupported secure enrollment
refuses, instead of issuing an ambient admin channel". Ledger: GAP-A9-9 "the harness delivery channel".

## What is built

**The carrier (`crates/ipc/src/delivery.rs`).** `Delivery::prepare(consumer, &capability)` writes one
fixed-layout record — magic `SLCD` (4), consumer id (8, little-endian), capability (32), CRC32C of the 44
bytes before it (4) — into a pipe both of whose ends are close-on-exec, and closes the write end.
`Delivery::spawn(program, args, environment, output)` runs the workload so that this child, and only it,
inherits the read end: on Unix the flag is cleared on a duplicate *in the forked child* between `fork` and
`exec` (`pre_exec`, one async-signal-safe `fcntl`), never in the parent, so a child another thread spawns
meanwhile inherits nothing; on Windows the standard library's `Command` cannot restrict inheritance (it
passes `bInheritHandles`; the handle-list attribute is unstable on 1.98, rust-lang/rust#114854), so the
spawn is `CreateProcessW` with a `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` naming the pipe handle and the
three standard handles — compiled and cross-linted here, run by the Windows CI lanes (`--test delivery`
added beside `--test rendezvous`). The child is told which descriptor by `SLATES_CONSUMER_FD` (Format: the
number on Unix, the handle value on Windows, decimal — a number leaks nothing). `delivered()` takes the
record once per process (a `OnceLock`, so every client of the process binds to the same consumer and no
second read ever happens): close-on-exec first, `fstat` kind check (a pipe or a socket end; a directory,
a device, a file are `WrongKind`), non-blocking reads (a short record is `WrongLength`, never a wait),
checksum before decode, close, zero. The closed taxonomy `DeliveryFault { Absent, NotANumber,
NotInherited, WrongKind, WrongLength { got }, Corrupt, AlreadyConsumed }` rides
`IpcError::CapabilityNotDelivered`. `attest_proof` moves here from the server (one definition; the server
re-exports it and its golden vector still pins it). `Output::Captured` with
`ConsumerChild::wait_with_output(capacity)` lets a harness read the workload's output under a bound it
chooses (`PayloadTooLarge` past it, after the child has ended). Apple has no `pipe2`: the two `fcntl`
calls follow the `pipe` at once and a harness must not spawn from another thread inside that window
(documented on the function); Linux and Windows have no window.

**The client (`crates/client/src/client.rs`).** `Client::connect`/`resume` ask for the delivery: absent
→ the account's own client; present and unusable → the typed refusal (never the account's ambient
authority); present → `Attest` (the proof keyed over this channel's client id) before the client is
handed back, so nothing runs on it as the account. The client keeps the capability: after a daemon
restart the reconnected channel is the account's until it attests, so `exchange`/`begin` bind it again
before a retried verb goes (`rebind_if_needed`; `round_trip` reports a reconnect instead of resending;
`rebinds()` counts). Typed verbs `enroll`, `revoke`, `share`, `attest`; `consumer()`. The SDKs' connect
path is this function, so a Python or Node workload spawned by a harness binds as the consumer with no
SDK change — proven by the SDK by-use suites still passing through the new connect (below).

**The harness verb (`crates/cli`).** The design names the CLI as the human's surface and the harness as
the owner of delivery, so the helper is `slates run [--keep] [--json] -- CMD [ARG ...]`: proves the
human's authority from the anchor segment (as `grant` does), enrolls a consumer, prepares the delivery,
announces the consumer first (`consumer: N` / `{"consumer":N}`), spawns the command with `SLATES_ENDPOINT`
naming the instance and only the delivery descriptor inherited, hands it the terminal, waits, revokes
unless `--keep`, exits as the command exited. Plus `enroll [--account UID]` (the capability shown once,
for a harness that delivers by its own means — Python `pass_fds`, Node `stdio`, a Windows `handle_list`,
with `Delivery::descriptor_name`), `revoke CONSUMER`, `share ID PRINCIPAL [--read] [--write] [--admin]`
(`uid:N`, `consumer:N`, `consumer:ACCOUNT/N`). `slates anchor` prints `issuer surface: export
SLATES_ANCHOR=… SLATES_ANCHOR_LEN=…` on macOS and Windows (the segment is the user's named object; the
object's mode is the authentication) so the issuer verbs can run from a shell; on Linux the handoff is a
descriptor only the anchor's children hold and nothing is printed — the human-surface gap of GAP-A9-10
stands there.

## Failing tests first (2026-09-14, this box at the memory wall: swap 16 039 MB of 17 408 MB used, 4 848 free pages, load 9.2)

| Test | Before | After |
|---|---|---|
| `crates/ipc/tests/delivery.rs::a_consumer_child_takes_the_capability_from_the_one_inherited_descriptor_and_nothing_else` (the test binary re-invoked through `Delivery::spawn`; the parent's decoy descriptor — close-on-exec on Unix, inheritable on Windows — must be closed in the child, and the delivery closed after the take) | new | ok, with `a_sibling_without_a_delivery_is_absent_and_a_stale_number_is_not_inherited` (`Absent`; a non-inherited number `NotInherited`): 2/2 in 0.01 s |
| `delivery::tests` hostile inputs: every-bit flip and a foreign magic `Corrupt`; whole/short-closed/short-open/oversize/altered/empty/wrong-kind (directory, `/dev/null`, a file, a tty where one exists)/non-number/closed-number | new | 9/9 |
| `crates/client/tests/consumer.rs::a_spawned_consumer_binds_through_the_inherited_capability_and_a_sibling_without_it_is_the_account` | with the bind at connect skipped (mutation): FAILED 1.36 s, the child exits 10 "bound to None" | ok 2.6 s: the child binds as the enrolled consumer, holds the capability raw or hex in no argument or environment value, makes a volume the account is refused `Forbidden` on; the sibling reports `consumer None`, `status forbidden`, `attest not_enrolled` |
| `…::a_client_holding_a_consumer_identity_binds_again_by_itself_after_a_daemon_restart` | with the re-bind after reconnect skipped (mutation): FAILED 1.89 s, `Refused(Forbidden { verb: "status" })` | ok: one reconnect, one rebind, served as the consumer; the account stays refused |
| `crates/cli` `verbs::harness_tests::run_spawns_the_workload_as_an_ephemeral_consumer_and_revokes_it_after` (an in-process daemon; the test binary re-invoked as the workload through `run`'s core) | with the revoke-after disabled (mutation): FAILED 0.96 s, "the ephemeral enrollment is revoked once the workload ends" | ok 1.0 s: the workload ran as the announced consumer, the account is refused on its volume, a later attest with the workload's own capability is `ConsumerRevoked`; a kept enrollment binds |
| `crates/sdk-python/tests/test_sdk.py::test_lifecycle_round_trip_over_a_live_daemon` | FAILED twice (0.9 s): "the destroyed volume is gone from list" — the test listed right after `destroy`, before the daemon's teardown slices (`docs/bugs/2026-09-14-sdk-tests-list-before-destroy-slices-finish.md`; measured gone 54–302 µs after the reply, once 5.06 s) | ok with a bounded poll, the Rust client test's discipline; the same poll in the async Python and both Node suites (siblings) |

The first cuts found two things in themselves: a re-invoked role must be named by its full libtest
path (`verbs::harness_tests::…`), else the child runs nothing and exits 0; and volume names are unique
per daemon across principals (a second run's `create mine` was `AlreadyExists` carrying the first
consumer's volume id).

## Measured

- `cargo test -p slates-ipc`: unit 17/17 (9 new), `--test delivery` 2/2, rendezvous 3/3, rings 6/6.
- `cargo test -p slates-client -- --test-threads=1`: consumer 2/2 (2.6 s), client 3/3, reap 1/1,
  async_core 1/1.
- `cargo test -p slates-cli --bin slates`: 21/21 (+1 ignored role), 0.96 s; `--test cli`: the gated
  flows skip loudly, `the_profile_and_the_usage_print` ok. `slates_run_spawns_the_command_as_an_ephemeral_consumer`
  is written under `SLATES_TEST_CLI=1` and was **not run here** (the charter's validation limits):
  `SLATES_TEST_CLI=1 cargo test -p slates-cli --test cli -- --exact slates_run_spawns_the_command_as_an_ephemeral_consumer --nocapture`.
- `cargo test -p slates-server --lib` 36/36; `--test daemon -- --exact distinct_consumers_under_one_uid_hold_only_the_rights_shared_with_them_until_revoked` ok 1.26 s.
- SDKs (the recipe in the memory note; a venv and the addon copy under `~/.cache/…-scratch`, outside the
  tree): Node `sdk.test.mjs` 3/3 and `sdk_async.test.mjs` 2/2; Python 5/5 after the poll fix.
- Gates: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo xtask check`
  (structural 27, literals, unsafe: ipc 8 → 35, every site named in `unsafe-budget.toml`); cross-lints
  `cargo clippy -p slates-ipc --all-targets --target x86_64-pc-windows-msvc --no-default-features --features slates-machine/pure-hash`
  and `--target x86_64-unknown-linux-gnu`, and the client lib on both targets, all clean (the client's test
  targets cannot cross-build: the server dev-dependency pulls zstd-sys/ring C code).

## Siblings swept and observations reported (not changed here)

- Volume names are unique per daemon across principals: a consumer's `create` of a name the account
  already used is refused `AlreadyExists { existing }` with the account's volume id — cross-scope
  existence revealed (§4.13 "cross-scope existence and timing must not reveal private data"). Decide
  whether names are per principal or the refusal carries no id.
- A destroy's teardown once took 5.06 s to leave the catalog on a two-shard daemon (5 of 6 rounds:
  54–302 µs); suggests the teardown task waited for a parked shard's wake. Recorded in the bug note.
- A consumer's own children inherit `SLATES_CONSUMER_FD` with the descriptor closed: they are refused
  `NotInherited` rather than becoming the account's clients — by design (a process told it is a consumer
  and finding no channel refuses); a harness that spawns from inside a workload scrubs the variable.
- On Linux the issuer verbs (`grant`, `enroll`, `revoke`, `run`) still need the anchor's descriptor, which
  only its children hold (GAP-A9-10's human-surface gap); the gated CLI flow skips there.

## Owed after this change

- The SDKs' own spawn helpers (`Delivery` bound into Python and Node so a harness in those languages
  prepares and spawns without the CLI); today they use `slates run`, or `slates enroll` plus their own
  `pass_fds`/`stdio`/`handle_list` spawn with `Delivery::descriptor_name`.
- The fleet leg (the consumer scope over the authenticated transport) and MCP servable roots against the
  access list, unchanged from the 2026-09-13 list.

## Paragraphs for the integrator (GAPS.md and SLATES_DESIGN.md are not edited on this branch)

**GAP-A9-9 row — replace the "Gap and source finding" cell:**

> Enrollment and the grant issuer are built to the A-8 specification for one host: consumers are trusted
> enrollments under an account with scoped rights, channels bind by a delivered capability (never by uid
> or channel class), rights are checked before any lookup or effect, and grant authority is a verified
> capability. The harness delivery channel is built (2026-09-14): the capability travels on an inherited
> descriptor — a close-on-exec pipe whose read end one child inherits (cleared in the forked child on
> Unix; a `CreateProcessW` handle list on Windows), named by `SLATES_CONSUMER_FD`, taken once and refused
> typed when absent, not inherited, the wrong kind, the wrong length, corrupt or consumed — the client
> binds at connect and again by itself after a daemon restart, and `slates run` is the harness verb (with
> `enroll`/`revoke`/`share`). Still open: the cross-host consumer scope on the transport, MCP roots against
> the access list, and the Linux human surface (the anchor's segment is a descriptor only its children
> hold).

**§4.13 status blockquote — add after the 2026-09-13 one:**

> **Status (2026-09-14).** The harness delivery channel is built, as the inherited descriptor
> (`agent/consumer-capability`: `1e81737`, `2a9d2ac`, `762bc91`): a harness writes the consumer id and
> the capability, under a magic and a CRC32C, into a pipe both of whose ends are close-on-exec and spawns
> the workload so that this child alone inherits the read end (the flag cleared in the forked child, never
> in the parent; on Windows a `CreateProcessW` handle list, since the standard library cannot restrict
> inheritance); the child is told which descriptor by a number in `SLATES_CONSUMER_FD`, takes the record
> exactly once (kind checked, read without blocking, checksum before decode, closed and zeroed), and
> `Client::connect` binds the channel with `Attest` before any verb — a present but unusable delivery
> refuses typed rather than falling to the account's authority; a client holding the capability binds
> again by itself after a daemon restart before its retried verb. `slates run -- CMD` is the harness verb
> (an enrollment for the command's lifetime, revoked after unless kept), with `enroll`, `revoke` and
> `share`. Proven across real processes: the workload holds the capability in no argument or environment
> value and owns what it creates; a sibling without the delivery is the account; the Windows arm runs in
> the CI lanes. Owed: the SDKs' own spawn helpers, the fleet leg, MCP roots, and the Linux issuer surface.
> Record: `docs/wip/enrollment.md` (2026-09-14 section).
