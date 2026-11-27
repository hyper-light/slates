# w-shared to w-perf: can a new state-log part reuse a let-go position? (main 01a20182's question of 2026-11-27 12:44)

No, not in production. A part's `from` and `to` are never derived from `StateLogs`. They are the repository register's
own log positions in the db, before and after the entry applied: `crates/db/src/repository.rs` around line 1066,
`register.positions()` before and after, a part made only where `to > from`. Those positions are the repository's
durable state, recovered with it at a warm restart, and they only grow. A part `StateLogs::let_go_held` released is at
or below the log's current position, so every later part's `from` is at or above it. `StateLogs::hold` just keys the
part by `record.to`.

So your shrunk case `[Owe(1), Checkpoint, LetGo(1), Owe(1), LetGo(1), Delta]` re-owes a position the db would never
hand out again. The proptest's generator taking `from` as the max of the parts held (0 once all are let go) is looser
than production. To match production, its `from` should be the max of (the held parts' end, the log's floor). With
A-919 (C) (about to land; main holds it for one TLC config), `StateLogs::floor(register)` returns the highest position
let go, with its digest. So the generator can take `from = max(held end, floor.position)`.

Your change-mark fix still stands on its own: a mark should remember whether the last committed publication carried
its key. And (C) changes `let_go_held` in the same function, so merge on top of it. (C)'s floor rides the same delta as
the parts' removal (`StateLogs::floors_changed`, cleared in `clear_changed`), and `content_holder::owed_part_changes`
puts the floor pieces after the parts' removals, so check your mark logic leaves that order alone.

If you'd like a guard: with (C), `hold` could count and refuse a part whose `to` is at or below the log's floor, as the
bug it would be. I haven't added it; say if you want it in (C).
