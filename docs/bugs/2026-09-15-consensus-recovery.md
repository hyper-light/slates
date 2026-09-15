# Complete the consensus recovery lifecycle

Date: 2026-09-15. Contracts: §4.8, AUD-07, AC-8.1, T-2.14, R1, R8.
Status: implemented; macOS, Linux and KIND replacement verification passed.

## Reproduction and cause

At `f9c0fe9`, every daemon boot creates a fresh voting identity because no complete Raft
publication survives in the anchor. This prevents identity reuse after whole-pod RAM loss,
but also discards a warm restart's usable quorum. Two related missing operations are explicit
quorum-loss recovery and trust enrollment of nodes absent from the deployment roster.

The existing content restart oracle, changed to require the same member and omit its second
bootstrap, failed in 0.99 s: member `9203723457332679535` became `14495841230230618895`.

```sh
env RUSTUP_TOOLCHAIN=1.98.0 CARGO_BUILD_JOBS=2 cargo test --offline -p slates-server --test recovery acknowledged_content_and_its_snapshot_survive_a_daemon_restart_byte_for_byte -- --exact --nocapture
```

## Warm retention

The anchor now has two node-wide consensus publication slots. Each slot holds both groups'
complete term, vote, log, commit position, original application base, learned view and group
identity, together with the node identity and replay parameters. Its size derives from the two
configuration logs and the application snapshot budget. The layout version changes with these
new regions. Disk is not involved.

The control shard retains changed state before releasing a transition's result. A changed
term, vote, log, committed position or learned view triggers publication. Heartbeats with no
retained changes do not. Failure closes the control shard; it cannot send an unretained vote.
Startup reads the publication once before starting shards, preventing concurrent readers from
racing the control shard's first publication. Complete but corrupt records refuse recovery;
only an explicitly unfinished slot can be ignored. Whole-anchor loss still creates a fresh
identity and joins through the surviving quorum.

This realizes [Raft Figure 2](https://raft.github.io/raft.pdf): persistent state changes precede
responses. A lost quorum still requires a separate operator-authorized recovery. See also
[etcd disaster recovery](https://etcd.io/docs/v3.6/op-guide/recovery/) for the distinction between
restarting a retained member and creating a new logical cluster from recovered state.

### Measurements so far

macOS arm64, Rust 1.98.0, serial runs, 2026-09-15:

- The content oracle above passes in **1.21 s** with its member retained and no second bootstrap.
- Both groups grant candidate 1 in term 7, survive a real daemon restart over the same anchor,
  and refuse candidate 2 in term 7: **1 test passed, 0.88 s**.

```sh
env RUSTUP_TOOLCHAIN=1.98.0 CARGO_BUILD_JOBS=2 cargo test --offline -p slates-server --lib a_warm_restart_preserves -- --nocapture
```

## Explicit quorum-loss recovery

`recovery-plan root|region` returns a digest of the exact retained copy, its previous group,
committed position, last log position, application version and voters. `recover` requires that
unchanged digest, a human recovery proof, and the operator's `--fenced --accept-loss`
acknowledgements. The operator compares available copies and fences former members before
approval. The daemon cannot prove that an unreachable former voter is stopped.

Recovery preserves the selected committed application configuration and creates a new genesis.
A regional recovery advances fencing epochs. A survivor joins the selected genesis only after a
separate plan and approval with `--join-group`. While that join is pending, its previous copy
remains retained, old-group traffic and local proposals refuse, and warm restart resumes the
pending join. Import validates the expected genesis and minimum application version before
replacing the old copy. A repeated approval returns its original receipt.

This operation can lose state absent from the selected copy. No timeout or discovery answer
invokes it. Ordinary warm restart restores both groups; a whole-anchor replacement joins only
where a quorum survives independently for each group.

## Verification record

Commands use `env RUSTUP_TOOLCHAIN=1.98.0 CARGO_BUILD_JOBS=2` and `--offline`; all runs are serial.

| Command after `cargo test --offline` | Observed on 2026-09-15, macOS arm64 |
| --- | --- |
| `-p slates-server --lib retention::tests -- --nocapture` | 3 passed, 2.46 s: every payload crash cut, completed corruption, publication capacity |
| `-p slates-server --lib -- --test-threads=1` | 85 passed, 16.73 s, including both groups' retained votes and recovery approval checks |
| `-p slates-server --test fleet a_warm_fleet_restart_recovers_its_root_and_regional_quorums -- --exact --nocapture` | 1 passed, 14.86 s before discovery integration: all three warm-restart, then the root representative is lost and explicitly recovered |
| `-p slates-cli --test cli recovery_approval_is_bound_to_the_reviewed_plan_and_anchor_issuer -- --exact --nocapture` with `SLATES_TEST_CLI=1` | 1 passed, 1.88 s: real processes, absent issuer refused, approved reset and idempotent retry |

The adjacent records cover [enrollment](2026-09-15-unlisted-node-enrollment.md) and
[overlay recovery](2026-09-15-overlay-recovery.md). Final Linux/KIND results are recorded below
when those bounded runs finish. The old logs still grow to the derived publication capacity;
compaction and complete-message quotas remain separate work in GAP-A9-11.

### Linux arm64 verification, 2026-09-15

A cached `rust:1.98` image supplied the missing Linux compiler. The supervised container had
network disabled, 2 CPUs, 6 GiB RAM and a 4 GiB executable tmpfs target; source and the cached
Cargo registry were read-only. No compiler or package was installed. The cached image lacks
Clippy, so Linux uses a native compiler check and macOS runs workspace Clippy.

| Command after `cargo test --offline` | Result |
| --- | --- |
| `-p slates-cluster --lib -- --test-threads=1` | 143 passed, 0.02 s |
| `-p slates-server --lib -- --test-threads=1` | 85 passed, 21.57 s |
| `-p slates-server --test daemon --test recovery -- --test-threads=1` | 8 daemon tests, 33.87 s; 3 recovery histories, 40.88 s |
| `-p slates-server --test fleet a_warm_fleet_restart_recovers_its_root_and_regional_quorums -- --exact --nocapture` | 1 passed, 16.75 s |
| `-p slates-cli --test cli recovery_approval_uses_the_provisioned_node_key_across_processes -- --exact` with `SLATES_TEST_RAM=/tmp` (the container's tmpfs) | Red: no separate-process issuer, 0.57 s. Green: 1 passed, 0.40 s |

The portable CLI proof uses a separately provisioned, node-specific `SLATES_RECOVERY_KEY`.
It rejects missing, wrong and oversized credentials, then approves and idempotently retries the
exact plan. The key is read-only, hidden by Debug and never grants landing or consumer authority.
A configured key that cannot be loaded refuses without switching authority sources.

The combined test/compiler cache hit its 4 GiB tmpfs ceiling during the final SDK macro link.
The check was stopped by that failure; clearing the disposable cache and checking independently
keeps the work bounded. `cargo clean` removed contents but could not unlink the tmpfs mount itself.

After clearing that cache, `cargo check --offline --workspace --all-targets` **passed in 29.60 s**.
Host `cargo clippy --offline --workspace --all-targets -- -D warnings` **passed in 7.52 s**;
`cargo xtask check` passed all structural, literal, unsafe-budget and version checks. The
verifier-return seam carries the same documented foreign-rustls `Arc` exception as its builder;
no new ownership primitive was added.

KIND's new-IP replacement gate passed: takeover **10.2 s**, replacement rejoin **10.4 s**, two
probe peers on each of three nodes, and a new volume creation accepted without bootstrap. Fresh
five-node/three-node formation also passed; [commands, image identity and limits](../wip/kind-lane.md).
