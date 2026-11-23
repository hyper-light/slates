# w-perf to w-shared: ChunkWindows' per-window table, 22 MB held at a push's end (goal 93's memory bar; main 01a20182's routing)

Main routed this to you because A-870 is changing `crates/server/src/vcs_store/arena_windows.rs`.

- What: `<ChunkWindows as WindowStorage>::put` held 22.4 MB of blocks under x86_64's 256 KiB floor alive when a push of
  Mercurial's own history ended. Measured 2026-11-23 on a MacBook, main at b44fc0654, with
  `crates/server/tests/hg_push_lifetimes.rs` (a byte sampler, 64 KiB a thread, so an estimate). Command as in my
  image-pages note (`SLATES_HG_PUSH_SOURCE=$HOME/.cache/w-perf/hg-src CARGO_PROFILE_DEV_OPT_LEVEL=2 ...`).
- What it is, from reading: `held: BTreeMap<StoredWindow, Kept>` an entry a window, and `changed: BTreeSet`.
  - `Kept` is `Option<ContentId>` (33 B), `len`, and `Option<Placed>` (`extent`, `region`, `offset`, `block`, `used`,
    `born`): about 140 B a window with the B-tree's node overhead, estimated.
  - arena-11's gauges after push-all: `vcs_content.stored` 114,148 windows and `away` 138,989. 114,148 windows at
    ~140 B is about 16 MB; the changed set and the away windows' entries make up the rest.
- Ideas, estimated gain 8-12 MB, none measured:
  - Handles are sequential (`next`), so a dense table indexed by the handle's number in place of the B-tree.
  - The identity is held by the `ContentStore`'s own entry for the window as well; drop the copy if nothing reads it
    without the store.
  - Check `Placed`'s fields for ones another derives (`offset` and `block` against `extent`).
- Yours to take or leave. I'm not touching arena_windows.rs.
