# A VMM exit hid its final diagnostic

Observed 2026-11-02 while checking main CI for A-198 Step 9.4; the harness serves AC-9.7 and §4.6's VM tests.

## Description and cause

Main CI run 41487560076 passes all three Windows lanes and 19 of 20 jobs. Its QEMU job fails all eight
`guest_vhost` cases only at clean poweroff, after their functional histories finish and the probe says `PROBE done`.
The original job 125421781666 and isolated retry 125478439631 both return SIGABRT. All 23 server virtiofs cases pass.
The preceding main run 41423577167 passes the same eight cases with the same QEMU 11.1.2 Debian +4 package and Rust
base-image digest. The cause of QEMU's abort is not established by those results.

The shared `Guest::quit` consumes the probe's Done reply, then waits for process exit without reading its stdout or
stderr again. Its owned reader threads can enqueue the VMM's final error after that reply. The assertion therefore
prints a transcript ending at `PROBE done` and omits the diagnostic needed to name the shutdown failure. The
`vhost-user` launcher uses exec, so the observed failed process is QEMU itself.

## Failing test first and exact edit

`crates/cli/tests/vm_harness.rs` re-invokes its test binary as an owned console child. The child acknowledges quit,
then cannot print its final error until the parent has consumed that acknowledgement and sends a second command.
It exits with code 3, distinct from libtest selecting zero tests. The parent must retain both the failure status and
the subsequent stderr. Its first protocol reply starts on a new line so libtest's unfinished role banner cannot
hide it.

Before the fix, `cargo +1.98.1 test -p slates-cli --test vm_harness -- --nocapture` fails its diagnostic assertion on
adacachy in 0.03 s, with the expected child exit code and the transcript ending at the second command. The build/test
runs in an owned, bounded systemd unit with its own Cargo target; the diagnostic log is
`~/.cache/slates-s9-vm-final-error-red.log`.

`Guest::quit` now delegates its exit wait to `wait_for_exit`, which drains the already-owned console readers within
the existing QUIT_WAIT after observing exit. The existing bounded transcript keeps their last lines. No VMM exit
status, timeout or functional assertion changes. The direct harness oracle requires no VM or mount and runs on Linux.
The root QEMU failure remains a separate investigation; this edit exposes its final diagnostic.

With the drain, the unchanged oracle passes one test with one explicitly ignored child role in 0.02 s on adacachy
(2026-11-02, the same bounded command; `~/.cache/slates-s9-vm-final-error-green.log`).

The exact mapped-view case that fails at poweroff in CI passes with this harness on adacachy in the existing
`slates-guest:latest` image: QEMU 11.1.2 Debian +4, guest kernel 6.12.113+deb13-amd64, one test in 1.11 s. Command:
`cargo test --locked -p slates-cli --test guest_vhost a_guest_maps_a_session_file_through_slates_own_device_with_dax_and_sees_each_change_as_its_writer_is_answered -- --exact --nocapture --test-threads=1`, with the image's kernel/modules and a statically linked probe. Both the owned systemd unit and container use derived CPU/memory caps; the container is removed on every terminal path. This is a local positive result, not a fix for the two CI shutdown failures. The final CI diagnostic is still required.

The initial fixture incorrectly used `cfg(unix)` although all three users of `tests/vm` are Linux-only. Main CI
41529944478's Mac compile failed at the shared PTY helper's Linux-only OpenptFlags::CLOEXEC. The fixture now uses
`cfg(target_os = "linux")`, matching `guest_dax`, `guest_image` and `guest_vhost`; the existing VM targets and their
checks are unchanged. This corrects the new fixture's declared platform, without adding an unused Mac VM path.
