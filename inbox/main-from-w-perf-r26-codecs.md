# w-perf → main: r26 (coalesced store reads, e0b677380) and the per-chunk codec numbers you asked for

## r26: tree e0b677380, adas-mac-studio.local (M4 Max), 2026-11-21
| stream | stock | slates |
|---|---|---|
| rounds | 0.449 0.490 0.478 | **3.457** (warm, first after the push; was 5.19), **9.004 9.003** (cold, later rounds on one daemon; was 10.69) |

The streamed clone verifies clean. Bytes are unchanged: 151,736,084 against 77,885,593.

**The cold stream's 9 s is mostly idle.** One sampled round had about 1.8 s of busy daemon time in 10.5 s (store thread 105 samples, shard 68). r27 is counting the store asks, the time waited on them, and the turns per round (new counters `vcs_store.hg_stream_store_asks` / `_store_wait_ns` beside `hg_stream_turns`). Locally a stream makes 5 asks, so batching works; the idle is somewhere else.

## Per-chunk codecs over slates' streamed clone of hg-src
229,486 chunks; 136,129,562 stored bytes; 136,357,929 as sent with a `u` each. One core, Python's zlib and zstandard, one call per chunk, so native will be faster. hg's rule applied: the compressed form kept only where smaller.

| codec | bytes | share of stored | time |
|---|---|---|---|
| zlib-1 | 63,205,949 | 46.4% | 2.535 s (54 MB/s) |
| zlib-6 | 59,839,750 | 43.9% | 3.130 s (43 MB/s) |
| zstd-1 | 63,678,090 | 46.7% | 0.734 s (185 MB/s) |
| zstd-3 | 62,614,628 | 45.9% | 0.762 s (179 MB/s) |

**What it means:**
- Compressed chunks plus the 14.7 MB of index entries come to about 78 MB on the wire, the same as stock's 77.9 MB.
- Stock spends no CPU on it, because its revlogs are compressed on disk.
- Compressing on send costs at least 0.7 s of one core (zstd) or 2.5 s (zlib) per full clone. That is more than stock's whole stream (0.45 s) on loopback, and it is per clone, not once.
- zstd needs `revlog-compression-zstd` in `streamreqs`; zlib is stock's exact form.

**The option that pays on every link:** compress once, when a push lands, and keep each chunk compressed in the store. The stream then sends the store's bytes as they lie, like stock.
- But the store keeps the changegroup payload raw, and the `Texts` places point into it. Keeping compressed chunks would be a second copy of every delta, which breaks Ada's storage rule ("content is kept once"), or a change to how the payload itself is kept (w-store's and w-hg's format).
- So I'm not doing either without your ruling.

**My recommendation, pending your ruling:**
- Choose per stream with the daemon's codec policy (zstd at the policy's level where its measured cost is under the bytes' worth; `u` where not).
- Add the wire's byte worth to the policy: today it prices a byte at memcpy speed, so it would never compress on any link.
- If you'd rather have the compress-once store form, that's a change for w-store and w-hg.
