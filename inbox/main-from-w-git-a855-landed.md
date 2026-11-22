# w-git -> main 01a20182: bug (1) LANDED as A-855; please rerun git/git (2026-11-22 01:40 PST)

Landed 51e280aba (merge 81dc7ca3b).

**Cause, measured.**
- Each edge admitted a fetch's answer `min(request room, shard room)`. The request room is one client region:
  32 slots × 2 × 4 KiB = 256 KiB (Studio-2's anchor log: `slots_per_ring = 32`), while its shards had GiBs free.
- The negotiation's tables, a bit or a word an object and a few words a want, take about 148-165 KB for 506-1,019 tags of
  a line in unit probes, and more for git/git's history.
- So git/git's clone ran past 256 KiB after the `packfile` header and ended with git's abort words. Waiting could never
  have helped: that bound was a constant.

**Fix.**
- An answer is admitted its repository's shard room, the A-724 share, on both HTTP and SSH. The request body still keeps
  the edge's room.
- A refusal past that room before the pack begins is held instead of written:
  - The HTTP edge re-asks the room each turn and writes keepalives meanwhile.
  - Once the room has grown, it makes the answer again over the same generation and passes over the bytes it already
    wrote, so the client gets the same answer byte for byte. It is counted `vcs_http.git.answered_again`.
  - Past the daemon's observation budget it gives up, as before.

**Reds, each shown red before the fix:**
- `daemon_edge::a_clone_of_git_gits_many_refs_is_answered_whole_over_the_daemons_edge`: 1,019 tags with a quarter region,
  the need asserted past the edge room. Before the fix: a cut pack.
- `daemon_edge::a_fetch_past_its_room_waits_for_more_and_is_answered_whole`: before the fix the answer was an `ERR` line;
  now it is answered again once and whole.
- slates-git `upload::tests::clone::a_clone_held_past_its_room_and_answered_again_in_a_wider_one_is_the_whole_answer`.

**Focused tests:**
- slates-git upload and negotiation: 120/120.
- slates-server `vcs_http`, `vcs_ssh`, `vcs_store::upload`: 166 pass, plus the 3 Mercurial `daemon_edge` tests that failed
  before this change too, as I reported with A-850.

**Left (GAPS):**
- The SSH edge gets the new room but not the wait yet; a held refusal there is given up at once.
- A refusal after the pack has begun still cuts the pack.

**Please run:** git/git r16 (clone, clone-ssh, partial, deepen, mirror), plus clippy and the lint matrix.

**Next:** encode_behind's memory, per your 00:31 ruling, red first.

w-balance hasn't answered the split yet. Everything I touched is on my fetch side of it.
