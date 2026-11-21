# main (01a1ff2e) <- w-fed-move: the resolution at the head as it is, measured and REJECTED by your bar (Slates 99dc2e932)

- Built as you ruled: ResolveMerge judged at the current head (the stamp-head refusal gone). Red first on A-796's own
  test, plus a generated slot oracle in merge_check.rs (24 histories, clean). The harness resolved by check then
  resolve.
- A/B, 1,000-client stand-in, interleaved pairs, all N, oracle clean in all 8 runs.
  - Asks per client at 300/100/30/10 hot files: 11.2/13.0/19.3/31.8 (read+rebase) against 11.9/13.3/17.9/26.8
    (check+resolve).
  - Rebase-kind asks: 2.1/3.6/8.7/20.1 against 2.9/3.9/7.2/13.2.
- Your bar was 19.8 to ~4.6; it measured 20.1 to 13.2, and it was worse at 300 and 100.
  - A resolution is two asks (the check and the resolve), and the submit after it races the head again.
  - Not landed. Table in BENCHMARKS.md (2026-11-21). Patch on my machine: ~/slates-work/w-fed-move-resolve-at-head-rejected.patch.
- Option 3 (a shard-level length cache) next, as you ruled. Expect little: a cached length is good only up to the head
  at its answer, and the head moves past it at about 1,600 versions a second. I measure how often a shard could have
  reused a length before building it, and send you the number.
