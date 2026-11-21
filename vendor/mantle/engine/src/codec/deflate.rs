//! Raw deflate, the engine's own: RocksDB's `kZlibCompression` block is a raw deflate stream (RFC
//! 1951) with a window of 2^14 bytes (`util/compression.cc`: `deflateInit2` and `inflateInit2`
//! at `window_bits` −14), its uncompressed size stored beside it by the block layer.
//!
//! A stream is blocks, bits read from each byte's least significant up. A block's header is
//! BFINAL (one bit) and BTYPE (two): 0 stored (to the next byte, LEN and its complement NLEN, LEN
//! bytes), 1 fixed Huffman codes, 2 dynamic codes described first (HLIT, HDIST and HCLEN, the code
//! length code's lengths in RFC 1951 §3.2.7's order, then the literal/length and distance code
//! lengths with repeats 16, 17 and 18). A Huffman code is read most significant bit first.
//! Literal/length symbols below 256 are bytes, 256 ends the block, 257 to 285 are lengths with
//! extra bits; a distance symbol follows each length, with its own extra bits.
//!
//! The decoder is canonical-code decoding as Mark Adler's `puff.c` (zlib's contrib) does it: the
//! codes' counts by length, each symbol read a bit at a time. It is given the size the block layer
//! stored and the window, and refuses a code the lengths do not make complete or make over-full, a
//! repeat past the code's lengths, a distance beyond the window or before the start, and anything
//! past the stated size. The encoder writes one block of fixed codes after LZ77 matching with a
//! table of 3-byte positions in the window; dynamic codes are not written yet (engine.md §5).

use crate::error::{Error, Malformed};

/// What the errors name.
const WHAT: &str = "a deflate stream";

/// The longest code: RFC 1951 §3.2.2, 15 bits.
const MAX_BITS: usize = 15;

/// Literal/length symbols in the fixed code: 288 (RFC 1951 §3.2.6).
const LITLEN_SYMBOLS: usize = 288;

/// Distance symbols in the fixed code: 32 (RFC 1951 §3.2.6).
const DIST_SYMBOLS: usize = 32;

/// The literal/length symbols a dynamic block may describe: 286 (RFC 1951 §3.2.7, HLIT ≤ 29).
const DYNAMIC_LITLEN: usize = 286;

/// The distance symbols a dynamic block may describe: 30 (RFC 1951 §3.2.7, HDIST ≤ 29).
const DYNAMIC_DIST: usize = 30;

/// The end-of-block symbol: 256 (RFC 1951 §3.2.5).
const END_OF_BLOCK: usize = 256;

/// Each length symbol's base length, 257 to 285 (RFC 1951 §3.2.5).
const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];

/// Each length symbol's extra bits.
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];

/// Each distance symbol's base distance, 0 to 29.
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];

/// Each distance symbol's extra bits.
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

/// The order the code length code's lengths are stored in (RFC 1951 §3.2.7).
const CLEN_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

/// The encoder's least match: three bytes, the format's least length.
const MIN_MATCH: usize = 3;

/// The encoder's longest: 258, the format's.
const MAX_MATCH: usize = 258;

/// The encoder's table of 3-byte positions: 2^15 entries, zlib's default hash size at memLevel 8
/// (`hash_bits = memLevel + 7`, deflate.c).
const HASH_BITS: u32 = 15;

/// The shift that keeps a hash's top `HASH_BITS` bits.
const HASH_SHIFT: u32 = 32 - HASH_BITS;

/// The largest window the format allows: 2^15 bytes (RFC 1951 §2).
const WINDOW_MAX: usize = 1 << 15;

fn bad(why: Malformed) -> Error {
    Error::corruption(WHAT, why)
}

/// `value` as a `usize`, or refused as too large.
fn size(value: u32) -> Result<usize, Error> {
    usize::try_from(value).map_err(|_| bad(Malformed::TooLarge))
}

/// The bits of a stream, least significant first.
struct Bits<'a> {
    input: &'a [u8],
    at: usize,
    held: u32,
    count: u32,
}

impl<'a> Bits<'a> {
    fn new(input: &'a [u8]) -> Self {
        Self {
            input,
            at: 0,
            held: 0,
            count: 0,
        }
    }

    /// The next `n` bits (at most 16), the first read lowest.
    fn take(&mut self, n: u32) -> Result<u32, Error> {
        while self.count < n {
            let byte = *self
                .input
                .get(self.at)
                .ok_or_else(|| bad(Malformed::Truncated))?;
            self.at = self.at.saturating_add(1);
            self.held |= u32::from(byte).checked_shl(self.count).unwrap_or(0);
            self.count = self.count.saturating_add(8);
        }
        let mask = 1u32.checked_shl(n).map_or(u32::MAX, |m| m.wrapping_sub(1));
        let value = self.held & mask;
        self.held = self.held.checked_shr(n).unwrap_or(0);
        self.count = self.count.saturating_sub(n);
        Ok(value)
    }

    /// `n` bits as a length or count.
    fn take_size(&mut self, n: u32) -> Result<usize, Error> {
        size(self.take(n)?)
    }

    /// Drops what is left of the current byte.
    fn align(&mut self) {
        self.held = 0;
        self.count = 0;
    }

    /// The next `n` whole bytes, after [`Bits::align`].
    fn bytes(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let end = self
            .at
            .checked_add(n)
            .ok_or_else(|| bad(Malformed::TooLarge))?;
        let bytes = self
            .input
            .get(self.at..end)
            .ok_or_else(|| bad(Malformed::Truncated))?;
        self.at = end;
        Ok(bytes)
    }
}

/// A canonical Huffman code: how many codes each length has, and the symbols in code order.
struct Code {
    counts: [u16; MAX_BITS + 1],
    symbols: Vec<u16>,
}

impl Code {
    /// The code `lengths` make; refused when over-full, or incomplete with more than one code
    /// (`puff.c` `construct`: one code of one bit is allowed incomplete, RFC 1951 §3.2.7).
    fn new(lengths: &[u8]) -> Result<Self, Error> {
        let mut counts = [0u16; MAX_BITS + 1];
        for &len in lengths {
            let slot = counts
                .get_mut(usize::from(len))
                .ok_or_else(|| bad(Malformed::Undecodable))?;
            *slot = slot.saturating_add(1);
        }
        let zero = usize::from(counts.first().copied().unwrap_or(0));
        let used = lengths.len().saturating_sub(zero);
        // The codes left unassigned at each length: never below none (over-full).
        let mut left: i64 = 1;
        for &count in counts.iter().skip(1) {
            left = left.saturating_mul(2).saturating_sub(i64::from(count));
            if left < 0 {
                return Err(bad(Malformed::Undecodable));
            }
        }
        if left > 0 && used > 1 {
            return Err(bad(Malformed::Undecodable));
        }
        // Where each length's symbols begin, in code order.
        let mut offsets = [0u16; MAX_BITS + 1];
        let mut next = 0u16;
        for (offset, &count) in offsets.iter_mut().zip(counts.iter()).skip(1) {
            *offset = next;
            next = next.saturating_add(count);
        }
        let mut symbols = vec![0u16; usize::from(next)];
        for (symbol, &len) in lengths.iter().enumerate() {
            if len == 0 {
                continue;
            }
            let offset = offsets
                .get_mut(usize::from(len))
                .ok_or_else(|| bad(Malformed::Undecodable))?;
            let slot = symbols
                .get_mut(usize::from(*offset))
                .ok_or_else(|| bad(Malformed::Undecodable))?;
            *slot = u16::try_from(symbol).map_err(|_| bad(Malformed::TooLarge))?;
            *offset = offset.saturating_add(1);
        }
        Ok(Self { counts, symbols })
    }

    /// The next symbol, its code read a bit at a time, most significant first.
    fn decode(&self, bits: &mut Bits<'_>) -> Result<usize, Error> {
        // The code read so far, the first code of its length, and the symbols before that length.
        let (mut code, mut first, mut index) = (0u32, 0u32, 0u32);
        for &count in self.counts.iter().skip(1) {
            code |= bits.take(1)?;
            let count = u32::from(count);
            if let Some(into) = code.checked_sub(first).filter(|into| *into < count) {
                let at = size(index.saturating_add(into))?;
                return self
                    .symbols
                    .get(at)
                    .map(|&symbol| usize::from(symbol))
                    .ok_or_else(|| bad(Malformed::Undecodable));
            }
            index = index.saturating_add(count);
            first = first.saturating_add(count) << 1;
            code <<= 1;
        }
        Err(bad(Malformed::Undecodable))
    }
}

/// The fixed codes of RFC 1951 §3.2.6.
fn fixed() -> Result<(Code, Code), Error> {
    let mut lengths = [0u8; LITLEN_SYMBOLS];
    for (symbol, len) in lengths.iter_mut().enumerate() {
        *len = match symbol {
            0..=143 => 8,
            144..=255 => 9,
            256..=279 => 7,
            _ => 8,
        };
    }
    Ok((Code::new(&lengths)?, Code::new(&[5u8; DIST_SYMBOLS])?))
}

/// `stream` inflated to exactly `len` bytes, its distances within a window of 2^`window_log`.
pub fn decompress(stream: &[u8], window_log: u32, len: usize) -> Result<Vec<u8>, Error> {
    let window = 1usize
        .checked_shl(window_log)
        .ok_or(Error::InvalidArgument {
            what: "a deflate window past the machine's word",
        })?;
    let mut out: Vec<u8> = Vec::new();
    out.try_reserve_exact(len)
        .map_err(|_| Error::LimitExceeded {
            what: "a deflate stream's output",
            limit: u64::try_from(len).unwrap_or(u64::MAX),
        })?;
    let mut bits = Bits::new(stream);
    loop {
        let last = bits.take(1)? == 1;
        match bits.take(2)? {
            0 => {
                bits.align();
                let header = bits.bytes(4)?;
                let pair = |at: usize| -> Result<u16, Error> {
                    let two = header
                        .get(at..at.saturating_add(2))
                        .ok_or_else(|| bad(Malformed::Truncated))?;
                    let mut le = [0u8; 2];
                    le.copy_from_slice(two);
                    Ok(u16::from_le_bytes(le))
                };
                let n = pair(0)?;
                if n != !pair(2)? {
                    return Err(bad(Malformed::Undecodable));
                }
                let bytes = bits.bytes(usize::from(n))?;
                if out
                    .len()
                    .checked_add(bytes.len())
                    .is_none_or(|end| end > len)
                {
                    return Err(bad(Malformed::CountMismatch));
                }
                out.extend_from_slice(bytes);
            }
            1 => {
                let (lit, dist) = fixed()?;
                codes(&mut bits, &lit, &dist, &mut out, len, window)?;
            }
            2 => {
                let (lit, dist) = dynamic(&mut bits)?;
                codes(&mut bits, &lit, &dist, &mut out, len, window)?;
            }
            _ => return Err(bad(Malformed::UnknownTag(3))),
        }
        if last {
            break;
        }
    }
    if out.len() != len {
        return Err(bad(Malformed::CountMismatch));
    }
    Ok(out)
}

/// A dynamic block's codes (RFC 1951 §3.2.7).
fn dynamic(bits: &mut Bits<'_>) -> Result<(Code, Code), Error> {
    let nlen = bits.take_size(5)?.saturating_add(257);
    let ndist = bits.take_size(5)?.saturating_add(1);
    let ncode = bits.take_size(4)?.saturating_add(4);
    if nlen > DYNAMIC_LITLEN || ndist > DYNAMIC_DIST {
        return Err(bad(Malformed::TooLarge));
    }
    let mut clens = [0u8; 19];
    for &at in CLEN_ORDER
        .get(..ncode)
        .ok_or_else(|| bad(Malformed::TooLarge))?
    {
        let slot = clens
            .get_mut(at)
            .ok_or_else(|| bad(Malformed::Undecodable))?;
        *slot = u8::try_from(bits.take(3)?).map_err(|_| bad(Malformed::TooLarge))?;
    }
    let clen = Code::new(&clens)?;
    let total = nlen.saturating_add(ndist);
    let mut lengths: Vec<u8> = Vec::with_capacity(total);
    while lengths.len() < total {
        let symbol = clen.decode(bits)?;
        let (value, repeat) = match symbol {
            0..=15 => (
                u8::try_from(symbol).map_err(|_| bad(Malformed::TooLarge))?,
                1,
            ),
            16 => {
                let previous = *lengths.last().ok_or_else(|| bad(Malformed::Undecodable))?;
                (previous, bits.take_size(2)?.saturating_add(3))
            }
            17 => (0, bits.take_size(3)?.saturating_add(3)),
            _ => (0, bits.take_size(7)?.saturating_add(11)),
        };
        if lengths.len().saturating_add(repeat) > total {
            return Err(bad(Malformed::TooLarge));
        }
        lengths.resize(lengths.len().saturating_add(repeat), value);
    }
    let (lit, dist) = lengths.split_at(nlen);
    // A block without an end-of-block code could not end (RFC 1951 §3.2.7).
    if lit.get(END_OF_BLOCK).copied().unwrap_or(0) == 0 {
        return Err(bad(Malformed::Undecodable));
    }
    Ok((Code::new(lit)?, Code::new(dist)?))
}

/// A length or distance: its base and extra bits for `symbol` in `bases` and `extras`.
fn based(bits: &mut Bits<'_>, symbol: usize, bases: &[u16], extras: &[u8]) -> Result<usize, Error> {
    let base = *bases
        .get(symbol)
        .ok_or_else(|| bad(Malformed::Undecodable))?;
    let extra = *extras
        .get(symbol)
        .ok_or_else(|| bad(Malformed::Undecodable))?;
    usize::from(base)
        .checked_add(bits.take_size(u32::from(extra))?)
        .ok_or_else(|| bad(Malformed::TooLarge))
}

/// A block's literals and matches, until its end-of-block code.
fn codes(
    bits: &mut Bits<'_>,
    lit: &Code,
    dist: &Code,
    out: &mut Vec<u8>,
    len: usize,
    window: usize,
) -> Result<(), Error> {
    loop {
        let symbol = lit.decode(bits)?;
        if let Ok(byte) = u8::try_from(symbol) {
            if out.len() >= len {
                return Err(bad(Malformed::CountMismatch));
            }
            out.push(byte);
            continue;
        }
        if symbol == END_OF_BLOCK {
            return Ok(());
        }
        let length = based(
            bits,
            symbol.saturating_sub(END_OF_BLOCK.saturating_add(1)),
            &LENGTH_BASE,
            &LENGTH_EXTRA,
        )?;
        let dsym = dist.decode(bits)?;
        let distance = based(bits, dsym, &DIST_BASE, &DIST_EXTRA)?;
        if distance > window {
            return Err(bad(Malformed::Undecodable));
        }
        if out.len().checked_add(length).is_none_or(|end| end > len) {
            return Err(bad(Malformed::CountMismatch));
        }
        let start = out
            .len()
            .checked_sub(distance)
            .ok_or_else(|| bad(Malformed::Undecodable))?;
        // A match longer than its distance repeats what it copies, a byte at a time.
        for at in start..start.saturating_add(length) {
            let byte = *out.get(at).ok_or_else(|| bad(Malformed::Undecodable))?;
            out.push(byte);
        }
    }
}

/// The bits the encoder writes, least significant first.
struct Writer {
    out: Vec<u8>,
    held: u64,
    count: u32,
}

impl Writer {
    /// Appends the low `n` (at most 32) bits of `value`, least significant first.
    fn put(&mut self, value: u32, n: u32) {
        self.held |= u64::from(value).checked_shl(self.count).unwrap_or(0);
        self.count = self.count.saturating_add(n);
        while self.count >= 8 {
            self.out.push(self.held.to_le_bytes()[0]);
            self.held >>= 8;
            self.count = self.count.saturating_sub(8);
        }
    }

    /// A Huffman code of `n` bits, which the format reads most significant first.
    fn put_code(&mut self, code: u32, n: u32) {
        self.put(
            code.reverse_bits()
                .checked_shr(32u32.saturating_sub(n))
                .unwrap_or(0),
            n,
        );
    }

    fn finish(mut self) -> Vec<u8> {
        if self.count > 0 {
            self.out.push(self.held.to_le_bytes()[0]);
        }
        self.out
    }
}

/// The fixed literal/length code of `symbol` and its length (RFC 1951 §3.2.6).
fn fixed_litlen(symbol: u16) -> (u32, u32) {
    let s = u32::from(symbol);
    match symbol {
        0..=143 => (s.saturating_add(0x30), 8),
        144..=255 => (s.saturating_sub(144).saturating_add(0x190), 9),
        256..=279 => (s.saturating_sub(256), 7),
        _ => (s.saturating_sub(280).saturating_add(0xc0), 8),
    }
}

/// The symbol index, extra value and extra bits of a match length or distance in a table of
/// bases.
fn symbol_of(value: usize, bases: &[u16], extras: &[u8]) -> (u16, u32, u32) {
    let index = bases
        .iter()
        .rposition(|&b| usize::from(b) <= value)
        .unwrap_or(0);
    let base = bases.get(index).map_or(0, |&b| usize::from(b));
    let extra = extras.get(index).map_or(0, |&e| u32::from(e));
    let over = u32::try_from(value.saturating_sub(base)).unwrap_or(0);
    (u16::try_from(index).unwrap_or(0), over, extra)
}

/// `input` deflated into one block of fixed codes, its distances within 2^`window_log`.
pub fn compress(input: &[u8], window_log: u32) -> Result<Vec<u8>, Error> {
    let window = 1usize
        .checked_shl(window_log)
        .filter(|w| *w <= WINDOW_MAX)
        .ok_or(Error::InvalidArgument {
            what: "a deflate window above 2^15",
        })?;
    let mut w = Writer {
        out: Vec::new(),
        held: 0,
        count: 0,
    };
    // The last block, of fixed codes.
    w.put(1, 1);
    w.put(1, 2);
    let mut table = vec![usize::MAX; 1 << HASH_BITS];
    let hash = |at: usize| -> Option<usize> {
        let three = input.get(at..at.checked_add(MIN_MATCH)?)?;
        let word = three
            .iter()
            .fold(0u32, |word, &byte| (word << 8) | u32::from(byte));
        usize::try_from(word.wrapping_mul(2_654_435_761) >> HASH_SHIFT).ok()
    };
    let mut ip = 0usize;
    while let Some(&byte) = input.get(ip) {
        let mut best = None;
        if let Some(h) = hash(ip)
            && let Some(slot) = table.get_mut(h)
        {
            let candidate = *slot;
            *slot = ip;
            if candidate != usize::MAX && ip.saturating_sub(candidate) <= window {
                let mut len = 0usize;
                while len < MAX_MATCH
                    && input.get(ip.saturating_add(len)).is_some()
                    && input.get(candidate.saturating_add(len)) == input.get(ip.saturating_add(len))
                {
                    len = len.saturating_add(1);
                }
                if len >= MIN_MATCH {
                    best = Some((len, ip.saturating_sub(candidate)));
                }
            }
        }
        let Some((len, distance)) = best else {
            let (code, n) = fixed_litlen(u16::from(byte));
            w.put_code(code, n);
            ip = ip.saturating_add(1);
            continue;
        };
        let (lsym, lval, lbits) = symbol_of(len, &LENGTH_BASE, &LENGTH_EXTRA);
        let (code, n) = fixed_litlen(lsym.saturating_add(257));
        w.put_code(code, n);
        w.put(lval, lbits);
        let (dsym, dval, dbits) = symbol_of(distance, &DIST_BASE, &DIST_EXTRA);
        w.put_code(u32::from(dsym), 5);
        w.put(dval, dbits);
        // The positions the match covers join the table, so later matches can reach them.
        for at in ip.saturating_add(1)..ip.saturating_add(len) {
            if let Some(h) = hash(at)
                && let Some(slot) = table.get_mut(h)
            {
                *slot = at;
            }
        }
        ip = ip.saturating_add(len);
    }
    let (code, n) = fixed_litlen(256);
    w.put_code(code, n);
    Ok(w.finish())
}
