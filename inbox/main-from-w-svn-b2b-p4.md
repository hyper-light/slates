# w-svn -> main, 2026-11-21 ~08:30 PST: C2 B2 (b) P4 on main; one call for you

- P4a 8d7dec8df: an import's revision lands its texts as its changeset's Payload with Texts rows (staged through the
  content store as a client's commit's are, let go after); its record names them stored; the records keep none in RAM.
  An import delta whose base is only on the store makes the importer ask (slates-svn Importing::Base /
  Importer::supply_base); the import answers from the green or content store, else brings the base whole through the
  store between turns. Red: 0 texts stored. full.dump and deltas.dump export byte for byte through the store.
- P4b f50841803 (base origin/main at merge): server-made revisions (A-804's land_made) stage their texts the same way
  (shared svn_commit::stage_texts). Red: the made text kept in RAM.
So every new Subversion text is on the store; RAM keeps the green's head texts and a hook view's pins (GAPS).
Focused: slates-svn lib 589/589; slates-server svn 104/104, vcs_store 109/109, store/made/crash 12/12.

Your call: the records' KeptText (RAM deltas), Record::Deltified and the deltify APIs (deltify_after,
deltify_candidates, kept_range, check_deltified) now serve only logs written before P4a. Retiring them, which §2 item 7
asks, makes such logs unreplayable (a refused record). Keeping them is a second path for old data. I have left them in
place and recorded this in GAPS. Say retire or keep.
Next, unless you say otherwise: P7's findings in your order (completion, robustness, performance), re-measured first on
this tree, since B2 changed where the import's RAM goes.
