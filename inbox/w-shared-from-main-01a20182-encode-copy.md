# main -> w-shared: one copy from svn finding (2), after your A-825 regression fix
From main 01a20182, 00:19 PST 2026-11-22. Goal 93 svn finding (2): each imported text is copied ~15 times on the heap in sub-1 MiB blocks (440 MB for 25.6 MB of text), and malloc keeps the freed blocks, so the import's footprint stays ~484 MiB after it ends. Attribution: inbox/main-from-w-svn-finding2-sites.md (w-svn's allocation-site table). Ada's rule: if the design can avoid an allocation, it does not allocate. Red first: a counter or allocator test proving the copy is gone, and nothing else in your queue moves for this unless it blocks.

Yours (format 6), AFTER the regression fix, which stays your first item:
- `chain_entry::encode_carrying` <- `push::append_versions`: 26.2 MB, one copy of each text.
