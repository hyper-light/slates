# A FUSE mount that ends in its turn keeps its registry attachment

**Status:** fixed 2026-10-23 (`turn` always puts the mount back; `ended` is its one teardown).

## Description

When a FUSE mount ends because its device does (the user unmounts it, or the device read fails), the daemon never
revokes or frees the mount's registry attachment. One registry slot leaks per such end. The registry holds
`MAX_ATTACHMENTS` (4096) slots per shard, shared by every attachment kind the shard admits (FUSE, NFS, the SDK, guest
devices). So after 4096 FUSE unmounts on one shard, every new attachment there is refused.

Found while building the journal's readers (§4.5; the plan "Journal budget derived", step 1). A FUSE mount's reader is
closed in `ended`, the mount's teardown. The new test `every_fuse_mount_is_a_journal_reader_from_its_start_to_its_end`
(`crates/cli/tests/cli.rs`) saw 2 readers after the second of two mounts was unmounted, where 1 was expected.

## Root cause

`turn` (`crates/server/src/fuse.rs`) takes the mount out of `fuse_mounts` to serve it, and puts it back at its end. But
four of its exits return `Turned::Ended` without putting it back, so the mount is dropped there:
- the volume is gone;
- the volume's slot is gone;
- the request turn ended (`Ok(Turn::Ended)`, the device's disconnect);
- serving failed (`Err(_)`).

The serve task then calls `ended`, which tears a mount down only if it finds it in `fuse_mounts`:

```rust
fn ended(s: &mut ShardState, attachment: u64) {
  if let Some(mount) = s.fuse_mounts.remove(&attachment) {
    s.attachments.revoke(mount.registry);
    s.attachments.drain(mount.registry);
  }
  ...
```

The mount is already gone, so the registry attachment is neither revoked nor drained. `end_attachment` frees registry
slots only for `mount_attachments` (NFS and SDK mounts), not for FUSE mounts.

Confirmed on the box (Linux 7.2.9) with temporary prints at each exit and in `ended`, running the test above:

```
DEBUG-READERS open key=0 readers=1
DEBUG-READERS open key=4294967296 readers=2
DEBUG-READERS turn-ended key=4294967296 attachment=281474976710658
DEBUG-READERS ended attachment=281474976710658 present=false
```

The second mount ended in its turn (the unmount), and `ended` found nothing to tear down.

## Impact

- **One registry slot per FUSE unmount, forever.** The attachment stays `Live` in the registry. Its in-flight count and
  its slot are never released. After 4096 on a shard, `Attachments::attach` refuses every new attachment on it, of any
  transport. A daemon that serves a mount per task (CI, agents) reaches that. Nothing else frees the slot short of a
  daemon restart.
- **Any teardown step added to `ended` is skipped on the common path.** The journal reader it closes is the first such
  step. Since the drop is silent, no counter showed the leak.
- No data is lost. The leaked attachment admits no requests, because its device is gone.

## Exact edits

- `crates/server/src/fuse.rs`: `turn` always puts the mount back. Its body moves into `turn_with`, which serves a mount
  it is lent and never touches `fuse_mounts`. `turn` takes the mount out, calls `turn_with`, and puts it back on every
  exit. So `ended` is the one place a mount is torn down: its reader closed, its registry attachment revoked and
  drained.
- `crates/bridge-core/src/authority.rs`: `Attachments::live`, the registry's occupied slots.
- `crates/ipc/src/protocol.rs`: `ShardReport` gains an appended `registry_attachments`, filled in
  `crates/server/src/verbs.rs`. `slates status` (`crates/cli/src/verbs.rs`) and MCP (`crates/mcp/src/lib.rs`) report
  it.
- Tests, both red before the fix (`crates/cli/tests/cli.rs`, the box):
  - a FUSE mount and its unmount leave the shard's registry attachments where they were;
  - every FUSE mount is a journal reader from its start to its end.

## Siblings swept

- No other transport's serve loop takes its state out of a map and drops it on an exit. NFS removes only replies there;
  the virtio-fs device loop owns its state in place.
- `merge_service.rs` takes a green's replica out to submit an increment, and drops it when the holder computes a
  different head (`RECOMPUTE_MISMATCH`). That looks deliberate: a diverged replica is not kept. It is noted here for
  review, not changed.

## Verification

On the box (Linux 7.2.9, load 30-77), `SLATES_TEST_CLI=1 cargo test -p slates-cli --test cli --
a_fuse_mounts_unmount_frees_its_registry_attachment every_fuse_mount_is_a_journal_reader --test-threads=1`.

**Before the fix**, both tests failed:
- the registry test with "the unmount freed its registry attachment: 1 held, 0 before the mount";
- the reader test at its check after the unmount.

The prints showed `turn-ended`, then `ended … present=false`, for every unmount.

**After the fix**, both passed. Every `turn-ended` was followed by `ended … present=true` and the reader's close,
including the restored mount's after a takeover:

```
DEBUG-READERS turn-ended key=4294967296 attachment=281474976710658
DEBUG-READERS ended attachment=281474976710658 present=true
DEBUG-READERS close key=4294967296 readers=1
```

The prints were removed before the commit.

