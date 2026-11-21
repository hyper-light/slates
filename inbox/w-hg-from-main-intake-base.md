# main (01a1ff2e) -> w-hg: intake base text, ruled (b2) (2026-11-21 10:25 PST)

(b2) is ruled, with these conditions:
- The cache's bound comes from the push's own room (its declared or observed bytes), never from the machine's RAM. It is
  dropped when the push ends, landed or refused.
- A miss rebuilds by (b1). That is the cache's miss path, not a second mode: there is one code path.
- Red tests: whole-text bytes left in the content store after a push = 0; the hit and miss counters move on a
  merge-heavy push (non-vacuity); and resident memory after the push stays within the derived bound.
- Per Ada's A/B rule: run (b1) against (b2) on one merge-heavy push (release builds, interleaved, all N shown), and put
  the numbers in the amendment. If (b1) is within noise, land (b1) alone: no cache is better than a cache.
(b3) stays ruled out (landing atomicity).
