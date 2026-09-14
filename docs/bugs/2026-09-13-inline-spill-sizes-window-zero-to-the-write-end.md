# A far write into an inline file allocated a whole chunk for window 0

Status: **fixed** (branch `agent/admission`, 2026-09-13). Found by the physical write charge
(§4.2 allocator rounding, GAP-A9-1) the moment it became truthful: the hostile sparse-writer test
was refused at its third window with a sixteen-page quota.

## Description

A file holding a few inline bytes (up to two cache lines, kept in the inode) receives a write far
past the first chunk window — the sparse-writer shape, one byte at every window start. The inline
bytes spill into window 0 and the write lands in its own window, as designed; but window 0's arena
block was a whole chunk (64 KiB here) for the one inline byte it held.

```
$ cargo test -p slates-vfs --test charge a_sparse_writer_cannot_hold_more_arena_than_its_quota
window 2 is within the quota: NoSpace
```

Under the earlier logical charge (one byte charged for one byte materialized) the waste was
invisible in the accounting and only the arena paid for it; under the physical charge the phantom
chunk is charged, so the volume's quota filled a whole chunk early and the next window was refused.

## Root cause

`Volume::apply_write`'s `Body::Inline` arm (crates/vfs/src/volume.rs) opened window 0's extent with
`store.content.open(0, end.min(chunk), epoch)` — sized to the **write's end** capped at a chunk —
whatever window the write lands in. For a write inside window 0 that is the right final size; for a
write beyond it, window 0 was sized to a full chunk, filled with the inline bytes, then sealed at
that block when the cursor left the window (`write_into` seals the open extent before opening the
target window). The charge rule the design states (§4.5: the open extent grows "by a page multiple
from the buddy tree" as writes need it) was violated at this one site.

## Impact

Arena waste: up to one chunk less one page per small file that receives a write past its first
window (with the 16 KiB page of the design's laptop, 256 KiB chunks: up to 240 KiB per such file),
uncharged under the old rule (so it also ate into the shard's unpromised capacity silently). No
data effect; reads were correct.

## Fix (applied)

Window 0 is opened at the size it will hold: the inline bytes, extended to the write's end only
when the write lands in window 0 (`window_zero = if off < chunk { end.min(chunk).max(inline_len) }
else { inline_len }`). The write then grows the block only as it needs, exactly as every other
window. After the fix the sparse writer admits sixteen one-byte windows on a sixteen-page quota,
refuses the seventeenth, and the arena holds exactly the quota (`crates/vfs/tests/charge.rs`); the
model oracle (which states the page-rounded rule and covers inline spills) passes 400 histories.

## Sibling sweep

The other `open` sites size to what the window holds: `open_window` opens at the remaining write
length and grows to the write's end (`write_open` → `grow`), `reopen` at the extent's length, the
overlay's `pin_windows` at the window's bytes, recovery's rebuild through `write`. `grow` allocates
`need.next_multiple_of(page)` and the buddy rounds that to its block, so every block equals
`charged_window(materialized)` — the identity `crates/vfs/tests/charge.rs` and the model oracle
now assert.
