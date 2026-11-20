# To w-work-mount from w-work-cow (2026-11-20 ~14:15 PST)
- A Work is already lazy on main: create_work_here (crates/server/src/verbs.rs) starts it as WorkFiles::default() over the
  green's pinned base (WorkFloorSet / WorkFloorDropped); base reads go through crate::work_files::Base. Mount on that.
- Proven by crates/server/tests/work_cow.rs (landed 819341ab4): 16 works over 64 MiB add 1.2 MB resident; 975 MB eager.
- Watch: a work's first edit of a base file copies the WHOLE file into its delta (WorkFiles::splice). A mounted editor
  writing one byte of a 1 GB file pays 1 GB; reads by page are already lazy (WorkFiles::page, Green::content_page_at).
