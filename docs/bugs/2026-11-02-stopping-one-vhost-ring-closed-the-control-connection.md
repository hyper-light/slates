# Stopping one vhost ring closed the control connection

Observed 2026-11-02 while requiring green main CI for A-198 Step 9.4. This is the §4.6 / A-169 device lifetime;
all three Windows lanes passed in main CI 41487560076.

## Description and logs

That run's QEMU job 125421781666 and retry 125478439631 fail all eight `guest_vhost` cases at poweroff, after their
functional histories finish. The bounded final-output drain in 4af76b1e exposes the error in main run
41529944478, job 125567224893, after `PROBE done`:

```text
Failed to read msg header. Read 0 instead of 12. Original request 11.
vhost VQ 1 ring restore failed: -5: Input/output error (5)
set virtio-user-fs status to 0 failed, old status: 15
Failed to set msg fds.
system/memory.c:2546: memory_region_del_eventfd: Assertion `i != mr->ioeventfd_nb' failed.
```

The clean-exit assertion stays intact. The earlier main run 41423577167 passed with the same QEMU 11.1.2 Debian +4
package; that did not prove the backend's stop rule.

## Root cause and impact

`VhostUserSeam::get_vring_base` replied to the first queue stop and set the whole seam's `stopped` flag. The control
loop then reported hangup; the device's terminal step closed its socket. QEMU stops its queues separately and still
needs that socket for queue 1's position and the remaining cleanup. It met EOF on request 11 (`GET_VRING_BASE`), then
aborted while removing an eventfd. The source and captured stderr agree on the failed exchange.

This contradicted A-169's existing rule that the device ends when the VMM closes its end. The
[vhost-user specification](https://qemu.readthedocs.io/en/master/interop/vhost-user.html#front-end-message-types)
requires a stop to drain the specified ring and return the device's next available descriptor index. Stopping all
rings suspends the device; control messages still work. SET_VRING_KICK restarts a stopped ring.
[QEMU's stop implementation](https://gitlab.com/qemu-project/qemu/-/blob/master/hw/virtio/vhost.c)
stops each queue before its remaining device cleanup (`do_vhost_dev_stop`).

The old reply also read `used.idx` from mutable guest memory, assuming it equalled the device's next available
index. A-196 can hold replies out of order; DAX can hold a taken request for acknowledgements. A stop must wait for
those requests rather than infer consumption from the used ring.

## Failing test first and exact edits

First change only the existing real-socket frontend test to stop queue 0 then queue 1, query control with both
stopped, and close the frontend before expecting terminal cleanup. On adacachy, 2026-11-02:

```sh
cargo +1.98.1 test --locked -p slates-server --test virtiofs \
  a_vhost_user_front_end_drives_a_guest_whose_file_the_host_reads_back -- --exact --nocapture
```

Before the fix: fails in 0.28 s, EOF on the second stop reply (`FrontEnd::stop`, line 981). After: passes in 0.27 s.
Logs: `~/.cache/slates-s9-vring-stop-{red,green}.log`. Each command runs in an owned systemd unit, with CPU/memory caps
derived from one quarter of available resources and a separate Cargo target.

- `virtqueue.rs` exposes device-owned taken/completed positions; equality proves drainage under the queue-size
  bound. `device.rs` forwards that progress without reading guest memory.
- `admission.rs` reports progress to the seam and serves only running queues. The in-process seam keeps running
  until revocation; the descriptor seam follows its protocol's ring state.
- `vhost_user.rs` keeps independent started and enabled states. One bounded pending stop per ring is answered
  after taken chains complete. Control remains connected; the VMM closing it still runs terminal cleanup.
- `serve.rs` settles stops at service boundaries and after held completions. A fully stopped, drained device runs
  no new DAX turn. Restarting by SET_VRING_KICK alone serves posted work.
- A second real-daemon test holds SETUPMAPPING at the frontend's acknowledgements and asks to stop. Control
  answers while the stop waits. The guest completion precedes stop position 2; a third request stays untouched
  until restart, then completes and leaves position 3.

## Verification and siblings

On adacachy, 2026-11-02, the in-flight/restart test passes in 0.64 s; `cargo +1.98.1 test --locked
-p slates-bridge-virtiofs` passes 89 tests, and all 24 server virtiofs cases pass (2.44 s, then 2.46 s after sharing
the test's stop/completion assertions; hardware-gated guest cases skip loudly).

The final workspace gate chain passes in 12.247 s in the same capped unit: `cargo +1.98.1 fmt --check &&
cargo +1.98.1 clippy --locked --workspace --all-targets -- -D warnings && cargo +1.98.1 xtask check &&
RUSTDOCFLAGS="-D warnings -A rustdoc::private_intra_doc_links" cargo +1.98.1 doc --locked --workspace --no-deps
--document-private-items`. Log: `~/.cache/slates-s9-vring-stop-gates-final.log`. No unsafe budget changes.

The existing `slates-guest:latest` image runs all eight real `guest_vhost` histories in 28.23 s, including the
original clean-poweroff assertions: QEMU 11.1.2 Debian +4, kernel 6.12.113+deb13-amd64, KVM. Command:
`cargo test --locked -p slates-cli --test guest_vhost -- --nocapture --test-threads=1`, with the image's kernel/modules
and statically linked probe. The owned systemd unit and disposable container both have derived CPU/memory caps;
the unit plus build takes 98.80 s and removes its container on every terminal path. Log:
`~/.cache/slates-s9-qemu-stop-fix.log`.

Main CI 41535967224 then passed the two-stop and in-flight/restart cases but exposed a live-fixture socket leak:
a sibling QEMU inherited the first VM's frontend because its CLOEXEC flag was cleared in the parent. The guest
powered off, but its backend correctly waited for that remaining peer. The separate failing real-exec test and fix
are recorded in `2026-11-02-a-live-vm-test-exported-its-socket-to-sibling-vms.md`; the device-end assertion stays intact.

Sibling sweep: the module header and previous socket test encoded the same wrong device-wide stop rule; both are
corrected. The old §6 credit paragraph assumed every reply completed before taking another; A-196 had already
replaced that assumption, and the paragraph now records the held-reply path and its queue-derived bound. A newly
attached device still starts at index zero: this change does not implement migration or reconfigure installed
ring layouts.
