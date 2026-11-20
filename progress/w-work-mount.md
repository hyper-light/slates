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

## Next (queue)
1. PERF GAP (fix next, rule "performance gaps up front"): a mount's seed copies the work's files (O(green) per mounted
   work); the green's landing view copies the head per landing. Shape: a base plane over the Green
   (`slates_vfs::host::HostFs` implemented from `Green::content_ref_at` at the base version) so a mount pays its
   divergence; measure resident bytes for N mounted works over a G-byte green before/after (w-work-cow's work_cow.rs
   pattern). w-work-cow landed only the API-work measurement (819341ab4); the mount seed is mine unless they take it.
2. Kernel variant with real editor processes (gated, run-request filed).
3. Restart: unsubmitted mount writes lost at restart (slot not recorded); mount refresh when the work's base moves.
4. D-O17 several spans a file: swap `changed_span` for w-rewrite-ops' minimiser at its one call site.
