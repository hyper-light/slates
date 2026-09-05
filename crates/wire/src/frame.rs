//! Frames: header plus body, with the class's cap checked before any allocation, the checksum
//! verified before decode, the kind looked up in the registry, and the schema hash in the first
//! body word checked against the registered one (§4.9 "Framing").
//!
//! The registry of kinds is built by the crate that owns the protocol's messages (the server and
//! the SDK cores in later phases); Phase 0 carries the mechanism and its tests.

use slates_machine::{Derived, derived};

use crate::codec::Wire;
use crate::crc32c::crc32c;
use crate::error::WireError;
use crate::header::{Class, Flags, HEADER_LEN, Header, MINOR};
use crate::request::RequestId;

/// The body length cap per class.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameCaps {
  // Format: one cap per class, indexed by `Class::index`.
  caps: [u32; 4],
}

impl FrameCaps {
  /// Caps from measurements: control and metadata frames fit what the class's latency budget
  /// lets the path carry (bandwidth × budget), never below one MTU; bulk frames take the largest
  /// size the memcpy curve still serves at full throughput; telemetry gets one MTU.
  pub fn derive(
    mtu_bytes: u32,
    bandwidth_bytes_per_ns: u64,
    control_budget_ns: u64,
    metadata_budget_ns: u64,
    bulk_cap: u32,
  ) -> Self {
    let small = |budget_ns: u64| -> u32 {
      u32::try_from(bandwidth_bytes_per_ns.saturating_mul(budget_ns))
        .unwrap_or(u32::MAX)
        .max(mtu_bytes)
    };
    // Format: one cap per class.
    let d: Derived<[u32; 4]> = derived!(
      [
        small(control_budget_ns),
        small(metadata_budget_ns),
        bulk_cap.max(mtu_bytes),
        mtu_bytes
      ],
      "control and metadata: max(bandwidth × class budget, MTU); bulk: max(memcpy knee, MTU); telemetry: MTU",
      [
        "wire.mtu",
        "wire.bandwidth",
        "wire.control_budget",
        "wire.metadata_budget",
        "memcpy"
      ]
    );
    Self { caps: d.get() }
  }

  /// Explicit caps (tests, and the one-host path where the transport is shared memory).
  pub const fn explicit(control: u32, metadata: u32, bulk: u32, telemetry: u32) -> Self {
    Self {
      caps: [control, metadata, bulk, telemetry],
    }
  }

  /// The cap for a class.
  pub fn cap(&self, class: Class) -> u32 {
    self.caps[class.index()]
  }
}

/// A registered kind: its class, its number, and the schema hash of its body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KindEntry {
  /// The class.
  pub class: Class,
  /// The kind number within the class.
  pub kind: u16,
  /// The body's schema hash.
  pub schema_hash: u64,
}

/// A decoded frame: the header and the body bytes after the schema word.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
  /// The header.
  pub header: Header,
  /// The body after the schema word, ready for `Wire::from_bytes`.
  pub body: Vec<u8>,
}

/// Encodes and decodes frames against a registry and a set of caps.
#[derive(Clone, Debug)]
pub struct Framer {
  caps: FrameCaps,
  kinds: Vec<KindEntry>,
}

/// Format: the schema word's width.
const SCHEMA_WORD: usize = 8;

impl Framer {
  /// A framer over `kinds` with `caps`.
  pub fn new(caps: FrameCaps, kinds: Vec<KindEntry>) -> Self {
    Self { caps, kinds }
  }

  /// The caps.
  pub const fn caps(&self) -> &FrameCaps {
    &self.caps
  }

  fn entry(&self, class: Class, kind: u16) -> Option<&KindEntry> {
    self
      .kinds
      .iter()
      .find(|k| k.class == class && k.kind == kind)
  }

  /// Encodes a message of a registered kind into one frame; refuses a body over the cap, so an
  /// oversized payload fails here rather than at the receiver.
  pub fn encode<M: Wire>(
    &self,
    class: Class,
    kind: u16,
    request: RequestId,
    flags: Flags,
    message: &M,
  ) -> Result<Vec<u8>, WireError> {
    let entry = self.entry(class, kind).ok_or(WireError::UnknownKind {
      class: class as u16,
      kind,
    })?;
    if entry.schema_hash != M::SCHEMA_HASH {
      return Err(WireError::SchemaMismatch {
        expected: entry.schema_hash,
        got: M::SCHEMA_HASH,
      });
    }
    let mut body = Vec::with_capacity(SCHEMA_WORD);
    M::SCHEMA_HASH.encode(&mut body);
    message.encode(&mut body);
    let length = u32::try_from(body.len()).unwrap_or(u32::MAX);
    let cap = self.caps.cap(class);
    if length > cap {
      return Err(WireError::FrameTooLarge { length, cap });
    }
    let checksum = if class.checksummed() {
      crc32c(&body)
    } else {
      0
    };
    let header = Header {
      minor: MINOR,
      flags,
      class,
      kind,
      length,
      checksum,
      request,
    };
    let mut out = Vec::with_capacity(HEADER_LEN + body.len());
    out.extend_from_slice(&header.encode());
    out.extend_from_slice(&body);
    Ok(out)
  }

  /// Decodes one frame from the front of `input`, advancing it. A short input reports how many
  /// bytes are needed (`Truncated`), which a streaming reader treats as "read more".
  pub fn decode(&self, input: &mut &[u8]) -> Result<Frame, WireError> {
    let header = Header::decode(input)?;
    let cap = self.caps.cap(header.class);
    if header.length > cap {
      return Err(WireError::FrameTooLarge {
        length: header.length,
        cap,
      });
    }
    let entry = self
      .entry(header.class, header.kind)
      .ok_or(WireError::UnknownKind {
        class: header.class as u16,
        kind: header.kind,
      })?;
    let length = usize::try_from(header.length).unwrap_or(usize::MAX);
    if input.len() < HEADER_LEN + length {
      return Err(WireError::Truncated {
        needed: HEADER_LEN + length - input.len(),
      });
    }
    let body = &input[HEADER_LEN..HEADER_LEN + length];
    if header.class.checksummed() {
      let got = crc32c(body);
      if got != header.checksum {
        return Err(WireError::ChecksumMismatch {
          expected: header.checksum,
          got,
        });
      }
    }
    let mut rest = body;
    let schema = u64::decode(&mut rest)?;
    if schema != entry.schema_hash {
      return Err(WireError::SchemaMismatch {
        expected: entry.schema_hash,
        got: schema,
      });
    }
    *input = &input[HEADER_LEN + length..];
    Ok(Frame {
      header,
      body: rest.to_vec(),
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use slates_wire_derive::Wire;

  #[derive(Wire, Debug, PartialEq)]
  struct Hello {
    name: String,
    cores: u32,
  }

  fn framer() -> Framer {
    Framer::new(
      FrameCaps::explicit(64, 128, 1 << 20, 32),
      vec![KindEntry {
        class: Class::Control,
        kind: 1,
        schema_hash: Hello::SCHEMA_HASH,
      }],
    )
  }

  #[test]
  fn a_frame_round_trips_with_its_checksum_and_schema_word() {
    let f = framer();
    let hello = Hello {
      name: "m5".into(),
      cores: 18,
    };
    let bytes = f
      .encode(
        Class::Control,
        1,
        RequestId {
          client: 1,
          sequence: 2,
        },
        Flags::default(),
        &hello,
      )
      .unwrap();
    assert_eq!(bytes.len(), HEADER_LEN + 8 + 4 + 2 + 4);
    let mut input = bytes.as_slice();
    let frame = f.decode(&mut input).unwrap();
    assert!(input.is_empty());
    assert_eq!(frame.header.length, 18);
    assert_eq!(Hello::from_bytes(&frame.body).unwrap(), hello);
  }

  fn good_frame(f: &Framer) -> Vec<u8> {
    let hello = Hello {
      name: "x".into(),
      cores: 1,
    };
    f.encode(
      Class::Control,
      1,
      RequestId::default(),
      Flags::default(),
      &hello,
    )
    .unwrap()
  }

  #[test]
  fn hostile_lengths_truncations_and_bit_flips_are_typed_refusals_without_allocation() {
    let f = framer();
    let good = good_frame(&f);
    let mut huge = good.clone();
    huge[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(matches!(
      f.decode(&mut huge.as_slice()),
      Err(WireError::FrameTooLarge { cap: 64, .. })
    ));
    assert!(matches!(
      f.decode(&mut &good[..20]),
      Err(WireError::Truncated { needed: 12 })
    ));
    assert!(matches!(
      f.decode(&mut &good[..40]),
      Err(WireError::Truncated { .. })
    ));
    let mut flipped = good.clone();
    flipped[HEADER_LEN + 9] ^= 0x10;
    assert!(matches!(
      f.decode(&mut flipped.as_slice()),
      Err(WireError::ChecksumMismatch { .. })
    ));
  }

  #[test]
  fn unknown_and_oversized_kinds_are_refused_on_both_ends() {
    let f = framer();
    let mut unknown = good_frame(&f);
    unknown[14] = 9;
    assert!(matches!(
      f.decode(&mut unknown.as_slice()),
      Err(WireError::UnknownKind { kind: 9, .. })
    ));
    let hello = Hello {
      name: "x".into(),
      cores: 1,
    };
    assert!(matches!(
      f.encode(
        Class::Control,
        2,
        RequestId::default(),
        Flags::default(),
        &hello
      ),
      Err(WireError::UnknownKind { kind: 2, .. })
    ));
    let big = Hello {
      name: "y".repeat(100),
      cores: 1,
    };
    assert!(matches!(
      f.encode(
        Class::Control,
        1,
        RequestId::default(),
        Flags::default(),
        &big
      ),
      Err(WireError::FrameTooLarge { cap: 64, .. })
    ));
  }

  #[test]
  fn a_foreign_schema_is_refused_by_hash() {
    #[derive(Wire)]
    struct Hello2 {
      name: String,
      cores: u64,
    }
    let f = Framer::new(
      FrameCaps::explicit(64, 64, 64, 64),
      vec![KindEntry {
        class: Class::Control,
        kind: 1,
        schema_hash: Hello2::SCHEMA_HASH,
      }],
    );
    let bytes = f
      .encode(
        Class::Control,
        1,
        RequestId::default(),
        Flags::default(),
        &Hello2 {
          name: "a".into(),
          cores: 1,
        },
      )
      .unwrap();
    let old = framer();
    assert!(matches!(
      old.decode(&mut bytes.as_slice()),
      Err(WireError::SchemaMismatch { .. })
    ));
    assert!(matches!(
      old.encode(
        Class::Control,
        1,
        RequestId::default(),
        Flags::default(),
        &Hello2 {
          name: "a".into(),
          cores: 1
        }
      ),
      Err(WireError::SchemaMismatch { .. })
    ));
  }

  #[test]
  fn caps_derive_from_the_path_and_never_fall_below_an_mtu() {
    let caps = FrameCaps::derive(1500, 1, 10_000, 100_000, 4 << 20);
    assert_eq!(caps.cap(Class::Control), 10_000);
    assert_eq!(caps.cap(Class::Metadata), 100_000);
    assert_eq!(caps.cap(Class::Bulk), 4 << 20);
    assert_eq!(caps.cap(Class::Telemetry), 1500);
    assert_eq!(
      FrameCaps::derive(1500, 0, 1, 1, 0).cap(Class::Control),
      1500
    );
  }
}
