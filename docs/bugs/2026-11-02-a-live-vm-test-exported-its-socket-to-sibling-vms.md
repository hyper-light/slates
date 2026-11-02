# A live VM test exported its socket to sibling VMs

Observed 2026-11-02 in main CI 41535967224, job 125587750210, while checking the A-169 ring-stop correction.
This is a test-harness descriptor leak; the corrected backend must continue to wait for the frontend's socket close.

## Description, logs and cause

The live server suite passes 23 cases and fails `a_linux_guest_mounts_the_volume_through_qemu_over_vhost_user`.
Its guest powers off successfully at 22:21:05 UTC; its device-end assertion times out 20 s later:

```text
guest memory mapped by the device: peak resident 520 KiB of 262144 KiB, locked 0 KiB
the device ended: Timeout
```

The other guest workloads run concurrently; pjdfstest finishes at 22:29:35 UTC. In `run_qemu_with`, the test
cleared CLOEXEC on its frontend socket in the parent before `Command::spawn`. Every sibling VM spawned meanwhile
could inherit that same socket and retain it until its own exit. Dropping the intended parent's descriptor after
QEMU exit therefore did not close the backend's peer. The old first-ring-stop hangup hid this leak by ending the
backend before waiting for the actual socket close.

## Failing test first and fix

Extract the existing parent-side inheritance change into `spawn_vm_command`, preserving it, then add a real-exec
oracle: start two owned test-binary children while the parent holds both frontend capabilities. Each child sends
readiness over its own inherited socket and waits for a quit command. End the first child and drop its parent end;
the first backend must see EOF while the second child still runs. No flag or internal field is asserted.

```sh
cargo +1.98.1 test --locked -p slates-server --test virtiofs \
  vm_descriptor::a_vm_exiting_closes_its_socket_while_a_sibling_vm_keeps_running -- --exact --nocapture
```

On adacachy, 2026-11-02, before the fix this fails in 20.04 s at the existing guest-test bound: the first child
exited, but the sibling still holds its socket. With the fix it passes in 0.00 s. Logs:
`~/.cache/slates-s9-vm-inheritance-{red,green}.log`. Each role has an owner that kills and reaps it on an assertion's
unwind; readiness uses the socket so libtest's stdout banner cannot make a vacuous pass.

`spawn_vm_command` now consumes the command and borrows the original socket until spawn returns. Only the child
clears CLOEXEC, in
[`CommandExt::pre_exec`](https://doc.rust-lang.org/std/os/unix/process/trait.CommandExt.html#tymethod.pre_exec).
The closure makes only the async-signal-safe fcntl call and an allocation-free errno conversion; the parent keeps
its descriptor close-on-exec throughout. The original raw descriptor number remains the QEMU argument. No lock,
serialization, timing change or weaker device-end assertion is added.

## Live verification and siblings

In the existing `slates-guest:latest` image on adacachy, two real QEMU guests run together and pass in 6.74 s:
the original mount/device-end case and the fsx/fsstress guest (2,000 fsstress operations). QEMU 11.1.2 Debian +4,
kernel 6.12.113+deb13-amd64, KVM, 2026-11-02. Command:

```sh
cargo test --locked -p slates-server --test virtiofs a_l -- \
  --skip a_live_guest_runs_pjdfstest_over_the_tag \
  --skip a_live_guest_runs_the_roster_workloads_identically_on_slates_and_on_its_ram \
  --nocapture --test-threads=2
```

The image supplies `SLATES_GUEST_QEMU`, `SLATES_GUEST_KERNEL` and `SLATES_GUEST_INITRD`. The owned systemd unit and
disposable container have CPU/memory caps derived from one quarter of available resources, source mounted read-only,
and owned cleanup; the run plus build takes 92.147 s. No FUSE device or SYS_ADMIN capability is needed by this pair.
Log: `~/.cache/slates-s9-qemu-inheritance-fix.log`.

The final capped workspace gate chain passes in 15.415 s: `cargo +1.98.1 fmt --check`, the server's virtiofs test
target, `cargo +1.98.1 clippy --locked --workspace --all-targets -- -D warnings`, `cargo +1.98.1 xtask check`, and
`RUSTDOCFLAGS="-D warnings -A rustdoc::private_intra_doc_links" cargo +1.98.1 doc --locked --workspace --no-deps
--document-private-items`. Log: `~/.cache/slates-s9-vm-inheritance-gates.log`; production unsafe ceilings stay unchanged.

Sibling sweep (`rg 'fcntl_setfd.*FdFlags::empty' crates/{cli,server}/tests`, 2026-11-02): two fleet CLI fixture
helpers use the same parent-side pattern, `inheritable` and `EnrolledFleet::start_anchor` in `crates/cli/tests/cli.rs`.
Their cross-child lifetime has not been measured here. They are recorded for follow-up; this change fixes the shared
spawn path for all four live server VM cases.
