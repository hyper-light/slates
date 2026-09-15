# A fresh RAM anchor can cast a second vote as an existing Raft voter

Date: 2026-09-14. Finding: AUD-07. Contract: §4.8, AC-8.1, R1, R8.
Status: corrected, with regression evidence below. This is separate from the session-retirement rejoin fix.

## Reproduction and impact

Two sequential RAM-only daemon boots on macOS arm64, Rust 1.98.0, reproduced conflicting grants
under voter `10720325669370085734` in term 7. The first granted candidate 1; the replacement,
with a fresh anchor, granted candidate 2. The fixture uses the daemon's actual member identity
and the production regional council over the same three-voter configuration.

```sh
RUSTUP_TOOLCHAIN=1.98.0 CARGO_BUILD_JOBS=2 cargo test --offline -p slates-server --lib ram_losing_boot -- --test-threads=1
# FAILED, 1.61 s: both grants name the same voter and term.
```

In a three-voter group these grants can support two leaders in one term. Losing acknowledged
log entries also invalidates leader completeness. Restoring transport connectivity does not
restore the state needed to vote safely. The same construction initializes the root group.

## Root cause

The stable certificate anchor and a supervision counter determine the member id. Both the
counter and Raft state disappear with a pod's RAM. A replacement repeats generation zero,
recreates the same manifest seed id, and constructs a council and root group with an empty
term, vote and log. Consensus replies have no publication barrier preserving that state in
the supervising anchor.

The Raft core also granted votes as a learner: only the election-timeout entrypoint checked
membership. Its direct election entrypoint and vote/pre-vote replies did not. A regression
failed on that grant and now passes after adding the receiver's membership guard (47 Raft
and wire tests pass, 0.00 s). This is necessary for replacement admission, not closure of AUD-07.

Raft requires a server that loses persistent state to enter under a new identity through a
membership change. Loss of a majority requires operator recovery that acknowledges possible
data loss. This follows the [Raft dissertation, §3.8](https://raw.githubusercontent.com/ongardie/dissertation/master/stanford.pdf).

## Implemented lifecycle

1. Keep the certificate anchor for TLS authentication and completion-record origins. Every daemon
   start derives a fresh member identity from a random boot nonce. The supervision counter is not
   voting identity: it resets on whole-pod RAM loss. This applies to laptops and fleets alike.
2. Start the regional council and root group uninitialized and unable to vote or campaign. Manifest
   ids route discovery and carry declared regions/domains; they are never initial voters. SWIM learns
   actual members through authenticated contact. Nonces have no numeric age ordering.
3. Explicit local-account `slates bootstrap root` creates the first region and root group;
   `slates bootstrap region` creates another region after the root has admitted it. The CLI reads the
   current member id and binds the request to that boot. A retry against a replacement refuses
   `ConsensusBootstrapStale`; repeating it on the same boot cannot reset a vote or log. Consumer
   channels and fleet forwarding cannot bootstrap. Ordinary startup and workload requests never do.
4. A fresh member fetches the group's original application base and complete retained Raft prefix
   once. Validation covers voters, terms, indexes and snapshot shape. The recipient keeps its own
   fresh id and clears the donor's personal vote. After initialization, a fetch cannot replace its
   Raft state. Joint consensus admits it; the predecessor is not resurrected as a voter.
5. Every live Raft exchange carries the immutable genesis identity of its group, and the message's
   sender must match the authenticated member. A separately bootstrapped group's traffic cannot
   change this group's term or log. Regional groups accept only their declared region's members.
6. Writes refuse `ConsensusNotInitialized` until the member has initialized groups and joined its
   regional configuration. Bootstrap publishes that authority to every owner shard before replying.

### Exact edits

- `server/{daemon,state,deploy,fleet,consensus}.rs`: fresh identities, uninitialized startup,
  authenticated discovery, explicit bootstrap, one-time join, group-bound consensus messages.
- `cluster/{raft,config_group,root_group,raft_wire,swim}.rs`: learner guards, validated complete state,
  common-base replay, sender checks, boot nonces. Both groups also apply singleton commits locally;
  the first live test found that waiting for a remote reply left these commits unapplied forever.
- `ipc/protocol.rs`, `cli/{args,verbs}.rs`, `server/{verbs,nfs}.rs`: bootstrap request and typed
  refusals, CLI operation, readiness gate. Existing fixtures explicitly create their initial groups.
- `anchor/segment.rs`: preserve the content handoff when attaching from the environment. The actual
  CLI test exposed this AUD-05 sibling: it discarded content before passing the handoff to shards,
  so the first post-bootstrap create correctly refused `ContentUnavailable` (red: 1.75 s).

### Election tail found by the stricter fixture

The initial fleet fixture now waits for voting membership to commit before injecting a loss.
That exposed another gap: after receiving the final entry that makes it the sole voter, a new
leader could retain an uncommitted entry from the previous term forever. Reconfiguration waited
for the tail to commit, but the new leader appended no current-term entry to make that possible.
The deterministic `root_group::tests::a_replacement_leader_commits_the_previous_terms_membership_tail`
failed with `committed_voters = None` and now passes (0.00 s). Both wrappers append and apply the
standard election no-op. Both live drivers also keep a leader active while it commits its own
removal, even when the uncommitted final configuration no longer lists it as a voter.

The live `three_daemons_form_a_fleet_and_the_survivors_retire_a_dead_node` history previously
stalled; the stronger formation gate failed twice in about 32 s and named the root's uncommitted
tail. After the no-op fix, it passed in 10.23 s. The existing gated CLI lifecycle test
`SLATES_TEST_CLI=1 cargo test --offline -p slates-cli --test cli the_anchor_supervises -- --test-threads=1`
passed in 1.80 s after the content-handoff correction.

### Limits and rejected approach

The lifecycle is deployment-independent: local processes, bare-metal clusters, VMs and Kubernetes
use the same address resolution, authenticated announcement and Raft admission. Configured DNS peers
are resolved on each new dial and join automatically after the explicit initial bootstrap. Discovery
does not grant voting authority. The current pinned roster cannot discover and enroll unlisted nodes;
a bounded discovery-provider interface and trust-enrollment protocol remain a separate requirement.

Raft state is **not retained in the anchor yet**. Even an anchor-preserving daemon restart joins
with a fresh identity. The content-recovery tests explicitly create a new standalone group after
restart; they prove retained volume bytes and completions, not automatic consensus recovery.

Each group needs its own surviving voter quorum. Losing that quorum never triggers automatic
bootstrap. A laptop loses its only voter on restart. The present root placement has one voter per
region, so a single-region fleet can lose its root quorum by losing that representative even when
its regional council retains a quorum. Safe recovery of those cases requires operator intervention;
retaining complete consensus state across warm restarts remains an availability improvement.
An initialized group refuses in-place bootstrap. Creating an unrelated new group does not recover
old committed state or fence an old group that may still be running.

The live wrappers do not compact their logs. Join transfers the retained prefix and refuses
nonzero snapshots without a matching compacted application base. Core snapshot recovery is tested;
live compacted-state transfer is not implemented. **Resource-bound sibling (GAP-A9-11):** the
transport bounds stream windows, but `Endpoint::request` and `serve_once_async` accumulate complete
messages. Complete-message quotas and bounded log retention remain owed; this fix makes no bounded
large-history transfer claim. Framing limits alone do not bound the allocation of a full join prefix.

An anchor-secret bootstrap proof was implemented during development and rejected: an independently
invoked Linux CLI cannot inherit the anchor's memfd. The shipped operation uses the existing local
account authentication and an explicit boot identity, with no second bootstrap path.

## Executed evidence

The final review extended both admission regressions with a newer learner fetch before Raft catches
up. They failed in 0.00 s: regional takeover epoch 3 instead of 2, and root version 5 instead of 3.
Both groups now keep one fetched read view separate from the deterministic Raft fold and discard it
when the log catches up. This prevents a fresh member from applying an already-fetched change twice.
All 143 cluster unit tests pass after that correction (0.01 s).

Bootstrap also exposed a durability boundary: a single admitted host reported zero loss by
counting its unadmitted peers as copies; the f=0 loss calculation also returned zero for one copy.
The new DB regression failed in 0.00 s. The calculation now limits copies to admitted hosts and
evaluates one-copy loss normally; bootstrap installs the policy result on the control shard too.
All 35 DB tests pass in 2.15 s. The daemon fixture now explicitly accepts risk in its permissive
case instead of pretending its unreachable manifest peers are admitted holders.

The broader daemon suite exposed two existing overlay-contract failures after the earlier AUD-05
publication correction: `the_daemon_serves_the_lifecycle_verbs_exactly_once_with_leases_and_typed_refusals`
and `the_merge_service_enforces_roles_pins_versions_and_seals_behind_the_barrier` expect overlay
creation to succeed without recoverable base state. It now refuses `ContentUnavailable`. Those
expectations remain unchanged; this task does not implement the missing base-recovery protocol.

Commands below ran serially on 2026-09-14, macOS arm64, Rust 1.98.0, with
`RUSTUP_TOOLCHAIN=1.98.0 CARGO_BUILD_JOBS=2` and no new dependencies downloaded.

| Command (following `cargo`) | Result |
|---|---|
| `test --offline -p slates-cluster --lib -- --test-threads=1` | 143 passed, 0.01 s. Includes learner refusal, regional/root prefix admission with retained votes, damaged state, snapshot restoration, and authenticated sender checks. |
| `test --offline -p slates-server --lib -- --test-threads=1` | 77 passed, 14.61 s. Includes the original two-vote RAM-loss regression and rejection of foreign-group messages and join state. |
| `test --offline -p slates-server --test daemon bootstrap_is_explicit -- --test-threads=1` | 1 passed, 0.94 s. A replacement rejects the previous member's bootstrap request and continues refusing writes. |
| `test --offline -p slates-server --test fleet a_whole_ram_replacement_joins_as_a_fresh_voter_and_commits_after_another_loss -- --test-threads=1 --nocapture` | 1 passed, 9.81 s. Three voters form, one whole anchor is lost, the replacement joins with a new id, all voters commit admission, another node is lost, and the surviving pair commits another membership change. |
| `test --offline -p slates-server --test daemon --test recovery -- --test-threads=1 --quiet --skip the_daemon_serves_the_lifecycle --skip the_merge_service_enforces_roles` | 6 daemon tests passed, 7.28 s; 3 recovery tests passed, 8.27 s. The two excluded overlay failures are recorded above. |
| `test --offline -p slates-cluster --test raft --test raft_live --test config_group_live --test root_group_live --test swim -- --test-threads=1` | 15 passed across five integration suites, 0.02 s total test time. |
| `test --offline -p slates-cli --bin slates --test cli -- --test-threads=1 --quiet bootstrap_names the_anchor_supervises` with `SLATES_TEST_CLI=1` | CLI grammar passed, 0.00 s; actual anchor/daemon lifecycle passed, 1.82 s. |
| `test --offline -p slates-client --test client a_session_outlives -- --test-threads=1 --quiet` | Client reconnect and completion replay passed, 1.49 s, with explicit standalone bootstrap. |
| `test --offline -p slates-server --test fleet -- --test-threads=1 a_whole_ram a_root_learner a_learner_fetches a_client_reads_a_cross a_committed_retirement a_takeover_successor a_fresh_restart` | Final transport sweep: 8 passed, 69.80 s. Includes whole-RAM replacement plus another loss, the separate session-retirement fix, both learner fetch paths, cross-region reads, shard publication and both NFS takeover paths. |
| `clippy --offline --workspace --all-targets -- -D warnings` | Passed, 12.00 s. |
| `run --offline -p xtask -- check` with `CARGO_NET_OFFLINE=true` | Structural wall (29 shipped crates), literals, unsafe budgets and version passed. |

Linux cross-Clippy (`-p slates-server -p slates-cli --all-targets --target x86_64-unknown-linux-gnu`)
could not compile `zstd-sys`: this host lacks `x86_64-linux-gnu-gcc`. No compiler or other tool was
installed. No SDK language tests, live mounts, bare-metal/VM deployments or KIND run were executed.

The live replacement history preserves the root representative while replacing a regional voter;
it does not claim root-quorum recovery. Root admission and same-term vote retention are separately
covered through real `RootGroup` messages. A new-IP KIND run was not performed in this task.
