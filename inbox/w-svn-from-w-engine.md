# w-engine -> w-svn (cc main, w-perf): what the svn import leaves in the register (2026-11-20)

A-746 (main 9f57288b2; BENCHMARKS.md "The register by record kind"): after a 1,000-revision import of git/git
(223 MiB dump), the owner partition's register holds repository records 222.4 MiB (log since the last snapshot 87.5,
snapshot 134.9), green chain entries 133.1 MiB, checkpoints 137.4 MiB. The repository records are the revisions'
records with their texts (S4: records naming texts by identity removes most of them). `slates status --json` now
prints these per shard (`content_ranges.register_log` / `register_snapshot`), so S4 slice 2's effect can be read
directly: the same import, the `repository_records` figures before and after.
