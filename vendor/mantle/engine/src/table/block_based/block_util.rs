//! `table/block_based/block_util.h` [R block_util.h:20-154]: decoding a block entry's header, and
//! reading a key's leading bytes as a big-endian integer for the uniformity scan.
//!
//! RocksDB's decoders take a pointer and a limit, assert three bytes remain on their fast path,
//! and return null on a malformed varint; here each takes the bytes left and returns how many it
//! consumed, and a header cut short is an error, never a read past the end.

use crate::db::dbformat::NUM_INTERNAL_BYTES;
use crate::error::Error;
use crate::util::coding::get_varint32_ptr;

/// An entry's header: shared key bytes, unshared key bytes, the value's length (0 where the
/// format leaves it out), and with separated keys and values the value's offset at a restart
/// point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EntryHeader {
    pub shared: u32,
    pub non_shared: u32,
    pub value_length: u32,
    pub value_offset: Option<u32>,
}

/// Reads one varint32 from `p` at `*at`.
fn varint(p: &[u8], at: &mut usize) -> Result<u32, Error> {
    let (v, n) = get_varint32_ptr(p.get(*at..).unwrap_or_default())?;
    *at = at.checked_add(n).ok_or(Error::truncated("block entry"))?;
    Ok(v)
}

/// `DecodeEntry` [R block_util.h:28-63] (`with_value_length`) and `DecodeEntryV4`/`DecodeKeyV4`
/// [R block_util.h:78-126] (without, as format_version 4 index blocks write it): the header at
/// the start of `p`, and how many bytes it takes. `with_value_offset` reads the value offset a
/// restart entry carries when keys and values are separated.
#[inline(always)]
pub fn decode_entry(
    p: &[u8],
    with_value_length: bool,
    with_value_offset: bool,
) -> Result<(EntryHeader, usize), Error> {
    // `DecodeKeyV4` refuses fewer than three bytes, though its header may take two: one more
    // for the value or its delta must follow [R block_util.h:84-89].
    if !with_value_length && p.len() < 3 {
        return Err(Error::truncated("block entry"));
    }
    // RocksDB's fast path [R block_util.h:39-44]: each field below 128 takes one byte, which is
    // how nearly every entry of a block of small keys and values is written.
    if !with_value_offset && let Some(&[a, b, c]) = p.first_chunk::<3>() {
        let fields = if with_value_length { a | b | c } else { a | b };
        if fields < 0x80 {
            let (value_length, used) = if with_value_length {
                (u32::from(c), 3)
            } else {
                (0, 2)
            };
            return Ok((
                EntryHeader {
                    shared: u32::from(a),
                    non_shared: u32::from(b),
                    value_length,
                    value_offset: None,
                },
                used,
            ));
        }
    }
    let mut at = 0usize;
    let shared = varint(p, &mut at)?;
    let non_shared = varint(p, &mut at)?;
    let value_length = if with_value_length {
        varint(p, &mut at)?
    } else {
        0
    };
    let value_offset = if with_value_offset {
        Some(varint(p, &mut at)?)
    } else {
        None
    };
    Ok((
        EntryHeader {
            shared,
            non_shared,
            value_length,
            value_offset,
        },
        at,
    ))
}

/// `ReadBe64FromKey` [R block_util.h:128-154]: the eight bytes of `key` from `offset`, as a
/// big-endian integer padded with zeros on the right, after dropping an internal key's eight
/// trailing bytes. RocksDB asserts an internal key holds those eight; here a shorter one is an
/// error.
pub fn read_be64_from_key(key: &[u8], is_user_key: bool, offset: usize) -> Result<u64, Error> {
    let key = if is_user_key {
        key
    } else {
        let user = key
            .len()
            .checked_sub(NUM_INTERNAL_BYTES)
            .ok_or(Error::truncated("internal key"))?;
        key.get(..user).unwrap_or_default()
    };
    let rest = key.get(offset.min(key.len())..).unwrap_or_default();
    let mut bytes = [0u8; 8];
    for (to, &from) in bytes.iter_mut().zip(rest) {
        *to = from;
    }
    Ok(u64::from_be_bytes(bytes))
}
