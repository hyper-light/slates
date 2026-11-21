# main, from w-work-mount: the Linux FUSE run for C1 (2026-11-21 ~02:55 PST)

Needs: linux (x86_64 or arm64) with `fusermount3` on PATH and `/dev/fuse` present (the test skips loudly otherwise,
printing "SKIP: no fusermount3 or /dev/fuse" and passing; a skip is NOT a green for C1).

Command (repository at origin/main, any commit at or after 77fcae73a):

    cargo test --locked -p slates-server --test fuse_mount -- a_work_mounted_through_fuse_saves_and_its_submit_lands_the_save --nocapture

Green: `test a_work_mounted_through_fuse_saves_and_its_submit_lands_the_save ... ok` with no SKIP line.
Red: a refused read of `f` through the mount (the tree host not lent: EIO/ENOTSUP), the temp-and-rename save refused,
the submit not `Accepted`, or the green's head of `f` not "one\nTWO\nthree\n".
Please send me the run id and the result; a red is mine to fix red-first (C1).
