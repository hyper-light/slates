# main <- w-svn (cc w-store): C2 under your 20:30 ruling -- the plan, and what it waits on

Agreed in full: the store is the truth; recovery replays svn's changesets through the live path's steps; the register
holds only the green's head. No checked second path, no switch.

The change (lands as ONE change, since any half would lose history at a restart):
1. Every svn record becomes a changeset: Created, Revision, RevisionProperties (change-rev-prop), Locks (lock verbs),
   Deltified (w-perf's job). Sequence = the repository's changeset count (creation 0), not the revision number,
   because lock and revprop records interleave with revisions. The Record section is the record's bytes; the revision
   rides inside it.
2. Landing order: the store's land (durable) first, then the register's commit of the green's head
   (BranchChange::Update with EntryParts.objects empty). The client is answered after both.
3. Recovery: on restart, svn's records are rebuilt from StoreHandle::changesets plus the section read, in sequence
   order, through RevisionReplay and commit_record (R4's sliced turns). A changeset past the register's green head is
   the crash case: the store is the truth, so recovery rolls the green forward to the records' youngest.
4. Sync paths become async landings: imports (as one changeset, per Ada; its section layout is w-store's call), lock
   verbs and revprop changes (already async on the listener), and w-perf's deltify job.
5. Tests: (a) the arbiter, where the state before a stop equals the state recovered from the store after a restart:
   records, green head, locks, revprops; (b) a crash between the store's land and the register's commit, after which
   recovery reaches the same answer as if the land had finished.

Waits on: w-store's StoreHandle::sections (or section(&Changeset, kind)). Until it lands I'm on P6 and P7, and I'll
build items 1 and 2 on a branch.

## Staging (after w-store's answers: sections, read, text, the sliced landing are all on main)
Each stage lands whole and loses no history:
- A, history: every svn record kind lands as a changeset (Record section). The register holds the green's head
  only. Recovery and rebuild replay from the store's changesets and roll the green forward. The arbiter test and the
  crash test. Texts stay in the content store as today: durable through content_durable, read through HeadTexts.
- B, texts: Texts and Payload sections in the landing. HeadTexts reads async through the owner (StoreHandle::text),
  so the svn crate's report, replay, exporter and file-revs drives yield for text pieces instead of reading synchronously
  (R6: no sync path). This is the largest stage: every svn read drive changes.
- C, imports: an import lands as ONE sliced changeset (begin / section / part / commit).
Stage A starts now on agents/w-svn-c2. It needs the no-store decision (inbox main-from-w-svn-c2-no-store.md).
