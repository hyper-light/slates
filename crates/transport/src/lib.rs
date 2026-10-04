//! The fleet claims-plane transport (§4.10a; design: `docs/wip/fleet-transport.md`). slates nodes speak over UDP: the
//! owned **QUIC session plane** here, and the sealed membership datagram plane, which is hyper-datagram's (A-67 H-2,
//! vendored from `../hyper-raft`; this crate's own control-datagram codec, seal and key schedule were removed with it,
//! the daemon never having used them). Never MCP, which is the agent-facing tool edge. This is the daemon↔daemon mesh,
//! where Meta-scale traffic lives; the laptop-degenerate is a local/loopback delivery, one code path (R8).
//!
//! **The session plane** — slates's owned QUIC dialect over `rustls::quic` (TLS 1.3, D-15, not hecate's
//! Noise): [`handshake`] (pinned-identity TLS 1.3), [`packet_number`] (RFC 9000 §17.1 truncation),
//! [`session`]/[`stream`]/[`conn`]/[`flow`] (frames, ordered streams, reliability, flow credit),
//! [`connection`] (the sans-io driver — reliable, flow-controlled, multi-stream) and [`endpoint`] (the
//! UDP edge that pumps it). A stream crosses the wire authenticated, encrypted, header-protected,
//! reliable and flow-controlled.
//!
//! **Owed:** the register/placement wiring (slice 5, §4.8) that rides the session plane; enrollment's
//! *distribution* half (configuration-group admission, `docs/wip/enrollment.md`); congestion control
//! (its validation needs a real network); and the session plane's tuning (probe timeout, MTU, BDP
//! window autotuning).
//!
//! This is a parser of external bytes, so every length is checked against the bytes that remain before it is read, and
//! every malformation is a typed refusal ([`FrameError`] for a field's bytes), never a panic, the same discipline as
//! `slates-merge`'s ops-document decode and `slates-bridge-fuse`'s ABI codec.

// The no-panic law (CLAUDE.md item 6), enforced here ahead of the workspace-wide lint: no indexing,
// slicing or string slicing that can go out of bounds outside tests.
#![cfg_attr(
  not(test),
  deny(
    clippy::indexing_slicing,
    clippy::string_slice,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result
  )
)]

pub mod congestion;
pub mod conn;
pub mod connection;
pub mod demux;
pub mod endpoint;
pub mod flight;
pub mod flow;
pub mod handshake;
pub mod keys;
pub mod pacer;
pub mod packet_number;
pub mod params;
pub mod pmtud;
pub mod receive;
pub mod reorder;
pub mod rtt;
pub mod session;
pub mod stream;
pub mod streams;

/// Fills `out` with `parts` laid end to end — how the fixed-layout byte arrays here (a key-derivation
/// context, an AEAD's associated data, a nonce) are assembled from their fields without indexing. Each
/// array is sized as the sum of its fields' sizes, so the parts fill it exactly.
pub(crate) fn fill_from(out: &mut [u8], parts: &[&[u8]]) {
  for (slot, byte) in out
    .iter_mut()
    .zip(parts.iter().flat_map(|part| part.iter()))
  {
    *slot = *byte;
  }
}

/// A refusal reading a fixed-layout field from external bytes (§4.10a): the bytes ended before it, never a panic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameError {
  /// The bytes ended before a field could be read.
  Truncated,
}

impl std::fmt::Display for FrameError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    let reason = match self {
      FrameError::Truncated => "the bytes ended before a field could be read",
    };
    f.write_str(reason)
  }
}

impl std::error::Error for FrameError {}

/// A cursor over the datagram bytes: every read is bounds-checked against what remains, so a torn
/// datagram yields [`FrameError::Truncated`] rather than an out-of-bounds panic.
pub(crate) struct Reader<'a> {
  // Shared with the seal path (module `seal`), which reads the prologue and the wire counter.
  bytes: &'a [u8],
  at: usize,
}

impl<'a> Reader<'a> {
  pub(crate) fn new(bytes: &'a [u8]) -> Reader<'a> {
    Reader { bytes, at: 0 }
  }

  pub(crate) fn remaining(&self) -> usize {
    self.bytes.len().saturating_sub(self.at)
  }

  pub(crate) fn is_empty(&self) -> bool {
    self.remaining() == 0
  }

  pub(crate) fn bytes(&mut self, len: usize) -> Result<&'a [u8], FrameError> {
    let end = self.at.checked_add(len).ok_or(FrameError::Truncated)?;
    let slice = self.bytes.get(self.at..end).ok_or(FrameError::Truncated)?;
    self.at = end;
    Ok(slice)
  }

  pub(crate) fn u8(&mut self) -> Result<u8, FrameError> {
    self
      .bytes(size_of::<u8>())?
      .first()
      .copied()
      .ok_or(FrameError::Truncated)
  }

  pub(crate) fn u16(&mut self) -> Result<u16, FrameError> {
    let b = self.bytes(size_of::<u16>())?;
    let mut word = [0u8; size_of::<u16>()];
    word.copy_from_slice(b);
    Ok(u16::from_le_bytes(word))
  }

  pub(crate) fn u32(&mut self) -> Result<u32, FrameError> {
    let b = self.bytes(size_of::<u32>())?;
    let mut word = [0u8; size_of::<u32>()];
    word.copy_from_slice(b);
    Ok(u32::from_le_bytes(word))
  }

  pub(crate) fn u64(&mut self) -> Result<u64, FrameError> {
    let b = self.bytes(size_of::<u64>())?;
    let mut word = [0u8; size_of::<u64>()];
    word.copy_from_slice(b);
    Ok(u64::from_le_bytes(word))
  }
}
