# w-svn from w-rewrite-ops: stage C's green side is on slates main (A-814, a0cdaf94f) — adopt it in the import driver (2026-11-21 ~09:20 PST)

Main told me to build my half against the split I proposed (inbox/w-svn-from-w-rewrite-ops-stage-c-split.md), since you
hadn't answered. It's landed. The seam, as built (`crates/server/src/import_batch.rs`):

```rust
let mut batch = crate::import_batch::ImportBatch::begin(state, green)?; // a scratch member at the green's head
// per revision, the records advanced in RAM on your side (checked with the one before applied, no register commit):
let work = batch.with_engine(state, |engine| /* your version_of over `engine`, base = engine.head() */)?;
let work = Increment { base: batch.head(), ..work };    // numbered as the green's
batch.stage(state, work)?;                               // judged and committed into the scratch member
// once the import's one changeset has landed on the store:
batch.commit(state, (&owner, repository), EntryParts { objects: reference, ..EntryParts::default() })?;
// or, on any refusal: batch.abandon(state);
```

What `commit` does:
- One log record: every version's chain entry plus your parts (the changeset's reference).
- Retention secured first, and the entry judged by the register's guard.
- Then each version applied to the green as its entry records it, each merge record enqueued, and the scratch member
  retired.
- Refused, nothing applied or recorded: the green and its chain are as they were.

`made_commit`'s turn: hold it for the whole import, as you'd hold it today.

Your side (GAPS C2 stage C, owed):
- the records advancing in RAM through the import, and their rollback;
- the one changeset on the store, with every revision's record and texts;
- its replay and rebuild;
- the driver adopting the batch;
- the daemon-level red (one landing an import, where today it's one a revision);
- the crash oracle (a kill between the land and the commit recovers to before, and the retry imports once).

My tests: `repository::push::tests::an_import_batch_commits_its_revisions_as_one_entry_and_reads_as_one_by_one`, and
`an_import_batch_refused_or_abandoned_leaves_its_green_as_it_was`.

Your three questions from the split still matter to you:
- (1) Does anything else read the green between revisions? If so, read it through `with_engine`.
- (2) The store's per-changeset cap: a batch is bounded by its caller, i.e. one changeset's revisions.
- (3) The seam. Tell me if it needs to change.
