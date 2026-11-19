# A macOS mount's capability was readable by any group granted loopback capture

Date: 2026-11-18. Area: `crates/api/src/mount.rs` (`available`, the macOS and BSD loopback mount), `crates/api/src/
error.rs` (`MountMissing`). Design: §4.6 "macOS" (the loopback NFS mount, A-34), §4.13 (the mount capability, AUD-01);
amendment A-535. Goal 85. Found by `crates/server/tests/nfs_hostile.rs`'s review (GAPS, 2026-10-04: "plaintext
capability on loopback"), decided here.

## Description

A slates mount on macOS is an NFSv3 mount over loopback. Its capability, the attachment and its 16-byte token, is in
every file handle the kernel sends and receives, in the clear. Only root can capture loopback on a Mac as it ships, but
Wireshark's ChmodBPF gives every `/dev/bpf*` device to the `access_bpf` group to read. On such a host every member
could read each mount's capability off loopback and replay it from an NFS client of its own, under any uid it named:
`AUTH_SYS` is a claim. slates offered the mount anyway.

`a_loopback_mount_is_offered_only_where_no_one_but_root_can_capture_loopback` (`crates/api/src/mount.rs`) holds the rule
on every host. With the old rule, which judged no host, it failed at a device its group may read: capture was taken
for closed.

## Root cause

The mount's confidentiality rested on loopback being private, and nothing checked that it was. The macOS kernel's NFS
client speaks neither RPC-with-TLS nor RPCSEC_GSS, so this leg cannot be encrypted, and the authority a mount's handles
carry is exactly as private as loopback capture is.

## Impact

Hosts whose BPF devices a group or another user may read (ChmodBPF installed): any member could read and write every
mounted volume with that mount's rights, kiln agents' workspaces among them. None of Ada's five Macs is such a host:
each reads `/dev/bpf0` as `crw------- root wheel`, with no `access_bpf` group (checked 2026-11-18).

## Exact edits (A-535)

- `crates/api/src/mount.rs`: `available` on macOS and the BSDs answers the loopback mount only where capture is closed
  (`loopback_available(capture_open(bpf_devices()))`). `bpf_devices` reads each BPF device's mode and owner (`/dev/bpf`,
  then `/dev/bpf0`, `/dev/bpf1` until one is missing, a read of the nodes' metadata, never an open). `capture_open`
  judges them: a device a group or anyone may read, or one a user other than root owns and may read. `mount` calls
  `available` first, so a mount there is refused before anything is attached.
- `crates/api/src/error.rs`: `MountMissing::PrivateLoopback`, whose words name the devices and the fix.
- `crates/api/src/lib.rs`: contract 43. `docs/library.md` names the refusal.
- Docs: the amendment log (A-535), GAPS.

## The candidates the GAPS entry named, rejected

- Refusing NFS calls on a connection a user process owns: a daemon running as the user cannot tell another user's
  process from the kernel's socket (`proc_pidfdinfo` refuses other users' processes), and the kernel's source port is
  unprivileged, since the slates mount clears `NFS_MFLAG_RESVPORT` (R10).
- Rotating the token per mount: each mount's attachment has its own token already, so a sniffed token is the live
  mount's.
- Weighed too: binding a token to the first connection that uses it. It breaks clients that open several connections
  on one mount (the hostile suite's honest clients, Linux's `nconnect`), and a mount's first call still races a sniffer.

## Tests

The unit test above: red with the old rule on the Mac Studio (d1323898, debug); the module 12 of 12, the contract
history and the error module's tests with the new. Command: `cargo test -p slates-api --lib mount::tests`.

## Owed

The macOS mount suites on CI's runners (main's), whose BPF devices' modes are not known here: a runner that grants
capture will refuse every macOS mount, typed.
