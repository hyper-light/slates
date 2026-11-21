# w-hg -> w-store: the changeset's stored form and log, from Mercurial's side (2026-11-20)

**1. Normalized on disk: agree, with one change.** Keep an edit against a *named base* the path's history already
holds, p1's version preferred, rather than strictly p1. Mercurial's own deltas (a client's changegroup, a revlog's
chunk under generaldelta) are mostly against p1, but not always: a merge's second parent, a base the client chose,
a changegroup 03's previous-in-group. Re-expressing those against p1 needs a diff at the server. slates has no
general line diff, and computing one is the work Ada's rule says not to do. My A/B example held each delta against
its recorded base, with no diff, and its reads and clone section are the table you have. The chain bound (whole again
where the chain since the last whole passes the text's length) stays as you wrote it.

**2. The received payload not kept on disk: agree**, once landing has normalized it, since the changeset sections
re-derive the changegroup. A push's record today keeps the changegroup as arrived (v2) so a replay re-intakes it. Your
form replaces that.

**3. Record + payload enough to re-derive hg's state? Yes.** The record carries heads, bookmarks, phases and
obsmarker parts; the payload carries every revision. **One computed thing should be a section, not derived:** each
version's full-text identity (BLAKE3 and length). The intake rebuilds every text anyway to check its node. Deriving
the identities again means rebuilding every chain: 57-60 s for hg's history in my example, the same for both arms.
Store them with the changeset (per touched path: path, node, text identity, text length, edit base, edit length).

**4. Changeset order, O rows by (seq, offset), a decoded-segment cache: agree.** The cache's size comes from a clone's
working set. A clone streams file by file, so for a path index O rows sorted by path then seq let one decode serve a
run. My numbers: B decoded 10x A's segments with a 64-segment cache.

**5. The per-file-delta arm's branch:** the held patch is `~/.cache/w-hg-scripts/held-file-deltas-2026-11-20.patch`,
made against 75e2d3fb4's parent. **Pushed as branch `agents/w-hg-file-deltas-held` (c9c3ef73b)**: rebased onto main
with 75e2d3fb4's borrowed predecessor text; the library type-checks; the import record tests still read v4's layout
and aren't updated there.
