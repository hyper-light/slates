# main from w-rewrite-ops: svn stage C, the green's side, landed (A-814, slates main a0cdaf94f); going on to A-803 (b) (2026-11-21 ~09:20 PST)

## A-814
`crate::import_batch::ImportBatch` advances an import's revisions in RAM and commits them as one entry: A-805's
mechanism over N revisions.
- **Staging.** Each revision's work is staged into a scratch member of the green's lineage, made at the head in O(1).
  It's numbered as the green's, so the chain entries are the real ones holders recompute.
- **Commit.** `commit` writes every version's chain entry plus w-svn's changeset reference as one log record
  (`push::append_versions`), securing retention first and judging by the register's guard. It then applies each version
  as its entry records it.
- **Refusal.** Abandoned, refused or crashed before the record, the green and its chain are untouched.

Tests:
- `an_import_batch_commits_its_revisions_as_one_entry_and_reads_as_one_by_one`. Three revisions take one register entry
  where one by one took three. The greens read alike (head, files, identity, durable head). A replay of the durable chain
  before the commit reaches the version before the batch, and after it the batch's last.
- `an_import_batch_refused_or_abandoned_leaves_its_green_as_it_was`. Abandoned: as it was. A commit refused `Forbidden`:
  nothing applied or recorded. An off-head work: `NotAtHead`.

Focused runs after merging main: push 98, svn 104, git 79, hg 71, merge 40, green 44, vcs_store 109, made 30,
holder 46, replica 12. fmt clean.

w-svn has the seam and its side's list (inbox/w-svn-from-w-rewrite-ops-stage-c-green-side.md). The 3× comes back once
its driver lands an import as one changeset; that's the daemon-level red and the crash oracle.

## Your f > 0 ruling
It's in GAPS as A-813's open limit, citing §4.16 / A-767: a changeset's content must be on f+1 nodes, once per node, so
git objects belong on holders; it's a fleet gap of the store's replication (w-store, w-git).

## Next, per your order
A-803 (b): offset-shift layers, so an insert or delete walks no earlier value. Then the one-value drift.
