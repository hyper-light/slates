# A direct read made a stale cached page fresh, and its write-back undid another path's write

**Status:** fixed 2026-10-22 (A-144, approved by Ada in plan mode). Root cause confirmed by the mount's own counters and
the volume's bytes, on a real FUSE mount.
**Severity:** a FUSE mount silently reverts another path's acknowledged write. The whole page of it went back to the
mount's stale copy. It needs three things together:
- a page of a file open for writing through the mount;
- another path's change to that page;
- one `O_DIRECT` read of the page, before the mount's invalidation of it lands, followed by a store into the page.

DAX through a mount maps such pages into a guest.
**Found:** GAPS' open "`O_DIRECT` reads" item, written 2026-10-22 from reading Linux 6.12's source and not reproduced
until this record.

## What happens

The dirty-page write-back rule (A-140; §4.6 "Cache posture") keeps, for each byte of a file open for writing, the
journal point its kernel's cached copy is as of, and whether another path changed it since. A write-back of a cached
page is applied except over its stale bytes. Two kinds of event reset a byte to fresh:
- a fetch;
- the mount's own write.

Every READ of such a file counted as a fetch: "a read changes nothing, so the bytes it returned are fresh as of the
head" (`note_opens_and_fetches`, `crates/bridge-fuse/src/channel.rs`).

A direct READ breaks that. It is answered from the volume, and it leaves the kernel's cached page as it was. One direct
read of a page that another path had just changed therefore marked the kernel's stale copy fresh. A store into that
copy made it dirty, and its write-back was judged fresh and applied whole.

## The evidence

The bare-metal box (Linux 7.2.9, x86_64, load 20–55 from other work), 2026-10-22. The test
`a_direct_read_does_not_make_a_cached_page_fresh` (`crates/bridge-fuse/tests/coherence_mount.rs`) runs over the
in-process mount harness:
1. Write a one-page file through the mount, map it shared, and fault its page in, so it is fetched.
2. Hold the mount's invalidation rounds (a new harness ask), so its kernel keeps the page.
3. Replace the file through another attachment.
4. [Read the page once with `O_DIRECT`.]
5. Store a byte through the map into the page the kernel still caches.
6. Release the rounds, so the kernel launders the page, and `msync`.

It asserts its own preconditions:
- the direct read returned the replacement's bytes;
- it reached the mount as a READ;
- the kernel still held the old page when the store was made.

| | write-backs after the store | bytes kept back | refused unknown | `msync` | the volume afterwards |
|---|---|---|---|---|---|
| without the direct read | 1 | **4,096** | 0 | Ok | the replacement, whole |
| with it, before the fix | 1 | **0** | 0 | Ok | **all 4,096 bytes the stale page's**, 3 of 3 |
| with it, after the fix | 1 | 0 | **1** | `EIO` | the replacement, whole, 5 of 5 |

### The first run of the test proved nothing, and why

It passed, with and without the direct read, at every write-back "refused unknown". The harness's wrapping bridge,
`FailingGather` (`crates/bridge-fuse/tests/common/mod.rs`), did not forward `data_changes` or `journal_capacity`. Both
arrived with A-140 with trait defaults: no changes, and a capacity of zero. Under those, every fetched file was lost at
once, and every map write-back in the in-process mount tests was refused.

So no `coherence_mount` test had ever exercised A-140. With the wrapper fixed, the other three tests stay green, the
chaos test included, and the reproduction above shows the bug.

The lesson: a test of a mechanism must show the mechanism ran. The test now asserts that the write-back arrived, and
how it was judged.

## Root cause

A READ through a handle opened `O_DIRECT` was counted as a fetch. It may not have filled the kernel's cache, so it said
nothing about the kernel's copy.

The mount cannot tell such a READ apart from a fill. On an `O_DIRECT` handle a READ is either of:
- a direct read;
- a page-cache fill: a fault on a map of that handle, or `generic_file_read_iter`'s buffered fallback after a short
  direct read.

Both carry the same open flags in `fuse_read_in.flags`, and the protocol has no other mark.

## The fix (A-144)

A READ through a handle whose open flags carry `O_DIRECT` is **ambiguous**. So is a body too old to carry the flags,
from before protocol 7.9; the mount negotiates up to 7.31.
- Bytes it covers that are fresh, after the records not yet consumed, stay as they are: both readings agree on them.
- The others become **unknown** as of the read's head.
- An unknown byte is settled by the next event that settles both readings:
  - a fetch, or the mount's own record, makes it fresh;
  - another path's record makes it stale.
- A write-back over an unknown byte is refused `EIO`, through the rule's existing "cannot be told" path. It is loud,
  because neither silent choice is safe:
  - "always a fetch" was this bug;
  - "never a fetch" would keep back a process's real store into a page it had just filled.

## Exact edits

- `crates/bridge-fuse/src/request.rs`: `ReadIn::flags`, the handle's open flags at byte 32 of `fuse_read_in`, `None`
  for a shorter body.
- `crates/bridge-fuse/src/coherence.rs`:
  - the byte state is three-way (`Byte`);
  - `PageStates::read_ambiguously`;
  - `stale_after` answers `None` over an unknown byte, via `known_stale_within`;
  - `change` settles an unknown byte;
  - the module doc;
  - the oracle's model gains the ambiguous read, written apart from the implementation's type, and a case-by-case
    test.
- `crates/bridge-fuse/src/channel.rs`:
  - the READ arm sends an ambiguous READ to `read_ambiguously`;
  - `may_bypass_the_cache` (`O_DIRECT` from `rustix`, since its value differs by architecture);
  - `pending_changes`, the walk of unconsumed records, now shared with the write-back judgment.
- `crates/bridge-fuse/tests/`:
  - `coherence_mount.rs`: the test above, with the asks `HoldGathers`/`ReleaseGathers` and the report's `writes`,
    `kept_back` and `unknown`;
  - `common/mod.rs`: the forwarding fix;
  - `abi.rs` and `codec.rs`: the open flags' offset, and the short body.
- Docs: `SLATES_DESIGN.md` §4.6 and A-144; GAPS.

## Verification

- **The oracle.**
  - `stale_bytes_follow_the_rule` (2,000 generated histories, with ambiguous reads) and the case-by-case test pass.
  - **Red-checked by mutation:** with an ambiguous read counted as a fetch again, both fail. The oracle reports a
    minimal history.
- **The box** (Linux 7.2.9, x86_64):
  - the whole `slates-bridge-fuse` suite, 127 tests across 19 binaries, green;
  - the reproduction 5 of 5 after the fix (the table);
  - workspace clippy `-D warnings` and the doc gate clean;
  - the CLI's mount-route tests green: `map` 7 of 7, `truncation` and `a_change_reaches` 1 of 1 each;
  - `fuse_mount` 3 of 3, and the recovery oracle 41 of 41.
- **macOS:** workspace clippy `-D warnings`, the doc gate, `cargo xtask check` and `cargo fmt --check` are clean. The
  lib, ABI and codec tests are green.

## Siblings

- The FUSE `Mock` bridge (`tests/dispatch.rs`) also takes the trait's defaults. It wraps no volume, so they are its
  truth.
- The virtio-fs direct route shares `serve_write`, and its byte states would come from the guest's READs (GAPS, "still
  open"). Its READs carry no open flags of the guest's, so the same question will need answering there when that
  route is built.
