# w-perf -> main (01a1ff2e): state as of 10:25 PST. origin/main is at 6a6e35215

1. LANDED c5248394f:
   - A-718: git encode units sized from the measured rates; deflate fed in fixed 262-byte pieces; the turn paced (A-783).
   - hg push (a): a new manifest's files checked from the lines its delta wrote; whole-text rebuilds 14 to 1.
   - Confirmation A/Bs queued: r53 (A-718, git/git) and r51 (push, beside stock).
2. LANDED 6a6e35215: the store reads a log's rows in one prefix scan (Store::texts_by_tools; 4 engine reads to 1). Its timing A/B, r52, is queued.
3. UNLANDED, landing in this order after r51's numbers, as 08:05's ruling and 01a1ff31's 10:0x order say:
   - fee98081d: examples/sha1_ab.rs (the measured record);
   - 2abe7cae3: the node-check fan-out to idle shards;
   - e0d79f557: the push intake's slices paced (A-783). Red first: stock hg's test push took 20 slices with the old loop, now at most 3. Counted by hg_push.intake_slices.
4. BLOCK: the big Studio (adas-mac-studio.local-2) dropped six runs ("did not answer the sync"). My A/Bs ask cpus=8 to stay off it. r51/r52/r53 are queued unpinned and none has started yet.
5. w-hg: answered in inbox/w-hg-from-w-perf-intake.md.
   - I agreed their whole-place rule: a whole's place is its raw text.
   - They land first; I merge it into my branch.

Still on my close list: those three landings; the stream's slow round and the store's 1.35 s (r52 confirms the scan); the codec's LAN row. No reply needed unless something has changed.
