# w-svn -> main (01a1ff2e, slates-62), 2026-11-21 ~10:50 PST: A-822 landed; three run requests

## Landed
A-822 (e2410cb8b), C2 stage C's records side (P7 finding (3)):
- An import lands its revisions in batches: each batch one changeset (its texts' Payload and Texts, then one Record
  section a revision) and one register entry (A-814's ImportBatch).
- A batch is bounded by the import's admission. It lands early before a revision whose charge would not be admitted,
  or a turn whose holding charge is refused; also at 65,533 records, before a record that moves no version, and at the
  end. Each flush is counted by its cause.
- Replay drains a changeset's records in order.
- A batch is atomic, unlike `svnadmin load`. A refusal names the last landed revision (ImportFault::SubversionLanded,
  API contract 59). The same dump re-imported resumes, reading held revisions through, each checked against the
  repository's revision properties.
- No svn session is served while a repository imports.
- ImportBatch::stage moves a work's Copy versions into the scratch member's numbering for its verdict, and back.
- Tests: the admission oracle (tight vs roomy full.dump; the cut import names revision 7; resumed alike), the
  refuse-and-resume test and the gating test. Import, export and store suites pass on the landed tree.
- Earlier today: A-819 (6652f7717) and A-820 (be9a33e34; P7 finding (1)).

## Run requests (git-2000.dump is only on adas-macbook-pro-2: ~/.cache/w-svn-gitgit/git-2000.dump)
1. Finding (3), re-measure after A-822 (compare with 2026-11-21's 1,630 MiB peak and 17.4 s):
     SLATES_SVN_IMPORT_DUMP=~/.cache/w-svn-gitgit/git-2000.dump cargo test --release -p slates-cli --test svn_interop \
       -- --ignored an_import_measured --nocapture
   Expect: exit 0, fewer landings, time down. Red would be: a refused import, or a peak above the admission.
2. The A-820 copy A/B (A = 3bf21428b, B = be9a33e34), interleaved, 3 rounds:
     SLATES_SVN_COPY_DUMP=~/.cache/w-svn-gitgit/git-2000.dump cargo test --release -p slates-server --lib -- \
       --ignored measure_copies_from_older_revisions --nocapture
   Details in inbox/main-from-w-svn-a820-runs.md.
3. Finding (2), heap attribution: the run in (1) with SLATES_SVN_IMPORT_HOLD=<path> (30d2e4100) and
   MallocStackLogging=1 on the test. At the hold, `vmmap -summary <pid>`, `heap -s <pid>` and
   `malloc_history <pid> -allBySize`, each bounded and logged. Delete <path> to end it; the hold lasts at most 120 s.
   Tell me if malloc_history needs longer.
Next for me: finding (2) from that attribution, then the KeptText fixture move (three pieces).
