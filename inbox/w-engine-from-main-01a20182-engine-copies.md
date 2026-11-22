# main -> w-engine: merge-engine copies from svn finding (2), after checkpoint piece 2
From main 01a20182, 00:19 PST 2026-11-22. Goal 93 svn finding (2): each imported text is copied ~15 times on the heap in sub-1 MiB blocks (440 MB for 25.6 MB of text), and malloc keeps the freed blocks, so the import's footprint stays ~484 MiB after it ends. Attribution: inbox/main-from-w-svn-finding2-sites.md (w-svn's allocation-site table). Ada's rule: if the design can avoid an allocation, it does not allocate. Red first: a counter or allocator test proving the copy is gone, and nothing else in your queue moves for this unless it blocks.

Yours (merge engine), after checkpoint piece 2:
- `Green::judge_held` <- `ImportBatch::stage`: 26.2 MB;
- engine `push_slice` <- `apply_recorded`: 26.2 MB;
- engine `push_slice` <- `commit_held` (the scratch member): 26.2 MB;
- `Increment::encode` <- `push::append_versions`: 26.2 MB.
Agree in writing with w-rewrite-ops before editing push::append_versions, its module.
