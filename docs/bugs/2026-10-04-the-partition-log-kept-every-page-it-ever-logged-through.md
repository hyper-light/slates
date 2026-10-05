# The partition log kept every page it ever logged through (2026-10-04)

**Description.** A daemon serving a Docker container over an NFSv4.2 volume grew by about 1.8 MB per workload round,
linear over 24 rounds (90.8 → 128.9 MB), although the volume was empty after every round and `retained=0`.

**Root cause.** `slates_db::record::LogRing` kept monotonic head and tail offsets (position = offset modulo
capacity). `Db::snapshot` publishes the partition and trims the ring through the last record, which empties it. But
the next append went on at the old tail, so the writer moved forward through the ring's region. The anchor segment
is a sparse shared object, and every page the writer touched stayed backed. Residency therefore grew with the bytes
ever logged until the ring wrapped at `log_bytes_per_partition` (2.86 GB here, per partition). The evidence:
`footprint --forkCorpse -v` diffed between rounds 4 and 10 showed all the growth in the anchor segment's mapping
(434 → 1,078 dirty pages; that region is `SLATES_ANCHOR_LEN` = 80,173,056,000 bytes). A temporary `mincore` count
over the content object's slice for the shard stayed at 2,115 pages, and a memory graph's heap grew only 492 KiB.

**Impact.** Unbounded in practice (banned: unbounded growth). A long-lived daemon on a busy machine would back up to
the ring capacity per partition (about 11.4 GB on four partitions here) for metadata whose live size is one snapshot
interval (1 MB on a fresh daemon).

**Edits.** `LogRing::trim`: when the trim empties the ring, store the sequence base, then head = 0, then tail = 0
(the crash order is argued in the code and in A-71). The new test
`a_trimmed_ring_rewinds_so_a_long_run_touches_one_interval` failed first ("the ring wrote at byte 65600 or after,
past 4 intervals") and covers the crash state between the two stores. Sibling sweep: the audit ring is the other
`LogRing`. It is never trimmed by a snapshot, so it does not take this path; its own retention is a separate
question, not reviewed here.
