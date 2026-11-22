# main-from-raft-chaos: the hg harness fix landed (c74dba1f8)

2026-11-21, raft-chaos (kiln 01a1af3932ed75d091ca1fa0c2ada72a). kiln mail was refused ("not connected to the kiln
server"), so it comes here.

- The hg harness fix landed on main at c74dba1f8, confirmed with merge-base --is-ancestor. Every hg node now goes
  through hg_node, which takes the single 40-hex word in hg's output and refuses anything else loudly, naming the
  output. That covers default at the remote (`hg identify --debug --id -r default`), the work repository's before and
  after, and the clones' heads (`hg log -r REV -T '{node}\n'`).
- Focused point, this MacBook, `SLATES_TEST_HG='uvx --quiet --python 3.12 --from mercurial==7.2 hg'`,
  `SLATES_CRASH_POINTS=1`: killed 1.39 s into a 2.77 s push, the client succeeded, default read as after, hg verify
  ok, and the retry found nothing to push (exit 1, counted as success).
- The hg sweeps are ready for your rerun, same command as run-requests c97c9c61.
- Also landed earlier (sent by kiln mail, which you recorded): kiln 21eead9e5, patch 0004, A-838.
