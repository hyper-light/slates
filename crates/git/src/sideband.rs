//! side-band-64k (A-582; git's `gitprotocol-capabilities.txt` "side-band, side-band-64k" and `gitprotocol-v2.txt`
//! "packfile section", `sideband-all` [B]; `sideband.c` [C]): a pack is streamed in data packets whose first byte names
//! a band. Band 1 carries the pack's bytes, band 2 progress for the client to show, band 3 a fatal error, after which the
//! stream ends. side-band-64k's packets are pkt-lines of up to their full 65520 bytes, so each carries at most
//! [`MAX_BAND_DATA`] bytes after its band byte. Under protocol v2's `sideband-all` every data packet of a response is
//! multiplexed so, its section names and lines included; the special packets (flush, delimiter, response end) never
//! are.
//!
//! [`demux`] reads one data packet's band; [`write`] writes bytes on a band in as many packets as they take.

use crate::out::Out;
use crate::pktline::{self, HEADER_LEN, MAX_DATA};
use crate::to_u64;

/// Format: the band byte's length, one byte before each multiplexed packet's bytes [gitprotocol-capabilities
/// "side-band, side-band-64k"].
const BAND_LEN: usize = 1;

/// Derived: the most bytes one side-band-64k packet carries after its band byte: a pkt-line's data, [`MAX_DATA`], less
/// the band byte, so 65515.
pub const MAX_BAND_DATA: usize = MAX_DATA.saturating_sub(BAND_LEN);

/// Format: band 1's byte, the pack's bytes [gitprotocol-v2 "packfile section"].
const DATA_BYTE: u8 = 1;

/// Format: band 2's byte, progress [gitprotocol-v2 "packfile section"].
const PROGRESS_BYTE: u8 = 2;

/// Format: band 3's byte, a fatal error [gitprotocol-v2 "packfile section"].
const ERROR_BYTE: u8 = 3;

/// A band of side-band-64k.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Band {
  /// Band 1: the pack's bytes (under `sideband-all`, every line of the response).
  Data,
  /// Band 2: progress, text for the client to show.
  Progress,
  /// Band 3: a fatal error; the stream ends after it.
  Error,
}

impl Band {
  /// The band's byte.
  pub fn byte(self) -> u8 {
    match self {
      Band::Data => DATA_BYTE,
      Band::Progress => PROGRESS_BYTE,
      Band::Error => ERROR_BYTE,
    }
  }

  /// The band `byte` names, if any.
  pub fn of(byte: u8) -> Option<Band> {
    match byte {
      DATA_BYTE => Some(Band::Data),
      PROGRESS_BYTE => Some(Band::Progress),
      ERROR_BYTE => Some(Band::Error),
      _ => None,
    }
  }
}

/// Why a data packet's bytes are not a multiplexed packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
  /// No band byte: a data packet with no data (`0004`).
  NoBand,
  /// A band byte other than 1, 2 and 3 (git's "bad band").
  UnknownBand {
    /// The byte.
    byte: u8,
  },
}

/// The band and the bytes of one multiplexed data packet, given its data (what [`pktline::Packet::Data`] holds).
pub fn demux(data: &[u8]) -> Result<(Band, &[u8]), Refusal> {
  let Some((&byte, bytes)) = data.split_first() else {
    return Err(Refusal::NoBand);
  };
  Band::of(byte)
    .map(|band| (band, bytes))
    .ok_or(Refusal::UnknownBand { byte })
}

/// Writes `bytes` on `band`, in packets that each carry at most [`MAX_BAND_DATA`] of them, as many as they take: none
/// for no bytes.
pub fn write(band: Band, bytes: &[u8], out: &mut impl Out) {
  for chunk in bytes.chunks(MAX_BAND_DATA) {
    out.put(&pktline::header(
      chunk
        .len()
        .saturating_add(BAND_LEN)
        .saturating_add(HEADER_LEN),
    ));
    out.put(&[band.byte()]);
    out.put(chunk);
  }
}

/// The bytes [`write`] writes for `len` bytes on one band: each packet's length and band byte besides the bytes.
pub fn written_len(len: u64) -> u64 {
  let packets = len.div_ceil(to_u64(MAX_BAND_DATA));
  len.saturating_add(packets.saturating_mul(to_u64(HEADER_LEN.saturating_add(BAND_LEN))))
}

#[cfg(test)]
#[path = "tests/sideband.rs"]
mod tests;
