# The CLI on the library lost two Windows refusals' words (2026-11-18, A-278, A-299)

## Description

The integrator's Windows lane (run 44257181904, job "windows") failed two WinFsp tests in `crates/cli/tests/winfsp.rs`.
Both passed before the CLI mounted and unmounted through `slates-api` (A-278).

- `a_refused_mount_time_hold_leaves_nothing_mounted`: a mount refused while the anchor was silent printed
  `slates: the host refused: Target("the WinFsp mount at E: was refused: the anchor's hold: ...")`. The test looks for
  the refusal's kind, `TargetUnavailable`, and found only the contract's `Debug` of `HostFault::Target`.
- `unmount_releases_every_healthy_drive_beside_a_volume_recovery_could_not_rebuild`: `slates unmount` of a healthy
  drive exited 0 with nothing on standard error. The test expects the drive released and the volume recovery could not
  rebuild named (`volume unrebuilt`) by its refusal, `NotRecovered`, as the old CLI named it.

## Root cause

- `Error`'s `Display` rendered `Error::Host` with `{fault:?}`. `HostFault` had no `Display`, so the daemon's
  `TargetUnavailable` reached a person as the contract's variant name and a quoted string.
- `Instance::unmount` answered `Result<(), Error>`. Its Windows scan asks every volume's status to find the drive's
  attachment. It kept each volume whose status was refused (`Unreported`) but returned them only inside
  `MountFault::NotMounted`, when no volume reported the drive. When another volume reported it, the drive was released
  and the names were dropped, so the CLI had nothing to print.

## Impact

Windows CLI users, from A-278's move (2026-11-18) until the fix:
- a mount refused for the anchor's hold no longer named `TargetUnavailable`;
- an unmount beside a volume recovery could not rebuild no longer said that volume's mounts were unknown.

Any consumer printing `Error::Host` saw `Debug` text on every platform. No data was affected: the drive was released,
and the refusals were refused as before.

## Fix

- `HostFault` and `Unreported` render in words (`crates/api/src/error.rs`). `HostFault::Target` names the daemon's
  refusal, `TargetUnavailable: <the daemon's words>`, and `Error::Host` uses that rendering. `MountFault::NotMounted`
  names its volumes through `Unreported`'s rendering.
- `Instance::unmount` answers `Unmounted { unreported }` (`crates/api/src/mount.rs`): on Windows, each volume the scan
  could not ask when the drive was released; empty on macOS and Linux, whose unmount asks no volume. `CONTRACT` 16.
- The CLI's Windows `unmount` names each unasked volume on standard error whether or not the drive was released
  (`crates/cli/src/verbs.rs`, `name_the_unasked`).

## Evidence

- Red first: `a_host_refusal_is_named_by_its_kind_in_words` (`crates/api/src/error.rs`) fails on the old rendering,
  which prints `Target("...")` and never `TargetUnavailable`.
- `an_unreported_volume_is_named_with_its_refusals_kind` shows how the CLI's line names a volume recovery could not
  rebuild (`volume unrebuilt`, `NotRecovered`, `BaseMissing`).
- The two live tests need WinFsp, which only CI's Windows runner has (jupiter does not), so the integrator's Windows
  lane is their proof.

## Siblings

- The rest of `Error`'s classes render their details with `Debug`: `Denied`, `Exhausted`, `Busy`, `Stale`,
  `Unsupported`, `Invalid`, `Unusable`, `Elsewhere` and `Protocol`. Each names its kind as the type does, which the
  tests and the CLI rely on (`NotRecovered`, `BaseMissing`), but a `Note` inside one prints quoted. A rendering in words
  per class is owed (GAPS, A-299's owed list).
- `Exhausted(Quota)` is also what a refused retention charge reaches a consumer as. The volume core types it
  `VfsError::NoSpace`, so kiln's base-read refusal (A-342's bug, 2026-11-18) read as a quota that was never the bound.
  §4.2 lists `RetentionBudgetExceeded`; the wire does not carry it yet.
