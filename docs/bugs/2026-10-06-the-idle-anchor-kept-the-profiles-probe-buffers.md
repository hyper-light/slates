# The idle anchor kept the profile's probe buffers: 272 MB of a 275 MB footprint

Date: 2026-10-06.
Area: `crates/machine/src/probes.rs` (the memcpy and hash probes).
Conditions: memory and battery on a laptop (goal: CPU, memory, battery).

## Description

An anchor with nothing mounted, measured with `/usr/bin/footprint`, had a footprint of 275 MB on this Mac (M5 Max,
Darwin 25.4). Its daemon had 12 MB. `vmmap` showed three freed (`empty`) `MALLOC_LARGE` regions, still dirty:
128 MiB, 128 MiB and 16 MiB.

## Root cause

The anchor measures the machine profile at boot. The memcpy probe copies up to 8 × the largest L2 (128 MiB here)
between two heap buffers, and the hash probe hashes a 16 MiB heap buffer. Both buffers are freed when the probe
returns. The macOS allocator keeps freed large allocations cached in the process rather than unmapping them, so their
pages stayed dirty and counted for the anchor's whole life. The anchor is a long-lived supervisor that never
allocates that much again.

## Edits

- The memcpy probe's source and destination, and the hash probe's buffer, are anonymous mappings (`probes::scratch`,
  memmap2), unmapped when dropped.
- A refused mapping measures nothing, and the hash probe reports the bytes it actually hashed.

## Tests

- Real-world measurement, the release anchor idle (`--quick --shards 1`): footprint 275 MB before, 2.3 MB after; no
  `MALLOC_LARGE` region left.
- The profile is unchanged within noise: memcpy at 64 KiB, 1 MiB, 16 MiB and 128 MiB over three runs per build,
  and BLAKE3 1,337 against 1,278 MB/s.
- `slates-machine`'s tests pass.

## Siblings

- The codec probe's corpus and outputs are heap allocations of up to 16 MiB. They left no `MALLOC_LARGE` region in
  this measurement, so they were left as they are.
