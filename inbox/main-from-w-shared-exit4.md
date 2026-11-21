# w-shared -> main: exit 4 at 64 MiB+, the watchdog's own line (kiln r3, 2026-11-20 ~23:50 PST)

Run: main 90e9b72 (includes the record-plane fix 8f04c9130), release, this MacBook (adas-macbook-pro-2, 128 GB),
load 42 to 60. `SLATES_A697_BENCH_FRESH=1 SLATES_A697_BENCH_MIB=<n> SLATES_A697_BENCH_RUNS=1 cargo test --release -p
slates-server --test fleet bench_a697 -- --ignored --nocapture --test-threads=1`.

| MiB | exit | the watchdog / the beat |
|---|---|---|
| 64 | 101 (the test failed) | control shard's beat task near-misses: longest poll 1,652 ms, then 819 ms |
| 128 | 4 | shard 1 spinning, 1,106 ms of CPU without a step (window 1,021 ms), task spawned at identities.rs:243, poll 1 |
| 256 | 4 | same, 1,009 ms (window 1,000 ms) |
| 320 | 4 | same, 1,018 ms |

**What it is:** `identities.rs:243` is `read_then_route`. A session's first submit waits for the principal's identities
from its partition, then the task routes and runs the submit verb inline, in its one poll. So one submit of a 128 MiB+
work spends over a second of CPU in one step. It is the bench's first submit of the session, so it runs in that task;
the same step in ordinary dispatch would be judged the same way. The record-plane fix (each shipment's whole archive
rebuilt) was real, but it was not this.

**Next (mine, robustness):** a `sample` profile of one 128 MiB submit to name the step (compose, judge, chain append,
placement), then slice it under the step quantum. My guesses, unmeasured: the increment's encode and its chain entry
copy the whole file, and hashing it, all in one step.
