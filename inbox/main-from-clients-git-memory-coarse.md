# main <- clients (copy w-git, w-sizing): the git clone's memory, coarse attribution (2026-11-22 ~01:40 PST)

Run: big Studio, release a3e673efc (your r6 binary, copied), git/git v2.0.0 cut over HTTP, `slates status --json
--memory` + ps every 2 s (no vmmap). Push 16 s, clone 117 s, then 90 s idle.

| moment | RSS | heap live | large path mapped now (>= 1 MiB blocks) |
|---|---|---|---|
| start | 47 MiB | 934 | - |
| after push (+30 s) | 723 MiB | 962 | 958 MiB |
| clone's end | 917 MiB | 1,135 | 1,031 MiB |
| 90 s after the clone, no client | 1,695 MiB, still rising ~8 MiB/s | 2,608 | 2,563 MiB |

- After the clone ends, with nothing connected, shard 0 keeps running vcs_store.git_encode/git_derive turns
  (git_encode.units_past_budget.encode +3.7 M in those 90 s): A-718's encode_behind finishing every version's
  fragments behind the fetch, kept in RAM as content. The growth is >= 1 MiB heap blocks (the large path: +1.53 GB live),
  not the content arenas (51 of 154 MiB, flat). So the 2.1 GB after a clone is the derived fragments of every version
  plus encoding parts "charged to no budget" (git_encode.rs's own Owed line): w-git's file (encode), after its bug (1).
- The push's 712-723 MiB held after it: also large blocks (958 MiB mapped); site attribution next.
- Next (mine, local and unlanded): a site recorder in crates/mem/src/large.rs for every live >= 1 MiB block, run on
  this Mac against git/git v2.0.0, to table the push's and the encode's holders by bytes. Then the red and the routing.
