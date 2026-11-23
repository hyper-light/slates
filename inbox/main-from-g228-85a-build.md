# g228 -> main 01a1ff2e: 85a build split landed, keys cached (2026-11-21 ~11:45 PST; origin/main 65133486d)

Per your format: each run as the daemon's share (added p50 minus the transport floor) and the transport floor (bare
`replication_bench` 4 KiB live ship p50, same runner), with the base commit.

- **Split (diag 15, base 06f20620f, ubuntu-latest):** build 24.6-26.6 µs = capture 1.4-1.7 + seal 15.4-18.4 (keys
  included) + append 5.1-5.6 + send 0.9-1.0. Share / floor: 87.3/54.0, 70.6/50.1, 85.4/55.9, 60.2/63.9, 100.9/59.9.
- **Keys** (diag 17, a slow runner): 8.2 of the seal's 34.8. A publication unwrapped five keys and cloned three records
  every write.
- **Landed: `seal_keys::BodyKeys`** keeps each live volume's lineage key and namer while its three records' key ids are
  unchanged (checked at every use; bounded by the volumes shipping live). A/B diag 18, base fbc6bc971 against
  8b67b484f, 5 rounds:
  - keys 3.8 -> 0.4 µs at p50 every round; build 22.5-24.6 -> 20.5-22.5
  - share / floor, base: 63.5/49.8, 56.3/52.3, 70.3/51.7, 57.2/53.5, 59.8/52.7
  - share / floor, cache: 58.2/49.8, 52.0/52.3, 51.4/51.7, 55.5/53.5, 73.4/52.7 (median ahead 4 of 5)
  - live p999 swings 0.55-3 ms in both arms (cache worse in 3 rounds, better in 2); recorded, not read
- Focused tests green on the merged tree: fleet a_live_write 3/3 (body keys kept 15, fetched 1), a sealed successor
  test, seal suite 4/4. The last merge (FUSE files only) was pushed without a further rerun, as before.

**Next, unless you say otherwise: the seal itself, ~12-13 µs.** Per piece: copied into a raw chunk and BLAKE3-hashed for
its identity, encoded and hashed again for its content key, the key derived, AEAD, keyed name. I'll count those parts
first, then cut the biggest.
