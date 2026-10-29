# QEMU's memory table after a map ended slates' own device (2026-10-29)

**Status:** fixed the same day, failing test first (A-189).

## Description

The first real run of slates' own virtio-fs device under QEMU 11.1.2 (diagnostics run 40874255591, x86_64 with KVM
and arm64 under QEMU's own emulation; `crates/cli/tests/guest_vhost.rs`) booted the guest, mounted the tag with
`dax=always` and mapped the file (`PROBE mapped dax=1`). The guest's first read through the map then returned memory
that was no longer the file: every test failed its first comparison. QEMU's own lines named the moment:

```
qemu-system-aarch64: vhost_set_mem_table failed: Input/output error (5)
qemu-system-aarch64: Failed to write msg. Wrote -1 instead of 52.
qemu-system-aarch64: vhost VQ 0 ring restore failed: -22: Invalid argument (22)
```

The read gave zeros on x86_64 and 0xff on arm64: unassigned memory, since QEMU had unmapped the window when the device
went away.

## Root cause

QEMU puts every shared memory section that has a descriptor in the memory table it sends a vhost-user back end
(11.1, `hw/virtio/vhost.c` `vhost_section`: a RAM section is kept when it has a descriptor and is mapped shared, the
test `vhost_no_private_memslots` asks for vhost-user). The range QEMU maps for a
`VHOST_USER_BACKEND_SHMEM_MAP` is such a section: RAM from the back end's descriptor, mapped shared. So each map the
device places comes back as a new `SET_MEM_TABLE` with the range added, under running rings, and each unmap as one
without it. slates' back end refused any memory table once a ring was configured (`set_mem_table`: "the memory table
changed under configured rings"), a rule written for hotplug, which would move what the device's ring layouts name.
The refusal is a protocol fault, so the device ended at the guest's first map. crosvm never sends such a table: it sends
one table, at activation, built from the VM's RAM (`devices/src/virtio/vhost_user_frontend/mod.rs` `set_mem_table`
over `GuestMemory::regions`, called from `activate`, at its main a615111b7f), and places the window through its own
memory client. That is why the five tests passed under crosvm.

## Impact

Under QEMU, slates' own device served a guest until its first DAX map and then ended. Nothing was lost that the guest
had synced; its later requests found no device. crosvm was not affected.

## Reproduction

`crates/server/tests/virtiofs.rs` `a_front_end_that_puts_each_placed_map_in_its_memory_table_keeps_its_device`: the
test front end sends QEMU's table after the map's acknowledgement. Before the fix it fails at that table, the back end
answering status 1 (adacachy, 2026-10-29, 1 run of 1).

## Exact edits

- `crates/bridge-virtiofs/src/vhost_user.rs` `set_mem_table`: a table under configured rings is taken when every region
  a configured ring lies in is still in it unchanged: the same object, by device and inode (`ObjectIdentity`, since a
  front end re-sends each region under a new descriptor), at the same guest and front-end addresses, from the same
  offset. A region the table keeps keeps its mapping, a new one is mapped, a dropped one is let go. A table that moves
  or drops a ring's region is refused, as before. `MappedRegion` records its object's identity, and gains `is` and
  `holds_user`.
- The test above, with `Guest::submit_with_payload_at` (`crates/server/tests/common/guest.rs`): a WRITE whose payload
  lies in the window's range must be read through the region the table added.

## Siblings found, reported

- Under crosvm, which leaves the window out of the back end's table, a request whose buffer lies in the window (a
  guest's `O_DIRECT` read into, or write from, a mapped file's pages) names memory outside every region the device
  knows; the queue's validation refuses it and faults the queue. No test reaches it.
- Without `VHOST_USER_PROTOCOL_F_CONFIGURE_MEM_SLOTS`, QEMU sends at most 8 regions; past them its own commit fails
  ("Failed preparing vhost-user memory table msg") and the back end keeps the table it has. The guest's maps still
  work, and a request buffer in a range past the eighth region meets the crosvm sibling above. Owed in GAPS.
