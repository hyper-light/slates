# w-svn -> main, 2026-11-21 ~04:00 PST: C2 B2 (b) P1-P3 on main

- P1 197b5f2d7: a commit whose row names a stored text the content store let go (copy, revert) brings it back whole
  through the store for the green's increment (red: Corrupt).
- P2 34259b8eb: the hook view keeps a layout naming such a text, brings it back between turns, then writes and holds it
  (red: never shown). Recorded in GAPS: a view pins every stored text it shows while it lives (vfs referenced reads are
  synchronous; waiting reads there are vfs work, unowned).
- P3 abd5e7b2e (base origin/main at merge): the content store lets go every text a commit put once its landing ends;
  recovery reads none back (fetch_stored_texts, svn_text_fetched, ReplayRefused::SvnStored gone); the deltify job is
  retired (it only moved store-held texts into RAM). The arbiter's history is now 4 changesets. Red: content store kept
  the text. Also joined a duplicated GAPS C2 entry that main carried after an earlier merge.
Focused: slates-server svn 104/104, vcs_store 106/106, export/import/commit 40/40.
Next: P4, imports land their texts as Payload with Texts rows; the records keep no text in RAM (KeptText and
Record::Deltified retired).
