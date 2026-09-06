# False statfs: the bridge invented total and free space instead of reporting the quota (BUG-9)

Date: 2026-09-05. One of the three bridge findings the A-9 audit left open under GAP-A9-3 (with
BUG-5 base-aware lookup and BUG-7 READDIRPLUS/FSYNC/LINK dispatch). Fixed.

## Description

`statfs`/`FSSTAT` reported a filesystem size and free space that were arbitrary functions of the
*used* amount, not the volume's real capacity: an empty volume reported a total of one block and
zero free; a volume with `U` used blocks reported a total of `2·U` and a free of `U`. So `df` on a
mount showed a size that grew with usage and a free figure unrelated to the quota a write is
actually refused past.

## Root cause

`VolumeBridge::statfs` (`crates/bridge-core/src/volume_bridge.rs`) computed:

```
let used = accounting.referenced_bytes / block;
blocks = max(1, used * 2);   // "total" — a multiple of the used amount
bfree  = used;               // "free" — equal to the used amount
```

The comment even acknowledged it: "the volume core does not expose a hard cap here … so free is
reported generously." But the volume core *does* have a cap — its `Quota` — the very figure the
quota admits writes against (`Quota::admit(referenced_bytes, charge)`); it was simply not exposed to
the bridge.

## Impact

- `df` and any capacity-aware client saw a nonsense filesystem size and free space. A tool that
  refuses to write when free is low, or sizes a copy by available space, would misbehave.
- No correctness impact on the data path (the quota is still enforced on write); the falsehood was
  confined to the reported statistics.

## Fix

- `Volume::capacity_bytes()` (`crates/vfs/src/volume.rs`) exposes the quota ceiling — a bounded
  quota's `limit`, a dynamic quota's `max`.
- `statfs` now reports the real triple: total = capacity, used = `referenced_bytes`, free =
  capacity − used (saturating), all in block units. Total and used are the same pair the quota
  admits against, so the free figure is the real remaining quota.
- File counts (`files`/`ffree`) stay zero, honestly meaning "not enforced here": the volume
  allocates inodes up to the store cap, and an exact free-inode count is owed with a store accessor
  (a smaller, separate item than the false-space figure this fixes).

Gated: `crates/bridge-core/tests/volume_bridge.rs`
(`statfs_reports_the_real_capacity_not_an_invented_figure`) drives statfs against the 1 GiB test
quota and asserts the total is the real capacity (262144 blocks), independent of usage, and that
free falls by the written blocks after a write. That test fails on the previous code, whose empty
total was one block — so it discriminates the fix. fmt, clippy, xtask, cross clean.

## Ledger

Closes the BUG-9 portion of GAP-A9-3. The GAPS.md line that lists "BUG-9 truthful statfs" among the
open bridge findings should drop it; left for the audit-doc owner to avoid a concurrent-edit
conflict on `docs/wip/GAPS.md`. BUG-5 (base-aware lookup) and BUG-7 (READDIRPLUS/FSYNC/LINK dispatch)
remain open under GAP-A9-3.
