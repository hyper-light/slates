# A large Mercurial import crash-looped the daemon (A-732)

## Description

Found by use (w-perf, 2026-11-20, Mac Studio, release build of main f8c9c95cd): `slates repository import mercurial
~/.cache/w-perf/hg-src` (Mercurial's own repository: 57,036 changesets, an 84 MiB store) ran past the room limit A-720
part 3 removed, then the anchor's watchdog killed the daemon: "shard 9 is spinning: 1140 ms of CPU without a step, past
its 1000 ms window". Every restart initialized shard 9 in about 85 ms and spun it again about a second later (1018,
1057, 1163 ms of CPU...): 11 restarts in about 12 minutes before the run was stopped. The CLI's import never answered.
Log: the Studio's `~/.cache/w-perf/evidence/hg-import-crashloop.log`.

## Root cause

Two steps each did a whole repository's work in one turn of its shard:

1. The import's landing wrote its record whole (`vcs_store::hg_import::landing_entry` calling
   `slates_hg::import::record::write` over the whole index: every revision's text read and copied in one step).
2. A recovery replayed each record of a repository's register as one unit of its slices (`vcs_store::recover` ->
   `replay_record` -> `replay_hg` -> `record::replay`, every revision of the record kept and appended in one call). The
   record the landing had made durable was replayed at every start, so the daemon never came back: a crash loop.

`vcs_store`'s own docs listed the second as owed ("the replay and a rebuild sliced a unit at a time within a record (a
record is one unit today)"), and `hg_import`'s the first ("the record and the bookmarks' works made in slices rather than
whole at the landing").

## Impact

A Mercurial repository whose record took more than the watchdog's window to write or replay could not be imported, and
once its record was durable the daemon could not start: every repository the daemon served was down. Smaller
repositories were unaffected (their records are written and replayed within a step).

## Exact edits

- `crates/hg/src/import/record.rs`: `Replay` (begin, step, finish, abandon): a record replayed a unit a step (a group's
  head, one revision, one changeset's manifest check, the markers and the end), its place a byte offset, so its owner
  hands it the record's bytes afresh at each step; `replay` is `Replay` run to its end (the one path). `Writer` (begin,
  step, finish): an import's record written a part a step (a group's head with its first revision, one revision, the
  markers), each part's room reserved before it is put; `write` is `Writer` run to its end (the one path). The whole-record
  helpers it replaced (`replay_into`, `replay_group`, `write_log`, `record_len`, `log_len`, `markers_len`) removed.
- `crates/server/src/vcs_store.rs`: `recover` replays a Mercurial record through `replay_hg_sliced`, a `Replay` step a
  unit of the recovery's slices (`hg_record_step`, `hg_step_in`), the record's room lent once and the shard's charge
  settled after each step; `replayed_hg` shared with the one-step `replay_hg` (made changesets noted from the record).
- `crates/server/src/vcs_store/hg_import.rs`: `Stage::Writing`: when the import's own work is done its record is written
  a part at a time within each turn's quantum (`write_turn`), and only then is it done, waiting to land with its record
  (`Stage::Done`'s `record`); `landing_entry` takes the record written, never writing one.
- Test (failing before the fix: one unit for the whole record, the record written in no turn of its own):
  `vcs_store::hg_import::tests::t_a732_a_recovery_replays_an_import_record_a_revision_a_unit`.

Owed: a rebuild's replay (`vcs_store::rebuild`, used where an append is refused after its store moved) still replays in
one step; the record's size itself, which S5 (A-729) makes identities alone.
