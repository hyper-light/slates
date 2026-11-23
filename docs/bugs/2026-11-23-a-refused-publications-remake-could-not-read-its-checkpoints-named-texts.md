# A refused publication's remake could not read its checkpoint's named texts (2026-11-23)

## Description
`repository::svn_commit::tests::a_revision_refused_at_publication_leaves_the_green_where_the_log_is_and_the_next_lands`
failed 3 of 3 alone at origin/main bb4551eff (w-shared; confirmed by w-git and here, macOS arm64). After a publication
refused before its append, the green read `None`, and the next commit was refused `Err(NotHeld)`, not `Ok(revision 2)`.
The daemon logged: `green svn:main:… fenced, its recovered history is not what was acknowledged: ContentMissing {
version: 1 }`.

## Root cause
A-869 (c14f88038) made a green's checkpoint name a file held by a content-plane object instead of carrying its bytes.
A restore reads those files from the content plane. A refused publication remakes the green synchronously, inside the
refused step, from its durable checkpoint and chain (`verbs::remake_green`). A client commit's texts are not kept in
the content store once landed: they are durable in the node's changeset store, and in RAM only in the green's blocks.
So the remake's restore found none of them and fenced the green. The cold recovery brings such texts back first
(`svn_texts::bring_back_named`), but that reads the node's store, which the synchronous remake cannot wait on.
A-869's module docs also said the texts were "held by the repository's records", which was wrong: the records hold the
names, and the node's store holds the bytes.

## Impact
Every refused publication of a Subversion green whose checkpoint named a text the content store did not hold whole
fenced the green. Every later commit was refused until a restart's recovery replayed it. Nothing durable was lost.

## Exact edits
- `crates/merge`: `Blocks::named_bytes` and `Green::bytes_named`, the bytes of the live blocks named for each wanted
  object, in one pass. Since A-866 a block named for an object holds that object's bytes, whichever file or version
  holds it.
- `crates/server/src/verbs.rs`: before the remake lets the engine go, `kept_for_remake` copies out the named texts the
  durable checkpoint and chain name that the plane does not hold whole (`svn_texts::named_lacking`, now `pub(crate)`).
  The replay reads named texts from the plane first, then from those kept (`GreenReplay::kept`, `replay_objects`,
  `named_objects::ReplayObjects`), in its checkpoint's restore and its chain entries' runs alike.
- `crates/merge/src/checkpoint.rs` module docs corrected.

## Siblings (reported, not changed)
- A takeover's adoption replays a green stepwise (`awaiting_adoption`'s `GreenReplay`) with no pre-pass and nothing
  kept. An adopted Subversion green whose checkpoint or chain names a text the successor's content store does not
  hold whole is fenced `ContentMissing` the same way. This was latent since A-865 for chain entries' runs.
- A text that neither the plane nor the replaced engine holds still fences the remake typed. That needs a deferred
  checkpoint lagging the fold and a superseded text, then a refused publication.
