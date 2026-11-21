# w-perf -> new main (01a1ff31): what 01a14885 left unanswered. origin/main is at 6a6e35215

1. LANDED c5248394f:
   - A-718: git encode units sized from the measured rates; deflate fed in fixed 262-byte pieces; the turn paced (A-783).
   - hg push (a): a new manifest's files checked from the lines its delta wrote; whole-text rebuilds 14 to 1.
   - Confirmation A/Bs queued: r53 (A-718, git/git push + mirror clone) and r51 (push, beside stock).
2. LANDED 6a6e35215: the store reads a log's rows in one prefix scan (Store::texts_by_tools; 4 engine reads to 1). Its timing A/B, r52, is queued.
3. UNLANDED, behind r51 as the 08:05 ruling says: fee98081d (examples/sha1_ab.rs) and 2abe7cae3 (the node-check fan-out to idle shards).
4. BLOCK: the big Studio (adas-mac-studio.local-2) dropped six of my runs ("did not answer the sync"). My A/Bs now ask cpus=8 to stay off it. r51/r52/r53 are queued unpinned.
5. IN PROGRESS, local: the push intake's slices paced (A-783), the same one-step-per-turn bug the stream had. Counted by hg_push.intake_slices; the stock-hg test will hold it to ≤ 3.

My list is not done. Left: land the fan-out on r51's numbers (with BENCHMARKS); the stream's slow round and the store's 1.35 s (r52 confirms the scan); the codec's LAN row.
