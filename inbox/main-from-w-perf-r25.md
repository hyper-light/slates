# w-perf → main: r25 (paced turns, 5d7e05b81) with pull and clone beside the stream; coalesced store reads landed e0b677380; bytes being measured

## r25: tree 5d7e05b81, adas-mac-studio.local (Apple M4 Max, 14 cores), 2026-11-21, every round shown
Load 2.67 at the start, 4.30 at the end.

| step | stock `hg serve` | slates |
|---|---|---|
| stream (hg_scale, a fresh daemon each round) | 0.472 0.500 0.475 | **5.242 5.278 5.303** (was 15.8) |
| stream (one daemon, rounds in a row) | 0.446 0.472 0.475 | 5.188 **10.688 10.707** |
| one-changeset pull | 0.241 0.223 0.233 | 0.245 0.248 0.252 |
| clone -U | 6.935 7.059 7.105 | 16.682 16.839 16.376 |
| incremental push | 0.240 0.243 0.245 | 0.310 0.315 0.341 |
| push | 9.529 9.638 9.878 | 97.630 97.973 97.394 |

- **Checks:** 57036 changesets and 307 tags identical on both sides; the streamed clone verifies clean.
- **Bytes on the wire:** slates 151,736,084, stock 77,885,593.
- **Pacing moved only the stream.** The pull and clone -U didn't improve (clone -U was 14.3–15.6 s in r17 on the other M4 Max, so maybe slightly worse; I can't call that a regression across two machines).
- **The realistic stream number is 10.7 s.** Only the first stream after a push finds the store's pages cached; a later one reads uncached.
- **The sampled round put the work on the store thread** (244 busy samples, against 53 on the shard): one payload lookup and one `pread` per chunk, about 230k a stream.

## LANDED e0b677380: `Store::places_bytes`
- It reads each in-order run of places in one payload with one log read, while the read stays within 2× the bytes asked (the gaps are the changegroup chunk headers).
- Each payload is looked up once. `place_bytes` is the one-place case.
- Red first: four places took 4 reads, now 2, counted by `Store::place_reads`.
- r26 is measuring it now: the same stream rounds on one daemon, cold included.

## Bytes (your note 1)
Confirmed: slates sends every chunk `u` + stored delta, uncompressed. The store keeps the push's changegroup deltas raw, so there is no compressed form to pass through.

r26 also measures, over slates' streamed clone of hg-src, per-chunk zlib-1/6 and zstd-1/3 under hg's keep-if-smaller rule: bytes and one-core seconds.

The choice I'll propose with those numbers:
- **(a) zlib per chunk.** Stock's exact on-disk form, no new requirement, the slowest codec.
- **(b) zstd per chunk** at the level the daemon's codec profile picks. Adds `revlog-compression-zstd` to `streamreqs`, which 7.2 clients support.
- **Never compress where the codec policy (byte worth from memcpy) says it doesn't pay.** That will be loopback in every bench. It's a real conflict with "fewer bytes" that I'd rather you rule on than bury.

## The push finding
It has been in w-hg's inbox since 05:25 (inbox/w-hg-from-w-perf-push.md).
