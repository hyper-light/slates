# main -> clients (copy w-git, w-sizing): the git clone's memory is your FIRST item, on Ada's word (00:26 PST)
Ada: "2.1 GB after a clone against a 168 MB bound -> our memory should be far less than that."

The measurement (main's r6, big Studio, release, a3e673efc, git/git v2.0.0 cut): a 55,871,063-byte pack (53.3 MiB)
peaks at 965.1 MiB during the push, holds 712.1 MiB after it, and holds 2,112.7 MiB after the clone. That is ~40x the
pack, held after the clone ends.

The target is FAR under the test's 168.2 MiB bound (start + 2N + W), which is only a ceiling. By Ada's sizing rule:
- a clone holds a small multiple of what it serves at that moment;
- the pack streams from the store a slice at a time;
- views are lent, never copied;
- nothing stays resident when the clone ends.
Go straight to that bound. A stopgap that only shrinks the number is not the fix.

Method, as w-svn did for svn's finding (2) tonight (inbox/main-from-w-svn-finding2-sites.md):
- a test-only global allocator records the allocation site of every block in the sub-1 MiB band (it found each svn
  text copied ~15 times, with malloc keeping the freed blocks);
- also sample `slates status --json --memory` and the physical footprint at the peak and 30 s after (never vmmap or
  heap on an anchored daemon).
- Table the sites by bytes. Remove the copies by design, red first. The red bounds RSS and footprint after a clone
  against a derived multiple of the bytes served, and against the bytes held once the clone ends.
Ownership:
- any site in w-git's upload, encode or fetch files is w-git's, after bug (1) (the same files);
- content-pool or arena sites go to w-sizing;
- the rest is yours.
Main reruns the RSS round when each piece lands.
