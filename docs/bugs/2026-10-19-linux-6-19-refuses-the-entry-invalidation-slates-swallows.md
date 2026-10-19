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

   **Correcting an earlier claim in this document: the hole is negative entries only.** It said that because a
   delete and both halves of a rename also travel as `Invalidation::Entry`, a name *removed* elsewhere must be
   equally invisible. That was reasoned from how the code routes the message and **not measured, and it is
   wrong.** Measured on the 6.19 host: create a name through the other attachment, stat it through the mount
   twice so the kernel holds it positively, then remove it through that attachment — the mount sees it go.
   `POSITIVEPROBE: created=present cached=present after_remove=absent`. Two of `coherence_mount`'s three
   scenarios pass on 6.19 for the same reason, and they run *before* the failing one.

   So the kernel refuses the invalidation of a **negative** dentry and honours it for a positive one, which
   fits the suspected cause — `fuse_reverse_inval_entry()`'s recent conversion to `start_removing_noperm()`,
   a path built for removing something that is *there*. The hole is exactly: **a name the kernel has cached as
   absent, then created elsewhere, is never seen.** Deletes, renames, attribute and data changes are all
   delivered.

## The decision is binary, and already priced

A *bounded* lifetime does not actually rescue the failing tests, and it is worth being exact about why: the
probe runs immediately after the creating mutation, so any bound longer than about zero still answers from the
cache. Making these tests green therefore means **not caching entries at all** for a volume's own objects —
not shortening the lifetime, removing it. So the choice is binary, and both sides already carry numbers:

- **Keep `Forever`.** Costs nothing. Leaves a real hole on kernels that refuse entry invalidation: a name
  created, removed or renamed through another attachment is invisible to that mount, indefinitely. The hole is
  no longer silent — `fuse.entry_invalidation_refused` moves the moment it happens.
- **Stop caching *negative* entries** (only those — positive entries keep `Forever`, since the kernel honours
  their invalidation). Closes the hole. Gives back the negative-entry share of what `c3c0024b` measured when it
  added "negative entries, kept pages and listings" together: a pip install's requests 21,709 → 19,222 and
  **lookups −28%** for all three. The negative share alone has never been measured apart from the other two and
  would have to be, before this is priced honestly.

**Recommendation, revised now the hole is known to be narrow: bound the negative-entry lifetime, keep
everything else.** R4 says correctness is never weighed against performance, and a name that is permanently
invisible is a correctness hole, so "keep `Forever` and accept it" does not survive the rule — the earlier
recommendation in this document was wrong on that point too. But the fix no longer costs the 28%: positive
entries, pages and listings all keep `Forever`, because the kernel honours their invalidation. Only the
negative-entry lifetime needs to stop being unbounded, and a *bounded* one keeps most of the repeat-miss
elision inside its window while making staleness finite on every kernel. The bound must be derived, not
chosen, and the negative share of the −28% must be measured before and after, which is the work this leaves.

Note what that does **not** do: it does not make these two tests green. They assert the `Invalidated` posture's
*immediate* visibility, and the probe runs at once, so any bound above about zero still answers from the cache.
If the posture for negative entries becomes "bounded", §4.6 says so and the tests assert the bound instead —
which is the honest shape, since the strong promise is one this kernel will not let slates keep.

The older reasoning, kept because it is the counter-argument: the upstream
history is churning this exact function — `fuse_reverse_inval_entry()` was recently converted to
`start_removing_noperm()` (`c9ba789dad15`) and has already needed one follow-up fix, which is what a regression
looks like rather than a deliberate ABI change. The cost of the alternative is permanent and paid everywhere,
against a hole on a subset of kernels. And the hole is now observable rather than silent, so a deployment on an
affected kernel can be told. If correctness-first is preferred regardless, the switch is one function —
`valid_for` in `crates/bridge-fuse/src/bridge.rs` — and the price above is what it costs.

Either way the two tests stay red on 6.19 and **should**: they assert a promise the kernel is refusing to
honour. Making them green by skipping on affected kernels would hide a real hole, so that is deliberately not
done.

Narrowing the kernel range below 6.13…6.19 needs a kernel between the two; the three hosts available are
6.12, 6.19 and 7.2, so it was not narrowed here.

## The fix is one branch, and §4.6 already mandates it

§4.6 does not leave this open. It says, in its own words:

> Infinite cache lifetimes require **proven** invalidation delivery and recovery for every mutation source; a
> notifier encoder alone cannot justify them. … A transport that cannot implement the requested coherence
> **refuses that guarantee**.

Delivery for a *negative* entry is no longer proven — it is disproven, on a shipping kernel. So granting one an
infinite lifetime is not a posture preference; it is already outside the rule the design states. R4 settles the
rest: correctness is never weighed against performance, so "keep it because the alternative costs lookups" is
not available.

**And a runtime degrade cannot rescue it, which is worth recording because it is the obvious idea.** The
machinery exists and looks perfect for this: `Attachments::set_coherence` is public, and
`VolumeBridge::cache_lifetime` already answers `CacheLifetime::Bounded { ns: 0 }` for
`CacheCoherence::Revalidated`, which `serve_lookup` turns into a plain uncached `ENOENT`. So the channel could
notice the first refusal — it now counts them — and drop this attachment to `Revalidated`. It does not work:
by the time a refusal is observed, the kernel is **already** holding that name as absent with an infinite
lifetime, and it will never ask again. Degrading changes what is granted next, and nothing recovers what was
granted before. The first stale name per mount would survive any adaptive scheme.

So the only fix that closes the hole is to not grant it in the first place:

- **Site:** `serve_lookup` in `crates/bridge-fuse/src/bridge.rs`, the branch
  `if matches!(looked, Err(VfsError::NotFound)) && valid != (0, 0)`. A miss stops being answered as a cached
  negative entry and goes back to a plain `ENOENT`, which is what the code did before `c3c0024b`.
- **No constant is needed.** Zero is not a tuning value; the existing `valid != (0, 0)` guard already routes it.
- **Scope:** negative entries only. Positive entries, pages and listings keep `Forever`, because the kernel
  honours *their* invalidation — measured above.
- **Cost:** the negative-entry share of `c3c0024b`'s "negative entries, kept pages and listings" (together:
  requests 21,709 → 19,222, lookups −28% on a pip install). That share has never been measured on its own and
  must be, before and after, on a Linux host.
- **Also required in the same change (CLAUDE.md):** §4.6's cache-posture paragraph and its status blockquote,
  the amendment log, and `c3c0024b`'s claim in `BENCHMARKS.md`, since this partly reverses it.

That is a change to a shipped caching promise on **every** platform, reversing a measured optimization, so it
is written up here rather than taken unilaterally. The analysis above is complete enough to act on in one pass.

## Reproduction

```
# red
SLATES_TEST_CLI=1 cargo test -p slates-bridge-fuse --test coherence_mount -- --test-threads=1 --nocapture
# green, same code, kernel 6.12, x86_64 userspace under Docker's linuxkit VM
docker run --rm --platform linux/amd64 --device /dev/fuse --cap-add SYS_ADMIN \
  --security-opt apparmor=unconfined -e SLATES_TEST_CLI=1 -v "$PWD":/w:ro -w /w rust:1.98.1 \
  bash -c 'apt-get -qq install -y fuse3 && cargo test -p slates-bridge-fuse --test coherence_mount'
```
