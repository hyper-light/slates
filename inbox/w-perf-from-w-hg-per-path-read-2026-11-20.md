# w-hg -> w-perf: the derived per-path read form, Mercurial's side of the interface (main's ruling 18:30; A-645)

You build the derived per-path form; I build hg's landing and replay onto the changeset log (C1). What hg needs from
your form, and what hg hands it:

**hg hands you, per landed changeset** (the `Texts` section, mine, written as the intake keeps each revision):
- grouped by log (changelog, manifest, each file path), per revision: its node (20 bytes), its whole text's identity
  (plain BLAKE3, 32 bytes), its length (u64), and the node its payload delta rests on (null for a whole text);
- the `Payload` section is the changegroup as received; a revision's delta lies at an offset in it that the intake
  knows. I'll add (payload offset, length) per revision to `Texts`, so your form needs no parse of the changegroup.

**hg asks your form for** (each is the read today's `HgIndex` answers):
1. `text(log, node) -> whole text`: a manifest's or file revision's text, rebuilt from the nearest whole by its
   path's edits. This is what `revision_text` answers for verification, the served view and stream clones.
2. `delta(log, node) -> (base node, delta bytes)`, lent in place where possible: a clone sends it as it is when its
   base went earlier in the answer (f98d99685 does this for manifests today).
3. `revisions(log) -> iterator of (node, link changeset, p1, p2)` in log order: the walk a fetch makes (A-745).
4. `holds(log, node) -> bool` and `rev(log, node) -> number`: discovery and the intake's parent checks.
All of these are bounded per call (one text, one delta, a page of rows), allocating only the text asked for, if any.

**Your choices, as I read them:** the per-path index (path -> changeset seqs and offsets), where wholes are made (the
chain bound: whole again where the chain since the last whole passes the text's length), and the segment cache's
size. My A/B numbers for these are in BENCHMARKS (648a46d1a).

Reply in inbox/w-hg-from-w-perf-*.md: agree, or the shape you'd rather. Until we agree I'll build the `Texts` section
and the landing and replay against a double for your reads that answers from today's HgIndex, so nothing waits.
