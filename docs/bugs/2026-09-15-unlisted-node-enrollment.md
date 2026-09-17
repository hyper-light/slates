# Unlisted nodes could not enter discovery

Date: 2026-09-15. Contracts: §4.8, §4.13, R8, AC-8.18, T-8.20.

## Reproduction and cause

Three nodes form a seed chain: the first knows itself, the second knows the first, and the
third knows the second. All hold certificates under one operator authority. The last node
appears in neither running node's manifest. The existing transport did not listen with an
empty seed list, admitted only static leaf pins, and never exchanged addresses. The bounded
live regression failed after **32.29 s** with no three-voter group; its log also showed that
trusting an issued certificate as though it were self-signed fails signature verification.

```sh
env RUSTUP_TOOLCHAIN=1.98.0 CARGO_BUILD_JOBS=2 cargo test --offline -p slates-server --test fleet an_unlisted_node_enrolls_through_one_seed_and_joins_the_existing_quorum -- --exact --nocapture
```

## Change

The manifest may supply `enrollment_roots`, public DER certificates of operator authorities.
Each issued node certificate carries the fleet TLS name and the signed DNS name
`r<region>.d<domain>.<fleet name>`. An explicit failure domain is required when issuing an
enrollable node; it must describe actual co-failure risk. The issuer's private key never
enters the daemon. Listed seeds retain their explicit certificate and topology declarations.

Discovery is a record-stream exchange, common to local processes, bare-metal hosts, VMs and
Kubernetes. It announces the local certificate and addresses, then pages one candidate per
exchange. Completed scans exchange generation checks. An issuer, expiry or signed-scope
failure refuses before dialing. The later TLS session checks the exact leaf and proves key
possession before SWIM can observe the member. Raft subsequently admits voters through the
existing join path. DNS and discovery never bootstrap consensus.

The address table and peer tasks share a bound derived from the runtime task budget, with
space for existing seeds. The table counts seeds even before they announce. Frame-derived
message caps are checked before decoding; refusals are counted by closed category. Direct
TLS contact can update a peer's address; relayed stale addresses cannot overwrite it.
Fresh dials consult the updated address and resolve DNS again. Accepted records survive warm
restart in the consensus publication and are revalidated against current enrollment policy.

This needs at least one reachable trusted seed to meet a fleet for the first time. No network
scan, multicast assumption, Kubernetes API dependency or manifest-wide synchronized edit is
required. It does not discover an entirely disconnected fleet without an address or trust input.

## Evidence and tests

The live regression passed in **3.27 s** after wiring enrollment. Both negative enrollment
tests passed in the **85-test server suite, 16.73 s**: forged region, domain and identity;
foreign issuer; malformed address; capacity; direct address updates and stale relays.
A transport regression checks that another valid leaf under the same authority cannot
impersonate the pinned server. Final platform results are in the consensus recovery record.

The certificate binding follows [RFC 5280 §4.2.1.6](https://www.rfc-editor.org/rfc/rfc5280#section-4.2.1.6).
Issuer/expiry checks use the same rustls client verifier as TLS. Scope checks use
[rustls `verify_server_name`](https://docs.rs/rustls/0.23.44/rustls/client/fn.verify_server_name.html)
in addition to certificate verification; name matching alone is not authentication.

## Linux verification, 2026-09-15

In the bounded, network-disabled `rust:1.98` container described in the consensus recovery record:

- `cargo test --offline -p slates-server --test fleet an_unlisted_node_enrolls_through_one_seed_and_joins_the_existing_quorum -- --exact`: **1 passed, 2.37 s**.
- `cargo test --offline -p slates-transport --test session an_issued_certificate_cannot_impersonate_another_leaf_under_the_same_authority -- --test-threads=1`: **1 passed, <0.01 s**.
- Full server library suite: **85 passed, 21.57 s**, including malformed enrollment, issuer/scope
  rejection, bounded admission and direct-versus-relayed address updates.

Warm revalidation does not publish partial rosters. A crash or refusal during revalidation leaves
all retained peers available to the next start, including those not examined before the failure.

Final macOS regression: `cargo test --offline -p slates-server --lib
discovery::tests::refused_revalidation_keeps_the_complete_roster_for_the_next_restart --
--exact --nocapture` **passed, 0.79 s**. It forces a warm restore to refuse at a one-peer
capacity, reloads the retained publication, and verifies both peers are discoverable when the
next start has capacity. The first short exact filter selected zero tests; only this fully
qualified execution counts as evidence.

## Correction (2026-09-17): the discovery exchange was unbounded

The enrollment and refresh exchanges this record added (`exchange_discovery`, over the record
session) awaited `Endpoint::request` with no deadline; `Endpoint::request` deliberately has none —
its caller owns the bound. A survivor refreshing discovery when the peer's process disappeared
therefore held that peer's record endpoint, and the link task that alone notices a replacement,
for good — a datagram socket reports no terminal error for a peer whose keys are gone — which is
how a replacement voter received no append (271 attempts, 0 sent) while its admission committed
around it. The exchange is now bounded by one deadline armed once at the measured control-plane
round budget's full span and by the link's validity (re-checked whenever it is woken; a peer change
wakes it), every outcome typed and counted, and a borrowed record session returns only to the slot
it left: `docs/bugs/2026-09-16-discovery-await-strands-a-replacement-raft-voter.md`, §4.8 status
2026-09-17.
