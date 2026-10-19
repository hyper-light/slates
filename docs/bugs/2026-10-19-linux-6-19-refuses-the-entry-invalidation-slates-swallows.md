# Linux 6.19 refuses `FUSE_NOTIFY_INVAL_ENTRY` with `ENOENT`, and slates reads that as "nothing stale"

Status: **root cause confirmed by instrumentation. The silent half is fixed — the refusal is counted, not
discarded. The posture half is a §4.6 decision and is Ada's call.**

## Description

At HEAD, on x86_64 Linux 6.19.10 (Fedora 44), two `slates-bridge-fuse` mount tests fail:

- `coherence_mount::a_change_through_another_attachment_reaches_the_kernel_without_a_request_and_a_refused_gather_is_retried`
  — `coherence_mount.rs:345`, "the name another attachment made in the root is seen through the kernel":
  left `"absent"`, right `"present"`.
- `owner_turn::a_mutations_reply_waits_for_its_barrier_and_a_refused_barrier_answers_eio` —
  `owner_turn.rs:292`, "the directory is in the volume, unpublished".

Eighty-three other kernel-mount tests pass on the same host in the same run (`cli` 33, `nfs_mount` 38,
`fuse_mount` 3, `attach_forms` 9), so this is not "FUSE is broken here".

## Root cause

The daemon writes its invalidations to `/dev/fuse` in `NotifyThread::start`
(`crates/bridge-fuse/src/channel.rs`), and treats two errnos as benign:

```rust
// The kernel answers `ENOENT` when it holds no such entry and `ENOTEMPTY` when a directory it would drop
// is in use — neither is a fault of the mount (it has nothing stale, or revalidates on next use).
Ok(_) | Err(rustix::io::Errno::NOENT | rustix::io::Errno::NOTEMPTY) => {}
```

Instrumenting that write with the notification code (the `error` word of `fuse_out_header`, offsets 4..8)
and running the identical build on two kernels gives the whole story:

| kernel | arch | `INVAL_ENTRY` (code 3, 34 bytes) | test |
|---|---|---|---|
| 6.12.76-linuxkit | x86_64 | **`wrote 34 bytes`** | **pass** (0.44 s) |
| 6.19.10-300.fc44 | x86_64 | **`SWALLOWED errno=2` (`ENOENT`)** | **fail** |

Everything else matches: same commit, same architecture, same page size (4,096), the same three
`INVAL_INODE` (code 2) writes before it, and on both kernels a final `INVAL_INODE` also answers `ENOENT`
(that one is benign and is not the difference).

So **Linux changed its answer to `FUSE_NOTIFY_INVAL_ENTRY` for this (parent, name) somewhere in 6.13…6.19**,
and the comment above is no longer true on the new kernels: the kernel refuses the invalidation *and keeps
serving the cached negative entry*. The test caches "absent" twice on purpose, so there is a negative dentry
with a lifetime; the invalidation that should drop it is refused; the probe then answers from that dentry
forever. Polling proves it is not lateness — the name was still `"absent"` after **10.27 s** of probing at
50 ms.

## Impact

§4.6's `Invalidated` posture is the promise that a change made through one attachment is **pushed** to the
kernel's caches, and that posture is what lets slates hand out unbounded entry lifetimes. On a 6.19 kernel
that promise does not hold for a name cached absent: a file another attachment (an SDK client, another mount,
an outsider edit under a base directory) creates is **never** seen through the mount. It is also silent —
CLAUDE.md #9 forbids a swallowed error, and this is one: the refusal is discarded, the cursor advances as
though the round were delivered, and no counter moves.

Not caused by anything recent. Eliminated by A/B from one tree, both arms failing identically: today's
`FUSE_MAX_PAGES` change, and `b9935dfe` (invalidations moved off the serving thread, `channel.rs` restored
from `b9935dfe~1`). Also not path length — unlike `ipc_mount`'s unrelated `ENAMETOOLONG`, these two fail from
a 7-character working directory too.

## What a fix has to decide (Ada's call, §4.6)

1. **Stop swallowing it — DONE.** `ENOENT` from an `INVAL_ENTRY` is no longer evidence that nothing is stale,
   and it is no longer discarded: `encode_invalidation` now says whether a message is an entry invalidation,
   the notify thread counts the entry refusals separately from the benign ones (an `ENOENT` on an *inode*
   invalidation, and `ENOTEMPTY`, still mean what the old comment said), the count rides the round's outcome
   into `ServeState`, and the server reports it as **`fuse.entry_invalidation_refused`** in `status`. Zero is
   the healthy reading; any movement means this mount's `Invalidated` posture is not being honoured by the
   kernel it is talking to. It is counted rather than raised because the refusal is **not retryable** — the
   next round is refused identically, and a round kept for retry is the unbounded loop CLAUDE.md #8 forbids.
   This was the part that is a defect under CLAUDE.md #9 whatever else is decided, so it did not wait.
2. **Then the posture — open, and narrowed to one lever by measurement.** Three options were on the table: a
   bounded negative-entry lifetime, invalidating the **parent inode** when the entry refusal comes back, or
   dropping to `Revalidated` for negative entries. Two of them are now closed by experiment on the 6.19 host:

   - *Parent-inode invalidation does not work.* Appending a `FUSE_NOTIFY_INVAL_INODE` for the parent directory
     after every entry invalidation — the notification the kernel **does** accept there, as the trace shows —
     leaves both tests failing exactly as before. The kernel takes the message and the stale entry survives it.
   - *`FUSE_EXPIRE_ONLY` is not the trigger.* Forcing the flag off, so every entry invalidation is a true drop,
     also leaves both tests failing.

   **So no notification slates can send clears the entry on 6.19, and the only remaining lever is the lifetime
   slates grants.** `valid_for` answers `CacheLifetime::Forever` for a volume's own objects, and the comment at
   the negative-entry reply says why that is sound: "Every way a name appears through another attachment
   invalidates it". On 6.19 that justification is simply false.

   **And the hole is wider than negative entries.** A delete, a rename's source and a rename's target all travel
   as `Invalidation::Entry` too, and the refusal is of the message, not of the case — so on such a kernel a name
   *removed* through another attachment also stays visible, not just a name created. `Forever` is therefore
   unsound for **every** entry on these kernels, which is why this is a §4.6 posture change and not a tweak:
   it reprices the core caching promise, it needs a derived bound rather than a chosen one, and it must re-take
   the measurement `c3c0024b` recorded (negative entries bought −28% lookups on a pip install) to show what the
   bound costs. That is a planned change with a design-doc and amendment-log entry, not an improvisation.

Narrowing the kernel range below 6.13…6.19 needs a kernel between the two; the three hosts available are
6.12, 6.19 and 7.2, so it was not narrowed here.

## Reproduction

```
# red
SLATES_TEST_CLI=1 cargo test -p slates-bridge-fuse --test coherence_mount -- --test-threads=1 --nocapture
# green, same code, kernel 6.12, x86_64 userspace under Docker's linuxkit VM
docker run --rm --platform linux/amd64 --device /dev/fuse --cap-add SYS_ADMIN \
  --security-opt apparmor=unconfined -e SLATES_TEST_CLI=1 -v "$PWD":/w:ro -w /w rust:1.98.1 \
  bash -c 'apt-get -qq install -y fuse3 && cargo test -p slates-bridge-fuse --test coherence_mount'
```
