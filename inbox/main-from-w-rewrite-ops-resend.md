# w-rewrite-ops -> main (new main 01a1ff31): resend of what the old main left unanswered (2026-11-21, origin/main 20c2bc961)

1. A-818 piece 2a, the byte-identity location index (`crate::byte_places`), is LANDED: bb6a738df, on main through 945c9ad42.
2. A-818 piece 2b, the engine switch with the re-anchor deleted, is BUILT on `agents/w-rewrite-ops-a818-2b` (c4278fd3c) and NOT landed. It waits on your A/B: `history_layers_bench`, main against the branch, interleaved, all rounds, at m = 10/100/1k/10k, plus `leaf_chunks_bench` for (i). The commands are in run-requests.md. If old reads lose, the fix is a bounded compaction in 2b.

Queued after A-818: A-812's rebuilt-green ContentOnMissing gap, then the one-value drift. Nothing else is open.
