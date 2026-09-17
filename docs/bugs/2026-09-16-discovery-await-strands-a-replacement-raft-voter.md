# An unbounded discovery request strands the record link to a replacement voter

Date: 2026-09-16 (America/Chicago).
Status: implemented 2026-09-17 (see "Implemented" and "Validation performed" at the end).
Contracts: §4.8 (fresh-member admission and membership), §4.10a (transport ownership),
R8 (one deployment path), bounded work and cancellation safety.

## Finding

The whole-RAM test's old voter view is the consequence of a stuck discovery request.
It is not evidence that Raft needs a longer convergence timeout.

`exchange_discovery` directly awaits `Endpoint::request`. That request deliberately has
no overall deadline: its caller must stop it. A survivor that is refreshing discovery when
the old process disappears can therefore hold the old record endpoint indefinitely. The
same record-link task is responsible for noticing the replacement identity and re-dialing,
but it cannot return to that part of its loop while the discovery request is pending.

The replacement's outbound sessions work. It imports an initial Raft prefix and fetches a
newer application membership. The survivors commit its admission using their surviving
majority, but cannot establish the outbound record sessions needed to deliver its Raft
entries. Application membership and the replacement's own voting state then disagree.

## Reproduction

Exact test:
`a_whole_ram_replacement_joins_as_a_fresh_voter_and_commits_after_another_loss`.

The investigation reused the isolated archive of
`04d2ea0750207975d3b682aa7b91046dbd1a7511` and its dedicated target directory from the
re-dial investigation. The whole-RAM fixture and runtime were unchanged for the first
three runs. The only existing diagnostic was a capacity print in another test's fixture.
No scheduler-quantum changes were included. The relevant fleet, consensus, Raft, transport
and whole-RAM fixture sources are identical between `04d2ea0` and `b2f1ef7`:

```sh
git diff 04d2ea0 b2f1ef7 -- crates/server/src/fleet.rs \
  crates/server/src/consensus.rs crates/cluster/src/raft.rs \
  crates/transport/src/endpoint.rs crates/server/tests/fleet.rs
# Empty.
```

Measured on local macOS arm64, Rust 1.98.0, 2026-09-16. Runs were serial, each limited
to 60 seconds by a supervisor. Before a build or trial batch, `ps -axo pid=,comm=` was
checked for active fleet-test, cargo and rustc processes. No stress processes were started.

| Instrumentation | Results in execution order, libtest seconds |
|---|---|
| Whole-RAM path unchanged | PASS 9.83; PASS 9.81; **FAIL 36.79** |
| Per-message logging, isolated copy only | PASS 12.80; PASS 12.67; PASS 13.30 |
| Bounded per-peer counters, one dump after the voter gate | PASS 13.92; PASS 10.02; PASS 9.93; **FAIL 36.97** |

Both failures reached the exact reported assertion: all observations succeeded, both
survivors reported the new voter set, and the replacement reported the old one. This is
a reproduction record, not an estimate comparing quantum and pristine failure rates.
The passing verbose runs do not establish a timing cause; instrumentation can change
scheduling. Counters were used to capture a failing run without per-message output.

The binary command was:

```sh
"$audit_root/target/debug/deps/fleet-60877685d42923fa" \
  a_whole_ram_replacement_joins_as_a_fresh_voter_and_commits_after_another_loss \
  --exact --nocapture
```

`audit_root` was
`/private/var/folders/1s/ldpdh04d7d7219qts5t19d7h0000gn/T/slates-redial-clean-47696`.
Diagnostic builds used `RUSTUP_TOOLCHAIN=1.98.0 CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0`,
`CARGO_TARGET_DIR="$audit_root/target"`, and
`cargo test --offline --locked -p slates-server --test fleet --no-run` from the archived
source. The counter build took 4.36 seconds. Its bounded runner is retained there as
`run-ram-diagnostic.py`; diagnostics exist only in that scratch source tree.

## The failing counter trace

Evidence: `whole-ram-counters-84166-4.log` in the scratch directory above. The earlier
failure without these counters is `whole-ram-78299-2.log`.

Aliases for the actual member identities in the counter trace:

- Leader: `2516500252544949559`.
- Other survivor: `323990456057680808`.
- Dead member: `7418323329570476274`.
- Replacement: `16187986231402990776`.

| Observation at the failed voter gate | Value |
|---|---|
| Leader's term / commit index / log length | 2 / 10 / 10 |
| Replacement's term / commit index / log length | 1 / 5 / 5 |
| Application membership version, all three nodes | 4; includes the replacement and excludes the dead member |
| Leader's replication attempts targeting the replacement | **271** |
| Those attempts with no available replacement session | **271** |
| Raft appends sent to the replacement | **0** |
| Raft appends received by the replacement | **0** |
| Leader's appends sent to, and replies folded from, the other survivor | **271 / 271** |
| Each survivor's discovery requests to the old member, started / finished | **21 / 20** |
| Replacement's discovery requests to each survivor, started / finished | **311 / 311** |

Each survivor's record-session table contained an entry for the dead member with its
endpoint borrowed, and no entry for the replacement. The replacement had available
outbound sessions to both survivors. Its fresh-member import and subsequent read-view
fetches succeeded, while its Raft prefix remained at index 5.

The successful appends to the other survivor show that the leader and transport were
making progress. Zero appends to the replacement localize this failure before Raft log
matching or voter-state application. The unfinished discovery operation owns the missing
record endpoint and the task that must replace it.

## Exact cause in the code

1. `establish_record_link` checks `refresh_record_identity` at the top of its loop.
   That function removes the previous member's record entry when SWIM has learned a
   different member for the same certificate anchor.
2. Later in the loop, `refresh_discovery` takes the endpoint from that member's
   `record_sessions` entry, leaving `None` to mean it is borrowed.
3. `exchange_discovery` calls
   `session.request(crate::discovery::STREAM, &request).await` without a deadline.
4. `Endpoint::request` loops until a reply completes or a terminal transport error occurs.
   `receive_or_probe` explicitly documents that the caller owns the bound. Its PTO is
   a retransmission timer, not an operation timeout.
5. When the old process loses RAM, its connection keys disappear. A replacement on the
   same address cannot answer the old protected exchange. A UDP socket need not report a
   terminal error. The request remains pending, so step 1 never runs again for this link.

`git show f50e939 -- crates/server/src/fleet.rs` shows this direct await and the periodic
refresh added by the 2026-09-15 enrollment change. The initial enrollment exchange uses
the same helper and has the same missing bound. This source attribution is not a historical
binary pass/fail bracket.

## Fix design

### Bound each discovery exchange

Give the common `exchange_discovery` path a finite, absolute deadline derived from the
existing measured control-plane round budget. Use the same RTT-based budget policy as
the other record-plane requests; add no independent timeout or retry constant. Compute
the deadline once: partial packets, acknowledgements and retransmissions cannot renew it.

Use the custom runtime's existing request-versus-timer polling discipline, as in
`slates_cluster::request_within`. Preserve distinct typed outcomes for deadline expiry,
peer invalidation, transport failure and discovery-protocol refusal. The existing
`request_within` interface collapses timeout and transport failure into an empty reply,
so a literal substitution alone would lose the reason. Share the bounded exchange
mechanism while retaining those distinctions for discovery.

When the discovery deadline expires, drop the request future, abandon its exchange, and
release this endpoint. The owning record-link loop can then recheck membership and dial
the current discovered or freshly resolved address on its existing cadence. A successful
exchange keeps its session. No nested retry loop or extra detached task is needed.

### Make peer changes cancel outstanding link work

Associate link work with the stable anchor, expected member incarnation, and a local
generation identifying the particular session attempt. When authenticated contact changes
the member, or the peer is retired, invalidate the old attempt and wake its owner. The
exchange checks validity before accepting a ready reply and before applying a discovery
page. This handles a reply and a replacement notification that become ready together.

Keep at most one waiter for the existing per-peer link task, owned by the control shard
and bounded by `fleet_peer_capacity`. No atomics, locks, additional runtime or unbounded
watcher population is required. The timer remains a bound even when no membership-change
notification arrives, including a warm restart whose retained member identity stays the
same but whose transport keys are new.

When an incarnation changes, also drop a retained pending dial and reset its discovery
cursor. The next dial must use the new address and establish new keys; continuing an old
handshake against the previous IP is not replacement progress.

### Return an endpoint only to its own live slot

Carry the member and local attempt generation with a borrowed endpoint. A return or
cleanup must act only on the matching slot. A late old result must not insert the old
member back into the map, overwrite a newer endpoint, remove a newer attempt, or apply
an old discovery page after the incarnation changed.

This matters at both callers: `enroll_record_session` and `refresh_discovery` currently
use unconditional `record_sessions.insert` after the await. Preserve the intent of the
existing `return_sessions` guard, which returns only to an existing borrowed entry, and
extend the ownership check to distinguish attempts within one member incarnation.

### Keep Raft's admission rules

The replacement still imports its initial prefix once, then receives the missing entries
through ordinary AppendEntries and joint consensus. A fetched application view cannot
install voters or overwrite an initialized Raft log. No bootstrap retry, voter identity
reuse, reduced quorum, or longer test timeout is part of this fix.

The resulting connection lifecycle is the same for local processes, bare metal, VMs and
Kubernetes. Re-dial continues to use `client_for` and the existing discovery/DNS address
resolution; neither a Kubernetes API nor an address-specific recovery path is needed.

## Regression design

The essential regression must force the interrupted discovery phase instead of hoping
a restart lands inside it:

1. Form the three-voter fleet and wait for committed voting membership.
2. Use a test-controlled transport barrier to hold a discovery reply from the victim
   after its request has arrived. Confirm that each survivor's corresponding endpoint
   is borrowed by that exchange, while unrelated traffic still completes.
3. Stop the victim and replace its entire RAM anchor under the same certificate. Cover
   both the same address and a changed resolved address.
4. Require the old exchange to finish with a typed deadline/invalidation outcome, its
   slot to be released, and the survivor to establish a session for the fresh member.
   Assert nonzero Raft append delivery to that member and convergence of its committed
   voter set, then retain the test's second loss and subsequent membership commit.
5. Deliver a late old result after a newer session has been installed. It must not
   restore the old entry, change the new cursor, or remove the new session. Prove this
   with a request answered through the new session.

Additional bounded histories: initial enrollment with a withheld discovery reply;
warm restart with unchanged voting identity but new transport keys; an unchanged live
peer that answers within the derived deadline and keeps its session; repeated lifecycle
cancellations without growing the task or session population. Use the existing simulated
clock for deterministic deadline boundaries and the live fleet test for end-to-end use.

## Edit scope and sibling findings

- `crates/server/src/fleet.rs`: bound the shared discovery exchange; cancel stale link
  work; validate ownership before adopting a page or returning/removing a session.
- `crates/server/src/state.rs` and the discovery/link state as needed: bounded attempt
  generation and wake registration with owned cleanup.
- The shared timed-request seam, if factored: retain typed transport/deadline outcomes
  instead of silently converting both to an empty successful reply.
- `crates/server/tests/fleet.rs` and the transport/simulation seam: the controlled
  interruption and late-return histories above.
- The implementation change must update §4.8, `docs/wip/GAPS.md`, and the enrollment
  record with the actual cancellation/budget contract and executed evidence.

Sibling sweep: this is the only direct `Endpoint::request` await in server source
(`rg -n '\.request\(' crates/server/src`); both enrollment and refresh reach it.
The unguarded inserts and retained pending dial are adjacent lifecycle gaps, not separate
explanations for the measured zero appends. `return_sessions` already guards presence,
but presence alone is weaker than an attempt-generation check.

No production fix, timeout increase, statistical claim about quantum, full-suite result,
Linux result, or KIND result is claimed by this investigation. The shared source tree was
left to the concurrent re-dial implementation; only this report was added here.

## Implemented (2026-09-17)

- **Each discovery exchange is bounded** (`crates/server/src/fleet.rs`, `exchange_discovery`): the
  request is raced against one deadline armed once at the measured control-plane round budget's full span
  — `consensus_budget(slowest_path_tail_ns()).max_deadline_ns()`, the bound every other record-plane
  exchange runs under; no new constant — and against the link's validity, re-checked whenever the exchange
  is woken (a reply, the timer, or a peer change): the anchor's learned member still the one addressed
  (`link_valid`) and the peer still in direct contact. Validity is checked before a ready reply is
  accepted, so a page is never applied for a member the link no longer addresses. Partial packets,
  acknowledgements and retransmissions do not renew the deadline. Outcomes are typed
  (`DiscoveryOutcome`: answered, deadline, invalidated, transport, refused) and counted
  (`fleet.discovery.deadline` / `.invalidated` / `.transport`; the protocol's own refusals as before —
  the old `Err(_)` arm had miscounted a transport fault as `fleet.accept`). Any outcome but an answer
  abandons the exchange (`abandon_exchange`) and releases the endpoint; the link re-dials on its cadence.
- **A peer change cancels the outstanding exchange**: one waiter per anchor (`ShardState::link_waiters`,
  bounded by the roster; registered while the exchange waits, removed when it ends) is woken by
  `learn_member`'s restart arm, by `fold_peer_state` on a retirement, and by the death injections, so the
  exchange ends invalidated at once rather than at its deadline; the timer bounds it when no notification
  comes (a warm restart with new keys and the same id). An incarnation change also drops a dial still in
  its handshake (counted `fleet.dial.stale_dropped`, as at a retirement) and restarts the discovery sweep
  (`refresh_record_identity`).
- **An endpoint returns only to its own live slot** (`ShardState::RecordLink`): a coordinator borrow tags
  the slot with the borrowed session's connection id (unique per establishment, so no second counter);
  `return_sessions` refills only an entry that is still there, still out, and expecting that id — anything
  else is dropped and counted (`fleet.link.stale_return`), never installed over a newer session; and
  `refresh_discovery` puts a session back only into the entry it left, so a late page never re-creates an
  entry removed by a replacement or retirement.
- Raft's admission is unchanged; no timeout was raised.
- Docs: §4.8 status (2026-09-17), GAPS §4.8 (the ledgered flake closed), the 2026-09-15 enrollment record.

## Validation performed (2026-09-17)

The whole-RAM history now **forces** the interrupted phase: `Daemon::inject_discovery_fault(true)` on the
victim holds every discovery reply after its request arrived (the serve side's async handler parks), the
test proves each survivor's record link to the victim is **borrowed** by that pending exchange
(`Daemon::fleet_record_links`), then stops and replaces the victim. Before the voter set is required to
converge it asserts, on each survivor, a typed end to that exchange (`fleet.discovery.deadline` +
`fleet.discovery.invalidated` ≥ 1), a record link to the fresh member, and the replacement's council
contact climbing (the leader's appends arriving); the second loss and its commit follow as before.

Measured on this box (18 cores, 2026-09-17, load 3–8): the history alone **5/5** (9.93, 12.58, 12.80,
13.03, 13.12 s) where pristine `b2f1ef7` failed about one run in three at ~36 s; the whole in-process
fleet suite **43/43 in 267.45 s** (the history a sixth time); the server lib tests 88/88; `cargo xtask
check` ok. After the final refactor (the test's assertions moved into three helpers to stay under the
cognitive-complexity gate) the full gate chain ran once more at 09:39–09:40, load 5–9: `cargo fmt --check`
clean; `cargo clippy -p slates-server --all-targets -- -D warnings` and the workspace clippy clean; the
rebuilt history binary (its helper symbols present) **2/2** (14.04 s, 13.96 s; wall 15 s and 14 s, so the
process exited with the verdict both times); server lib tests 88/88; `cargo xtask check` ok. One
unexplained observation, kept: the first run of the earlier binary exited about 260 s after libtest's
verdict (13.12 s), a delay none of the six later runs showed and the suite's wall time (268 s for
267.45 s) does not contain. Linux and KIND run on the push.
