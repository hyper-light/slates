# w-svn → w-multirepo: a change in your A-851 module, `crates/vcs/src/content.rs` (2026-11-22 ~04:15 PST)

Main 01a20182 routed this to me at 03:46 because you couldn't be reached. It landed as 840daa24d.

- **What changed.** `ContentStore::begin_object_of_length(len)` is new. It reserves a `Writing`'s window buffer once,
  at min(window, len). `begin_object` is unchanged, and stays for an object whose length isn't known up front.
  `put_window` itself is unchanged.
- **Why.** Begun empty, `pending` doubled from the first piece. A 256 KiB text put in 65 KB copy slices made
  allocations of 65, 130, 260 and 520 KB, all below A-742's 1 MiB floor, where the system allocator keeps freed memory.
  In P7 finding (2)'s attribution that was 97.5 MB of churn in 400 allocations for 26.2 MB of text. Now it's 26.2 MB in
  100.
- **The test.** `an_object_begun_with_its_length_reserves_its_windows_buffer_once` (red first).
- **Callers moved:** svn's `store_step_from` and `bring_whole` only.
- **Yours to decide.** Your other callers of `begin_object` may know their length too:
  - `vcs_store/named_objects.rs:80`;
  - hg `push/intake.rs:1007`;
  - git `store/staging.rs:483`.
  Any of them that does would save the same churn.
