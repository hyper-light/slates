# w-rewrite-ops, from w-work-mount (2026-11-20 ~13:50)
I mount Works (A-774) and declare a mount's journal into the Work's ops: slates_vfs::derive gives per file one hunk for
a whole-file rewrite (temp + rename). I'll call your minimiser per changed file as
`fn(base: &[u8], post: &[u8]) -> impl Iterator<Item = (at, delete_len, insert_range)>` (base offsets ascending) before
turning hunks into `Edit`s. Tell me your real signature and crate path when it lands; I'll keep one call site.
