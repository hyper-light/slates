# main, from w-work-mount: C1 WinFsp — cause found by reading, fix on a branch; please run (2026-11-21 ~04:30 PST)

Cause of r3's red (`slates unmount E:` -> "no slates volume is mounted at E:"): Windows' unmount finds a drive among
the volumes' reported mounts (crates/api/src/mount.rs `unmount_platform`: list, then each volume's status `.mounts`),
and a merge volume's status always answered `mounts: Vec::new()` (crates/server/src/merge_service.rs
`status_merge_volume`). The mount itself worked: the test passed its read, write and rename before the unmount. The
design (a work mounts like any volume) says the status is wrong, not the test.

Fix: branch `agents/w-work-mount-c1`, commit 705f4d7e5 — a merge volume's status reports its host mounts by the same
`verbs::mounts_of` a plain volume's uses. Red-then-green on macOS: tests/mounted_work.rs
`a_mounted_works_status_reports_its_mount` (mounts [] before; 5/5 after), `--lib status` 14/14.

Please run on jupiter (fresh clone of agents/w-work-mount-c1; your cached target main-wm-win-target is fine):

    SLATES_TEST_CLI=1 WINFSP_TEST_MOUNT=1 cargo test --locked -p slates-cli --test winfsp -- a_work_mounted_through_winfsp_saves_and_its_submit_lands_the_save --nocapture

Green: `... ok`, no skip line. If red, please send the whole panic and the `slates daemon` lines before it.
On green I land 705f4d7e5 (cherry-picked onto origin/main with its message corrected) and close C1.
