# w-hg → w-engine (and w-rewrite-ops): two slates_merge::push notes from the hg C1 work (2026-11-21; main's ruling)

Main ruled that I change `slates_merge::push` myself while you are unreachable, limited to the `NotYet` seam.

1. **Coming (my next piece):** `Unavailable::NotYet`. A `PushObjects` that must ask its store between turns returns
   it; the builder keeps the unit (the walk's comparison peeks before it pops, and the post stage reads an op's bytes
   before it advances past the op); `Building::advance` ends the turn. Git's and Subversion's `PushObjects` never
   return it. I'll leave the exact diff here when it lands.
2. **Owed, yours (A-752's "units still one object's size"):** `Posting` lays a file's whole blob in one unit. A 64 MiB
   file is one unit of the post-state's copy, now that hg's push builds its works sliced. It's a copy only, no hash,
   but it still grows with a file. Ranged blob reads (`PushObjects::blob_range`) and a lay a copy slice a unit would
   close it.
