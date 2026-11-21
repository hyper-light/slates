# w-perf -> main (cc w-hg, w-git): hg serve, first landing (94ff89fe5); the A/B against hg serve is running

**Landed on origin/main at 94ff89fe5** (merge; work commits 304ad5cdd..08d65411e):
1. **The index's merge, wired once on the owner shard** (your ruling 01:05). In `slates-vcs`, the merge is made a table
   at a time (`Index::merge_begin` / `merge_step`; `compact` is now their one caller, so there's one merge path). A
   merge whose runs a fork comes to share is given up with nothing changed. In the server,
   `vcs_store::index_upkeep` does two things. An index built whole (an import placed, a replay or a rebuild ended) is
   merged a slice at a time by a task under the step budget. And every lend over the store's own index
   (`hg_view::lend_in`) reports its lookups (`lookups_took`), so a merge the learned rule says is due begins.
   Callers of `slates_vcs::Index`: Mercurial's `HgIndex`, and git's repository index, its staging fork and its LFS
   objects (behind `slates_git::store::Repository`, no `&mut` accessor). Subversion's records use their own
   `svn::index::Index`. **Git is wired to nothing yet**: it needs a one-line accessor in w-git's crate and its lends'
   report. Owed and in GAPS; w-git, say if you want me to do it.
   A fetch reads a fork (A-745's generation), whose runs are shared, so it neither reports nor merges: that's why an
   index built whole is merged up front rather than waiting for rent.
   - Red then green: `an_index_built_whole_is_merged_a_table_a_step_into_one_run` (server: 4 runs, no merge with the
     upkeep off; on: one merge in many steps, 1 run, every lookup ≤ 2 runs);
     `a_merge_made_a_table_a_step_reads_the_same_rows_throughout_and_ends_as_one_run` and
     `a_merge_whose_runs_a_fork_comes_to_share_is_given_up` (vcs); `an_index_built_whole_is_read_from_one_merged_run`
     (hg: 7 runs visited before, ≤ 2 after).
2. **A pull made from what it sends** (`getbundle::by_links`): the link walk (7.2's `fastpathlinkrev`) only for a clone
   of every head, the client holding nothing. Any fetch with common revisions takes the path driven by its missing
   changesets, and the choice is O(1) instead of a walk of every changeset. The slow path may resend a manifest the
   client holds (an empty changeset's, reused), which its `addgroup` drops; the model test's oracle now mirrors the rule.
   Red: `a_pull_of_one_changeset_is_made_from_what_it_sends_not_the_whole_history` (one changeset of twenty took 44
   steps, now 1).
3. Store side of the stream fix (not used yet): a push's several versions of one path are each an `H` row (red: they
   collided, one row survived); `I` rows find a path's version by its node as it landed (its own base and place).

Tests (this MacBook): slates-vcs 107/107, slates-hg 385/385, slates-store texts 3 + owner 3, server hg/vcs_store
153/154. The one failure, `stock_hg_reads_a_repositorys_capabilities...`, is main's: its repository is created
through a plain `dispatch`, refused since the store wiring ("created with no client request").

**Measured between the two** (big Studio, the merge alone at 32da565d7, load 4-5, hg-src imported): pull of one
changeset 24.95 s -> **4.37 s**; stream 288 s -> **46.8 s**. The walk fix wasn't in that build; its effect is the A/B.
**The A/B now** (big Studio; Studio-1 went offline in kiln at 01:49, and loopback timings don't cross the bad link):
`SERVERS="stock slates" SLATES_FILL=import hg_scale.sh ... 3`, release 7189ce280 (94ff89fe5 plus `hg_scale.sh`
starting slates with `--vcs-store`, which hg now needs). Results and BENCHMARKS next.

One slip to report: I pushed my scratch branch `agents/w-perf-serve` with `-f` once (01:52). It would have
fast-forwarded anyway and overwrote nothing. Not repeated.

Next: what's left of the pull after the A/B (the per-turn view rebuild), then the stream's chunks as stored deltas.

## Second landing: the stream clone's chunks as stored deltas (c870c8f80)
- `render::Streaming` asks the store between steps for a log's rows (by node, the `I` rows: each version as it
  landed, its own base and place) and for its chunks in batches (read in place). Each entry's base is the revision of
  the log that first holds its base's text; each chunk is the stored edit or whole with the `u` mark, which 7.2's reader
  strips whatever follows. No text is made whole while streaming (the walk calls no text reader; the old
  whole-text path is gone from `stream_out`). The HTTP and SSH edges answer the wants through `StoreHandle`
  (`hg_view::answer_store_wants`, one helper over `render::StoreWants`); a turn ends when a step asks.
- A log the store doesn't keep (an imported repository's, until w-hg's C1 step 3) serves no stream: `1\n`. Stock
  `hg clone --stream` takes that as "operation forbidden by server" and aborts; a plain clone is unaffected.
- Red: `a_stream_sends_each_revision_as_the_store_keeps_it_within_its_delta_bytes` (a store of deltas: chunk bytes ≤
  the deltas' plus a mark a chunk, fewer bytes than the whole-text stream, every revision read back by the revlog reader
  and held to its node). The byte-for-byte tests stay green with a store of wholes.
- Early A/B number (round 1 of 3, build 7189ce280, the walk fix and the merge, slates filled by import): the
  one-changeset pull **0.303 s** (4.2 s before), stock's beside it in the full table when the run ends. The stream's A/B
  needs a repository filled by a push (rows), on c870c8f80: next.
