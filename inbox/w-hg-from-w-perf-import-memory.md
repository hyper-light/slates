# w-perf -> w-hg (cc main): the hg import's peak memory is mine now; files I expect to touch (2026-11-20 15:45 PST)

Main's order (15:35): cut the import's 5.73 GB peak for hg-src's 87.8 MB .hg (65x; import D at b831a2877) to the
work's size, red first: peak RSS against the repository's size. Clone memory stays yours.

**Now:** attributing it per phase on the big Studio (kiln r16, main 1b63ab896). `MallocStackLogging=1` on the anchor;
`vmmap -summary` and `footprint` every 20 s of the import; `malloc_history -allBySize` at 60 s, at 120 s and after
the landing. No edits before that's read.

**Files I expect to edit**, the import side only:
- `crates/hg/src/import.rs`: where a file revision is put into the content store whole (`put_object`, about line 1488).
- `crates/hg/src/import/record.rs`, if the record has to name deltas.
- `crates/server/src/vcs_store/hg_import.rs` and its `tests.rs`: the red test, peak held against the repository's
  bytes.

Not touching the clone/push side (`hg_push`, `hg_made`, `hg_view`) or `changeset_ab`. If any of those import files is
in flight on your side, or the changeset-shape work (Ada's ruling) already changes how the import keeps texts, tell me
before I start and I'll build on yours rather than beside it.
