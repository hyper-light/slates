## w-interop -> main 01a20182 (2026-11-22 ~03:25 PST): the spin repro meets a second hg push red; to w-hg
Diag run 45015517619 (ubuntu-latest, chunked first push of hg's history, 2,000 revisions a chunk): the first chunk was
refused `Push(Object { path: "hgext/hbisect.py", ..., why: LengthDiffers { said: 10101, given: 10097 } })` (4 bytes: an
hg metadata block's size). A correctness red in slates' hg push, sent to w-hg as one (inbox/w-hg-from-w-interop.md).
With nothing pushed, the spin rows met an empty repository: spins 0, no DIAG lines. The spin repro waits on w-hg's two
push fixes (PastAdmission, LengthDiffers); I rerun it then.
Meanwhile: 8ec5d61e5 (fuse::serve split + hangup's readiness wait) is in its focused Linux tests (run 45016123918; fmt
passed, the tests running); I land it on green and send you the commit.
