# A landed revision's bring-back asked for room inside a lend, where the pool is out of reach (2026-11-23)

## Description
`vcs_http::daemon_edge::tests::a_landed_file_revisions_let_go_windows_are_brought_back_whole_by_its_path_and_node`
failed on plain Linux as well as under ThreadSanitizer. On adacachy (r602, load about 60) it printed "120 of 120 files'
windows let go before; 113 brought back" and file 113's bring-back answered false. Under TSan (CI run 45318320246) it
stopped at file 81. `hg_push::bring_back` answered a bare false for each of its refusals, every one through `.ok()?`,
so nothing said which had fired.

## Root cause
- `hg_push::bring_back` made its room with `vcs_store::within_budget(state, lend, ..)` inside `vcs_store::lent_state`.
  `lent_state` lends the shard's chunk store to the repository content store and leaves a placeholder in its place,
  and the arena and its pool go with the chunks. So `within_budget`'s `lend` asked the placeholder to grow
  (`Store::make_room`) and read its room (`Store::admittable`). The placeholder can claim nothing, so the step had only
  the slack the arena already held beyond its commitments and headroom. `lent_state`'s own docs say its step "grows
  nothing" and that `lend` runs before it.
- The slack depends on timing. A committed publication gives the arena's wholly free extents back to the pool
  (`publish_shard`, then `Store::release_idle`), keeping only what commitments, headroom and the pressure hold need.
  When a publication (the idle sweep's, or a verb's) landed between the push and the bring-backs, the arena kept
  407,277 bytes of slack, room for 49 files of 8 KiB; with one more 256 KiB extent kept, 81. Without one, the push's
  high-water capacity was still held and all 120 fit.
- Five more steps did the same: `hg_push::brought_back` (a push's work reading back a text an import let go), wrapped in
  `lent_state` at both of its call sites; `hg_import::brought_back` (an import's work), at both of its; and
  `hg_push::bring_back_log` (w-hg's P1, the changelog's and manifests' texts). The GAPS seam entry records that
  `brought_back` was moved into the lend after an `OverBudget { room: 0 }` refusal outside it. That refusal came from
  having no lend at all; `within_budget` already lends the chunks itself (`Lent`), so the wrapper only hid the pool.

## Impact
A home's read of a window let go once landed, a push whose work needed a text an import let go, and an import's own
work were refused at load, on a timing. The read answered none, the push `WORK_TEXT_UNREAD`, the import
`ContentUnavailable`. No kind was counted, so nothing named the room.

## Edits
- `hg_push::bring_back`, `bring_back_log`, both `brought_back` call sites, and both of `hg_import`'s: `within_budget`
  is called outside any `lent_state`, so its `lend` grows the arena from the pool before the chunks are lent.
- The guard, so the pattern fails at its first test instead of at load (main 01a20182's ruling of 21:30): room asked
  while the shard's chunk store is lent marks the content store's budget out of reach
  (`ContentStore::set_budget_out_of_reach`), so every put of that step that needs a byte is refused by its own kind,
  `ContentRefused::OutOfReach`, whatever slack is left. The ask is counted (`vcs_store.room_out_of_reach`).
  `VcsStores` knows the chunks are lent from its own `lend_in` and `hand_back`.
- Typed refusals instead of a bare false (§2 item 9): `bring_back` answers `BringBackRefused` (not held, store
  unanswered, no text, the content store's refusal, not whole), each kind counted where it is refused.
  `homes::bring_back_landed` passes it on, with its own pack-path kinds (`LandedRefused`: unread, the content store's
  refusal, not whole), and `homes::read` answers `HomeReadRefused`. A member's read is answered the nearest kind the
  wire carries. `read_in_region` and the healer, which can only say absent, count the kind.

## Evidence
On adacachy2 (32 cores, 122 GiB, load 47 to 50, debug builds, 2026-11-23 21:10 to 21:21), with probes printing every
false path and the shard budget's terms:
- Plain, 100 runs of the test: 97 passed. Two were refused `OverBudget { need: 8192, room: 5869 }`, from file 49 and
  from file 81, each with `capacity 4194304` or `4456448`, `headroom 2621440`, `hold 0`, `debt 0`, while the shard's
  admittable read outside the lend was 43,990,914,797. The third failure was a different fault (the watchdog's exit 4
  in `create_empty_hg`, handed to w-ci). No other false path fired.
- The publication pinned before the bring-backs, interleaved: as on main, 0 of 10 passed, each exactly 49 brought back
  and then `room: 5869`, the shard grown 0 times. With `within_budget` outside the lend, 10 of 10 passed, the shard
  grown from its pool 7 times a run.
- Under a 4 GiB memory bound (a `systemd-run --user --scope`), 20 interleaved runs each: as on main 19 of 20 (one
  refused at file 57), outside the lend 20 of 20. Plain, outside the lend: 100 of 100.
