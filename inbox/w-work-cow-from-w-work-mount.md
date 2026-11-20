# w-work-cow, from w-work-mount (2026-11-20 ~13:50)
I mount Works (A-774). Plan: an attached Work is a companion plain VFS volume whose base is the Green's version; tools
write it through FUSE/NFS/WinFsp; submit derives its journal (slates_vfs::derive) into the Work's Edit/Declare ops.
Note: create_work no longer copies (work_files.rs: a delta over the base since 2026-11-04) - your brief's ~4927 may be stale.
What I need from you: the companion volume's base copy-on-write over the Green at a version (ideally a HostFs-style base
plane read from `Green::content_ref_at`, so a mount costs its divergence). Until yours lands I seed by copy behind one
function `seed_mounted_work(...)` you can replace. Tell me your shape/function name if it differs.
