# A guest's memory was reported as its page cache, and its residency was unmeasured (AUD-29-77)

**Date:** 2026-10-01. **Audit:** AUD-29-77 ("Extend measured residency and sink boundary across the chosen
runtime/VMM… Do not infer protected guest RAM from 'DAX disabled', tmpfs, shared memory or a report enum.
Distinguish a protected export from a protected whole workload").

## Description

The guest transports' residency said the bytes beyond what slates protects were the `guest_page_cache`. That is only
part of it. The device writes every reply into buffers in the guest's memory, and over vhost-user it maps that memory
into the daemon. That memory is the VMM's: its owner decides whether it is locked, swapped, dumped or snapshotted.
Nothing had measured what the device's mapping holds or whether slates locks any of it.

## Root cause

The report's wording predated a real VMM binding. Before AUD-29-68's vhost-user leg there was no real guest memory to
measure.

## Impact

The report understated what lies beyond slates' protection: the whole of the guest's memory, not just its cache.
Nothing was exposed that the guest did not already hold.

## Exact edits

- `crates/mcp/src/lib.rs` `beyond_protection`: a guest's residency now names `guest_memory`, the VMM's memory, which
  holds its page cache and the device's reply buffers. The doc states the measurement.
- `crates/server/tests/virtiofs.rs`: the live guest's run samples this process's `/proc/self/smaps` every poll. The
  daemon runs in the test process, so the device's mapping is visible there. The run asserts the mapping was seen and
  that 0 KiB of it is locked, and prints its peak resident size.

## Measurement

Linux arm64 (Docker Desktop's VM on Apple Silicon), QEMU 10.0.13 under software emulation, a 256 MiB
`memory-backend-memfd` guest (Debian 6.12.111), 2026-10-01. The device's mapping of the guest's 262,144 KiB peaked at
580, 516 and 452 KiB resident over three runs (the rings and the buffers it touched), locked 0 KiB every time.

The command:

```
SLATES_GUEST_QEMU=… SLATES_GUEST_KERNEL=… SLATES_GUEST_INITRD=… \
  cargo test -p slates-server --test virtiofs a_linux_guest -- --nocapture
```

The image recipe is in `docs/bugs/2026-10-01-vhost-user-descriptors-closed-with-an-earlier-message.md`.

## What this does not claim

The VMM's own residency of the guest's memory (whether QEMU locks it, whether its host swaps it) is the VMM's
configuration and is reported as beyond protection, never as protected. A protected export is not a protected
workload.
