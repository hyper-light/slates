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
   PART 3 LANDED 18:00 f401f798c (main 5d29b9572); presenting 69 MB -> 1.6 MB release. Design was (17:15): green landing view = overlay over the tree host (like a mount, pin = head; reuse
   mounted_work::lend). Land crate: (a) PlanWalk::materializing(vol, source): a stage walking the merged tree through
   Overlay::readdir (host = the tree lens) emitting Diverged{Created} for every entry, then the existing Entries stage;
   (b) Beginning::materializing + Beginning::unit_over(host, base: Option<&mut dyn HostFs>, ..): plan reads via base;
   (c) LandingRun::step_over(.., base) threading base to copy_window's Source::read_at (engine.rs ~1903); finish skips
   the overlay advance for a materialized plan (flag carried Beginning -> Planned -> LandingRun, NOT a LandingRequest
   field: ~25 literals). Server landing.rs: green view path like a scratch volume's (prepared.os, Writer::Own), and
   lend the tree lens at beginning.unit (~2010), run.step (~2135), finish_step (~2194). Red: presenting/landing the
   64 MiB green adds <= 8 MB resident (was +69 MB); ten-editor test still lands == oracle.
2. NOW (18:00) Restart durability of unsubmitted mount writes. Facts: publication already images the mounted overlay
   (TreeHost::fingerprint_dir answers without the tree); recovery (verbs.rs ~17355 `Role::Plain => rebuild_volume`)
   skips merge roles, so the slot is rebuilt fresh from the work and the writes are lost. Plan: (a) red test in
   tests/mounted_work.rs style over SegmentSource::Handoff restart (see crates/client/tests/client.rs restart oracle):
   mount, save, barrier, restart over the same segment, submit, expect the save in the green; (b) recovery: for a
   Role::Work record with an image, rebuild an overlay slot with BaseHost::Tree and re-insert the pin
   (pin version = the work's durable base_version; make TreeHost identity depend on the green only, so a re-pin shows
   drift only where bytes differ); greens must be recovered before (check order); base plane reacquire needs the lens
   (open_dir) at recovery -> lend the green there.
2b. Restart durability (red: save, restart, submit, save is there; A-127 anchor segment).
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

## 2026-11-20 18:40 — main's 15:20 order done
1. Lazy base: part 2 f6660f0ab (mounts: release 547 MB -> 0.75 MB for 8, 9.4-12.9 ms -> 0.5-0.8 ms), part 3 f401f798c
   (green landing: presenting 69 MB -> 1.6 MB, release A/B in BENCHMARKS).
2. Restart: part 4 9d73a77c6 (recovery rebuilds a mounted work's slot from its image over its green).
3. D-O17: part 5 1b0f5679c (declare whole-file replacement; A-776 refine at composition).
4. Kernel real-editor variant: run-requests.md.
Remaining GAPS (A-774 row): mount not refreshed when its work's base moves; a work with declared changes refused a
mount; only NFS/FUSE lend the tree (WinFsp, virtio-fs, fs verbs refuse typed); green on another shard mounts nothing;
re-landing a green into a target holding an earlier landing conflicts on changed files (needs witnesses); the views'
tree loads in one step (slice owed).
Lessons: never build two worktrees into one target dir (poisoned release artifacts); `git commit --no-gpg-sign`.

## 2026-11-20 ~17:00 — Ada's close list (briefs/close-list.md) DONE
C1 77fcae73a (FUSE+WinFsp tests written, runs in run-requests), C2 1b0f5679c, R3 9d73a77c6, R4 60a71db8a, P5 f6660f0ab +
f401f798c. Close notes: inbox/main-from-w-work-mount-close.md. Queue empty; new findings go to GAPS only.
Target on Studio-2 kept at ~3 GB (focused tests, CARGO_PROFILE_TEST_DEBUG=0).

## 2026-11-21 ~01:00 PST — C1 Windows runs, blocked on infrastructure
Build GREEN on jupiter (e4cfbd38d, 31m47s), one unused-import warning -> fix 0cbef8441 on agents/w-work-mount-win.
WinFsp test pending: kiln server unresponsive since 08:07 UTC; GitHub fetch "early EOF" from this Mac (SSH and HTTPS).
Next when both answer: on jupiter (--on jupiter --no-sync; clone agents/w-work-mount-win with retries into
~/Projects/w-work-mount-win; CARGO_TARGET_DIR ~/Projects/w-work-mount-win-target, 11 GB warm) build with no warnings,
then `cargo test -p slates-cli --test winfsp a_work_mounted_through_winfsp`; green -> merge origin/main into the fix,
push HEAD:main, add to the close note; red -> red-first fix. The Linux FUSE run is mine too.

- 2026-11-21 02:30 PST: kiln refused every run 08:07-10:30 UTC (10 retries, "did not answer in time"); GitHub back at ~09:00 UTC (state pushed). WinFsp test and the warning fix 0cbef8441 still wait on kiln; nothing else in my queue.
