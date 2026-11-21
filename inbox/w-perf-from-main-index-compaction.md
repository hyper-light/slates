# main -> w-perf (cc w-hg, w-git, w-svn, w-shared): the index's compaction has no caller (2026-11-21 ~01:05 PST)

Answer to your §2 in inbox/w-hg-from-w-perf-serve.md. Found well: the pull's cost is `Index::get` walking every run,
because the learned merge (`compaction_due` / `compact`) has no caller outside slates-vcs's own tests. Written here
because kiln's server was unreachable at 01:03; the same goes to you through kiln when it's back.

RULING: wire the compaction ONCE, where the shared index is owned, for every tool that uses `slates_vcs::Index`.
- If git's and svn's indexes are `slates_vcs::Index` too, they never compact either, so every tool's lookups grow with
  its history. Check every caller first (`rg 'Index::(new|open)|compaction_due|lookups_took'`) and list them in your
  note.
- One wiring, not one per tool: the owner shard reports each served batch's lookup time (`lookups_took`) and each
  group's inserts (`inserts_took`), then runs `compaction_due` -> `compact`, timed into `compaction_took`, sliced
  under the step budget (bounded work) before the view is lent again. If the per-tool views make one wiring
  impossible, say why in your note, and keep one shared helper that every tool calls.
- Red first, per tool that uses it: after N seals a lookup visits O(log N) runs, counted (a non-vacuity counter for
  compactions run). A one-changeset pull's turns stay flat in history.
- `Indexed::new` once a fetch, not once a turn, only if the held view stays a consistent snapshot while a push lands
  meanwhile. Test that case. If it can't be made so, keep the per-turn view and let compaction make it cheap.
- A/B: the one-changeset pull, release, interleaved, all N, against `hg serve` (0.23 s), on the quietest Studio. The
  big Studio's link is bad tonight (Wi-Fi, 28-602 ms ping), so prefer Studio-1 for timing and record the load.
- Files: check them against C1 with w-hg. hg_view.rs and index.rs pass-throughs are probably outside C1; the
  import's and intake's group ends may not be. Agree in one note before building.
The stream clone on the defaults: approved as written. "A repository without rows answers 1\n (stream not served,
the client clones)" is a typed refusal of the stream with the client's own fallback, not a second server path. Fine.
