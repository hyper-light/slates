# Base routing, FUSE semantics and attachment lifetimes — the GAP-A9-3 / GAP-A9-4 sweep (2026-09-14)

Branch `agent/base-fuse` from `4f5deae` (main's head on 2026-09-14). Six commits, each a
piece the charter names, each with its failing test first and the before/after numbers below.
Machine: this macOS box (Darwin 25.4.0, arm64), shared with three other agents building; at
the end of the run `sysctl -n vm.swapusage` read `total = 17408.00M used = 16039.38M free =
1368.62M` and `vm_stat` line 2 `Pages free: 4682` — the memory wall the coordinator named; no
timing failure below was judged a defect without that reading in hand.

## 1. Inventory (file:line at `4f5deae`; what the audit found; what the code did; verdict)

| Finding | Audit | Code at `4f5deae` | Verdict |
|---|---|---|---|
| BUG-4 handles grow | `open_handle` appends, `release` clears without reuse | `crates/bridge-core/src/volume_bridge.rs:26–34, 235–251, 513–528`: a bounded generational `slates-mem` slab (65,536 slots, 256 per segment), reuse with a bumped generation, typed `SlabFull` at the bound; tested to 1,000 cycles only | Fixed at the seam; the beyond-capacity case (AC-3.12) untested — now tested (`150825f`) |
| BUG-5 base lookup depends on listing | `VolumeBridge::lookup` bypasses the base plane | `volume_bridge.rs:347–363` routes lookup through `Overlay::lookup_no`, which loads the listing on demand (`crates/vfs/src/base.rs:786–821, 662–683`); but every **mutating** verb (`create/mkdir/symlink/link/unlink/rmdir/rename`, the four halves of `setattr`) used the plain `Volume::*_no` forms, which record no witness, drop the whiteout after a listing invalidation and never check the base for a name | Lookup fixed; the metadata sweep was open — closed by `991c84e` |
| BUG-6 writeback flag | `1 << 8` advertised for writeback | `crates/bridge-fuse/src/abi.rs:139` is `1 << 16`; no header-independent vectors; the design's own text "bit 8 is FILE_OPS" is wrong (the header: `FUSE_FILE_OPS (1 << 2)`, `FUSE_SPLICE_MOVE (1 << 8)`) | Fixed; vectors added by `9a960bb`, which also found the `flags2` defect below |
| BUG-7 undispatched READDIRPLUS/FSYNC/LINK | dispatch answers ENOSYS | `crates/bridge-fuse/src/bridge.rs:95/101/112` dispatch all three; `tests/dispatch.rs` covers them | Fixed |
| BUG-8 ignored setattr fields | only size and mode handled | The seam applies every field (`SetAttr{size,mode,uid,gid,atime,mtime}`); the FUSE edge passed a `FATTR_*_NOW` time through as the kernel's value, ignored `FATTR_CTIME` and any unknown `valid` bit under a success reply, ignored `FATTR_KILL_SUIDGID` | Seam fixed; edge residue closed by `9a960bb` (ctime carried since `991c84e`) |
| BUG-9 false statfs | `blocks = 2*used`, `bfree = used` | `volume_bridge.rs:733–765` reports the quota ceiling and `referenced_bytes` — for a **dynamic** volume the bare `max`, which the shard cannot back | Partly fixed; the ledger row saying it "stays open" was stale for the invented-figure half and right for the honourable-capacity half — closed by `41b9771` |
| BUG-10 dropped rename flags | `rename2` flags dropped | `volume_bridge.rs:652–681` honours NOREPLACE, refuses EXCHANGE `EINVAL`; the NOREPLACE existence check used the plain lookup (a not-yet-looked-up base name was replaced silently), and the edge dropped any flag bit it did not map (`RENAME_WHITEOUT`, undefined bits) | Residue closed by `991c84e` (overlay-aware check) and `9a960bb` (unknown flags `EINVAL`) |
| BUG-14 helper orphan | `receive_device(&ours)?` before `child.wait()` | `crates/bridge-fuse/src/mount.rs:161–208`: `HelperGuard` kills and reaps on every exit; Linux-only, untested anywhere, no deadline (a helper that never answers blocked forever) | Fixed but unproven; proven and bounded by `150825f` |

## 2. The pieces

### (1) `991c84e` — base metadata mutations through the bridge go through the overlay rules

`crates/bridge-core/tests/base_overlay.rs` drives the bridge over the base plane's simulated
host (the host oracle: outsider edits, a controllable clock, a watcher that overflows).
Before-run at `4f5deae`'s routing (the same tests over HEAD's `volume_bridge.rs` with only the
`dyn HostFs` signature change): **8 of 10 failed** —

```
cargo test -p slates-bridge-core --test base_overlay
  chmod/chown/utimes/truncate of an untouched base file: no witness (diverged set []),
    truncate to 3 read back 15, mtime 222 read back 0 (the live-disk stat undid them)
  rename: {"lib.rs","lib2.rs","main.rs"} listed where {"lib2.rs","main.rs"} was right
  NOREPLACE onto a not-yet-looked-up base name: Ok(()) where Err(AlreadyExists) was right
  link over a base name: Ok(()); create over a base name: Ok(())
  unlink after a hint invalidated the listing: {"f","g"} listed where {"g"} was right
```

After: 10/10. Changes: `Overlay::{chown, set_times, open_base}` and the by-inode wrappers
`Overlay::{create_file_no, mkdir_no, symlink_no, link_no, unlink_no, rmdir_no, rename_no}`
(`crates/vfs/src/base.rs`); `Volume::set_times` takes each time as an `Option` (`UTIME_OMIT`)
plus an explicit change time; `SetAttr.ctime`; the seam's host is `dyn HostFs`
(`with_base(Box<dyn HostFs>)`, `attached(Option<&mut dyn HostFs>)`) and `slates-base` left
bridge-core's dependencies; every mutating verb routes through `Overlay` when the bridge has a
host. Record: `docs/bugs/2026-09-14-base-mutations-bypass-overlay.md`.

### (2) `9a960bb` — the FUSE edge honours or refuses every setattr field and rename flag; 18 header vectors

`crates/bridge-fuse/tests/dispatch.rs` over a recording mock. Before-run at `991c84e`'s
`bridge.rs`: **5 of 7 failed** (`setattr_now_flags_resolve_to_the_volume_clock...`,
`setattr_with_an_unhonoured_valid_bit_is_einval...`, `setattr_kill_suidgid...`,
`rename2_with_a_flag_the_seam_does_not_carry...`, `replies_carry_the_change_time...`); the
ctime-carry and xattr-ENOSYS cases already held. After: 16/16 in `dispatch.rs`.
`crates/bridge-fuse/tests/abi.rs`: 18 vectors transcribed by hand from
`include/uapi/linux/fuse.h` (torvalds/linux master, 2026-09-14, 7.46) and `linux/fs.h`:
served and unserved opcodes, every INIT flag against its neighbours, `FATTR_*`, `RENAME_*`,
notify codes, struct sizes, and the byte offsets of `fuse_in_header`, `fuse_setattr_in`,
`fuse_read_in`/`write_in`, `fuse_rename(2)_in`, `fuse_entry_out`/`fuse_attr`/`fuse_attr_out`,
`fuse_open_out`/`write_out`/`kstatfs`, `fuse_dirent(plus)`, `fuse_init_out` and the three
notifications. xattrs (T-1.21): opcodes 21–24 are `ENOSYS`, the precise unsupported error
(the kernel's `EOPNOTSUPP` to the caller); the volume core carries no xattrs
(`crates/vfs/src/export.rs:531`).

### (3) `41b9771` — statfs reports the capacity the shard can honour

`statfs_of_a_dynamic_volume_reports_only_what_the_shard_can_honour` at `9a960bb`: `the total
is within what the shard can honour (262144 blocks, budget 16777216)` — a 1 GiB total against
a 16 MiB shard budget. After: `Quota::honourable_limit` (bounded: the held reservation;
dynamic: `min(max, granted + budget.admittable())`), `Volume::honourable_capacity_bytes`,
bridge-core 34/34.

### (4) `b4eeb2d` — real invalidation delivery; bounded lifetimes for live base entries; `flags2` never negotiated

Before: nothing produced kernel invalidations (`grep -rn "inval_" crates` found only the
encoder module). Seam: `Invalidation`, `CacheLifetime`, `InvalidationCursor`,
`Bridge::{cache_lifetime, invalidations, seen}`; base plane: a hint sequence and per-directory
stale marks bounded by the listings table, `Overlay::take_stale_base_entries`,
`Volume::is_live_source`; FUSE: `HAS_EXPIRE_ONLY` negotiated, `inval_entry` carries
`FUSE_EXPIRE_ONLY`, nanosecond lifetimes on `EntryOut`/`AttrOut`, `serve_blocking` writes the
owed invalidations before each request. Tests: `crates/bridge-core/tests/invalidation.rs` (4):
an SDK write/truncate/rename/create/chmod owes exactly `[Inode{f,data}, Inode{f,data}]`, then
`[Entry{root,"f"}, Inode{root}, Entry{root,"g"}, Inode{root}, Entry{root,"h"}, Inode{root}]`,
then `[Inode{f, data: false}]`, and the transport's own create owes none; a watcher hint
expires the directory's live entries and the next lookup re-lists (host calls 5 against a
steady 4) and shows the outsider's 31-byte size; an overflow expires every loaded directory;
live entries get `Bounded{ns: granularity}` and created or copied-up files `Forever`.
Found by the vectors: `negotiate` read `flags2` from the wrong offset (a padding word that does
not exist; `fuse_init_in` is `major, minor, max_readahead, flags, flags2, unused[11]`) and
never echoed `INIT_EXT`, so no second-word capability had ever negotiated
(`the_init_reply_has_the_headers_layout`: `left: 0, right: 34359738368`). Record:
`docs/bugs/2026-09-14-fuse-init-flags2-never-negotiated.md`.

### (6) `150825f` — the handshake is portable, deadline-bounded, reaps or cancels; handles proven bounded

`crates/bridge-fuse/tests/handshake.rs` against real child processes on this host: a helper
that exits 7 without a descriptor is reaped and the error carries `code: Some(7)`; a helper
that never answers (`exec sleep 30`) is killed at the derived deadline (a hundred times the
slowest of five `sh -c true` handshakes) and reaped with `SIGKILL`; this binary re-invoked in
the helper role sends `/dev/null` over the socket and it arrives as a character device. The
first form made the helper's socket end inheritable in the parent and **2 of 3** parallel
runs timed out (another test's helper inherited it and held the socket open); the socket now
reaches the child as its standard input (`dup2` in the child, both ends close-on-exec here,
`_FUSE_COMMFD=0`), and **5 of 5** runs are green. Handles: 65,537 open/close operations
against a 65,536-slot arena leave 256 slots allocated and nothing open; a stale handle from a
reused slot never acts on the slot's new holder; the open past the bound is the typed
slab-full refusal, lifted by one release (bridge-core `tests/volume_bridge.rs`, 3 tests).

### (5) `1ebd7da` — attachment generations, in-flight pins, the barrier

`Attachments::{begin, end, generation, barrier}`, `OpContext.generation`,
`VfsError::BarrierIncomplete{attachment, generation}`; `serve_blocking` brackets each request.
`crates/bridge-core/tests/barrier.rs` (4): a barrier closes every live generation of the
volume and only that volume; it refuses while a request is in flight and changes nothing; a
consumer revoked mid-request is `BarrierIncomplete` until `drain`; by use through the bridge a
write admitted before the barrier and one after belong to generations 1 and 2, and a snapshot
taken between them reads back exactly `one` while the head reads `two`. Before: the registry
had neither generations nor in-flight accounting (`grep generation
crates/bridge-core/src/authority.rs` at `150825f`: none).

## 3. Every command and its result (all in the worktree, 2026-09-14)

| Command | Result |
|---|---|
| `cargo test -p slates-bridge-core` | 49/49 (admission 5, authority 2, barrier 4, base_overlay 10, invalidation 4, volume_bridge 20; before `991c84e`: base_overlay 1/10) |
| `cargo test -p slates-bridge-fuse` | 61/61 + 1 ignored helper role (unit 4, abi 18, base_overlay 3, codec 13, dispatch 16, handshake 3, volume_bridge 4) |
| `cargo test -p slates-vfs --no-fail-fast` | every suite green; `tests/derive.rs::net_apply_equals_raw_replay` failed once in a full run and passed on two re-runs of 300 cases; `tests/model.rs::a_tight_quota_refuses_the_same_writes_as_the_model` failed once and passed alone (400 cases) — both proptests with `failure_persistence: None`, neither's history touches `set_times`/`chown` (grep of `common/drive.rs`, `common/steps.rs`: no hits); recorded below for the integrator to check at HEAD |
| `cargo test -p slates-bridge-nfs` | 62/62 |
| `cargo test -p slates-bridge-fskit` | 12/12 |
| `cargo test -p slates-server --lib` | 34/34 |
| `cargo fmt --check` | clean (exit 0, no diff) |
| `cargo clippy -j 3 --workspace --all-targets -- -D warnings` | clean (`Finished dev profile ... in 15.05s` on the warm cache; cargo's pre-existing future-incompatibility note about the dependency `proc-macro-error2 v2.0.1` is not a lint) |
| `cargo xtask check` | `structural: ok (27 shipped crates)`, `literals: ok`, `unsafe: ok` — bridge-core and bridge-fuse stay at an unsafe budget of 0 (the handshake, the notifications and the barrier use only I/O-safe wrappers) |
| `cargo clippy -p slates-bridge-fuse --all-targets --target x86_64-unknown-linux-gnu -- -D warnings` | **cannot run here**: `zstd-sys` (a default feature of `slates-archive`, which `slates-vfs` pulls with defaults on) needs `x86_64-linux-gnu-gcc`; the Windows target fails the same way on `string.h`. CI lints natively per OS (`.github/workflows/ci.yml:31,153,266`); the Linux-only code (`channel.rs`, `mount.rs`'s `mount`/`unmount`) is checked in the container run of §4 or owed to the Linux lane |

## 4. What the CI Linux lane must confirm

The bounded Docker run the charter allowed could not happen: `docker info` answered `Error
response from daemon: Docker Desktop is unable to start` on 2026-09-14 (disk was not the
reason: 296 GiB free on `/`). So no compiler on this box has seen the Linux-only code of this
branch — `crates/bridge-fuse/src/channel.rs` (the serve loop with `begin`/`end`, `serve_one`,
`write_invalidation`) and the `mount`/`unmount`/`Mount` items of `mount.rs` — and the Linux
lane is the first to compile them. Everything else in those files (`handshake`, the guard, the
notification encoders, the negotiation) is compiled and tested here.

- `crates/bridge-fuse/src/channel.rs` compiles and `serve_blocking` delivers the owed
  invalidations to `/dev/fuse` before each request (`write_invalidation`), with `ENOENT` and
  `ENOTEMPTY` absorbed; a real mount then shows an outsider's change to a base file at the next
  `stat` without an unmount (AC-3.10's mounted half).
- `mount()` over the real `fusermount3` through `handshake` (the socket as the helper's stdin,
  `_FUSE_COMMFD=0`), and the deadline the daemon derives from its failover SLO.
- `FUSE_HAS_EXPIRE_ONLY` negotiates on a 6.2+ kernel and expire-only entry invalidations are
  accepted on a busy directory.
- The barrier's kernel half: the writeback flush (`FUSE_NOTIFY_RETRIEVE` of dirty pages) before
  a snapshot, once the daemon holds a per-volume registry to call `barrier()` on.

## 5. Siblings found, not changed here

- `Fingerprint`/`BaseEntry` carry no owner: an untouched base entry reports uid/gid 0 through
  every mount (`ls -l` in an overlay shows base files as root's). Needs an owner field on the
  read-only seam (`crates/vfs/src/host`, edited concurrently by the digest work).
- The design text §4.6 "WRITEBACK_CACHE is bit 16, while bit 8 is FILE_OPS" is wrong: the
  header names bit 8 `FUSE_SPLICE_MOVE` and bit 2 `FUSE_FILE_OPS`.
- `refusal_of_vfs` (`crates/server/src/error.rs:109`) maps `BarrierIncomplete` through its
  catch-all to `Refusal::BadRequest{reason}`; a wire `Refusal::BarrierIncomplete` is a schema
  change for the integrator's enum pass.
- No per-volume attachment registry exists in the daemon: the NFS export mints one per request
  (`crates/bridge-nfs/src/procedures.rs` `Export::new`), the virtio-fs device one per admission
  (`crates/bridge-virtiofs/src/admission.rs:390`). The snapshot verb cannot call `barrier()`
  until one exists; on this host the NFS mount's barrier is the shard's serial order.
- The daemon's `attach` verb still returns `path: None` with `AttachForm::Root` — honest, but
  not a ready device binding; returning the NFS export reference is a wire change.
- `RENAME_EXCHANGE` stays refused `EINVAL` (the errno `renameat2` gives an unsupported flag);
  implementing it needs a journal op the merge deriver understands.
- `Overlay::rename` onto an existing base *target* records the source's whiteout but no
  witness of the replaced target's fingerprint (a landing-plane verdict concern, §4.15).
- `fuse_init_out.max_background`/`congestion_threshold` are 0 (kernel defaults); §4.6 derives
  them from shard count × per-shard in-flight budget — owed with the driver's measurement.
