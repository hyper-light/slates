# A renamed WinFsp view kept its old bytes

Observed 2026-11-02 while completing A-198 Step 9.4.

## Description and measured failure

The CLI oracle opens a mapped view of `file.bin`, renames the file to `renamed.bin` through the SDK, kills the
daemon, reconnects to its successor and writes the same inode through `renamed.bin`. The original view must follow
the file. On arm64 it instead keeps its earlier bytes for the ten-second deadline. The test fails in 11.79 seconds
in [run 41459980132](https://github.com/hyper-light/slates/actions/runs/41459980132), diagnostics branch
`diag-s9-rename-red-1`, commit `ae668ec1020e16a4721c87b0beda4072b83aa8ec`.

Command: `cargo test -p slates-cli --test winfsp a_winfsp_view_of_a_renamed_file_follows_its_successor -- --exact --nocapture --test-threads=1`,
with `SLATES_TEST_CLI=1 WINFSP_TEST_MOUNT=1`.

The same runner passes the taken-read oracle with one counted captured request, all seven earlier active CLI tests,
zero lost drives of twenty, the foreign-process refusals, anchor regressions and native documentation. The new
failure concerns cache identity across a namespace change; keeping the volume and settling the taken request alone
does not establish that property.

## Root cause

Recovery previously derived an open's coherence name from `Volume::path_of_inode`. After the SDK rename this is the
new name. The held ledger records file identity, but not the name of the kernel's cached file node. WinFsp's
notification implementation looks that node up by its exact name, rather than finding every node of an inode.
[WinFsp notification source](https://raw.githubusercontent.com/winfsp/winfsp/v2.1/src/sys/file.c),
`FspFileNodeInvalidateCachesAndNotifyChangeByName`.

The existing A-190 coherent purge also opens its target by path through the drive. Even when the old name is known,
the VFS namespace may no longer contain it. A control operation that reaches the held file by identity needs to be
distinguished from an ordinary caller's namespace lookup; recreating a user-visible old name would be wrong.

The trace in [41465398912](https://github.com/hyper-light/slates/actions/runs/41465398912) distinguishes both losses.
Without a kill, the purge targets `\file.bin` and is refused once. After takeover, reconstruction targets
`\renamed.bin`; two purges and two echoes complete there while the original view still keeps its old bytes.

## Correction

The held ledger now records the kernel's original name, including aliases, independently of the namespace home.
Two checksummed versions per slot preserve a complete name through a torn rename update. The section reserves the
format-derived name cap and commits touched ranges only. A saved successful kernel rename resumes its name updates
one context per shard turn before its response reaches the driver.

A purge carries both the retained name and file identity. Its drive opens use a mount-specific restricting SID in a
less privileged copy of the daemon's token. Only that marker and the daemon's PID authorize identity resolution;
ordinary opens still resolve the namespace and cannot revive a removed name. The native marker-copy and path-scope
oracles pass. The callback-free channel oracle exposed a separate progress defect, recorded in
[the private-purge progress bug](2026-11-02-a-private-purge-waited-for-a-callback-its-refused-open-could-not-produce.md).
The remaining live failure was then traced to
[a missing volume subscription after an empty round](2026-11-02-a-winfsp-empty-round-left-the-next-change-unheard.md):
the rename could consume its directory-only invalidations and park without hearing the following file write.
The mount now rearms at the delivered generation before parking.

## Verification

Diagnostics [41484542209](https://github.com/hyper-light/slates/actions/runs/41484542209), commit
`a7e5bce5c4ec70c9773a434887cc66c9b865a90e`, pass without operation traces on both Windows architectures. Each
runner passes all nine live CLI tests and loses zero drives in twenty kills; every iteration requires the original
handle's counted read and the original mapped view following the successor, with four restored reads in the final
daemon. The live renamed view records one completed purge/echo; the renamed takeover records two. Neither revives
the removed public name, and no purge is refused or kept. All six separate foreign process-rights requests are
denied, and the independent second daemon is refused while its original owner still serves.

The same run passes all 54 bridge tests, one separately enabled real-driver captured-read oracle (one counted
settlement, 0.15 s on x86_64 and 0.19 s on arm64), all 32 active anchor tests, native clippy and private Windows
documentation. The CLI histories take 41.51 s on x86_64 and 21.76 s on arm64. Mac formatting, workspace clippy,
structural/literal/unsafe checks and private workspace documentation pass in 92.15 s on the same code.
