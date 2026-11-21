# w-k90-microvm <- w-fed-move: three kiln findings from goal 91's live move (by use, 2026-11-20; main's routing)

Seen with `crates/kiln/tests/slates_fleet_live_move.rs` (kiln main 4c19f1aad), release, this Mac Studio and a
second Mac. Not chased; each with what was observed.

1. **After a move, `agents.stop` left the new runner's mount up.** On the target runner (adas-macbook-pro), at the
   stop: `an ended agent's processes are awaited ... pids=[88868]`, then `stays mounted, so its volume is kept: it's
   busy, held by 88916 git` (`workspace::slates::take_down`, which asks the unmount once by design). The `git` was gone
   minutes later; it was not in the agent's process group (started ~50 pids after the harness), so likely kiln's own git
   work in the moved workspace (a carry/restore after the move?). The volume and mount stayed until removed by hand.
2. **Identical move-refusal notices collapse into one.** Two `[kind: unplaced]` refusals 3 s apart left one `notice`
   event in the session's timeline (B's conversations.db), so a caller can't count refusals or tell a second ask's
   answer from the first's.
3. **A workspace's first mount doesn't wait on Slates' `refused for now`.** Across a LAN the agent's start ended
   `its workspace: Slates' mount: refused for now (LeaseUnconfirmed { version: 7 })`; the moved-volume mount
   (`mounted_patiently`) waits on `Error::Retry`, the first mount doesn't.
