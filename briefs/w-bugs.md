# w-bugs: the worker for kiln goal 87 "Every open slates bug is fixed, failing test first and red-checked (Ada 2026-11-06:
# if it is a bug in Slates, fix it)" (main, 2026-11-19 ~21:45 PST)

You run ON jupiter (Windows x86_64), so Windows builds and WinFsp tests are local to you. Your own progress file:
~/fanout-state/progress/w-bugs.md (create it; keep it current after every step; you compact automatically and never end
for context). Before anything, find where earlier agents left each bug: progress/g258-winfsp-hardlinks.md (A-498, A-551,
A-576; g258 owned them), progress/g079-every-open-bug.md (the goal-83 audit's B2-B16 and the A-419 siblings),
amendments.tsv's rows. Check main (`git log origin/main`) for what already landed; build only what is missing.
This goal's own rule: each bug fix has its failing test first AND is red-checked (the fix taken out, the test fails).

## Open steps (one piece at a time, each landed as one commit plus the merge)
1. A-498: Mercurial's hardlinked clones and stores byte-identical on WinFsp, and the WinFsp delete/purge hang.
2. A-551 (B2, the Windows half).
3. A-576: a folding WinFsp drive's volume folds names by Windows' own upcase table, so a name Windows folds alike is the
   same name (see the goal's full text in progress/g258-winfsp-hardlinks.md).
4. The bridge and transport bugs from the goal-83 audit (B2-B16, B7a/B7b A-419 siblings), each fixed failing test first.
Reproduce each by use first (a real WinFsp mount on jupiter; stock hg for A-498). WinFsp facts: memory
winfsp-mount-facts (A-177: cache flags, per-open contexts) -- the notes in g258's progress file cover them.
