# System contract audit, 2026-09-05

Status: source-review findings at Slates `a1059ed`; BUG-12 subsequently fixed in `d9cb6e5`; no implementation was changed by this audit and
no executable regression was run. These findings are not claims about production
incidents. The evidence commands were `rg` and `sed` over the named source. The design
response is A-9; authoritative tracking is `../wip/GAPS.md` §8i.

Workspace update, 2026-09-05: separate archive work committed as `540fb5b` (also including
the initial audit/review files), followed by the ledger fix `d9cb6e5`. The latter fixes BUG-12's
acceptance-epoch refresh and removes BUG-13's forced candidate-zero reachability. Its commit
record reports the regression failing before the fix, passing afterward, and 40 passing DB
tests (`git show -s --format=%B d9cb6e5`). This docs pass inspected the change but did not rerun
those tests. BUG-12 is fixed by that separate commit; BUG-13's direct adopted-value assertion
and the broader protocol evidence remain open. Findings below preserve the `a1059ed` baseline.

## 1. Findings and exact implementation sites

| Id | Trigger and observable consequence | Source and required correction |
|---|---|---|
| BUG-1 | Create with `--locked`: the server records the flag without establishing locked backing. | `crates/server/src/daemon.rs` region construction; `crates/server/src/verbs.rs` `create`; `crates/mem/src/region.rs` `map` initializes `locked: false`. Reserve from prefaulted locked usable capacity and refuse unsatisfied guarantees. |
| BUG-2 | A region contains a non-power-of-two page count: reservation accounting can promise more than the buddy allocator serves. | `crates/mem/src/arena.rs` `add_region` rounds usable granules down; `crates/server/src/daemon.rs` budgets the unrounded configured reserve. Derive admission from actual usable bytes. Example: six mapped pages yield four allocatable pages. |
| BUG-3 | Dynamic growth competes with an outstanding bounded claim. | `crates/server/src/verbs.rs` `HostPressure` checks host availability without debiting the same claim budget; all volumes use the shard store. Prevent unreserved consumers from using promised capacity. |
| BUG-4 | Repeated open/close grows memory even with one live handle. | `crates/bridge-fuse/src/volume_bridge.rs` `open_handle` appends; `release` clears without reuse. Use a bounded generational handle arena and a typed full refusal. |
| BUG-5 | Open a known base filename before listing its parent: lookup can miss an existing file. | `VolumeBridge::lookup` bypasses `Volume::with_host(...).lookup_no`. Route lookup through the base-aware path without requiring READDIR first. |
| BUG-6 | FUSE INIT advertises a different flag from the documented writeback capability. | `crates/bridge-fuse/src/abi.rs` `WRITEBACK_CACHE = 1 << 8`; Linux ABI uses `1 << 16`. Correct the wire value and test against independent kernel vectors. |
| BUG-7 | Kernel sends advertised READDIRPLUS: dispatch returns ENOSYS. FSYNC and hard-link creation also have no dispatch implementation. | `crates/bridge-fuse/src/init.rs` `wanted`; `bridge.rs` dispatch. Advertise only implemented semantics and complete the required operation set before claiming POSIX support. |
| BUG-8 | SETATTR requests ownership or timestamps: only size and mode are handled, with success possible for ignored fields. | `VolumeBridge::setattr` and the reduced `Bridge` signature. Implement or explicitly refuse each requested field; success must describe actual effects. |
| BUG-9 | Capacity query: free space is derived from current usage rather than the quota and usable reservation. | `VolumeBridge::statfs` sets `blocks = max(2 * used, 1)` and `bfree = used`. Report the claim's capacity and accountable available space. |
| BUG-10 | RENAME2 carries no-replace or exchange flags: the dispatch drops them and calls ordinary rename. | `bridge.rs` `serve_rename`, `request.rs` `RenameIn`, and `Bridge::rename`. Preserve and implement flags or refuse unsupported semantics before mutation. |
| BUG-11 | Daemon restart preserves catalog identity but recreates scratch content empty and drops local snapshots. | `crates/server/src/verbs.rs` `rebuild_recovered`, `rebuild_volume`, `reconcile_lost`. Persist content roots, bytes and witnesses in anchor-owned RAM with atomic recovery boundaries; do not describe current recovery as content survival. |
| BUG-12 | A sequence of minority writes and changing takeover quorums rewrites a committed ledger value. | `crates/db/src/ledger.rs` `Holder::reconcile` skips matching identities without refreshing their accepted epoch; `adopt` selects by that epoch. The trace below is a source-derived counterexample, fixed by `d9cb6e5` with a recorded regression. |
| BUG-13 | Protocol tests cannot generate the quorum transition in BUG-12. | `crates/db/tests/ledger.rs` oracle `reachable` always includes candidate zero; continuity checks adopted length, not adopted values. `d9cb6e5` removes the candidate-zero restriction and strengthens generation. The immediate adopted-log check still compares length; direct value comparison and message-level histories remain owed. |
| BUG-14 | Early failure receiving the mount helper's descriptor returns before waiting for its child. | `crates/bridge-fuse/src/mount.rs`: `receive_device(&ours)?` precedes `child.wait()`. Keep an owner that reaps or cancels the helper on every exit, with a derived handshake deadline. |

Other incompleteness: bridge invalidation encoders are not a demonstrated end-to-end
coherence path; overlay metadata mutations need a sibling sweep for base-aware routing;
the CLI returns attachments without a mounted path; grants cannot yet be issued through
the documented CLI; MCP, native macOS/Windows bridges and virtio-fs are not implemented.
These are tracked as integration gaps, not silently classified as fixed bugs.

## 2. Ledger counterexample (BUG-12)

Three distinct holders A, B and C, fault tolerance f=1, quorum size two. Each step
is permitted by the public `Owner::take_over` / `Owner::propose` interface. `X@1`
means identity X with stored accepted epoch 1. Epoch numbers are structural sequence
labels, not tunables.

1. Epoch 1 proposes X to A only. A holds X@1; no quorum commits.
2. Epoch 2 takes over through B+C, sees empty logs, and proposes Y to B only.
   B holds Y@2; no quorum commits.
3. Epoch 3 takes over through A+C, adopts X, then proposes a following record Z
   through A+C. A's matching prefix is skipped, leaving X@1; C stores X@3.
   X and Z are now quorum-acknowledged.
4. Epoch 4 takes over through A+B. At position zero, `adopt` chooses Y@2 over X@1.
5. The new owner proposes another record through the quorum. Reconciliation replaces
   X with Y at a previously committed position: NoLoss and TotalOrder fail.

An accepted value's epoch must describe its latest acceptance even if the bytes agree.
Also audit `Owner::replicate`: its documented prefix-extension precondition is not
checked before it replaces the owner's log. Distinct-holder counting, epoch overflow,
delayed old messages and unavailable holders' hidden state require adversarial tests.

## 3. Validation owed before closure

Each bug fix starts with a failing observable regression, followed by the minimal fix
and a sibling sweep. Bridge regressions must reach real kernel mounts as well as the
codec seam. Recovery tests must read bytes, metadata, witnesses and snapshots after a
crash, not merely find catalog IDs. Reservation tests must have a second consumer try
to steal committed capacity. Distributed tests must control individual messages and
compare every historically committed value after recovery.

The prior TLA+ results remain historical evidence for their modeled transitions; they
do not validate this implementation or amendment A-9. Model revalidation is owed to the
§4.8 implementer under the project's explicit tooling authorization rules. No new model
checking job, installation or CI dependency is authorized or added by this docs change.
