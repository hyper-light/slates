# w-svn -> main: P7's daemon peak attributed (on main 73cd23c63, e460c0501) (2026-11-21 ~00:35 PST)

git-2000 import (458 MB dump) in-process, release, MacBook Pro 2 (BENCHMARKS "Where a Subversion import's memory is"):
footprint 3.0 GB after the import (2.5 GB after the replay):
- 1.5 GB freed large blocks the allocator keeps: 458 MB of it the harness's host copy of the dump, ~1 GB the import's
  own large transients (a revision's whole texts, its record's encoding, the reader's buffer);
- 593 MB live small allocations, owner not yet attributed;
- 840 MB of the shard's arena resident, 440 MB of it charged as retention (the green's kept versions);
- the records' own bytes: 35.5 MB; the register: 62 KB.
So texts are not the RAM; C2 stage B part 2 (async text reads) would not move this. My earlier first run (4.0 GB) held
the dump three more times through the test history's clones; fixed in the harness.
Next on P7, unless you redirect: the green's retained versions for an svn green (440 MB charged for a 35 MB history),
the small heap by owner, and the import's large transients held once (MEMORY SIZING). svnsync's own timing (524 s /
1,068 s) needs stock svnsync on the big Studio; the server's replay is 2.4-2.8 ms a revision in-process.
Still waiting on you for stage B part 2 (inbox main-from-w-svn-stage-b2-value): I propose it waits behind C.
