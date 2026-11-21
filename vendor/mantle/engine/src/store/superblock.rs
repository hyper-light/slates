//! The superblock (docs/design/engine-structure.md §3, §7): two copies at pages 0 and 1, the
//! copy of generation `g` at page `g mod 2`, each naming a checkpoint whole. A copy is written only
//! after everything it names is durable, so the newest copy that verifies is always a complete
//! checkpoint; a copy torn by a crash fails its checksum and the other is taken (SplinterDB's
//! superblock, research/34 §1).
//!
//! Payload, little-endian:
//!
//! | Bytes | Field |
//! |---|---|
//! | 0..8 | [`MAGIC`] |
//! | 8..12 | page size |
//! | 12..16 | pages an extent |
//! | 16..24 | generation |
//! | 24..32 | the last applied Raft index |
//! | 32..40 | the root page, or `u64::MAX` for none |
//! | 40..48 | extents the file holds (the allocator map's length) |
//! | 48..52 | the map's extents, `m` |
//! | 52.. | the `m` map extents, 8 bytes each, in the map's order |

use crate::error::{Error, Malformed};

/// The superblock's magic number: "mantleSB" in ASCII, little-endian.
pub const MAGIC: u64 = u64::from_le_bytes(*b"mantleSB");
/// The fixed fields' bytes.
const FIXED: usize = 52;

/// A checkpoint as a superblock copy names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Superblock {
    /// The page size every page of the file has.
    pub page_size: u32,
    /// The pages an extent holds.
    pub extent_pages: u32,
    /// The checkpoint's generation, one more than the one before.
    pub generation: u64,
    /// The last Raft index the checkpoint's state includes.
    pub applied: u64,
    /// The structure's root page, if it has one.
    pub root: Option<u64>,
    /// Extents the file holds: the allocator map's length.
    pub extents: u64,
    /// The extents holding the allocator map, in its order.
    pub map: Vec<u64>,
}

fn corrupt(why: Malformed) -> Error {
    Error::Corruption {
        what: "a superblock",
        why,
    }
}

/// The most map extents a superblock in a payload of `room` bytes can name.
pub fn max_map_extents(room: usize) -> usize {
    room.saturating_sub(FIXED) / 8
}

/// The page size a payload's magic and fixed fields name, before its page is verified: what a
/// reader that does not know a store's page reads its superblock copies at.
pub fn named_page(payload: &[u8]) -> Option<u32> {
    let magic = payload.first_chunk::<8>().map(|b| u64::from_le_bytes(*b))?;
    let page = payload
        .get(8..)
        .and_then(<[u8]>::first_chunk::<4>)
        .map(|b| u32::from_le_bytes(*b))?;
    if magic == MAGIC { Some(page) } else { None }
}

impl Superblock {
    /// The payload's bytes.
    pub fn encoded_len(&self) -> Result<usize, Error> {
        self.map
            .len()
            .checked_mul(8)
            .and_then(|m| m.checked_add(FIXED))
            .ok_or(corrupt(Malformed::TooLarge))
    }

    /// Writes the payload into `out`, which must hold [`Self::len`] bytes.
    pub fn encode(&self, out: &mut [u8]) -> Result<(), Error> {
        let count = u32::try_from(self.map.len()).map_err(|_| corrupt(Malformed::TooLarge))?;
        let fixed = [
            &MAGIC.to_le_bytes()[..],
            &self.page_size.to_le_bytes()[..],
            &self.extent_pages.to_le_bytes()[..],
            &self.generation.to_le_bytes()[..],
            &self.applied.to_le_bytes()[..],
            &self.root.unwrap_or(u64::MAX).to_le_bytes()[..],
            &self.extents.to_le_bytes()[..],
            &count.to_le_bytes()[..],
        ]
        .concat();
        let (head, rest) = out
            .split_at_mut_checked(FIXED)
            .ok_or(corrupt(Malformed::TooLarge))?;
        head.copy_from_slice(&fixed);
        for (slot, &extent) in rest.as_chunks_mut::<8>().0.iter_mut().zip(&self.map) {
            *slot = extent.to_le_bytes();
        }
        if rest.len() < self.map.len().saturating_mul(8) {
            return Err(corrupt(Malformed::TooLarge));
        }
        Ok(())
    }

    /// Reads a payload, checking its magic and that its map fits it.
    pub fn decode(payload: &[u8]) -> Result<Self, Error> {
        let u64_at = |at: usize| {
            payload
                .get(at..)
                .and_then(<[u8]>::first_chunk::<8>)
                .map(|b| u64::from_le_bytes(*b))
                .ok_or(corrupt(Malformed::Truncated))
        };
        let u32_at = |at: usize| {
            payload
                .get(at..)
                .and_then(<[u8]>::first_chunk::<4>)
                .map(|b| u32::from_le_bytes(*b))
                .ok_or(corrupt(Malformed::Truncated))
        };
        if u64_at(0)? != MAGIC {
            return Err(corrupt(Malformed::BadMagic));
        }
        let count = usize::try_from(u32_at(48)?).map_err(|_| corrupt(Malformed::TooLarge))?;
        let end = count
            .checked_mul(8)
            .and_then(|m| m.checked_add(FIXED))
            .ok_or(corrupt(Malformed::TooLarge))?;
        let map_bytes = payload
            .get(FIXED..end)
            .ok_or(corrupt(Malformed::Truncated))?;
        let root = u64_at(32)?;
        Ok(Self {
            page_size: u32_at(8)?,
            extent_pages: u32_at(12)?,
            generation: u64_at(16)?,
            applied: u64_at(24)?,
            root: if root == u64::MAX { None } else { Some(root) },
            extents: u64_at(40)?,
            map: map_bytes
                .as_chunks::<8>()
                .0
                .iter()
                .map(|b| u64::from_le_bytes(*b))
                .collect(),
        })
    }

    /// The page this checkpoint's copy goes to: 0 or 1 by the generation's parity.
    pub fn slot(&self) -> u64 {
        self.generation & 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_superblock_reads_back_as_written() {
        let sb = Superblock {
            page_size: 4096,
            extent_pages: 64,
            generation: 9,
            applied: 1234,
            root: Some(77),
            extents: 12,
            map: vec![3, 5],
        };
        let mut out = vec![0u8; sb.encoded_len().unwrap()];
        sb.encode(&mut out).unwrap();
        assert_eq!(Superblock::decode(&out).unwrap(), sb);
        assert_eq!(sb.slot(), 1);
        let none = Superblock { root: None, ..sb };
        let mut out = vec![0u8; none.encoded_len().unwrap()];
        none.encode(&mut out).unwrap();
        assert_eq!(Superblock::decode(&out).unwrap().root, None);
    }

    #[test]
    fn a_wrong_magic_or_a_short_map_is_corrupt() {
        assert!(matches!(
            Superblock::decode(&[0u8; FIXED]),
            Err(Error::Corruption {
                why: Malformed::BadMagic,
                ..
            })
        ));
        let sb = Superblock {
            page_size: 4096,
            extent_pages: 64,
            generation: 1,
            applied: 0,
            root: None,
            extents: 1,
            map: vec![1, 2, 3],
        };
        let mut out = vec![0u8; sb.encoded_len().unwrap()];
        sb.encode(&mut out).unwrap();
        assert!(Superblock::decode(&out[..out.len() - 1]).is_err());
    }
}
