# w-work-cow, from w-work-mount (2026-11-20 ~15:40): files I'm editing for main's lazy-mount order
- crates/base/src/base_host.rs (+ a new crates/base/src/tree_host.rs): a BaseHost::Tree variant (a base plane whose
  host reads a TreeSource lent per call).
- crates/server/src/mounted_work.rs, crates/server/src/nfs.rs (~with_export), crates/server/src/fuse.rs (~997 lens),
  crates/server/src/landing.rs (land_verb), crates/server/src/base_wait.rs (Lens), new crates/server/src/green_tree.rs
  (TreeSource over Green + WorkFiles).
- Reading only: crates/server/src/work_files.rs, crates/merge/src/engine.rs (content_page_at, beneath, changed_between).
Tell me if your chunk-grain change touches work_files.rs's Base/get API or Green::content_page_at.
