# The sandbox asked the mapping, not the span, and refused rows that were backed

> Status: fixed 2026-10-20 (`crates/vfs/src/daxzone.rs`). Found immediately after
> `2026-10-20-the-dax-sandbox-was-reserved-by-mmap-address-luck.md`: with the placement right, the DAX suite
> still skipped tests non-deterministically, which is how a gate tells you it is not one.

## What happened

With the mapping placed congruently, §4.6's L1 sandbox was reserved reliably over a *fresh* shared object —
but the DAX suite's own loud skips stayed random: **3, 1, 0, 1, 2 across five identical runs**, naming
different tests each time. A test that opts out on a coin flip is not a gate, and the same refusal reaching
the daemon means L1, and L2 and B2 above it, are absent whenever it fires.

## Root cause

`verify_contiguous` asked whether a span is one physical huge page by reading the **mapping's** PMD-mapped
bytes from `/proc/self/smaps` before and after faulting the span in, and requiring the difference to be at
least a huge page. The span is not the mapping. Measured on Linux 6.12.76 (aarch64) with the suite's 73 tests
in parallel, a probe at every refusal point in the reservation path:

| Refusal | Per run | What it was |
|---|---|---|
| `Q_NOMEASURE` → `Q_CACHED` | 9–11, repeated 16–17 | **No reading at all.** The kernel merges a mapping with an identical neighbour; the parser matches an entry beginning *exactly* at the region's base; after a merge none does. `None` → refused → cached as refused. |
| `R_FINAL` | 1–4 | **A difference that means nothing.** Every one of these had `MADV_COLLAPSE` reporting **success** for the span — one while the mapping's total *fell* from 14 MiB to 4 MiB as its other pages were broken up. |
| alignment, allocation, ceiling, slots | 0 | not involved |

Both failures are the same mistake: a whole-mapping total cannot answer a question about one span inside it,
because the kernel reshapes the rest of the mapping, and merges the mapping itself, for its own reasons.

## The fix

Ask the span's own oracle. `MADV_COLLAPSE` reports success only when every huge-page-aligned region of the
range it was given is PMD-mapped — **including one that already was** — so its verdict is about the span, and
it needs no `smaps` entry to exist. It now decides; the `smaps` reading is kept, no longer fatal when absent,
as the leg for a kernel before Linux 6.1 that has no `MADV_COLLAPSE` to ask.

The old comment's reasoning — "asked of the kernel, not of `madvise`'s return" — was right about
`MADV_HUGEPAGE`, which is a preference that succeeds while doing nothing, and was carried over to
`MADV_COLLAPSE`, which is a request that reports what it achieved. One sentence of the design, applied to the
wrong call.

## Proof

- DAX suite loud skips on Linux: **3, 1, 0, 1, 2 → 0, 0, 0, 0, 0, 0** across six runs. Every L1/L2/B2 test
  now executes.
- 73 DAX tests pass on Linux aarch64 and macOS; the VFS suite, the 41-test recovery arbiter and workspace
  clippy are green on both.

## Measured and rejected

Splitting the span into its own VMA so `smaps` could report it alone — advising the span with
`MADV_HUGEPAGE`. The mapping is advised whole when it is made, so a sub-range advice changes no VMA flag and
the kernel does not split: `huge_backed_bytes(span_address)` answered `None` (measured). Per-page attribution
through `/proc/self/pagemap` reads zero PFNs without `CAP_SYS_ADMIN`, which R10 refuses. An earlier reading
of the same numbers — that the kernel was genuinely declining under memory pressure, and that only giving
each sandbox its own mapping could fix it — was wrong; the collapse verdict disproved it.

## What makes the new oracle sound (and what would break it)

`MADV_COLLAPSE` reports on the huge-page-aligned regions its range **wholly covers**. A range covering none
of them has nothing to report and returns success having done nothing — measured 2026-10-20 on Linux 6.12.76
(aarch64): a span of one huge page starting one page past a boundary came back successful, with no huge page
anywhere near it. Written as a test expecting a refusal, that is a red test; it is recorded here instead,
because the behaviour is the kernel's and the lesson is about our use of it.

So the verdict means what this code takes it to mean **only for a span that is huge-aligned and huge-sized**,
which is what the alignment check immediately above it guarantees. Removing that check, or moving the collapse
before it, turns the oracle into a rubber stamp that certifies isolation nobody has. The check now says so at
its site.

## Verified on a second architecture and a second THP policy

Run on real x86_64 hardware (not emulation): Linux 7.2.9, 4 KiB base page, `shmem_enabled=`**`[advise]`** —
a different policy from the aarch64 host above, where it is `[never]`. The placement holds and the whole
8 MiB object came back huge-backed from the *advice* alone (`advice gave 8388608 bytes, collapse returned
true and left 8388608 of 8388608`), which is the permitted branch of
`an_object_that_can_hold_a_huge_page_is_mapped_where_one_forms_and_collapses_to_it` running for the first
time anywhere. DAX loud skips: **0 across five runs**, 73 tests pass; mem, VFS, the 41-test recovery arbiter
and the 87 device-seam tests are green. The checkout was a throwaway extracted from `git archive HEAD` and
removed afterwards, so the box's own working tree was never touched.
