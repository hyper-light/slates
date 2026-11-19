# An update judged locks by the caller's guards, not the lock table (2026-11-19, A-656 continued (3))

## Description

Found by g234 while landing A-656 continued (2), and given back as this piece by main. An update's drive
(`crates/svn/src/report/drive.rs`) asked the caller's `Guards::lock_token` for the lock held on a path, as
`reporter.c` asks `svn_fs_get_lock` in `update_entry` and `delta_files`, while a commit's checks, the lock verbs and
DAV's lock answers read A-602's lock table. For one path and one lock the two could answer differently: with alice's
lock on `/trunk/a.txt` in the table and none in the guards, an update from a working copy holding that lock's token
opened `a.txt` and took its token away (`svn:entry:lock-token` with no value), though the table, and so a commit,
still honored the lock.

## Root cause

The report drive (A-616 part 2) was built before A-602's lock table and took the locks held from the caller, as it
took read access (A-656 continued (2)). When the table became the repository's record of locks, the drive kept asking
the caller.

## Impact

A working copy updated through svn:// could lose a lock token its lock still held, so the next commit would not
present it and an unlock would not name it; or keep a token whose lock was gone. No caller in slates implements
`Guards` yet (the daemon's listener is A-656 part 2, owed): the fixtures' guards held the one recorded lock by hand,
so no served session was affected.

## Edits

- `crates/svn/src/report/drive.rs`: `Guards` removed; `Sources` holds the lock table and the call's instant (`locks`,
  `now`); `held_token` reads a held lock's token with `LockTable::get_lock`, an expired lock none; the generic
  parameter `G` gone from every function.
- `crates/svn/src/serve.rs`: `Sources::guards` removed and `Sources::now` added; the drive's sources take the table
  and the instant; `G` gone. `report/exchange.rs` likewise.
- Tests: `serve::tests::an_update_keeps_a_lock_the_lock_table_holds_and_takes_away_one_it_does_not`, failing first;
  `report/drive/green.rs`'s `FixtureGuards` removed, `locks_of` a table holding the recorded session's lock, `NOW` the
  instant; the drive's, the exchange's and the connection's tests read the table.

## Sweep

Every lock read in `crates/svn` was read on 2026-11-19: the commit drive's checks (`check_lock` over the table), the
lock verbs and DAV's lock answers (`LockTable::lock`, `unlock`, `get_lock`) and now the update's drive all read
A-602's table; nothing else asks the caller for a lock.
