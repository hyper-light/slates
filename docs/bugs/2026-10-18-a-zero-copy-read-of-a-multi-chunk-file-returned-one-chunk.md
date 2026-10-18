# A zero-copy read of a multi-chunk file returned one chunk, silently

- **Date:** 2026-10-18
- **Area:** `crates/server/src/merge_service.rs` (`try_local_map`, A-135), `crates/client/src/client.rs`
  (`Client::map_read` → `MappedRead`)
- **Status:** found by benchmarking; **fixed in this change**, red-checked on the path where it lives
- **Class:** silent truncated read across two paths of one API (CWE-129 adjacent; a short read with no
  error)

## Description

`Client::map_read` has two paths and one contract — `MappedRead::bytes()` is "the file's bytes". The
copy path reads the file in pages and returns all of it. The zero-copy path maps **the content block at
offset 0** and answers as though that block were the file. For any file larger than one content chunk
the caller therefore gets the first chunk and **no indication that the rest exists**.

Every A-135 test used a 20 KiB file — the first of them says so in its own comment, "one chunk, so one
block" — so the suite never asked the question.

## The measurement

Found while benchmarking what the confined-descriptor rule costs
(`crates/client/examples/map_read_ab.rs`, x86_64 Linux, the rule lifted so the zero-copy path is
reachable):

| file written | `map_read` returned | equal |
|---|---|---|
| 64 KiB | 65,536 | **true** |
| 256 KiB | **65,536** | false |
| 1 MiB | **65,536** | false |
| 4 MiB | **65,536** | false |

Exactly one 64 KiB chunk, whatever the file. The copy path returned every byte of the same files.

## The fix

`try_local_map` now asks for the file's size before it maps, and takes the existing copy fallback when
the block it would map is smaller than the file. The caller always gets every byte; the zero-copy
upgrade is taken only where it is the whole answer. Mapping all of a file's chunks is the owed
extension — the device already does an N-map for a multi-chunk guest window — and is not this.

## Verification, on the path where the bug lives

The bug is currently unreachable in production because the confinement rule
(`docs/bugs/2026-10-18-a-read-handed-the-peer-the-whole-content-arena.md`) forces the copy path. So the
fix was verified with that rule lifted, in three arms on x86_64 Linux:

```
ARM 1  zero-copy reachable, the fallback in place     1 passed
ARM 2  zero-copy reachable, the fallback removed      FAILED:
       "256 KiB: a MapRead returned 65536 of 262144 bytes (path zero-copy)"
ARM 3  as shipped                                     1 passed
```

Arm 2 is the red-check: the new test
(`crates/client/tests/client.rs` `a_map_read_returns_the_whole_file_at_every_size`, sizes 4 KiB to
1 MiB) is a live guard, not a hopeful one.

## What the benchmark was for, and what it found instead

The bench exists to price the confinement rule, and it does (`docs/wip/BENCHMARKS.md`): on a
single-chunk read the zero-copy path is about **10–15× faster at p50 and ~23× at p99**. It found this
bug on the way, which is the better result — and the reason the bench now reports latency only. A
MiB/s figure computed from the *requested* size is a lie on exactly the path that returns less than
it, and it was quietly reporting 676,704 MiB/s for a 4 MiB file it had read 64 KiB of.
