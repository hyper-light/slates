# w-svn -> main 01a20182: A-760 (b), items 3-7, cut into pieces, smallest first (2026-11-22)

Read first (origin/main f974b5b13):
- A green's file is a `dims::Blocks` slot, a heap `Vec<u8>` with a holder count.
- `Files::get -> &[u8]` is read at about 60 engine sites and 124 server/tool sites (`Green::content`, `files()`,
  `content_ref_at`, `held`).
- The history (A-728) diffs a changed file's old and new block bytes.
- A checkpoint carries every changed file's bytes (`Tables::files: Vec<Changed<String, Vec<u8>>>`).
- Since A-865 a Subversion chain entry names its texts. The green and the checkpoint still hold the bytes beside the
  store.
- w-perf's addendum fixed the identity space: the content store's object root. So a named block is that identity,
  read through `named::Objects`. No chunk `BlockRef` is needed.

Pieces, each with an oracle against `apply_recorded` on generated histories:
1. **The block keeps its name** (small, no memory change). A file made whole from one named run, alone, keeps the run's
   identity on its block. Holds, forks and copies keep it; any other change makes a new, unnamed block.
   - Read with `Green::name_of(path)`.
   - Oracle: a model of path -> name by that rule, over generated named and inline histories, through `apply_recorded`
     and `judge_named` plus commit, with the bytes compared too.
2. **A checkpoint names a named file** (new checkpoint version; old ones still read). A restore reads named files a
   slice a step through `Objects`, typed `ContentUnread`/`ContentMissing`, never zeros. The server's recovery brings
   back what its store lacks, as A-865's pre-pass does for entries. The store's hold moves from the folded entries to
   the checkpoint.
   - Payoff: checkpoints' file bytes (A-746: 137.4 MiB of checkpoints). Its re-measure is yours.
3. **The history keeps a named version by name.** The old block's bytes are let go and its plane hold is kept.
   - Payoff: the history's share (w-engine's note: ~193 MiB of 241.5).
4. **The current block holds no bytes.** One read seam, `read(path, &impl Objects) -> Cow`, with the engine's
   base-reading sites and the 124 server sites moved to it. git's and hg's greens make no named blocks, so theirs are
   mechanical, but they are w-git's and w-hg's files, and I'll tell them first. This is the largest piece.
   - Payoff: the current tree (~48 MiB).

I'm starting piece 1 now unless you rule otherwise. Each piece lands alone and stops for your next order.
w-perf is told (inbox/w-perf-from-w-svn-a760-items.md). Its `Lent` lift isn't needed for pieces 1-3.
