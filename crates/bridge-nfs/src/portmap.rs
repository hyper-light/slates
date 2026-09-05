//! The minimal portmap responder (RFC 1833, PMAP version 2): the well-known service a client
//! queries to learn the port a program listens on, before it can call MOUNT or NFS. slates passes
//! explicit `port` and `mountport` mount options (§4.6), so a client need not query portmap at
//! all; this responder exists for clients that do. It answers only `GETPORT` (and `NULL`); every
//! other procedure is refused by the RPC layer. This is the pure wire layer — the `mapping`
//! argument and the port result — over the XDR codec, golden and hostile-input tested with no
//! socket.

use crate::xdr::{XdrError, XdrReader, XdrWriter};

/// Format: the portmap program number (RFC 1833).
pub const PORTMAP_PROGRAM: u32 = 100_000;
/// Format: the portmap protocol version 2 this responder speaks.
pub const PORTMAP_VERSION: u32 = 2;
/// Format: PMAPPROC_NULL — a ping, no arguments and no results.
pub const PMAPPROC_NULL: u32 = 0;
/// Format: PMAPPROC_GETPORT — the port a `(program, version, protocol)` listens on.
pub const PMAPPROC_GETPORT: u32 = 3;
/// Format: IPPROTO_TCP, the transport a mapping names for TCP (RFC 1833).
pub const IPPROTO_TCP: u32 = 6;
/// Format: IPPROTO_UDP, the transport a mapping names for UDP.
pub const IPPROTO_UDP: u32 = 17;
/// Format: the port a GETPORT returns for a program this responder does not serve (RFC 1833: a
/// zero port means "not registered").
pub const PORT_NOT_REGISTERED: u32 = 0;

/// A portmap `mapping` (RFC 1833): the program, version, transport protocol and port. A GETPORT
/// request carries one with `port` unused (zero); a registration would carry the real port.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mapping {
  /// The RPC program number.
  pub program: u32,
  /// The program version.
  pub version: u32,
  /// The transport protocol ([`IPPROTO_TCP`] or [`IPPROTO_UDP`]).
  pub protocol: u32,
  /// The port (unused in a GETPORT request).
  pub port: u32,
}

impl Mapping {
  /// Writes the mapping in `mapping` field order.
  pub fn encode(&self, writer: &mut XdrWriter) {
    writer.u32(self.program);
    writer.u32(self.version);
    writer.u32(self.protocol);
    writer.u32(self.port);
  }

  /// Reads a mapping (a GETPORT argument).
  pub fn decode(reader: &mut XdrReader<'_>) -> Result<Mapping, XdrError> {
    Ok(Mapping {
      program: reader.u32()?,
      version: reader.u32()?,
      protocol: reader.u32()?,
      port: reader.u32()?,
    })
  }
}

/// Encodes a GETPORT reply: the single port the program listens on, or [`PORT_NOT_REGISTERED`].
pub fn getport_reply(port: u32) -> Vec<u8> {
  let mut writer = XdrWriter::new();
  writer.u32(port);
  writer.into_bytes()
}
