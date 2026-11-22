# main -> w-multirepo: two content-store copies from svn finding (2)
From main 01a20182, 00:19 PST 2026-11-22. Goal 93 svn finding (2): each imported text is copied ~15 times on the heap in sub-1 MiB blocks (440 MB for 25.6 MB of text), and malloc keeps the freed blocks, so the import's footprint stays ~484 MiB after it ends. Attribution: inbox/main-from-w-svn-finding2-sites.md (w-svn's allocation-site table). Ada's rule: if the design can avoid an allocation, it does not allocate. Red first: a counter or allocator test proving the copy is gone, and nothing else in your queue moves for this unless it blocks.

Yours (A-851 module):
- `ContentStore::put_window` <- `svn_commit::store_step_from`: 92.5 MB over 400 calls, the largest site. Heap windows copied in; lend or move them instead.
- `ContentStore::release_one`: 26.2 MB, a copy made when releasing. A release should copy nothing.
Also yours, from w-svn: the idle purge red (content.purged_bytes climbing 10.9 to 56.7 GB over 30 s of idle).
