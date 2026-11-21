# w-git -> main: the whole-history push cut-off, named (2026-11-21 ~02:00 PST)

**Repro (Studio-2, main 5ee55fde3, release, `--vcs-store`).** A stock git push of `~/.cache/w-interop/git-git.git`
(611 MB) over the HTTP edge fails at 72-76 s with curl 18, as w-interop saw. The small cut-1.0 history pushes fine (5.1
s, landed, homed twice). The status counters showed no refusal, restart or kill, and no homing or landing: the push
never reached its decision. Scripts are in `~/.cache/w-git-diag/push_repro.sh` and `cut_patch.py` (a diagnostic patch,
never for main).

**Which side closes, and why (the diagnostic build's connection log):**
```
DIAG CUT body 64 / 128 / 192 / 256 MiB            the request's body, read whole
DIAG CUT body_end Ok(()) after 2826 ms
DIAG CUT reply gap 45031 ms, then 5000, 5000, 5000  keepalives every 5 s (R2's fix holds)
DIAG CUT reply refused Failed(Status { code: 413, reason: "Content Too Large" })
DIAG CUT connection end why=engine closed ended=Some(Aborted)
```
The server closes it. About 65 s in, an answer step refuses the push `PastAdmission`: the push's working set outgrew
the room its shard admitted it (`exchange_status`'s 413). The reply's head went out at the request (`StatusMode::AtHead`),
so the 413 can't be sent, and the edge aborts. git sees that as curl 18.

**Two layers:**
1. **A refusal after the head is a broken connection instead of git's words.** git's receive-pack reports a failed push
   in-band: `unpack <reason>` and each ref `ng`, or band 3 under side-band. Fix: the HTTP site answers every refusal
   that comes after the head in git's words, so the client prints the reason. That's mine; red test: a push refused past
   its admission mid-answer over the HTTP edge gets git's report, not an aborted connection. Starting now.
2. **A whole-history push doesn't fit the room its shard admits.** Stock git takes it in 10.77 s. The push holds its pack
   in place (B2) plus what checking and staging build, all charged to the shard's room. A-719's rule (a push's working
   set bounded and independent of its size, admission by the slice in flight) isn't met for the answer's phases. I'll
   name the phase whose charge passes the room in the same diagnostic run and send its numbers. That's a design piece
   (the answer's working set sliced); tell me if you want it before C5 2b's piece 3.
