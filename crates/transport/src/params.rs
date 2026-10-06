//! The session plane's transport parameters (§4.10a; RFC 9000 §7.4 and §18 in shape): what each end
//! declares about itself inside the TLS 1.3 handshake, carried by `rustls::quic` in the QUIC transport
//! parameters extension, so the peer's parameters are authenticated with its certificate — tampering
//! fails the handshake. Until 2026-09-28 the handshake carried a fixed tag (`b"slates-quic-v1"`) and
//! nothing else; path MTU discovery (RFC 8899 applied as RFC 9000 §14.3) needs each end to know the
//! largest datagram its peer will read, so the parameters now have a codec.
//!
//! The encoding follows slates's dialect (D-15: fixed-layout little-endian, not QUIC's varints): a list of
//! entries, each a `u16` identifier, a `u16` value length and the value. As in RFC 9000 §7.4.2 an unknown
//! identifier is skipped, so a later version can add a parameter an older peer ignores, and a repeated
//! identifier is refused. The dialect version is a required parameter: a peer speaking another version is
//! refused at the handshake, typed, instead of mis-reading its packets later.
//!
//! The parameters:
//! - [`ID_DIALECT`] — the dialect version, `u32`; must equal [`DIALECT_VERSION`].
//! - [`ID_MAX_UDP_PAYLOAD`] — the largest UDP payload this end reads (RFC 9000 §18.2
//!   `max_udp_payload_size`), `u32`; at least [`MIN_DATAGRAM_BYTES`] (a smaller value is refused, as the
//!   RFC refuses one below 1200). Absent means the RFC default, [`DEFAULT_MAX_UDP_PAYLOAD`].
//!
//! This is a parser of bytes a peer chose, so every length is checked before it is read and every
//! malformed input is a typed refusal, never a panic.

use crate::endpoint::MIN_DATAGRAM_BYTES;

/// Format: the identifier of the dialect-version parameter.
pub const ID_DIALECT: u16 = 1;
/// Format: the identifier of the largest-UDP-payload parameter (RFC 9000 §18.2 `max_udp_payload_size`).
pub const ID_MAX_UDP_PAYLOAD: u16 = 2;
/// Format: the session dialect this build speaks. Raised only by a change to the packet or frame format:
/// 2 added the `Ping` frame (kind 11) path-MTU probes carry (2026-09-28).
pub const DIALECT_VERSION: u32 = 2;
/// Format: RFC 9000 §18.2 — the `max_udp_payload_size` a peer that states none is taken to read, the
/// largest UDP payload an IPv4 datagram carries (65,535 − 8 bytes of UDP header).
pub const DEFAULT_MAX_UDP_PAYLOAD: u32 = 65_527;
/// Format: the bytes of an entry's identifier and length fields.
const ENTRY_HEADER_BYTES: usize = 4;

/// One end's transport parameters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransportParameters {
  /// The largest UDP payload this end reads, bytes (at least [`MIN_DATAGRAM_BYTES`]).
  pub max_udp_payload: u32,
}

/// Why a peer's transport parameters were refused (RFC 9000's `TRANSPORT_PARAMETER_ERROR`, typed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamsError {
  /// The peer presented no parameters at all.
  Missing,
  /// An entry's header or value runs past the end of the bytes.
  Truncated {
    /// Where the entry starts.
    at: usize,
  },
  /// An identifier appears twice.
  Repeated {
    /// The identifier.
    id: u16,
  },
  /// A known parameter's value has the wrong length.
  BadLength {
    /// The identifier.
    id: u16,
    /// The length found.
    length: u16,
  },
  /// No dialect version was presented.
  NoDialect,
  /// The peer speaks another dialect version.
  Dialect {
    /// The version it presented.
    version: u32,
  },
  /// The largest UDP payload is below the path floor every QUIC path carries.
  PayloadTooSmall {
    /// The value presented.
    bytes: u32,
  },
}

impl std::fmt::Display for ParamsError {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      ParamsError::Missing => write!(formatter, "the peer presented no transport parameters"),
      ParamsError::Truncated { at } => {
        write!(
          formatter,
          "a transport parameter at byte {at} runs past the end"
        )
      }
      ParamsError::Repeated { id } => write!(formatter, "transport parameter {id} appears twice"),
      ParamsError::BadLength { id, length } => {
        write!(formatter, "transport parameter {id} has length {length}")
      }
      ParamsError::NoDialect => write!(formatter, "the peer presented no dialect version"),
      ParamsError::Dialect { version } => write!(
        formatter,
        "the peer speaks dialect {version}; this build speaks {DIALECT_VERSION}"
      ),
      ParamsError::PayloadTooSmall { bytes } => write!(
        formatter,
        "the peer reads at most {bytes}-byte datagrams, below the {MIN_DATAGRAM_BYTES}-byte floor"
      ),
    }
  }
}

impl std::error::Error for ParamsError {}

impl TransportParameters {
  /// This end's parameters: it reads every datagram into a buffer of the largest UDP payload
  /// ([`crate::receive::RECEIVE_BUFFER_BYTES`]), so that is what it declares; path MTU discovery then finds
  /// how much of it the path between the two ends carries.
  pub fn local() -> TransportParameters {
    TransportParameters {
      max_udp_payload: u32::try_from(crate::receive::RECEIVE_BUFFER_BYTES).unwrap_or(u32::MAX),
    }
  }

  /// The encoded parameters, the dialect version first.
  pub fn encode(&self) -> Vec<u8> {
    let mut out = Vec::with_capacity(
      ENTRY_HEADER_BYTES
        .saturating_add(size_of::<u32>())
        .saturating_mul(2),
    );
    put_u32(&mut out, ID_DIALECT, DIALECT_VERSION);
    put_u32(&mut out, ID_MAX_UDP_PAYLOAD, self.max_udp_payload);
    out
  }

  /// Decodes a peer's parameters, refusing anything malformed (see [`ParamsError`]).
  pub fn decode(bytes: Option<&[u8]>) -> Result<TransportParameters, ParamsError> {
    let bytes = bytes.ok_or(ParamsError::Missing)?;
    let mut dialect = None;
    let mut max_udp_payload = None;
    let mut at = 0usize;
    while at < bytes.len() {
      let (id, value, next) = entry(bytes, at)?;
      match id {
        ID_DIALECT => set_once(&mut dialect, id, u32_value(id, value)?)?,
        ID_MAX_UDP_PAYLOAD => set_once(&mut max_udp_payload, id, u32_value(id, value)?)?,
        // An unknown identifier is skipped (RFC 9000 §7.4.2), so a later dialect can add parameters.
        _ => {}
      }
      at = next;
    }
    match dialect {
      None => return Err(ParamsError::NoDialect),
      Some(DIALECT_VERSION) => {}
      Some(version) => return Err(ParamsError::Dialect { version }),
    }
    let max_udp_payload = max_udp_payload.unwrap_or(DEFAULT_MAX_UDP_PAYLOAD);
    let floor = u32::try_from(MIN_DATAGRAM_BYTES).unwrap_or(u32::MAX);
    if max_udp_payload < floor {
      return Err(ParamsError::PayloadTooSmall {
        bytes: max_udp_payload,
      });
    }
    Ok(TransportParameters { max_udp_payload })
  }
}

/// Appends one `u32`-valued entry.
fn put_u32(out: &mut Vec<u8>, id: u16, value: u32) {
  out.extend_from_slice(&id.to_le_bytes());
  out.extend_from_slice(
    &u16::try_from(size_of::<u32>())
      .unwrap_or(u16::MAX)
      .to_le_bytes(),
  );
  out.extend_from_slice(&value.to_le_bytes());
}

/// The entry starting at `at`: its identifier, its value, and where the next entry starts.
fn entry(bytes: &[u8], at: usize) -> Result<(u16, &[u8], usize), ParamsError> {
  let truncated = ParamsError::Truncated { at };
  let header_end = at.checked_add(ENTRY_HEADER_BYTES).ok_or(truncated)?;
  let header = bytes.get(at..header_end).ok_or(truncated)?;
  let (Some(&[id_low, id_high]), Some(&[length_low, length_high])) =
    (header.get(..2), header.get(2..))
  else {
    return Err(truncated);
  };
  let id = u16::from_le_bytes([id_low, id_high]);
  let length = usize::from(u16::from_le_bytes([length_low, length_high]));
  let value_end = header_end.checked_add(length).ok_or(truncated)?;
  let value = bytes.get(header_end..value_end).ok_or(truncated)?;
  Ok((id, value, value_end))
}

/// A `u32` value, or the length refusal.
fn u32_value(id: u16, value: &[u8]) -> Result<u32, ParamsError> {
  <[u8; 4]>::try_from(value)
    .map(u32::from_le_bytes)
    .map_err(|_| ParamsError::BadLength {
      id,
      length: u16::try_from(value.len()).unwrap_or(u16::MAX),
    })
}

/// Sets a parameter that may appear once.
fn set_once(slot: &mut Option<u32>, id: u16, value: u32) -> Result<(), ParamsError> {
  if slot.replace(value).is_some() {
    return Err(ParamsError::Repeated { id });
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Golden vector (§4.10a; CLAUDE.md "golden vectors for everything on the wire"): the encoding of a
  /// 1,500-byte receiver — the dialect entry first, then the payload entry, each `id | len | u32`,
  /// little-endian. A change to these bytes is a change to the wire and must bump the dialect.
  #[test]
  fn the_encoding_is_pinned_by_a_golden_vector() {
    let encoded = TransportParameters {
      max_udp_payload: 1_500,
    }
    .encode();
    assert_eq!(
      encoded,
      [
        0x01, 0x00, 0x04, 0x00, 0x02, 0x00, 0x00, 0x00, // dialect 2
        0x02, 0x00, 0x04, 0x00, 0xdc, 0x05, 0x00, 0x00, // max UDP payload 1500
      ]
    );
    assert_eq!(
      TransportParameters::decode(Some(&encoded)),
      Ok(TransportParameters {
        max_udp_payload: 1_500
      })
    );
  }

  /// RFC 9000 §7.4.2 and §18.2: an unknown parameter is skipped, and an absent payload limit is the
  /// RFC default.
  #[test]
  fn an_unknown_parameter_is_skipped_and_an_absent_limit_is_the_default() {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&[0x99, 0x00, 0x03, 0x00, 0xaa, 0xbb, 0xcc]);
    put_u32(&mut bytes, ID_DIALECT, DIALECT_VERSION);
    assert_eq!(
      TransportParameters::decode(Some(&bytes)),
      Ok(TransportParameters {
        max_udp_payload: DEFAULT_MAX_UDP_PAYLOAD
      })
    );
  }

  /// Hostile input (CLAUDE.md "hostile-input tests on every parser of external bytes"): nothing, a
  /// truncated header, and a length past the end (including `u16::MAX`) are refused typed.
  #[test]
  fn absent_or_truncated_parameters_are_refused_typed() {
    assert_eq!(TransportParameters::decode(None), Err(ParamsError::Missing));
    assert_eq!(
      TransportParameters::decode(Some(&[0x01, 0x00, 0x04])),
      Err(ParamsError::Truncated { at: 0 })
    );
    assert_eq!(
      TransportParameters::decode(Some(&[0x01, 0x00, 0xff, 0xff, 0x01])),
      Err(ParamsError::Truncated { at: 0 })
    );
  }

  /// Hostile input: a repeated identifier and a wrong-length value are refused typed.
  #[test]
  fn repeated_or_misshapen_parameters_are_refused_typed() {
    let mut repeated = TransportParameters::local().encode();
    put_u32(&mut repeated, ID_MAX_UDP_PAYLOAD, 1_500);
    assert_eq!(
      TransportParameters::decode(Some(&repeated)),
      Err(ParamsError::Repeated {
        id: ID_MAX_UDP_PAYLOAD
      })
    );
    assert_eq!(
      TransportParameters::decode(Some(&[0x01, 0x00, 0x02, 0x00, 0x01, 0x00])),
      Err(ParamsError::BadLength {
        id: ID_DIALECT,
        length: 2
      })
    );
  }

  /// Hostile input: no dialect, another dialect, and a payload limit below the floor are refused typed.
  #[test]
  fn a_foreign_dialect_or_a_limit_below_the_floor_is_refused_typed() {
    let mut no_dialect = Vec::new();
    put_u32(&mut no_dialect, ID_MAX_UDP_PAYLOAD, 1_500);
    assert_eq!(
      TransportParameters::decode(Some(&no_dialect)),
      Err(ParamsError::NoDialect)
    );
    let mut other = Vec::new();
    put_u32(&mut other, ID_DIALECT, DIALECT_VERSION + 1);
    assert_eq!(
      TransportParameters::decode(Some(&other)),
      Err(ParamsError::Dialect {
        version: DIALECT_VERSION + 1
      })
    );
    let mut small = Vec::new();
    put_u32(&mut small, ID_DIALECT, DIALECT_VERSION);
    put_u32(&mut small, ID_MAX_UDP_PAYLOAD, 1_199);
    assert_eq!(
      TransportParameters::decode(Some(&small)),
      Err(ParamsError::PayloadTooSmall { bytes: 1_199 })
    );
  }

  /// Hostile input: every prefix of a valid encoding, and every single-bit flip of it, decodes or is
  /// refused — never a panic.
  #[test]
  fn every_prefix_and_bit_flip_decodes_or_is_refused() {
    let valid = TransportParameters {
      max_udp_payload: 9_000,
    }
    .encode();
    for cut in 0..valid.len() {
      let _ = TransportParameters::decode(valid.get(..cut));
    }
    for index in 0..valid.len() {
      for bit in 0..8 {
        let mut flipped = valid.clone();
        if let Some(byte) = flipped.get_mut(index) {
          *byte ^= 1 << bit;
        }
        let _ = TransportParameters::decode(Some(&flipped));
      }
    }
  }
}
