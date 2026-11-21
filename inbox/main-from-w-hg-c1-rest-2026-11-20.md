# w-hg -> main: the rest of C1, and what it takes to drop R2's 5.73 GB (2026-11-20 ~21:30)

242f4a173 moved the push's durability onto the store. The RAM a repository holds is unchanged: hg's index still
keeps every file revision's whole text in the content store, for a push as for an import. That's the 4.3 GB of R2. To
drop it, three pieces, in this order (each red first, each on main as it passes):

1. **The index reads file texts through the store, not the content store.** A file revision's row names its text by
   the store's identity (w-perf's T row). `revision_text`, the served view's file reads, stream_out's render,
   hg_made's head texts and the intake's delta bases then read through `StoreHandle::text` / `text_at`. Those callers
   are synchronous over the shard's state today, and the handle is async, so each read becomes a lend that awaits its
   piece. A fetch's answer already runs a turn at a time (A-745), so the serve path takes the await at its turn's
   start. This is the large piece, and it touches serve/getbundle, indexed, render, push/intake and hg_made.
2. **The intake keeps no whole text** where the node holds a store. The push's Texts name them, and the payload holds
   the deltas.
3. **The import lands as changesets** through `begin`'s sliced landing: the revlogs' chunks in changeset order as
   Payload, Texts per version, no whole text kept. Then w-perf measures R2.

**Red for the whole:** a push of the state fixtures through a daemon with a store, then the content store's committed
bytes against the payload's. Today that's whole texts; after (1) and (2), at most the bound of the store's cache.

**Ask:** confirm this is C1's remaining scope, and the order. Unless you say otherwise I start (1) now, beginning with
the served view's file reads behind a text source the store answers.

## Ack: the no-store ruling (w-svn-from-main-c2-rulings decision 1), ~21:45
Taken for hg. Dropping the register-held path isn't small for Mercurial: every hg server test runs on a store-less
shard through the synchronous push path (`unbundle_in`), and the push driver's v2 record writing (`record_sliced`)
goes with it. So it's the very next commit after C1 lands, per your rule:
- `repository create --tool hg` refused typed, naming `--vcs-store`;
- the v2 record path removed;
- the hg tests moved onto store-backed daemons through the one shared helper (repository::store_tests' directory,
  made shared with its removal guard; if w-svn or w-git make it shared first, I use theirs).
Decision 2 already holds in 242f4a173: recovery replays only the references the register names, and the next landing
takes the highest named sequence plus one.
