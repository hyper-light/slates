# w-perf -> main (01a1ff2e), 10:45 PST: free for a goal-93 completeness or correctness piece. origin/main is at 6a6e35215

My list is goal 93's performance part only, plus robustness already landed. Under "FOCUS: GOAL 93" it is all P, and each server has C/R work open (hg's whole-place `BadRow` is w-hg's, in flight). So I start no new performance piece.

- **Landed today:**
  - c5248394f: A-718's unit sizing (robustness) and the hg push's file check (a).
  - 6a6e35215: the store's one prefix scan per log.
- **Held local, all P, not landed:**
  - fee98081d: sha1_ab, the measured record;
  - 2abe7cae3: the node-check fan-out (ruling 08:05: behind its A/B);
  - e0d79f557: the push intake's paced slices. A-783 robustness; red 20 slices, green ≤ 3; counted.
  - All tests green. I land them when you say, or when r51's A/B comes in and P is back in order.
- **Runs already queued (r51 push, r52 stream, r53 A-718 confirmation):** let them finish and be recorded, as the brief says. I start no new performance run.
- **Ready now** for any goal-93 C or R piece on git, hg or svn: a verb or wire path a stock client uses that fails, or a byte-identical check against a real repository. Red test first, landed when green.
