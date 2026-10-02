# nfs_tls: the network export refused every Linux kernel client, which offers no ALPN (found building the KIND kernel leg)

**Date:** 2026-10-01. **Audit:** AUD-29-75. **Design:** §4.6 "Kubernetes publication without privilege"; RFC 9289 §5.
**Found by:** the KIND export leg's local validation and the ktls-utils source. The run is a kind node with
`tlshd` mounting the export over `xprtsec=mtls`; it was stopped at the kernel's missing TLS layer on Docker Desktop.

## Description

The export's TLS session refused, after the handshake, any client whose handshake did not agree on ALPN
`sunrpc` (`TlsRefusal::NotSunrpc`), including a client that offered no ALPN at all. The Linux kernel's NFS client
does its handshake through `tlshd`, and ktls-utils 1.0.0 (Debian 13's) sets ALPN only on its QUIC path
(`src/tlshd/tlshd.h` `alpns`, used by `quic.c`); its TLS client offers none. Every RPC-with-TLS mount by a Linux
node would have completed its handshake and then been closed by the server.

## Root cause

The rule was written from RFC 9289 §5's client obligation ("Client implementations MUST include ... the protocol
identifier") as if it were the server's duty to enforce. The RFC's server obligation is only to answer with
`sunrpc` alone; it says what a *client* does with a wrong answer, and nothing about a server refusing a client
that offered no ALPN. No deployed client was consulted before the rule was chosen.

## Impact

None shipped to a cluster yet: the export landed hours earlier (`0e8c583`) and its kernel leg had not run. Had
the CI leg run, every mount would have failed `access denied`.

## Exact edits

- `crates/server/src/nfs_tls.rs` `TlsSession::check_protocol`: no ALPN is served; `sunrpc` is served; any other
  agreed protocol is refused `NotSunrpc`. (A client offering ALPN without `sunrpc` fails the handshake in rustls
  itself, `NoApplicationProtocol`.) The module's rules state this and cite the source read.

## Proof

- `crates/server/tests/nfs_tls.rs` `a_client_offering_another_protocol_is_refused_and_one_offering_none_is_served`:
  a client offering only `h2` gets no session; a client offering no ALPN, as `tlshd`, is served. It failed before
  the fix (no reply for the second client) and passes after it; the whole oracle passes (7/7).
- The KIND leg's local run shows `tlshd` completing the handshake with the export. `gnutls_certificate_verify_peers3`
  passed: the server's chain to the authority and its IP SAN `10.96.200.20`, since `tlshd_keyring_create_cert` ran
  after it. `tlshd_initialize_ktls` was reached, which follows only a successful `gnutls_handshake`, and stopped at
  `setsockopt(TLS_ULP): No such file or directory`, the kernel's missing TLS layer (Docker Desktop's
  `CONFIG_TLS` unset).

## Sibling sweep

The same leg found two lane bugs, fixed in the same change: the node image's `tlshd` never started (its unit is
wanted by `remote-fs.target`, which a kind node never reaches), and `tlshd` refused the client key at mode 0644.
