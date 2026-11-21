# w-svn -> main: C2 stage B part 1 on main (40c3f8d1e), and P6/P7 numbers on stage A (2026-11-21 ~00:20)

Stage B part 1 (e69375553): a client's commit lands the texts it put in its changeset, in one sliced landing (Payload
streamed from the content store a piece at a time, a `Texts` entry per text, then the Record; one flush). The store
holds every text svn's history names. The commit no longer waits for the shard's image to keep its texts (A-727 slice
b's barrier, gone for svn); a recovery reads a stored text the image lacks back from the store. Tests: 136/136 affected
(the barrier test holds one text fetched back; the arbiter holds revision 1's Payload/Texts/Record).
Next in B: part 2, texts read through the owner so the content store need not hold them all (R6).

Measured on stage A (BENCHMARKS "Subversion on the changeset store", release, MacBook Pro 2, load 20-30, git-2000):
- P7 import: 24.5 s with 2,001 landings vs 8.2 s before the store: ~8 ms a revision is the per-changeset flush. Stage
  C (one sliced changeset an import) buys that back. RSS after import 4,065 MiB (5,582 before), records' texts 34.8 MB:
  the daemon peak is still ~3.6 GiB above start for a 458 MB dump, not yet attributed.
- P6: update r1000..r1500 over the tunnel 0.14 s (2.3 MB, whole), checkout r1500 0.46 s: the server's drive is not the
  2.7-5.6 s. The 524-byte refusal earlier was my script's (client's closing success sent mid-drive, E210004).
- svnsync replay 300 revisions: 0.73 s (2.4 ms each).
Stock legs (svn_interop, svn_http_reports, deltav-v1 stock) are queued on adas-mac-studio.local-2 as you said (kiln
r2); P8 (a_large_commit_measured, release) follows there after r2, not to stack two release builds on your box.
