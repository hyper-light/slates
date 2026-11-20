# main <- w-fed-move: goal 91 step "The live move across two Mac runners" is DONE (2026-11-20 ~14:55 PST)

Please mark goal 91's step "The live move across two Mac runners" complete in kiln's UI.

**Proof by use** (kiln main 4c19f1aad's test, release, `SLATES_FLEET_LIVE_MOVE=adas-macbook-pro.local
SLATES_FLEET_LIVE_MOVE_HOST=192.168.1.88`, run 10): the fleet formed across this Mac Studio (.88) and adas-macbook-pro
(.86); the agent worked in its captured clone on the Studio; kiln moved it to the MacBook (at ask 9, after `unplaced`);
its turn there appended to the file its first turn wrote, inside the moved volume's mount on the MacBook, which held
both turns' work byte for byte; the old runner refused a write attachment by kind (`Busy(WriteLease { epoch: 1 })`)
and served nothing stale. One Mac, three runners, run 25: passed end to end.

**Landed**
- Slates main 0f681456 (A-756, fix 0fc610f9): a moved/taken-over volume claims its name's alias, so its mount finds it
  (was NOENT in 4 of 5 moves). Red/green fleet test `a_captured_clone_moved_to_another_node_is_found_there_by_its_name`.
- kiln 3d8e479cf (founder), 5a294413c (share with the target's Slates user), 4aabdba18 + 9129995e8 (the test),
  64412a674 (vendor patch 0004 = A-756, test, W7 docs), 4c19f1aad (test follow-up).

**Findings for you (by use, not chased)**
1. kiln: after a move, `agents.stop` left the new runner's mount up: `take_down` met `busy, held by 88916 git` once
   and asks only once by design; the git was gone minutes later (likely kiln's own git work in the workspace after
   the move). The test's last step fails on it; I took the MacBook's runner down by hand (unmounted first).
2. Slates: most first move asks are refused `unplaced` (the head not placed within the measured failover delay); across
   this Studio and adas-mac-studio-3 every ask in 5 minutes was (run 11). The forward lane logs its floor past the SLO.
3. kiln: identical move-refusal notices collapse into one, so a caller can't count refusals from the timeline.
4. kiln: a workspace's first mount doesn't wait on Slates' `refused for now` (`LeaseUnconfirmed` across a LAN); the
   moved-volume mount does.
5. This Studio's LAN address changed to 192.168.1.88 at the 10:35 reboot; adas-mac-studio-2 couldn't reach it for a while.
6. Cleanup owed: ~/.cache/kiln-live-move/9154583-live-c on adas-macbook-pro-2.local (.71, your MacBook; I picked it
   as a runner before seeing its address, files only, no runner or Slates started) — it was offline when I went to
   remove it.
