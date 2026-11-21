# w-git -> main: layer 2, the next wall is per-ref history walks in the decision; a design question (03:55 PST 2026-11-21)

Measured on Studio-2 (release plus timeline diagnostic; git/git whole-history push, 611 MB, 1,027 refs):

| Answer phase | Wall | Shard time |
|---|---|---|
| checks (after piece 1) | 16.0 s | 12.7 s |
| seal | 3.7 s | 1.6 s |
| homing and landing | 1.3 s | 26 ms |
| deciding the 1,027 commands | over 350 s, unfinished | 210 s and still going |

Pieces landed or landing:
- Piece 1, ON MAIN (b7d2ae37e): the connectivity check charges per object, not per edge. The push is no longer refused.
- Piece 2, landing now (8baa1cf16): every decision step had folded over all of the push's commits (about 75,000) to
  size a restage nobody needed; a stack sample put 3,408 of 3,620 busy samples there. It is now two counts. Decision
  steps went from about 0.5 ms to about 30 µs. Focused tests green: git store 115/115, server 51/51.

The wall now is the volume of admission work, not the cost per step. A create walks what its head reaches beyond "the
newest commit that is some green's current head" (A-614, `store/admitting.rs` Base then Beyond). git/git's refs are
mostly tags at old commits. When their command comes up, no green's head is an ancestor of theirs, so each tag walks
its whole history for its green's reached set. Cost is about refs × history. Stock git walks once per push.

Options:
- (B) Mine to build, inside A-614 as designed. Decide a push's creates (non-atomic) in ascending generation of their
  heads, still reporting outcomes in the order sent. Each create then finds an earlier command's head (the previous
  tag or branch) as its base, and walks only the segment beyond it: about the history once in total, plus the base
  searches. Observable change: under non-atomic, refs append in generation order rather than the order sent, which
  only matters if the store refuses part-way. Atomic keeps today's cost (all appends at the end), and I'd note that
  in GAPS.
- (A) Design, Ada's call. A create from a commit that is no green's head records its reached set as "the closure of
  H", derived on first read (A-647 derived data already allows deriving). The admission then walks only what is new to
  the repository, once per push, as git does, and a fetch of such a ref derives the set the first time.

Unless you rule otherwise, I'll build (B) next: red = git/git's push unanswered at 1,200 s, green = it lands. Then the
A/B against stock git, the checks' 16 s being the next thing to cut.

## Correction (04:05 PST): (B) as written helps branches only

I read the base search before building. `Repository::green_at` maps a head object's position to its green, and an
annotated tag's green has the tag object as its head, not the commit it peels to. So the base search from a newer
tag's commit never meets an older tag's green. git/git's refs are mostly annotated tags, so sorting by generation
alone would not end the walls.

(B') would also need:
- an index map from each green head's peeled commit to its green, kept and replayed like `heads` (new derived index
  state, with recovery consequences);
- the new reached set taken as the base's minus the base's own tag chain (the old tag objects), since the new ref
  does not reach those.

That puts (B') close in size to (A): reached sets derived on first read, with the admission walking only what is new to
the repository once per push. I'll hold on both until you rule (or Ada does, if it's A-614's call). Meanwhile I'm landing
piece 2 once its merged-tree tests pass. Then, unless you say otherwise, I'll take the checks' 16 s, which is on my side
of the line either way.
