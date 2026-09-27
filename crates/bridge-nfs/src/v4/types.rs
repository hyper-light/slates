//! The NFSv4 XDR types the server reads and writes (RFC 7863): bitmaps, state ids, session ids and
//! channel attributes, each with a bounds-checked decoder (a length past its cap is refused before it
//! is allocated) and an encoder.

use crate::xdr::{XdrError, XdrReader, XdrWriter};

/// Format: `NFS4_OPAQUE_LIMIT`, the largest opaque an owner or verifier carries (RFC 7863).
pub const OPAQUE_LIMIT: usize = 1024;
/// Format: `NFS4_SESSIONID_SIZE`.
pub const SESSIONID_SIZE: usize = 16;
/// Format: `NFS4_VERIFIER_SIZE`.
pub const VERIFIER_SIZE: usize = 8;
/// Format: `NFS4_OTHER_SIZE`, the opaque part of a state id.
pub const OTHER_SIZE: usize = 12;
/// Format: `NFS4_FHSIZE`, the largest v4 file handle.
pub const FHSIZE: usize = 128;
/// Format: the most words a `bitmap4` this server reads may hold: the attribute numbers it knows run
/// to RFC 8276's `xattr_support` (82), which fits in three words; a longer bitmap is read and its extra
/// words ignored, up to this bound.
pub const BITMAP_WORDS_MAX: usize = 8;

/// A `bitmap4`: bit `n` is bit `n % 32` of word `n / 32`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Bitmap(pub Vec<u32>);

impl Bitmap {
  /// A bitmap with exactly `bits` set.
  pub fn of(bits: &[u32]) -> Bitmap {
    let mut bitmap = Bitmap::default();
    for bit in bits {
      bitmap.set(*bit);
    }
    bitmap
  }

  /// Sets bit `bit`.
  pub fn set(&mut self, bit: u32) {
    let word = usize::try_from(bit / u32::BITS).unwrap_or(usize::MAX);
    if word >= BITMAP_WORDS_MAX {
      return;
    }
    if self.0.len() <= word {
      self.0.resize(word + 1, 0);
    }
    self.0[word] |= 1 << (bit % u32::BITS);
  }

  /// Whether bit `bit` is set.
  pub fn has(&self, bit: u32) -> bool {
    let word = usize::try_from(bit / u32::BITS).unwrap_or(usize::MAX);
    self
      .0
      .get(word)
      .is_some_and(|value| value & (1 << (bit % u32::BITS)) != 0)
  }

  /// The set bits, ascending.
  pub fn bits(&self) -> impl Iterator<Item = u32> + '_ {
    self.0.iter().enumerate().flat_map(|(word, value)| {
      (0..u32::BITS)
        .filter(move |bit| value & (1 << bit) != 0)
        .map(move |bit| u32::try_from(word).unwrap_or(0) * u32::BITS + bit)
    })
  }

  /// The bits set in both.
  pub fn intersect(&self, other: &Bitmap) -> Bitmap {
    Bitmap(self.0.iter().zip(&other.0).map(|(a, b)| a & b).collect())
  }

  /// Reads a `bitmap4`, at most [`BITMAP_WORDS_MAX`] words.
  pub fn decode(reader: &mut XdrReader<'_>) -> Result<Bitmap, XdrError> {
    let words = usize::try_from(reader.u32()?).map_err(|_| XdrError::BadLength)?;
    if words > BITMAP_WORDS_MAX {
      return Err(XdrError::BadLength);
    }
    let mut out = Vec::with_capacity(words);
    for _ in 0..words {
      out.push(reader.u32()?);
    }
    Ok(Bitmap(out))
  }

  /// Writes a `bitmap4`, trailing zero words dropped.
  pub fn encode(&self, writer: &mut XdrWriter) {
    let used = self
      .0
      .iter()
      .rposition(|word| *word != 0)
      .map_or(0, |at| at + 1);
    writer.u32(u32::try_from(used).unwrap_or(0));
    for word in &self.0[..used] {
      writer.u32(*word);
    }
  }
}

/// A `stateid4`: a sequence number and twelve opaque bytes naming the state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stateid {
  /// The state's sequence (`seqid`); zero in a request means "the current one" (RFC 8881 §8.2.2).
  pub seqid: u32,
  /// The state's identity.
  pub other: [u8; OTHER_SIZE],
}

impl Stateid {
  /// The anonymous state id: all zeros (RFC 8881 §8.2.3).
  pub const ANONYMOUS: Stateid = Stateid {
    seqid: 0,
    other: [0; OTHER_SIZE],
  };

  /// Reads a `stateid4`.
  pub fn decode(reader: &mut XdrReader<'_>) -> Result<Stateid, XdrError> {
    let seqid = reader.u32()?;
    let mut other = [0u8; OTHER_SIZE];
    other.copy_from_slice(reader.fixed(OTHER_SIZE)?);
    Ok(Stateid { seqid, other })
  }

  /// Writes a `stateid4`.
  pub fn encode(&self, writer: &mut XdrWriter) {
    writer.u32(self.seqid);
    writer.fixed(&self.other);
  }

  /// Whether this is one of the special state ids (all zeros, or the "bypass" all ones, RFC 8881
  /// §8.2.3), which name no opened state.
  pub fn is_special(&self) -> bool {
    self.other == [0; OTHER_SIZE] || self.other == [u8::MAX; OTHER_SIZE]
  }
}

/// A `sessionid4`.
pub type SessionId = [u8; SESSIONID_SIZE];

/// Reads a `sessionid4`.
pub fn decode_sessionid(reader: &mut XdrReader<'_>) -> Result<SessionId, XdrError> {
  let mut id = [0u8; SESSIONID_SIZE];
  id.copy_from_slice(reader.fixed(SESSIONID_SIZE)?);
  Ok(id)
}

/// A `channel_attrs4` (RFC 8881 §18.36): what a session's channel carries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChannelAttrs {
  /// `ca_headerpadsize`.
  pub header_pad: u32,
  /// `ca_maxrequestsize`: the largest request, in bytes.
  pub max_request: u32,
  /// `ca_maxresponsesize`: the largest reply, in bytes.
  pub max_response: u32,
  /// `ca_maxresponsesize_cached`: the largest reply the slot cache keeps.
  pub max_response_cached: u32,
  /// `ca_maxoperations`: the most operations in one compound.
  pub max_operations: u32,
  /// `ca_maxrequests`: the slot count.
  pub max_requests: u32,
}

impl ChannelAttrs {
  /// Reads a `channel_attrs4`; a requested RDMA read depth (`ca_rdma_ird`) is read and ignored, since
  /// this server offers no RDMA channel.
  pub fn decode(reader: &mut XdrReader<'_>) -> Result<ChannelAttrs, XdrError> {
    let attrs = ChannelAttrs {
      header_pad: reader.u32()?,
      max_request: reader.u32()?,
      max_response: reader.u32()?,
      max_response_cached: reader.u32()?,
      max_operations: reader.u32()?,
      max_requests: reader.u32()?,
    };
    let ird = reader.u32()?;
    if ird > 1 {
      return Err(XdrError::BadLength);
    }
    for _ in 0..ird {
      reader.u32()?;
    }
    Ok(attrs)
  }

  /// Writes a `channel_attrs4` with no RDMA read depth.
  pub fn encode(&self, writer: &mut XdrWriter) {
    writer.u32(self.header_pad);
    writer.u32(self.max_request);
    writer.u32(self.max_response);
    writer.u32(self.max_response_cached);
    writer.u32(self.max_operations);
    writer.u32(self.max_requests);
    writer.u32(0);
  }

  /// The attributes this server grants for a request of `asked` under its own limits `offer`: each
  /// size and count the smaller of the two, with at least one slot and one operation, and no header
  /// padding (RFC 8881 §18.36.3: the server may reduce what the client asks).
  pub fn negotiated(asked: &ChannelAttrs, offer: &ChannelAttrs) -> ChannelAttrs {
    ChannelAttrs {
      header_pad: 0,
      max_request: asked.max_request.min(offer.max_request),
      max_response: asked.max_response.min(offer.max_response),
      max_response_cached: asked.max_response_cached.min(offer.max_response_cached),
      max_operations: asked.max_operations.min(offer.max_operations).max(1),
      max_requests: asked.max_requests.min(offer.max_requests).max(1),
    }
  }
}
