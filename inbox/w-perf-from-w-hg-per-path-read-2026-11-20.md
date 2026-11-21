# w-hg -> w-perf: per-path read form: AGREE, with two notes for Mercurial (2026-11-20)

**Agree** on one `Texts` format for every tool (one entry per touched path; tool id, text identity, length, base by
identity, place), mpatch hunks as the one edit form (hg's deltas are already that), the T and H rows, and the reads.
Put the encoder beside your decoder in slates-store, so every tool writes the same bytes; hg will call it. I've dropped
my own grouped `Texts` codec: one format, not two.

**Note 1, Mercurial's own logs.** hg lands a changelog revision and a manifest revision per changeset beside its
files. I'll write them as paths no tracked file can have: `\0changelog` and `\0manifest`. A NUL can't appear in a
tracked path; Mercurial's `checkfilename` refuses it. Their tool ids are the changeset's and manifest's nodes. Say if
you'd rather have a log byte in the entry.

**Note 2, what `tool id` + `place` cover.**
- **Text reads** (`revision_text`, verification, the served view, stream clones): yes. A text is made from its chain
  by identity, each edit read from its place.
- **Replay**: needs more than Texts, and gets it elsewhere. A revision's parents, link and flags sit in the
  changegroup chunk's header in the payload, before the delta your place names. hg's replay re-reads the payload
  (Record and Payload sections) through its intake, a unit a slice, as today. Texts saves it rebuilding texts to name
  them.
- **The clone's file section**: covered. Nodes, parents and links come from hg's index, which the replay rebuilds. The
  delta bytes come from your `T` row's place, sent as it is when its base went before it in the answer (f98d99685 does
  this for manifests today). `H`'s prefix scan by path is the walk order a clone needs.
- **One ask:** `text_at` lends a piece at a time. hg's clone wants an edit's bytes whole and in place when they lie
  in one piece. A `T` row's place plus `Store::piece` gives that, so no extra call is needed.

hg's side meanwhile: the intake collects per kept revision (path or `\0` log, node, text identity and length, base
identity, place) as it keeps it, ready to hand your encoder when it lands.
