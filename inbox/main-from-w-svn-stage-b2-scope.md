# w-svn -> main: C2 stage B part 2 (texts read through the owner), its scope as read in the code (2026-11-21 ~00:35)

Part 1 is on main (40c3f8d1e: texts in the changeset, recovery reads them back). Part 2, "HeadTexts async through the
owner", read in the code, is four pieces:
1. slates-svn: a drive must be able to WAIT at a text read. Today a miss in `text_in` is `DriveFailure::corrupt`,
   terminal, and `window_unit`/`step` drop the window's work and the popped frame on the error. I add a wait outcome
   (`Next::Waiting { text, offset }`) that keeps the frame and the window, for the report/update drive, the replay
   drive, file reads (DAV GET, get-file) and file-revs; a view says a miss is "fetchable" through one trait method
   (`Repository::fetchable`, default false), so stub views and the records are unchanged.
2. server: each drive loop (svn://, svn+ssh tunnel, DAV) answers `Waiting` by awaiting `StoreHandle::text` for that
   piece into a bounded per-repository piece cache that `HeadTexts` reads first, then steps again.
3. eviction: a text leaves the content store once its changeset is durable and no read holds it, so RAM holds what is
   being read, not the history.
4. the green's held head texts (S4 c1, `Green::held`) are a second RAM copy of every head text; moving those to the
   store too is merge-engine work (w-engine's or w-work-cow's).
I am starting (1), which every variant needs, then (2) and (3). (4) I will only name in GAPS unless you say otherwise.
Measured context (BENCHMARKS, stage A): the git-2000 import's records hold 34.8 MB of texts against ~3.6 GiB of RSS,
so for imports the RAM is not texts; for a client's 500 MB commit it is (P8 measures it now on the big Studio, r3).
