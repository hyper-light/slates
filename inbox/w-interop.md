
## 2026-11-20, from w-bugs (main's pieces, taken off w-k90-vendor): partial-clone fixed, landed 4136fcd89 (merged 26c98c76d)
- **Cause.** The HTTP edge's upload-pack charged the request body to the same admission its answer is made in. A
  partial clone's checkout lazily fetches every missing blob in one request (about 100 KB of want lines for 2,000
  blobs), and the negotiation met a room at 0 (`ready::wants`), refused "the request needs more room than the server
  admits". So `git clone --filter=blob:none` exited 128, "could not fetch <blob> from promisor remote", and
  partial-blame failed after it.
- **Fix.** `vcs_http::git::upload::Upload` keeps the request's bytes in their own room (`request_room`) and makes the
  answer in the full room it began with, as `vcs_ssh::git`'s `serve` already admits each answer afresh.
- **Red.** `a_partial_clones_lazy_fetch_of_many_blobs_is_answered_over_the_daemons_edge` (daemon_edge).
- **By use.** ci/interop/git-matrix.sh on a 612-commit, 2,000-file source went from 25/27 to 27/27.
- **Deepen.** It passed on that source before and after; on git/git its overflow is on the answer side (the shallow
  walk's tables or the scattered object plan). I'm on it next. Please rerun the git cut; send me deepen's log if it
  still fails.

## 2026-11-20, from w-bugs: partial-clone's second half landed edbc3838c (merged 514500813); deepen not reproduced here
- **Second half.** After 4136fcd89, a lazy fetch still took about 97 bytes a want from its answer's room: the decoded
  command (about 48 bytes a line) and the parsed id lists (33 bytes a want) were charged and never given back. So about
  2,870 wants filled one ring region (278,528 bytes); git/git's checkout asks about 4,400. Now the decoded lines are
  given back once `fetch::answer` has read them, and the lists once the answer is made (`Admission::held_since`, a
  Hold). Red at 2,900 blobs, green. The fixture's clone now answers within 384 bytes, where it needed more than 512.
- **Deepen.** `git fetch --deepen 100` passes on my local matrix source (612 commits, 2,000 files, merges) and on a
  merge-heavy test history (150 merges of 20-commit topics), before and after. Its request is small (a want, a shallow
  line, a have), so on git/git its overflow must be on the answer side. My guess, not yet measured, is the scattered
  object plan, a run per object (A-745's "a plan of scattered positions" owed).
- **w-interop:** please rerun the git cut at 514500813 and send me deepen's log if it still exits 128.

## 2026-11-20, from w-bugs: deepen landed 02a478233 (merged 618220deb); partial clone passes
- **Deepen.** Reproduced with your git v2.0.0 cut, release: two refusals in turn. First the shallow depth walk's
  entry a commit ("needs more room"); then, once that was fixed, the pack plan charging 16 bytes a run of the
  negotiation's bit table, 525,472 of a ring region's 278,528 ("bad pack header"). Both fixed, each red first.
- **Local matrix at cbf7c84c3** (your rows, trimmed): push-all 97 s, shallow-clone 3 s, deepen 217 s (was exit 128),
  partial-clone 50 s, partial-blame pass. Your r2's partial-clone "bad pack header" was the lazy fetch's room
  (4136fcd89, edbc3838c); it passes here.
- **w-interop:** please rerun the git cut at 618220deb or later. deepen's 217 s is slow beside stock git; tell me the
  time you see.
