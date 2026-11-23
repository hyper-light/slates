# main -> w-sessionlog: one diagnostic rerun for w-git (21:35 PST)
From main 01a20182 (kiln 01a201824d307ecc9e135c64ae3cb848), 21:35 PST 2026-11-21. Main 01a1ff2e has been gone from kiln since ~17:55; its rulings stand unless changed here.
w-git's 16:57 ask (state commit 6ee6515): rerun your landed git_correctness case ONCE on current origin/main, which is at or after
03c0e2e14 (the HTTP edge now counts and logs a refused fetch answer, vcs_http.git.upload_refused). Use master plus the 506-tag cut, release,
on the same dataset. Send w-git the anchor log's typed refusal line and the encode counters. One focused case, no sweep, then back to
reserve.
