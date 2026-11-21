# w-work-mount progress (A-774)

Brief: briefs/w-work-mount.md. Checkout: ~/slates-work/w-work-mount (clone, --reference w-balance --dissociate).
Target dir: ~/.cache/w-work-mount-target.noindex (CARGO_INCREMENTAL=0) -- set CARGO_TARGET_DIR on EVERY command.
Signing: kiln's GIT_CONFIG_* env beats `-c commit.gpgsign=false`; use `git commit --no-gpg-sign`, `git merge --no-gpg-sign`.

## Landed
- 2026-11-20 14:20 a2415c930 (merge 089bc78a0 on origin/main): pieces 1-3 + by-use test.
  - crates/server/src/mounted_work.rs: `ensure_slot` (a work's slot under its own id on first attach/NFS mount, seeded
    with its files over its base, snapshotted = mount base, `state.merge.mounted`); `declare_mount` (submit first
    derives the mount's touched paths since its base and declares each through `verbs::edit_op`/`declare_op`, one
    splice a file via `changed_span`; then re-snapshots the base); `ensure_green_view` (land of a green = its head
    seeded into a slot under the green's id, `state.merge.green_views`, then `land_verb_os` as any volume, grant R10);
    `release` at destroy.
  - Hooks: verbs::submit (declare_mount), verbs::attach (ensure_slot for works), nfs.rs entries()/with_export
    (ensure_slot), landing::land_verb (green view), merge_service::destroy_merge_volume (release).
  - Recovery rebuilds only Role::Plain slots, so a merge slot is ignored at restart and rebuilt on use.
  - Test crates/server/tests/mounted_work.rs: 10 editors (temp + rename over daemon NFS), verdicts 7 accepted, identical,
    accepted, conflict; grant lands; landed bytes == oracle; 0.80-0.90 s debug, macOS arm64, this Mac, local.
    Red on f727b9ec4: MNT of a work answered MNT3ERR_NOENT; then land refused "landing a merge volume".

## Main's order 2026-11-20 15:20 (current queue)
1. NOW lazy base read (mount + landing copy nothing but edits). Red test written: crates/server/tests/mounted_work_cow.rs
   (uncommitted in ~/slates-work/w-work-mount). RED numbers (debug, this Mac, base 779840fba): 8 mounts over a 64 MiB
   1024-file green add +548 MB resident (68.5 MB/mount, bound 11 MB); mount made in 34.2 ms median vs 1.6 ms over a
   16-file green; presenting the green's landing +69 MB. NOTE test-profile attachment cap ~15 per partition: keep
   mounts <= 14 per test.
   DESIGN: overlay (VFS base plane) whose host is a new `slates_base::BaseHost::Tree(TreeHost)`: TreeHost holds only
   handle tables; it answers HostFs only through `TreeHost::lend(&dyn TreeSource)` -> a lens; plain lends refuse
   typed (BaseUnavailable). Server implements TreeSource over (&Green, version, &WorkFiles) [mount] or (&Green, head)
   [landing view]. Lens sites: nfs.rs ~392 (with_export, destructure adds `greens`, `works`), fuse.rs ~997, landing,
   declare_mount. Submit declares from `Volume::diverged()` (base.rs ~948) read through the Overlay with the lens.
   Old version listing: Green::beneath(head) corrected by changed_between(pin, head) (no files_at walk).
   LANDED 17:05 f6660f0ab (main 7f8eb5cbb), release A/B in BENCHMARKS. Was: STATUS 16:40: mount half DONE in tree: crates/vfs/src/host/tree.rs (TreeHost/TreeLens/TreeSource),
   BaseHost::Tree, verbs::admit_over/AdmitBase, server green_tree.rs, mounted_work.rs rewritten (overlay + diverged
   declaration, MountPin in reachable_floor), nfs.rs/fuse.rs lend sites. Debug: 8 mounts +1.5 MB (was 548 MB), mount
   1.4 ms vs 0.6 ms small (was 34 ms). Both mounted_work tests green. Release A/B in ~/.cache/w-work-mount-ab (ab3.log).
   GOTCHA: never build two worktrees into one target dir -- release artifacts got poisoned (cargo took the other tree's
   slates-vfs/db as fresh); the A/B's before side uses ~/.cache/w-work-mount-ab-target.noindex (delete after).
   NEXT (part 3): landing of a green without a head copy: land crate needs a read-only base host beside the target
   host (Source::read_at uses landing.host) + PlanWalk listing untouched tree-base entries as created.
   PART 3 DESIGN (17:15): green landing view = overlay over the tree host (like a mount, pin = head; reuse
   mounted_work::lend). Land crate: (a) PlanWalk::materializing(vol, source): a stage walking the merged tree through
   Overlay::readdir (host = the tree lens) emitting Diverged{Created} for every entry, then the existing Entries stage;
   (b) Beginning::materializing + Beginning::unit_over(host, base: Option<&mut dyn HostFs>, ..): plan reads via base;
   (c) LandingRun::step_over(.., base) threading base to copy_window's Source::read_at (engine.rs ~1903); finish skips
   the overlay advance for a materialized plan (flag carried Beginning -> Planned -> LandingRun, NOT a LandingRequest
   field: ~25 literals). Server landing.rs: green view path like a scratch volume's (prepared.os, Writer::Own), and
   lend the tree lens at beginning.unit (~2010), run.step (~2135), finish_step (~2194). Red: presenting/landing the
   64 MiB green adds <= 8 MB resident (was +69 MB); ten-editor test still lands == oracle.
2. Restart durability of unsubmitted mount writes (red: save, restart, submit, save is there; A-127 anchor segment).
3. D-O17 multi-span: w-rewrite-ops' minimiser on main 561c655a3, one call site (`changed_span` in mounted_work.rs).
4. Kernel-mount real-editors variant -> run-requests.md only.
w-work-cow fixes "first edit copies whole file" at chunk grain in the content layer; tell them files via inbox first.

## Next (queue, older)
1. PERF GAP (fix next, rule "performance gaps up front"): a mount's seed copies the work's files (O(green) per mounted
   work); the green's landing view copies the head per landing. Shape: a base plane over the Green
   (`slates_vfs::host::HostFs` implemented from `Green::content_ref_at` at the base version) so a mount pays its
   divergence; measure resident bytes for N mounted works over a G-byte green before/after (w-work-cow's work_cow.rs
   pattern). w-work-cow landed only the API-work measurement (819341ab4); the mount seed is mine unless they take it.
2. Kernel variant with real editor processes (gated, run-request filed).
3. Restart: unsubmitted mount writes lost at restart (slot not recorded); mount refresh when the work's base moves.
4. D-O17 several spans a file: swap `changed_span` for w-rewrite-ops' minimiser at its one call site.
