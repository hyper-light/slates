# A READ reply past 64 KiB closed the RPC-with-TLS export's connection

- **Date:** 2026-10-25
- **Status:** fixed 2026-10-25, in the same change as this record.
- **Subsystem:** the network export's RPC-with-TLS session, `crates/server/src/nfs_tls.rs` (`TlsSession::send`;
  §4.6 "Kubernetes publication without privilege", AUD-29-75).
- **Severity:** data unavailable. Through a kubelet-mounted export, a file whose read reply came to more than 64 KiB
  could not be read at all. The reading process hung for good: the mount is `hard`, and every retry met the same
  refusal.
- **Exposure:** every RPC-with-TLS client of `slates export` reading a file past about 64 KiB in one READ (a Linux
  client's readahead asks for up to `rsize`, 256 KiB on the lane's mount). The loopback NFS mount (macOS, no TLS) is
  not affected. The KIND lane's leg had never sent a large READ: its round trip is 67 bytes, and its other files were
  read from the node's own page cache.
- **Found by:** G-mount step 5, the KIND lane's map leg (`cargo xtask kind export`, step 6), on adacachy with a
  scratch `kind` v0.33.0 (Ada's go, 2026-10-25).

## Symptom

The map leg's pod mapped a 64 KiB file and never printed that it had. Its thread was in the kernel on its first touch
of the map:

```
[<0>] folio_wait_bit_common+0x17e/0x2f0
[<0>] filemap_fault+0x34a/0x3c0
```

A plain `read` of the same file from another pod (`dd`, `wc -c`) hung the same way, in `nfs_file_read`; a 67-byte and a
4 KiB file read at once. The node's `mountstats` showed READ: 2,953 calls, 5,906 transmissions, 2,952 errors and
134,088 bytes back, and the daemon's counters showed `nfs.tls.session` at 2,520 and climbing. Each READ attempt cost a
whole TLS session. `slates read` served all 65,536 bytes of the file from the daemon itself.

## Root cause

`TlsSession::send` handed a whole framed reply to rustls with `writer().write_all(...)`. rustls 0.23 seals records into
a buffer capped at 64 KiB (`sendable_tls: ChunkVecBuffer::new(Some(DEFAULT_BUFFER_LIMIT))`,
`DEFAULT_BUFFER_LIMIT = 64 * 1024`, `common_state.rs`) and takes no more plaintext once the cap is reached. So a write
past it returns 0, and `write_all` fails with "failed to write whole buffer". The serve loop counted `nfs.tls.session`
and closed the connection. The client reconnected, did the TLS handshake again, retried the READ and lost the
connection again, forever.

## Fix

`send` hands rustls the reply in the pieces it takes, and between pieces moves the sealed records out of rustls's
buffer into the session's own (`TlsSession::sealed`). `transmit` takes those first. The session's buffer holds at most
one reply's records, since the serve loop transmits after every reply. A write that takes nothing and seals nothing is
refused, so the loop always ends.

## Verification

- Failing test first: `crates/server/tests/nfs_tls.rs`
  `a_reply_larger_than_the_tls_send_buffer_is_sealed_whole` sends one record of the codec's largest message
  (`MAX_MESSAGE`) through a session after an in-memory mutual handshake. Before the fix it failed with
  `Io("writing a reply into the session: failed to write whole buffer")`; after it, the client decrypts exactly the
  record. All 8 tests of the file pass.
- The KIND leg on adacachy: see the G-mount step 5 record in `docs/wip/GAPS.md`.

## Sibling sweep

`TlsSession::send` is the only rustls `writer()` in the workspace. The fleet transport seals through QUIC's own record
layer, which has no such buffer. `crates/transport/examples/rpc_tls_bench.rs` moves only handshake flights.
