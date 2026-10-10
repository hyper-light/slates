# A region's first publication scanned every granule on a verb's path

**Found:** 2026-10-10, chasing the landing-fairness lane's longest shard step (126–185 ms per run at load 27–29, in
both arms of A-130's A/B). **Status: fixed** the same day. A second, separate long step remains open (GAPS, the same
day).

## Symptom

- In each run of `landing_fairness a_large_landing` (debug build, load about 27), one probe waited 115–187 ms behind a
  single shard step.
- The landing was not the cause. Timed in a scratch build: its begin took 5.1 ms, its finish 16.5 ms, and its slices at
  most 4.3 ms.

## Root cause

- **The profile.** `sample` of the test process put 131 of the shard thread's 600 samples in `verbs::dispatch` →
  `publish_shard` → `ChunkArena::capture` → `Buddy::capture`, iterating a byte slice. That run's longest step was
  178 ms.
- **The deferral.** A-64's allocator keeps no live-block bitmap until its first capture (`imaged`, `d256cde`), so an
  allocator nothing publishes from pays one flag test. The first capture then reads every state byte of the region,
  one per granule, to find the live heads. Later captures are incremental.
- **The size.** A scratch print in that branch showed one first capture per run: a region of 8,388,608 granules (a
  32 GiB pool extent of 4 KiB granules), 64.6 ms in that debug run.
- **Why on a verb's path:** the first mutating verb after a region is added publishes, and its shard step pays the
  scan.

## Impact

- **Release:** 2.9–3.4 ms per 32 GiB region, measured with a scratch example (`Buddy::new(4096, 23)` with 64 blocks
  allocated, first capture timed, three rounds). It is paid once per region per daemon life, by whichever client's
  verb publishes first after the region is added: a p99 outlier that scales with the region.
- **Debug builds** (every test lane): 64–131 ms. That is a heartbeat-scale stall, inside a shard step.
- **No correctness impact:** the scan finds the right heads. Only its time and place were wrong.

## Fix

- **`Buddy::new_imaged`.** A fresh buddy starts imaged with an empty bitmap, which is exactly what a first capture's
  scan would find, since a fresh region holds no block. Every allocation and claim keeps the bitmap, as it already did
  after a first capture.
- **`ChunkArena::published`.** A builder (like `discarding_from`) under which every region the arena adds uses
  `new_imaged`. The daemon's shard arena (`daemon::shard_arena`) is built published, before recovery adds its held
  regions and claims the blocks its image names.
- **Untouched:** an allocator nothing publishes from keeps the deferral and its one flag test. The instruction-count
  gate's `buddy_alloc_free_split` builds a plain buddy.
- **Counters:** `Buddy::scanned_granules` and `ChunkArena::scanned_granules` count what first captures read: zero for a
  published arena. This is the non-vacuity check.

## Tests

- `an_allocator_started_imaged_keeps_every_rule_without_scanning` runs `no_block_an_image_names_is_ever_handed_out_again`'s
  20,000-step random history against the deferral model over a buddy started imaged. Every rule holds at every step, and
  it scans nothing. The original history now also asserts its scan happened.
- `a_published_arena_names_what_a_scanning_one_names_and_scans_nothing` compares the two arenas: the same blocks named
  at every capture, including a fresh region's claimed block as recovery adds it. The unpublished arena scanned each
  region once (48 granules); the published one scanned none.
- **Under Miri:** both, and the three existing capture tests.
- **By use:**
  - server recovery 39/39, daemon 22/22, seal 2/2;
  - client 10/10, anchor 15 (6 ignored, as before), vfs 256;
  - the held-connection restart flows (`SLATES_TEST_CLI=1 --test nfs_held`): 3/3, with every committed file
    surviving daemon kills byte for byte.
- **The daemon itself:** with the scratch print restored for one run, no first-capture scan ran.

## Sibling sweep

- **Other arenas that publish:** only the shard arena's captures are recovery publications (`publish_shard`). The merge
  service's and the cluster content arenas are never captured, so they keep the deferral.
- **Other O(region) work on a verb's path:**
  - `Buddy::new` allocates the region's per-granule arrays: a state byte, two `u32` links, a `u64` incarnation and
    five bitmap bits, about 17.6 bytes a granule. That is about 148 MB (141 MiB) for a 32 GiB extent, built when the
    pool hands out the extent. It is memory, not a per-verb scan, and is noted here for the memory work.
  - The idle purge is already sliced (A-105).
