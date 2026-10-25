# The guest probe's last reply was cut short

- **Date:** 2026-10-25
- **Status:** fixed 2026-10-25, in the same change as this record.
- **Subsystem:** the real-VM tests' guest, `crates/guest-probe/src/guest.rs` (`power_off`).
- **Severity:** test diagnostics. A guest that could not boot reported why in a reply cut off mid-word, and the reply
  that answers `Quit`, which every real-VM test waits for, ran the same risk.
- **Exposure:** every test that boots `slates-guest-probe`: `crates/cli/tests/guest_dax.rs`, `guest_image.rs` and
  `guest_vhost.rs`, under crosvm and Firecracker alike.
- **Found by:** the "no DAX window" red-check of G-vhost step 3, which makes the guest's `dax=always` mount fail.

## Symptom

The three tests failed at the right check, but with the guest's reason truncated, two different ways in one run:

```
  left: Fatal("mount virtiofs tag slates at /mn")
  left: Fatal("mount virtiofs tag slates at /mnt: I")
```

## Root cause

`say` writes a reply and flushes the probe's own buffer, and `power_off` then calls `reboot(2)` at once. A write to a
terminal returns once the tty layer holds the bytes, not once the serial port has sent them, and the VM's end drops
whatever the port still holds. The host's reader got the line up to where the VM stopped, without its newline, and
parsed it as a whole reply.

## Fix

`power_off` drains the console with `tcdrain(3)` before it syncs and ends the VM ("tcdrain() waits until all output
written to the object referred to by fd has been transmitted", termios(3)). A drain that fails is said on stderr,
and the VM ends anyway.

## Verification

On adacachy (2026-10-25), the same red-check with the drained probe failed at the same check with the whole reason:

```
  left: Fatal("mount virtiofs tag slates at /mnt: Invalid argument (os error 22)")
```

and, with the fixed probe, `guest_vhost.rs` passed 3 of 3 twice, `guest_dax.rs` 3 of 3 and `guest_image.rs` 4 of 4.

## Siblings

None found: the probe is the only program that ends a VM, and `power_off` is its only way out.
