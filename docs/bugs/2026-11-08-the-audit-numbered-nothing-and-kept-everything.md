# The audit numbered nothing and kept everything

**Found** 2026-11-08 by the sibling sweep of `2026-11-08-a-volume-listing-outgrew-one-reply.md`, while paging `audit`.

**Reproduced by use** (one-shard daemon, a volume's landing presented again and again, each presentation recording
`landing_planned`): `audit(0)` was refused "reply too large for the bulk area" between 50 and 100 records. Paging it
found three defects under the reply:

1. **Every record was numbered 0.** The server made each record with `seq: 0` (`landing.rs`, the presentation's,
   the grant's and the landing engine's records) and the partition kept it as given, so `slates audit --since N` had
   never filtered anything and there was no cursor to page by. Seen by use: 51 records, all `seq` 0.
2. **The audit grew for the partition's life.** `Partition::audit` was a `Vec` pushed to and never trimmed, copied
   whole into every partition snapshot, against banned item 8; its doc said "the ring holds the rest", and no ring
   held anything.
3. **A client read its own shard's audit only.** Each shard kept its own partition's records, and `Audit` was
   answered from the shard the client sat on, so on a multi-shard daemon a client whose shard was not the landing
   volume's owner read none of its records (reproduced: 0 of 20 on two shards).

**Fix (A-265).**
- (1) The partition numbers each record in `apply` from a counter in its own state, kept in its snapshot, so a replay
  numbers each record as the live partition did, whatever is retained. Test first:
  `audit_records_are_numbered_in_order_and_a_recovery_numbers_them_the_same` (`crates/db/tests/audit.rs`): 80 records
  numbered 0 to 79, and the same after a recovery from a snapshot and the log.
- (2) The partition retains a bounded window, `PartitionCaps::audit_records`, derived at boot from memory:
  `table_bytes / size_of::<AuditRecord>()`, the same table share the volume records draw from, logged by the boot
  profile. Past it the oldest record is dropped and counted (`Partition::audit_dropped`), in `apply` from the caps
  alone, so a replay keeps the same window; the snapshot carries only the window. The design's `landing rate ×
  operator audit horizon × record size` may lower the count once a landing rate is measured; nothing measures one
  yet, so no horizon is applied. Test first: `the_audit_keeps_its_bounded_window_newest_and_counts_what_it_dropped`:
  a window of 16 over 40 appends keeps 24 to 39 and counts 24, after a replay and after a snapshot too.
- The audit ring (`RegionKind::Audit`, a page a shard, `AUDIT_PAGES_PER_SHARD`) was never written: the records were
  always ops in the partition's log, and the snapshots carry the window, both in the anchor segment as §4.8's data
  model asks. The region and its geometry field are removed and the layout version is 7.
- (3) One partition holds the audit, the control shard's, as §4.15 has the records "written through the control
  shard" and §4.8 one `AuditLog`. An atom on another shard owes the control partition its records and sends them as
  one batch once it commits: a presentation's and a granted landing's completion wait for the batch before they reply,
  so the caller's next `audit` reads them, and a grant, a sync verb, sends its record after its transaction (a counted
  refusal, `landing.audit_unappended`, when the control shard cannot take it). On the control shard the records are in
  the atom itself. `Audit` is served on the control shard, wherever the client sits, and `--since N` keeps its
  meaning. Test first: `a_client_on_any_shard_reads_every_shards_audit` (`crates/server/tests/audit_pages.rs`): a
  volume on the second of two shards, 20 presentations and a granted landing of three files, read at once by four
  clients on both shards. Before the fix it failed: `reader 0: 0 landing_planned records of 21 presentations`.
- A record that crosses shards is not in the owner's atom: the owner's records commit, then the batch does. A crash
  between the two loses the batch (the owner's landing, grant and the request's completion stand); a batch refused at
  the control shard is counted. The landing's cost of the crossing, measured: a granted landing of 1,000 files of 1 KiB from the shard that is not the control shard, its 1,000 `entry_written` records and its others crossing as one batch (`crates/server/tests/audit_land_cost.rs`, release, two shards, this Mac at load 40 to 100, before and after alternated ABBA three times, 2026-11-09): before, a landing's median 968 ms and fastest 430 ms; after, 827 ms and 424 ms (25 landings each, the first pair dropped as the warm-up). No cost shows within the noise: the crossing is one message and one transaction against a landing's own writes and fsyncs.
- `audit` is answered a page at a time; the client walks the pages. `slates audit --json` from 0 exports every record
  still retained.
