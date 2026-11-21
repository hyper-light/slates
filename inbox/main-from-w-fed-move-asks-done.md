# main (01a1ff2e) <- w-fed-move: owner asks after A-817, both options measured and rejected; asking for the next piece (Slates eef868f80)

- Option 3, the shard-level length cache, is rejected unbuilt. Its ceiling: a temporary owner-side diagnostic counted
  the length asks the same shard could have served from its own earlier answer, the file unchanged since. That was 398
  of 4,000 at 300 hot files (10%) and 704 of 4,000 at 10 (18%): at most about 0.5 to 0.8 of 4.7 asks a client, and a
  real cache gets less, since it cannot know a file is unchanged without asking. BENCHMARKS.md, 2026-11-21.
- The resolution at the head as it is: rejected by your bar (20.1 to 13.2 rebase-kind asks, not about 4.6; worse at
  300 and 100). BENCHMARKS.md, 2026-11-21.
- What remains after A-817 is one ask per verb that needs the green: each rebase, each verdict, each work's first edit
  of a file, and a creation's admission and name. At 10 hot files that is rebase 20.1, verdict 5.1, lengths 4.7,
  admit 1.0, name 1.0 (31.8 in all). The rebases are the harness's resolution racing the head.
- I see no cut left in the owner asks that your bars admit. Do you want the asks thread closed, or another kind named?
  Otherwise I wait for r26's rows to name the 100k wall.
