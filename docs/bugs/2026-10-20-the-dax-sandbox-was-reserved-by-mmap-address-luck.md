# The DAX sandbox was reserved, or not, by where `mmap` happened to put the object

> Status: fixed 2026-10-20 (`crates/mem/src/shared.rs`, `crates/mem/src/region.rs`). Found by asking what
> §4.6's L1 sandbox does on a host whose shmem huge pages are off — the kernel's default, and so most hosts.

## What happened

§4.6's L1 sandbox gives a tenant a physical huge-page row of its own; L2's moving target and B2's integrity
guard are built on it. In the daemon the content arena is an **anchor-backed shared memory object**
(`crates/server/src/content_pool.rs:261`, `Region::shared`), which is the configuration DAX actually runs in.

In that configuration the sandbox was reserved on some runs and refused on others, on one unchanged host,
with no input differing between them. Measured 2026-10-20, Linux 6.12.76 (aarch64, Docker Desktop VM on an
Apple Silicon Mac), `shmem_enabled` = `always within_size advise [never] deny force`:

- Eight volumes, each over its own shared object, asked for a first shift. The answer was **0 of 8 or 8 of 8**,
  varying run to run: five consecutive runs of the whole-suite case gave refuse, reserve, refuse, refuse,
  reserve.
- Two further DAX tests skipped on this host for the same reason (3 loud skips in the suite, now 1).

Nothing ever claimed an isolation it did not have: the refusal is the typed `VfsError::NoPhysicalIsolation`,
and on the runs that succeeded the backing was measured and real. The defect is availability, and because what
was unavailable is a Rowhammer defence, a daemon that believed it had L1/L2/B2 mostly did not.

## Root cause

A huge page behind a memory object is addressed twice over — by where it sits in this process and by where it
sits in the object — and the kernel forms one only where the two agree: `address ≡ offset` modulo the huge
page. The code required only the first of those, and asked for neither.

- `DaxZone::reserve` takes its span from the buddy allocator, which hands a huge-sized allocation out at a
  huge-aligned **offset inside its region** — so `base` was always a multiple of 2 MiB.
- The span's address is `region.base() + base`, and `Region::shared` mapped the object wherever `mmap` chose.
  On Linux that is a 64 KiB boundary: **13 of 16 objects began off a huge boundary**, remainders 128 KiB to
  1.7 MiB. `verify_contiguous` then refused at its alignment check, before measuring anything.
- On the ~1-in-32 runs where `mmap` landed on a huge boundary, address and offset agreed and the sandbox was
  reserved. That is the whole of the difference between the two outcomes.

Three measurements pin it, each taken through the product's own primitives:

| Span | `MADV_HUGEPAGE` | `MADV_COLLAPSE` | PMD-backed |
|---|---|---|---|
| Congruent (mapping on a huge boundary) | 0 bytes | returns true | **2 MiB** |
| Address aligned, offset not | 0 bytes | refused `EINVAL` | 0 |
| Offset aligned, address not | 0 bytes | refused `EINVAL` | 0 |

The first row is the one that matters beyond this bug: on a host with shmem huge pages switched **off**, an
explicit `MADV_COLLAPSE` over a congruent span still delivers a full huge page, because it is a request rather
than a preference. So the sandbox is obtainable on a stock Linux host — it was only ever the placement that
denied it.

A first attempt that aligned the *address* by cutting the span out of a double-sized allocation was measured
and rejected: it left the file offset incongruent, `MADV_COLLAPSE` returned `EINVAL`, and the sandbox stayed
refused. The alignment has to be the mapping's.

## The fix

`crates/mem/src/shared.rs` — a shared object's mapping is **placed** rather than left to `mmap`. A new `Mapped`
owns a `PROT_NONE` reservation one huge page longer than the window, maps the object over the congruent
address inside it with `MAP_FIXED`, and gives the reservation back whole at drop. The slack costs address space
and no memory. memmap2 offers no placement, so the mapper is rustix; the address is kept exposed rather than as
a pointer, exactly as the Windows view already is, so the object stays `Send` without an unsafe impl. Where the
host has no huge page the grain is its page, which every mapping meets already — the same arithmetic, asking
for nothing, so there is no second path.

`crates/mem/src/region.rs` — locking now speaks in bytes (`lock_bytes_whole`, `lock_bytes_range`), one
implementation that the memmap2 wrapper calls too, since a mapping is its bytes whoever made it.

`unsafe-budget.toml` — slates-mem 37 → 45, every new site named with its reason.

## Proof

- `crates/vfs/tests/dax.rs::a_sandbox_in_a_shared_object_is_decided_by_the_host_not_by_where_mmap_put_the_object`
  — several independent stores over their own shared objects must answer alike. Before: 0 of 8 or 8 of 8 by
  run. After: **8 of 8, six consecutive runs**.
- `crates/mem/tests/shared_region.rs::an_aligned_span_of_a_shared_object_collapses_to_a_huge_page_whatever_the_shmem_policy_says`
  — the premise, asserted on a host where the sibling proof skips. It searches for a congruent mapping rather
  than waiting on luck, so it measures rather than hopes.
- Suites green after: slates-mem (macOS and Linux aarch64), slates-vfs (73 DAX tests, both), DAX loud skips on
  Linux 3 → 1.

## The 32-bit bound, measured rather than reasoned

Bounding the placement's grain — the huge page only where the object can hold one, the page otherwise — was
argued from i686's roughly 3 GB of address space and a shared object per client session: a huge page of slack
per session would spend an address space that small. That was reasoning, not evidence.

CI's `i686 (32-bit narrowing, by test)` lane runs `cargo test -p slates-rt -p slates-mem` under
`--platform linux/386`, so the full `slates-mem` suite executes as 32-bit code. On run 39518402693 all four of
the placement's tests **ran and passed there** — `a_placed_mapping_costs_a_page_of_slack_and_returns_every_byte_of_it`
and its quiet-child half, the congruent-mapping proof, and the hostile-length refusal. The slack bound the
argument was about is therefore held on the architecture the argument was about.
