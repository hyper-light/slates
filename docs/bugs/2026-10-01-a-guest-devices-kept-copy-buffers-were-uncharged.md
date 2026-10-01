# A guest device's kept copy buffers were uncharged (AUD-29-77)

**Date:** 2026-10-01. **Audit:** AUD-29-77 (P1: "charge actual retained costs exactly once to their owner";
"distinguish a protected export from a protected whole workload"). **Design:** §4.2, §4.6 A-9 ("copy buffers
and replies consume the attachment's credits").

## Description

1. The virtio-fs device reuses one request buffer and one reply buffer across chains. Each chain's copy bytes
   were charged to the attachment and released when its used element was published. But the buffers kept their
   capacity: up to the largest chain the credit admitted, held on the heap with nothing charged for it.
2. The transport report named where a transport's bytes may live. It did not say which of those places slates
   protects (its own locked, dump-excluded RAM) and which it does not: the kernel's page cache, a runtime's VM, a
   guest's page cache.

## Root cause

1. The charge followed the chain's lifetime, not the buffer's.
2. The report had no field for the protection boundary.

## Exact edits

- `crates/bridge-virtiofs/src/credit.rs`: `ChainAdmission` changes to:
  - `admit(bytes)`: one request, plus the buffers' growth;
  - `complete()`: the request is released;
  - `return_bytes(bytes)`: buffers the device lets go.

  The bytes stay charged while the device keeps them, and the attachment's reclaim returns them whole.
- `crates/bridge-virtiofs/src/device.rs`:
  - `charge_copy` charges the growth and then reserves exactly that, so kept capacity equals what is charged.
  - When the growth does not fit beside the kept buffers, it gives them back and charges the chain whole.
  - `Device::retained_copy_bytes` reports the kept bytes.
- `crates/mcp/src/lib.rs`: every residency reports `protected: daemon_ram` and its `beyond_protection` list.
  The CLI's text line gains `protected=` and `beyond=` from the same function.

## Proof

- `crates/bridge-virtiofs/tests/device.rs` `the_copy_buffers_a_device_keeps_stay_charged_to_its_attachment`:
  - The run: INIT, CREATE, a 32 KiB WRITE, a 32 KiB READ, then GETATTR, under a credit that fits one large
    chain.
  - Passing: the ledger's bytes equal the kept buffers after every chain, and the READ is served.
  - Mutated to charge nothing, it fails "after INIT".
  - Mutated to drop the give-back, the READ is refused (wanted 32,528 bytes, 1,712 available).
- `crates/bridge-virtiofs/tests/admission.rs`: the credit test now asserts that the bytes in flight equal the
  kept buffers.
- `crates/mcp/tests/mcp.rs`: `slates.volume.stat` reports each transport's protected and beyond-protection
  places. This failed before the change.

## Not done here

A measured residency of a real runtime VM or VMM (protected guest RAM) needs a real VMM memory provider.
That is AUD-29-68. Until then the report states those places as beyond protection and claims nothing for
them.
