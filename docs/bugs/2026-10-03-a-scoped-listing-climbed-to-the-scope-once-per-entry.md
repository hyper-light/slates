# bridge-core: a scoped listing climbed to the scope once per entry (3 ms a page at depth 64, 229 ms at 4,096)

**Date:** 2026-10-03. **Audit:** AUD-29-76 (its follow-up measurement of the per-object scope check).
**Design:** §4.6 scoped exports. **Found by:** `crates/bridge-core/examples/scoped_listing_bench.rs`.

## Description

`ScopedBridge::readdir` checked every entry of a page with `within(entry, scope)`, a climb of the entry's parents to
the scope. That climb costs about 40 ns per level, so a page cost entries × depth: a 1,024-entry page took 486 µs at
depth 8, 2.97 ms at depth 64, 24 ms at depth 512 and 229 ms at depth 4,096. `lookup` climbed twice, once for the
parent and once for the result.

## Root cause

Each check started from scratch. But the listed directory, and a lookup's parent, had already been admitted by the
same request, and the subtree relation is transitive: an object beneath an admitted directory is inside the scope.

## Impact

Performance only: the answers were right. A client listing a large directory deep in a scoped export (an NFS
`READDIR`/`READDIRPLUS`, a FUSE `readdir`, a guest's listing) paid milliseconds per page.

## Exact edits

- `crates/bridge-core/src/scoped.rs` `within_admitted(object, parent)`: `within(object, parent) || within(object,
  scope)`. It equals `within(object, scope)` whenever `parent` was admitted. An entry homed in the listed directory
  costs one step; an alias homed elsewhere falls back to the climb. `readdir`'s entries and `lookup`'s result use it.

## Proof

- **Measured** (`docs/wip/BENCHMARKS.md`, 2026-10-03): a 1,024-entry page now costs 76 µs at depth 64 (was
  2,973 µs) and 297 µs at depth 4,096 (was 229 ms). A lookup is halved: 2.74 µs at depth 64, was 6.16 µs.
- **The fallback** (`crates/bridge-core/tests/volume_bridge.rs`
  `an_alias_homed_in_another_directory_inside_the_scope_is_listed_and_found`): with the fallback removed, the alias
  is not listed and the test fails; with it, the test passes.
- **No regression:** the existing rule tests still pass:
  - `a_scoped_bridge_reaches_nothing_outside_its_directory` (an alias homed outside stays hidden);
  - bridge-core's 64 tests;
  - the server's scoped attach and guest tests;
  - the CLI's `slates mount --subtree` through a real macOS NFS mount.

## Sibling sweep

Every result check in `scoped.rs` was read. `lookup` and `readdir` are the two that check an object found under an
admitted directory; both now use `within_admitted`. Every other method checks the object the request names, which
has no admitted directory to start from, and still climbs once.
