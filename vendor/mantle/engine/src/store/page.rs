//! A page of the engine's own files (docs/design/engine-structure.md §3): a header, then the
//! payload, sealed by a CRC-32C over the page and the page's own address, so a page read from the
//! wrong place fails as a corrupt one does.
//!
//! Layout, little-endian:
//!
//! | Bytes | Field |
//! |---|---|
//! | 0..4 | CRC-32C of bytes 4.. of the page, then of the page's address (8 bytes) |
//! | 4 | kind |
//! | 5 | format, [`FORMAT`] |
//! | 6..8 | zero |
//! | 8..12 | payload length |
//! | 12..20 | the generation of the checkpoint the page was written for |
//! | 20.. | payload, then zeros to the page's end |

use crate::error::{Error, Malformed};
use mantle_crc::{crc32c, crc32c_extend};

/// The header's bytes.
pub const HEADER: usize = 20;
/// The page format this engine writes and reads.
pub const FORMAT: u8 = 1;

/// What a page holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// One of the two superblock copies.
    Superblock,
    /// A page of the allocator's reference counts.
    Map,
    /// A page of the structure the store holds: a tree node, a branch's page.
    Node,
}

impl Kind {
    fn byte(self) -> u8 {
        match self {
            Self::Superblock => 1,
            Self::Map => 2,
            Self::Node => 3,
        }
    }

    fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(Self::Superblock),
            2 => Some(Self::Map),
            3 => Some(Self::Node),
            _ => None,
        }
    }
}

/// A verified page's header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// What the page holds.
    pub kind: Kind,
    /// The payload's bytes, from [`HEADER`].
    pub len: usize,
    /// The generation of the checkpoint the page was written for.
    pub generation: u64,
}

fn corrupt(why: Malformed) -> Error {
    Error::Corruption {
        what: "a store page",
        why,
    }
}

/// The checksum of `page` at `address`: bytes 4 to the end, then the address.
fn checksum(page: &[u8], address: u64) -> Result<u32, Error> {
    let body = page.get(4..).ok_or(corrupt(Malformed::Truncated))?;
    Ok(crc32c_extend(crc32c(body), &address.to_le_bytes()))
}

/// The payload's room in a page of `page_size` bytes.
pub fn capacity(page_size: usize) -> usize {
    page_size.saturating_sub(HEADER)
}

/// Seals `page`, whose payload of `len` bytes is already at [`HEADER`]: zeros past the payload,
/// the header, then the checksum over the page and `address`.
pub fn seal(
    page: &mut [u8],
    address: u64,
    kind: Kind,
    generation: u64,
    len: usize,
) -> Result<(), Error> {
    let end = HEADER
        .checked_add(len)
        .ok_or(corrupt(Malformed::TooLarge))?;
    page.get_mut(end..)
        .ok_or(Error::InvalidArgument {
            what: "a page payload longer than the page",
        })?
        .fill(0);
    let len32 = u32::try_from(len).map_err(|_| corrupt(Malformed::TooLarge))?;
    let head = page
        .get_mut(4..HEADER)
        .ok_or(corrupt(Malformed::Truncated))?;
    // Kind, format and two reserved bytes; the payload length; the generation: written in
    // place, as a page is sealed for every page written.
    let (tag, rest) = head
        .split_first_chunk_mut::<4>()
        .ok_or(corrupt(Malformed::Truncated))?;
    *tag = [kind.byte(), FORMAT, 0, 0];
    let (length, rest) = rest
        .split_first_chunk_mut::<4>()
        .ok_or(corrupt(Malformed::Truncated))?;
    *length = len32.to_le_bytes();
    rest.first_chunk_mut::<8>()
        .ok_or(corrupt(Malformed::Truncated))?
        .copy_from_slice(&generation.to_le_bytes());
    let crc = checksum(page, address)?;
    page.get_mut(..4)
        .ok_or(corrupt(Malformed::Truncated))?
        .copy_from_slice(&crc.to_le_bytes());
    Ok(())
}

/// Verifies `page`, read from `address`: its checksum, format and kind, and a payload within it.
/// Every failure is a typed corruption, which feeds repair (CLAUDE.md §6).
pub fn verify(page: &[u8], address: u64) -> Result<Header, Error> {
    let stored = page
        .first_chunk::<4>()
        .map(|b| u32::from_le_bytes(*b))
        .ok_or(corrupt(Malformed::Truncated))?;
    if stored != checksum(page, address)? {
        return Err(corrupt(Malformed::ChecksumMismatch));
    }
    let head = page
        .get(4..HEADER)
        .and_then(<[u8]>::first_chunk::<16>)
        .ok_or(corrupt(Malformed::Truncated))?;
    let [
        kind,
        format,
        z0,
        z1,
        l0,
        l1,
        l2,
        l3,
        g0,
        g1,
        g2,
        g3,
        g4,
        g5,
        g6,
        g7,
    ] = *head;
    if format != FORMAT {
        return Err(corrupt(Malformed::UnknownVersion(u64::from(format))));
    }
    if z0 != 0 || z1 != 0 {
        return Err(corrupt(Malformed::Forbidden));
    }
    let kind = Kind::from_byte(kind).ok_or(corrupt(Malformed::UnknownTag(kind)))?;
    let len = usize::try_from(u32::from_le_bytes([l0, l1, l2, l3]))
        .map_err(|_| corrupt(Malformed::TooLarge))?;
    if len > capacity(page.len()) {
        return Err(corrupt(Malformed::TooLarge));
    }
    Ok(Header {
        kind,
        len,
        generation: u64::from_le_bytes([g0, g1, g2, g3, g4, g5, g6, g7]),
    })
}

/// The payload of a verified page.
pub fn payload(page: &[u8], header: Header) -> Result<&[u8], Error> {
    let end = HEADER
        .checked_add(header.len)
        .ok_or(corrupt(Malformed::TooLarge))?;
    page.get(HEADER..end).ok_or(corrupt(Malformed::Truncated))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sealed(address: u64, payload: &[u8]) -> Vec<u8> {
        let mut page = vec![0xAAu8; 4096];
        page[HEADER..HEADER + payload.len()].copy_from_slice(payload);
        seal(&mut page, address, Kind::Node, 7, payload.len()).unwrap();
        page
    }

    #[test]
    fn a_sealed_page_verifies_and_gives_back_its_payload() {
        let page = sealed(42, b"hello");
        let header = verify(&page, 42).unwrap();
        assert_eq!(
            header,
            Header {
                kind: Kind::Node,
                len: 5,
                generation: 7
            }
        );
        assert_eq!(payload(&page, header).unwrap(), b"hello");
        // The bytes past the payload were zeroed, so they are covered as written.
        assert!(page[HEADER + 5..].iter().all(|&b| b == 0));
    }

    #[test]
    fn a_page_read_from_another_address_is_corrupt() {
        let page = sealed(42, b"hello");
        assert!(matches!(
            verify(&page, 43),
            Err(Error::Corruption {
                why: Malformed::ChecksumMismatch,
                ..
            })
        ));
    }

    #[test]
    fn every_flipped_bit_is_caught() {
        let page = sealed(9, b"payload bytes");
        for byte in 0..page.len() {
            for bit in 0..8 {
                let mut bad = page.clone();
                bad[byte] ^= 1 << bit;
                assert!(verify(&bad, 9).is_err(), "byte {byte} bit {bit}");
            }
        }
    }

    #[test]
    fn a_payload_longer_than_the_page_is_refused() {
        let mut page = vec![0u8; 64];
        assert!(seal(&mut page, 0, Kind::Node, 0, 64).is_err());
    }
}
