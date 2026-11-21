//! Huffman-coded literals (RFC 8878 §4.2): the tree description (§4.2.1), its weights direct or
//! FSE-compressed (§4.2.1.1, §4.2.1.2), the prefix codes they give (§4.2.1.3), and the streams
//! (§4.2.2), one or four behind a jump table (§3.1.1.3.1.6).

use super::Corrupt;
use super::bits::{Backward, Writer};
use super::fse::{
    EncodeState, EncodeTable, SYMBOLS_MAX, State, Table, normalize, read_distribution, table_log,
    write_distribution,
};

/// The longest prefix code (§4.2.1: "limits the maximum code length to 11 bits").
pub(super) const MAX_BITS: u32 = 11;
/// The most weights a description holds: literals 0 to 254, the last one's being deduced
/// (§4.2.1.2).
const MAX_WEIGHTS: usize = 255;
/// The weights' FSE accuracy log at most (§4.2.1.2).
const WEIGHTS_MAX_LOG: u32 = 6;
/// Weights an FSE description can name: 0 to `MAX_BITS`.
const WEIGHT_SYMBOLS: usize = 12;
/// A direct description's header bytes start here (§4.2.1.1).
const DIRECT: u8 = 128;
/// The jump table's length before four streams (§3.1.1.3.1.6).
const JUMP_TABLE: usize = 6;

/// One entry of the decoding table: the literal and the bits its code takes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Entry {
    symbol: u8,
    bits: u8,
}

/// A decoding table indexed by the stream's next `max_bits` bits; empty until a description is
/// read. Reading one reuses the table's allocation, as cloning into one does.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Huffman {
    max_bits: u32,
    entries: Vec<Entry>,
    /// The weights' FSE table, kept for its allocation.
    weights: Table,
}

impl Clone for Huffman {
    fn clone(&self) -> Self {
        Self {
            max_bits: self.max_bits,
            entries: self.entries.clone(),
            weights: Table::default(),
        }
    }

    fn clone_from(&mut self, source: &Self) {
        self.max_bits = source.max_bits;
        self.entries.clone_from(&source.entries);
    }
}

impl Huffman {
    /// Forgets the table, keeping its allocation.
    pub(super) fn unset(&mut self) {
        self.entries.clear();
    }

    /// Whether a description was read: treeless literals need one (§3.1.1.3.1.1).
    pub(super) fn is_set(&self) -> bool {
        !self.entries.is_empty()
    }

    /// Reads a tree description from the front of `bytes` into this table, returning the bytes
    /// it took. A description that fails leaves the table empty.
    pub(super) fn read(&mut self, bytes: &[u8]) -> Result<usize, Corrupt> {
        let read = self.read_description(bytes);
        if read.is_err() {
            self.unset();
        }
        read
    }

    fn read_description(&mut self, bytes: &[u8]) -> Result<usize, Corrupt> {
        let header = *bytes.first().ok_or(Corrupt::Huffman)?;
        let mut weights = [0u8; MAX_WEIGHTS];
        let (count, used) = if header >= DIRECT {
            direct_weights(bytes, header, &mut weights)?
        } else {
            fse_weights(bytes, header, &mut self.weights, &mut weights)?
        };
        self.set_weights(weights.get(..count).ok_or(Corrupt::Huffman)?)?;
        Ok(used)
    }

    /// Makes this the table of a series of weights, the last literal's weight deduced by
    /// completing to a power of two (§4.2.1).
    fn set_weights(&mut self, weights: &[u8]) -> Result<(), Corrupt> {
        if weights.len() >= 256 {
            return Err(Corrupt::Huffman);
        }
        // One pass counts the literals of each weight and finds the largest. Indexes are masked
        // to the counts' sixteen slots, so the pass has no bounds test and no branch; a weight
        // past `MAX_BITS` is refused after it, before any count is used.
        // Four count tables, a weight to each in turn, so a run of equal weights is not one chain
        // of increments through one slot (`HIST_count_parallel_wksp`'s four tables), summed after.
        let mut lanes = [[0usize; 16]; 4];
        let mut largest = 0u8;
        let (fours, rest) = weights.as_chunks::<4>();
        for four in fours {
            for (lane, &w) in lanes.iter_mut().zip(four) {
                largest = largest.max(w);
                if let Some(c) = lane.get_mut(usize::from(w & 15)) {
                    *c = c.wrapping_add(1);
                }
            }
        }
        for (lane, &w) in lanes.iter_mut().zip(rest) {
            largest = largest.max(w);
            if let Some(c) = lane.get_mut(usize::from(w & 15)) {
                *c = c.wrapping_add(1);
            }
        }
        let mut counts = [0usize; 16];
        for lane in &lanes {
            for (c, &l) in counts.iter_mut().zip(lane) {
                *c = c.wrapping_add(l);
            }
        }
        if u32::from(largest) > MAX_BITS {
            return Err(Corrupt::Huffman);
        }
        // A literal of weight `w` takes `2^(w-1)` of the total (§4.2.1): at most 255 literals of
        // at most 2^10 each, so neither the shift nor the sum overflows.
        let mut total = 0u32;
        for (w, &c) in counts
            .iter()
            .enumerate()
            .take(MAX_BITS as usize + 1)
            .skip(1)
        {
            let c = u32::try_from(c).map_err(|_| Corrupt::Huffman)?;
            let shift = u32::try_from(w.saturating_sub(1)).map_err(|_| Corrupt::Huffman)?;
            total = total
                .checked_add(c.checked_shl(shift).ok_or(Corrupt::Huffman)?)
                .ok_or(Corrupt::Huffman)?;
        }
        if total == 0 {
            return Err(Corrupt::Huffman);
        }
        // The bits of the next power of two above the total.
        let max_bits = u32::BITS.saturating_sub(total.leading_zeros());
        if max_bits > MAX_BITS {
            return Err(Corrupt::Huffman);
        }
        let rest = 1u32
            .checked_shl(max_bits)
            .and_then(|p| p.checked_sub(total))
            .ok_or(Corrupt::Huffman)?;
        if !rest.is_power_of_two() {
            return Err(Corrupt::Huffman);
        }
        let last = u8::try_from(u32::BITS.saturating_sub(rest.leading_zeros()))
            .map_err(|_| Corrupt::Huffman)?;
        // Codes are handed out by weight, lowest first, and by literal within a weight
        // (§4.2.1.3): each weight's run of the table starts where the lower weights' end.
        let c = counts.get_mut(usize::from(last)).ok_or(Corrupt::Huffman)?;
        *c = c.checked_add(1).ok_or(Corrupt::Huffman)?;
        let mut start = [0usize; MAX_BITS as usize + 2];
        let mut next = 0usize;
        for w in 1..=usize::try_from(max_bits).map_err(|_| Corrupt::Huffman)? {
            let count = *counts.get(w).ok_or(Corrupt::Huffman)?;
            if let Some(s) = start.get_mut(w) {
                *s = next;
            }
            next = next
                .checked_add(
                    count
                        .checked_shl(u32::try_from(w.saturating_sub(1)).unwrap_or(0))
                        .ok_or(Corrupt::Huffman)?,
                )
                .ok_or(Corrupt::Huffman)?;
        }
        let size = 1usize.checked_shl(max_bits).ok_or(Corrupt::Huffman)?;
        if next != size {
            return Err(Corrupt::Huffman);
        }
        // Every entry is written below (the runs sum to the size), so a table of the same size
        // is not cleared first.
        if self.entries.len() != size {
            self.entries.resize(size, Entry::default());
        }
        // Number_of_Bits = Max_Number_of_Bits + 1 - Weight (§4.2.1), by weight; every weight is
        // at most max_bits, as the total's power of two bounds each.
        let mut bits_of = [0u8; MAX_BITS as usize + 2];
        for (w, b) in bits_of.iter_mut().enumerate().skip(1) {
            *b = u8::try_from(
                max_bits
                    .saturating_add(1)
                    .saturating_sub(u32::try_from(w).unwrap_or(0)),
            )
            .unwrap_or(0);
        }
        // The literals sorted by weight, by literal within a weight: each weight's run of the
        // table is then filled by one loop whose piece size is fixed (`HUF_readDTableX1_wksp`
        // sorts by weight the same way), not by a choice per literal. Absent literals (weight
        // 0) sort into a region of their own ahead of the rest, so placing one takes no branch.
        let mut sorted = [0u8; 256];
        let mut place = [0usize; 16];
        let mut first = 0usize;
        for (p, &c) in place.iter_mut().zip(&counts) {
            *p = first;
            first = first.checked_add(c).ok_or(Corrupt::Huffman)?;
        }
        // Masked as the counting was: every weight is now at most `MAX_BITS`, and the places
        // stay below the 256 literals counted.
        for (symbol, &w) in (0u8..=255).zip(weights.iter().chain(std::iter::once(&last))) {
            if let Some(p) = place.get_mut(usize::from(w & 15)) {
                if let Some(slot) = sorted.get_mut(*p & 255) {
                    *slot = symbol;
                }
                *p = p.wrapping_add(1);
            }
        }
        let mut from = *counts.first().ok_or(Corrupt::Huffman)?;
        for w in 1..=usize::try_from(max_bits).map_err(|_| Corrupt::Huffman)? {
            let count = *counts.get(w).ok_or(Corrupt::Huffman)?;
            let to = from.checked_add(count).ok_or(Corrupt::Huffman)?;
            let symbols = sorted.get(from..to).ok_or(Corrupt::Huffman)?;
            from = to;
            let span = 1usize << w.saturating_sub(1);
            let at = *start.get(w).ok_or(Corrupt::Huffman)?;
            let end = count
                .checked_mul(span)
                .and_then(|n| n.checked_add(at))
                .ok_or(Corrupt::Huffman)?;
            let bits = *bits_of.get(w).ok_or(Corrupt::Huffman)?;
            let run = self.entries.get_mut(at..end).ok_or(Corrupt::Huffman)?;
            let entry = |symbol| Entry { symbol, bits };
            // A run is a power of two long: written in pieces of a fixed size, each a store or
            // two (`HUF_DEltX1_set4`'s switch on the run's length, here once a weight).
            match span {
                1 => {
                    for (e, &s) in run.iter_mut().zip(symbols) {
                        *e = entry(s);
                    }
                }
                2 => {
                    for (e, &s) in run.as_chunks_mut::<2>().0.iter_mut().zip(symbols) {
                        *e = [entry(s); 2];
                    }
                }
                4 => {
                    for (e, &s) in run.as_chunks_mut::<4>().0.iter_mut().zip(symbols) {
                        *e = [entry(s); 4];
                    }
                }
                _ => {
                    for (e, &s) in run.chunks_exact_mut(span).zip(symbols) {
                        for piece in e.as_chunks_mut::<8>().0 {
                            *piece = [entry(s); 8];
                        }
                    }
                }
            }
        }
        self.max_bits = max_bits;
        Ok(())
    }

    /// The symbol the lane's next code names, its bits consumed: the lane must hold `max_bits`
    /// since its last refill. The table has `2^max_bits` entries, so the masked index is in it.
    #[inline(always)]
    fn decode(&self, lane: &mut Lane<'_>) -> u8 {
        let mask = self.entries.len().wrapping_sub(1);
        let index = lane.top(self.max_bits) & mask;
        let entry = self.entries.get(index).copied().unwrap_or_default();
        lane.consume(u32::from(entry.bits));
        entry.symbol
    }

    /// Decodes the rest of one lane into `out`, a symbol a refill.
    #[inline(always)]
    fn rest(&self, lane: &mut Lane<'_>, out: &mut [u8]) {
        for byte in out {
            lane.refill(self.max_bits);
            *byte = self.decode(lane);
        }
    }

    /// Decodes one stream into `out`, which it fills exactly; the stream must be consumed
    /// exactly (§4.2.2).
    fn stream(&self, stream: &[u8], out: &mut [u8]) -> Result<(), Corrupt> {
        let mut lane = Lane::new(stream)?;
        // Four codes of at most 11 bits under one refill (`HUF_decodeStreamX1`'s unrolling).
        let quad = self.max_bits.saturating_mul(4);
        let (fours, tail) = out.as_chunks_mut::<4>();
        for four in fours {
            lane.refill(quad);
            for byte in four {
                *byte = self.decode(&mut lane);
            }
        }
        self.rest(&mut lane, tail);
        if !lane.finished() {
            return Err(Corrupt::Huffman);
        }
        Ok(())
    }

    /// Decodes four streams into their segments, interleaved: each round takes four codes of
    /// every stream, the four streams' chains independent so their table reads overlap
    /// (`HUF_decompress4X1_usingDTable_internal_fast`'s loop). The rounds stop where the
    /// shortest segment, the fourth, runs out of whole groups of four; each stream then finishes
    /// alone, and each must be consumed exactly (§4.2.2).
    fn four_streams(&self, streams: [&[u8]; 4], outs: [&mut [u8]; 4]) -> Result<(), Corrupt> {
        let [i1, i2, i3, i4] = streams;
        let [o1, o2, o3, o4] = outs;
        let (mut b1, mut b2, mut b3, mut b4) = (
            Lane::new(i1)?,
            Lane::new(i2)?,
            Lane::new(i3)?,
            Lane::new(i4)?,
        );
        let (q1, t1) = o1.as_chunks_mut::<4>();
        let (q2, t2) = o2.as_chunks_mut::<4>();
        let (q3, t3) = o3.as_chunks_mut::<4>();
        let (q4, t4) = o4.as_chunks_mut::<4>();
        let rounds = q4.len();
        let (q1, r1) = q1.split_at_mut(rounds.min(q1.len()));
        let (q2, r2) = q2.split_at_mut(rounds.min(q2.len()));
        let (q3, r3) = q3.split_at_mut(rounds.min(q3.len()));
        for (((a, b), c), d) in q1.iter_mut().zip(q2.iter_mut()).zip(q3.iter_mut()).zip(q4) {
            b1.reload();
            b2.reload();
            b3.reload();
            b4.reload();
            for (((w, x), y), z) in a.iter_mut().zip(b.iter_mut()).zip(c.iter_mut()).zip(d) {
                *w = self.decode(&mut b1);
                *x = self.decode(&mut b2);
                *y = self.decode(&mut b3);
                *z = self.decode(&mut b4);
            }
        }
        // What each segment has left: its whole groups past the rounds, then its tail.
        for (bits, groups, tail) in [
            (&mut b1, r1, t1),
            (&mut b2, r2, t2),
            (&mut b3, r3, t3),
            (&mut b4, &mut [][..], t4),
        ] {
            self.rest(bits, groups.as_flattened_mut());
            self.rest(bits, tail);
            if !bits.finished() {
                return Err(Corrupt::Huffman);
            }
        }
        Ok(())
    }

    /// Decodes `streams` (one stream, or four behind a jump table) into `out`, which takes
    /// exactly the regenerated size (§3.1.1.3.1.6).
    pub(super) fn literals(
        &self,
        streams: &[u8],
        four: bool,
        out: &mut [u8],
    ) -> Result<(), Corrupt> {
        if !four {
            return self.stream(streams, out);
        }
        let (jump, rest) = streams
            .split_at_checked(JUMP_TABLE)
            .ok_or(Corrupt::Huffman)?;
        let size = |i: usize| -> usize {
            let at = i.saturating_mul(2);
            let lo = jump.get(at).copied().unwrap_or(0);
            let hi = jump.get(at.saturating_add(1)).copied().unwrap_or(0);
            usize::from(u16::from_le_bytes([lo, hi]))
        };
        let (s1, s2, s3) = (size(0), size(1), size(2));
        let first3 = s1
            .checked_add(s2)
            .and_then(|x| x.checked_add(s3))
            .ok_or(Corrupt::Huffman)?;
        if first3 > rest.len() {
            return Err(Corrupt::Huffman);
        }
        let segment = out.len().div_ceil(4);
        // The first three segments are whole; the fourth takes what is left, at most 3 bytes
        // fewer, never a negative size.
        if segment.checked_mul(3).is_none_or(|three| three > out.len()) {
            return Err(Corrupt::Huffman);
        }
        let (in1, rest) = rest.split_at_checked(s1).ok_or(Corrupt::Huffman)?;
        let (in2, rest) = rest.split_at_checked(s2).ok_or(Corrupt::Huffman)?;
        let (in3, in4) = rest.split_at_checked(s3).ok_or(Corrupt::Huffman)?;
        let (out1, rest) = out.split_at_mut_checked(segment).ok_or(Corrupt::Huffman)?;
        let (out2, rest) = rest.split_at_mut_checked(segment).ok_or(Corrupt::Huffman)?;
        let (out3, out4) = rest.split_at_mut_checked(segment).ok_or(Corrupt::Huffman)?;
        self.four_streams([in1, in2, in3, in4], [out1, out2, out3, out4])?;
        Ok(())
    }
}

/// One Huffman stream read from its end (§4.2.2), its next bits held at the top of a word, as
/// the reference's fast loop holds them (zstd 1.5.7 `huf_decompress.c`,
/// `HUF_decompress4X1_usingDTable_internal_fast_c_loop`): a code's table index is the word's top
/// bits, and taking the code shifts the word. Bits below the stream's start read as zero, and a
/// lane read past its start can never finish, so corrupt input fails when the stream ends.
struct Lane<'a> {
    bytes: &'a [u8],
    /// The stream's bits below the word's lowest, a whole number of bytes.
    base: usize,
    /// The next bits, highest first.
    word: u64,
    /// The word's bits still to read; negative once a read went past the stream's start.
    avail: i64,
}

impl<'a> Lane<'a> {
    /// A lane positioned below the stream's final 1 bit. A stream that is empty or whose last
    /// byte is zero has no such bit and is corrupt.
    fn new(bytes: &'a [u8]) -> Result<Self, Corrupt> {
        let last = *bytes.last().ok_or(Corrupt::Huffman)?;
        if last == 0 {
            return Err(Corrupt::Huffman);
        }
        let marker = i64::from(7u32.saturating_sub(last.leading_zeros()));
        let pos = i64::try_from(bytes.len())
            .map_err(|_| Corrupt::Huffman)?
            .checked_sub(1)
            .and_then(|b| b.checked_mul(8))
            .and_then(|b| b.checked_add(marker))
            .ok_or(Corrupt::Huffman)?;
        let mut lane = Self {
            bytes,
            base: 0,
            word: 0,
            avail: 0,
        };
        lane.load(pos);
        Ok(lane)
    }

    /// Loads the word with the `pos` bits below the position: as many as eight bytes hold,
    /// left-aligned. The stream's last eight bytes and a position past its start take the cold
    /// path.
    #[inline(always)]
    fn load(&mut self, pos: i64) {
        if let Ok(pos) = usize::try_from(pos) {
            // The lowest byte whose word still reaches `pos`: the word then holds 57 to 64 of
            // them, or all there are.
            let byte = pos.saturating_sub(57) / 8;
            if let Some(w) = self.bytes.get(byte..).and_then(<[u8]>::first_chunk::<8>) {
                let base = byte.wrapping_mul(8);
                // `pos - base` is 1 to 64.
                let have = u32::try_from(pos.wrapping_sub(base)).unwrap_or(0);
                self.word = u64::from_le_bytes(*w)
                    .checked_shl(64u32.wrapping_sub(have))
                    .unwrap_or(0);
                self.avail = i64::from(have);
                self.base = base;
                return;
            }
        }
        self.load_rare(pos);
    }

    /// [`Self::load`] where fewer than eight bytes are left or the position is past the start.
    #[cold]
    #[inline(never)]
    fn load_rare(&mut self, pos: i64) {
        let Ok(pos) = usize::try_from(pos) else {
            // Past the start: nothing left to load.
            self.word = 0;
            self.avail = pos;
            self.base = 0;
            return;
        };
        let byte = pos.saturating_sub(57) / 8;
        let mut w = [0u8; 8];
        if let Some(src) = self.bytes.get(byte..) {
            for (d, s) in w.iter_mut().zip(src) {
                *d = *s;
            }
        }
        let base = byte.wrapping_mul(8);
        let have = u32::try_from(pos.wrapping_sub(base)).unwrap_or(0);
        self.word = u64::from_le_bytes(w)
            .checked_shl(64u32.wrapping_sub(have))
            .unwrap_or(0);
        self.avail = i64::from(have);
        self.base = base;
    }

    /// Loads the word at the lane's position whether or not it still holds enough: the
    /// interleaved rounds take this over [`Self::refill`], whose test on each lane is a branch
    /// the four lanes' differing code lengths make unpredictable, where a load is a few
    /// instructions (`HUF_decompress4X1_usingDTable_internal_fast_c_loop` reloads every lane
    /// every round). A round reads at most 44 bits, and a load leaves at least 57 unless the
    /// stream's start is nearer.
    #[inline(always)]
    fn reload(&mut self) {
        let pos = i64::try_from(self.base)
            .unwrap_or(0)
            .wrapping_add(self.avail);
        self.load(pos);
    }

    /// Makes `bits` readable (at most 57) unless the word already reaches the stream's start.
    #[inline(always)]
    fn refill(&mut self, bits: u32) {
        if self.avail < i64::from(bits) && self.base > 0 {
            let pos = i64::try_from(self.base)
                .unwrap_or(0)
                .wrapping_add(self.avail);
            self.load(pos);
        }
    }

    /// The word's top `bits` (1 to 11), as a table index.
    #[inline(always)]
    fn top(&self, bits: u32) -> usize {
        usize::try_from(self.word.checked_shr(64u32.wrapping_sub(bits)).unwrap_or(0)).unwrap_or(0)
    }

    /// Consumes `bits` (at most 11).
    #[inline(always)]
    fn consume(&mut self, bits: u32) {
        self.word = self.word.checked_shl(bits).unwrap_or(0);
        self.avail = self.avail.wrapping_sub(i64::from(bits));
    }

    /// Whether every bit was read and none past the start (§4.2.2).
    fn finished(&self) -> bool {
        self.base == 0 && self.avail == 0
    }
}

/// Weights written directly, 4 bits each, two to a byte, high nibble first (§4.2.1.1), into
/// `out`; returns how many and the bytes taken.
fn direct_weights(
    bytes: &[u8],
    header: u8,
    out: &mut [u8; MAX_WEIGHTS],
) -> Result<(usize, usize), Corrupt> {
    // Number_of_Symbols = headerByte - 127 (§4.2.1.1); `header` is at least `DIRECT`, so at most
    // 128 symbols.
    let count = usize::from(header.saturating_sub(DIRECT - 1));
    let len = count.div_ceil(2);
    let end = len.checked_add(1).ok_or(Corrupt::Huffman)?;
    let packed = bytes.get(1..end).ok_or(Corrupt::Huffman)?;
    for (w, slot) in packed
        .iter()
        .flat_map(|b| [b >> 4, b & 0xF])
        .take(count)
        .zip(out.iter_mut())
    {
        *slot = w;
    }
    Ok((count, end))
}

/// Weights compressed by FSE, two interleaved states over one table, until a state's update
/// reads past the stream's start (§4.2.1.2), into `out` with `table` rebuilt for them; returns
/// how many and the bytes taken.
fn fse_weights(
    bytes: &[u8],
    header: u8,
    table: &mut Table,
    out: &mut [u8; MAX_WEIGHTS],
) -> Result<(usize, usize), Corrupt> {
    // The FSE-compressed series is `headerByte` bytes long (§4.2.1.1).
    let end = usize::from(header).checked_add(1).ok_or(Corrupt::Huffman)?;
    let body = bytes.get(1..end).ok_or(Corrupt::Huffman)?;
    let mut norm = [0i16; SYMBOLS_MAX];
    let (symbols, log, used) = read_distribution(body, WEIGHT_SYMBOLS, WEIGHTS_MAX_LOG, &mut norm)?;
    table.set_distribution(norm.get(..symbols).ok_or(Corrupt::Huffman)?, log)?;
    let stream = body.get(used..).ok_or(Corrupt::Huffman)?;
    let mut bits = Backward::new(stream)?;
    let mut states = [State::init(table, &mut bits), State::init(table, &mut bits)];
    let mut count = 0usize;
    let mut push = |count: &mut usize, w: u8| -> Result<(), Corrupt> {
        *out.get_mut(*count).ok_or(Corrupt::Huffman)? = w;
        *count = count.checked_add(1).ok_or(Corrupt::Huffman)?;
        Ok(())
    };
    'decode: loop {
        // A pair of weights reads at most 2 × 6 bits, and a reload leaves at least 57 unless the
        // word starts at the stream's first byte, where reads past it still count down to the
        // overflow that ends the loop: one reload a pair, no test a weight.
        bits.reload();
        for i in 0..2 {
            // One weight short of the most, so the other state's last symbol still fits.
            if count >= MAX_WEIGHTS.saturating_sub(1) {
                return Err(Corrupt::Huffman);
            }
            let state = states.get_mut(i).ok_or(Corrupt::Huffman)?;
            push(&mut count, state.symbol(table))?;
            state.update_ensured(table, &mut bits);
            if bits.overflowed() {
                // The other state's symbol is the last.
                let other = states.get(i ^ 1).ok_or(Corrupt::Huffman)?;
                push(&mut count, other.symbol(table))?;
                break 'decode;
            }
        }
    }
    Ok((count, end))
}

/// The literals a Huffman table codes: every byte value (§4.2.1).
pub(super) const LITERALS: usize = 256;

/// Code lengths of at most `limit` bits (at most [`MAX_BITS`]) for `counts` (one per literal, 0
/// for absent), into `lengths`, as the reference makes them (zstd 1.5.7
/// lib/compress/huf_compress.c, `HUF_buildCTable_wksp`): a minimum-redundancy code, and where
/// it is deeper than `limit`, that code repaired to the limit by `HUF_setMaxHeight`
/// ([`set_max_height`]). Absent literals get 0; a single present literal gets 1.
///
/// The repair is not the optimal length-limited code (package-merge's), and is kept over it on
/// measurement: on RocksDB-format blocks of object-store and association metadata, at the
/// reference's depth for the literals' count, package-merge's lengths took about a quarter of
/// compression's time at 4 KiB blocks, and the sections they gave were no smaller once the
/// table's description is counted (smaller on one corpus by 0.1%, larger on the other). The
/// optimal code minimizes the coded bits alone; the description follows the lengths.
pub(super) fn code_lengths(counts: &[u32], limit: u32, lengths: &mut [u8; LITERALS]) {
    lengths.fill(0);
    // The present literals by count, then by literal, as the rows merge them.
    let mut leaves = [(0u64, 0u16); LITERALS];
    let mut n = 0usize;
    for (s, &c) in counts.iter().enumerate().take(LITERALS) {
        if c > 0
            && let Some(leaf) = leaves.get_mut(n)
        {
            *leaf = (u64::from(c), u16::try_from(s).unwrap_or(0));
            n = n.saturating_add(1);
        }
    }
    let Some(leaves) = leaves.get_mut(..n) else {
        return;
    };
    if let [(_, only)] = leaves {
        if let Some(l) = lengths.get_mut(usize::from(*only)) {
            *l = 1;
        }
        return;
    }
    leaves.sort_unstable();
    let leaves = &*leaves;
    // The minimum-redundancy code in place, in time linear after the sort; past the limit it
    // is repaired. A code of `n` lengths needs `2^limit >= n`: the reference's depth for the
    // literals always leaves more (`optimal_log` takes at least `highbit(max_symbol) + 2`), and
    // with exactly `n` every length is the limit.
    if huffman_within(leaves, u32::from(u8::MAX), lengths) {
        set_max_height(leaves, limit, lengths);
    }
}

/// Repairs `lengths`, a minimum-redundancy code for `leaves` (sorted by count, then literal; at
/// least two), to at most `target` bits, as the reference does (zstd 1.5.7
/// lib/compress/huf_compress.c, `HUF_setMaxHeight`, its nodes the leaves by count descending).
/// Every length past `target` is cut to it, which overspends the code space; the overspend is
/// repaid by lengthening the cheapest shorter codes, a rank at a time, and any overshoot is
/// given back to the longest. The code stays complete. With `2^target` equal to the leaves'
/// count, the only complete code is every length `target`.
fn set_max_height(leaves: &[(u64, u16)], target: u32, lengths: &mut [u8; LITERALS]) {
    let n = leaves.len();
    let Some(last) = n.checked_sub(1) else {
        return;
    };
    // Node `i`, the `i`-th heaviest leaf: its count and literal.
    let node = |i: usize| last.checked_sub(i).and_then(|j| leaves.get(j)).copied();
    let count = |i: usize| node(i).map_or(0, |(c, _)| c);
    let mut bits = [0u32; LITERALS];
    for (i, b) in bits.iter_mut().enumerate().take(n) {
        *b = node(i)
            .and_then(|(_, s)| lengths.get(usize::from(s)))
            .map_or(0, |&l| u32::from(l));
    }
    let at = |bits: &[u32; LITERALS], i: usize| bits.get(i).copied().unwrap_or(0);
    let largest = at(&bits, last);
    if largest <= target {
        return;
    }
    if 1usize.checked_shl(target) == Some(n) {
        for b in bits.iter_mut().take(n) {
            *b = target;
        }
    } else {
        repay(&mut bits, last, target, largest, count);
    }
    for (i, &b) in bits.iter().enumerate().take(n) {
        if let Some((_, s)) = node(i)
            && let Some(l) = lengths.get_mut(usize::from(s))
        {
            *l = u8::try_from(b).unwrap_or(u8::MAX);
        }
    }
}

/// [`set_max_height`]'s cut and repayment over `bits`, the nodes' lengths heaviest first, the
/// deepest `largest` at `last`. Kraft sums are counted in units of `2^-largest`, then of
/// `2^-target`, as the reference counts them.
fn repay(
    bits: &mut [u32; LITERALS],
    last: usize,
    target: u32,
    largest: u32,
    count: impl Fn(usize) -> u64,
) {
    let get = |bits: &[u32; LITERALS], i: usize| bits.get(i).copied().unwrap_or(0);
    let shift = largest.saturating_sub(target);
    let unit = |b: u32| 1i64.checked_shl(largest.saturating_sub(b)).unwrap_or(0);
    // Cut every length past the target, totting up what that overspends.
    let mut total = 0i64;
    let mut k = last;
    while get(bits, k) > target {
        total = total
            .saturating_add(unit(target))
            .saturating_sub(unit(get(bits, k)));
        if let Some(b) = bits.get_mut(k) {
            *b = target;
        }
        let Some(next) = k.checked_sub(1) else {
            break;
        };
        k = next;
    }
    // `k`: the lightest node shorter than the target.
    while get(bits, k) == target {
        let Some(next) = k.checked_sub(1) else {
            break;
        };
        k = next;
    }
    total >>= shift;
    // The lightest node of each length `target - r`, by `r`.
    const RANKS: usize = MAX_BITS as usize + 3;
    let mut rank_last: [Option<usize>; RANKS] = [None; RANKS];
    let mut current = target;
    for pos in (0..=k).rev() {
        let b = get(bits, pos);
        if b < current {
            current = b;
            if let Some(r) = usize::try_from(target.saturating_sub(b))
                .ok()
                .and_then(|r| rank_last.get_mut(r))
            {
                *r = Some(pos);
            }
        }
    }
    let rank = |rank_last: &[Option<usize>; RANKS], r: usize| rank_last.get(r).copied().flatten();
    // Repay: lengthen the node whose rank's cost is the next power of two above what is owed,
    // unless two of the rank below cost less (each lengthening a node of rank `r` repays
    // `2^(r-1)`). Each pass repays at least one unit, so at most `total` passes run.
    while total > 0 {
        let owed = u64::try_from(total).unwrap_or(0);
        let mut r = usize::try_from(u64::BITS.saturating_sub(owed.leading_zeros())).unwrap_or(0);
        while r > 1 {
            match (rank(&rank_last, r), rank(&rank_last, r.saturating_sub(1))) {
                (None, _) => r = r.saturating_sub(1),
                (Some(_), None) => break,
                (Some(high), Some(low)) => {
                    if count(high) <= count(low).saturating_mul(2) {
                        break;
                    }
                    r = r.saturating_sub(1);
                }
            }
        }
        while r < RANKS.saturating_sub(1) && rank(&rank_last, r).is_none() {
            r = r.saturating_add(1);
        }
        let Some(p) = rank(&rank_last, r) else {
            break;
        };
        let paid = u32::try_from(r.saturating_sub(1)).unwrap_or(0);
        total = total.saturating_sub(1i64.checked_shl(paid).unwrap_or(0));
        if let Some(b) = bits.get_mut(p) {
            *b = b.saturating_add(1);
        }
        // The lengthened node is the new lightest of the rank below if that was empty, and
        // leaves its own rank to the node before it if that one shares the rank.
        let below = r.saturating_sub(1);
        if rank(&rank_last, below).is_none()
            && let Some(slot) = rank_last.get_mut(below)
        {
            *slot = Some(p);
        }
        let rank_bits = usize::try_from(target)
            .ok()
            .and_then(|t| t.checked_sub(r))
            .and_then(|b| u32::try_from(b).ok());
        let previous = p
            .checked_sub(1)
            .filter(|&q| Some(get(bits, q)) == rank_bits);
        if let Some(slot) = rank_last.get_mut(r) {
            *slot = previous;
        }
    }
    // An overshoot is given back to the longest codes: each step shortens a node of rank 0
    // (`target` bits) to rank 1, so at most `-total` steps run.
    while total < 0 {
        match rank(&rank_last, 1) {
            None => {
                while get(bits, k) == target {
                    let Some(next) = k.checked_sub(1) else {
                        break;
                    };
                    k = next;
                }
                let first = k.saturating_add(1);
                if let Some(b) = bits.get_mut(first) {
                    *b = b.saturating_sub(1);
                }
                if let Some(slot) = rank_last.get_mut(1) {
                    *slot = Some(first);
                }
            }
            Some(r1) => {
                let next = r1.saturating_add(1);
                if let Some(b) = bits.get_mut(next) {
                    *b = b.saturating_sub(1);
                }
                if let Some(slot) = rank_last.get_mut(1) {
                    *slot = Some(next);
                }
            }
        }
        total = total.saturating_add(1);
    }
}

/// Minimum-redundancy code lengths for `leaves` (sorted by count, at least two), computed in
/// place (Moffat and Katajainen, "In-place calculation of minimum-redundancy codes", WADS 1995):
/// the tree's internal weights, then each node's depth, then each leaf's. Writes them into
/// `lengths` and returns true when none exceeds `limit`; writes nothing and returns false
/// otherwise.
fn huffman_within(leaves: &[(u64, u16)], limit: u32, lengths: &mut [u8; LITERALS]) -> bool {
    let n = leaves.len();
    if !(2..=LITERALS).contains(&n) {
        return false;
    }
    let mut a = [0u64; LITERALS];
    for (slot, &(w, _)) in a.iter_mut().zip(leaves) {
        *slot = w;
    }
    let Some(a) = a.get_mut(..n) else {
        return false;
    };
    let last = n.wrapping_sub(1);
    // Every index below is under `n`: `root < next < n` and `leaf` is checked against `n`.
    let at = |a: &[u64], i: usize| a.get(i).copied().unwrap_or(u64::MAX);
    // Phase 1: each internal node's weight, a parent's index stored in each child's place.
    let (mut root, mut leaf) = (0usize, 2usize);
    let second = at(a, 1);
    if let Some(y) = a.first_mut() {
        *y = y.saturating_add(second);
    }
    for next in 1..last {
        let first = if leaf >= n || at(a, root) < at(a, leaf) {
            let w = at(a, root);
            if let Some(r) = a.get_mut(root) {
                *r = u64::try_from(next).unwrap_or(u64::MAX);
            }
            root = root.wrapping_add(1);
            w
        } else {
            let w = at(a, leaf);
            leaf = leaf.wrapping_add(1);
            w
        };
        let second = if leaf >= n || (root < next && at(a, root) < at(a, leaf)) {
            let w = at(a, root);
            if let Some(r) = a.get_mut(root) {
                *r = u64::try_from(next).unwrap_or(u64::MAX);
            }
            root = root.wrapping_add(1);
            w
        } else {
            let w = at(a, leaf);
            leaf = leaf.wrapping_add(1);
            w
        };
        if let Some(slot) = a.get_mut(next) {
            *slot = first.saturating_add(second);
        }
    }
    // Phase 2: each internal node's depth, from its parent's.
    if let Some(r) = a.get_mut(n.wrapping_sub(2)) {
        *r = 0;
    }
    for next in (0..n.saturating_sub(2)).rev() {
        let parent = usize::try_from(at(a, next)).unwrap_or(usize::MAX);
        let depth = at(a, parent).saturating_add(1);
        if let Some(slot) = a.get_mut(next) {
            *slot = depth;
        }
    }
    // Phase 3: the leaves' depths, deepest for the smallest counts.
    let (mut avail, mut used, mut depth) = (1usize, 0usize, 0u64);
    let mut root = n.checked_sub(2);
    let mut next = Some(last);
    while avail > 0 {
        while let Some(r) = root
            && at(a, r) == depth
        {
            used = used.wrapping_add(1);
            root = r.checked_sub(1);
        }
        while avail > used {
            let Some(i) = next else {
                return false;
            };
            if let Some(slot) = a.get_mut(i) {
                *slot = depth;
            }
            next = i.checked_sub(1);
            avail = avail.wrapping_sub(1);
        }
        avail = used.wrapping_mul(2);
        depth = depth.wrapping_add(1);
        used = 0;
    }
    if a.iter().any(|&d| d > u64::from(limit) || d == 0) {
        return false;
    }
    for (&d, &(_, symbol)) in a.iter().zip(leaves) {
        if let Some(l) = lengths.get_mut(usize::from(symbol)) {
            *l = u8::try_from(d).unwrap_or(0);
        }
    }
    true
}

/// An encoding table: each literal's code and its length.
#[derive(Clone, Debug)]
pub(super) struct HuffmanEncoder {
    codes: [(u32, u8); LITERALS],
    /// The weights written in the tree description, the last present literal's left out: the
    /// first `weights_len`.
    weights: [u8; LITERALS],
    weights_len: usize,
}

impl HuffmanEncoder {
    /// The table of `lengths` (each at most [`MAX_BITS`], at least two present): weights from the
    /// lengths (§4.2.1), codes handed out as the decoder's table lays them (§4.2.1.3).
    pub(super) fn new(lengths: &[u8; LITERALS]) -> Result<Self, Corrupt> {
        let max_bits = u32::from(lengths.iter().copied().max().unwrap_or(0));
        if max_bits == 0 || max_bits > MAX_BITS {
            return Err(Corrupt::Huffman);
        }
        let weight = |len: u8| -> u8 {
            if len == 0 {
                0
            } else {
                u8::try_from(max_bits.saturating_add(1).saturating_sub(u32::from(len))).unwrap_or(0)
            }
        };
        let last = lengths
            .iter()
            .rposition(|&l| l > 0)
            .ok_or(Corrupt::Huffman)?;
        let mut weights = [0u8; LITERALS];
        let mut per_weight = [0u32; MAX_BITS as usize + 2];
        for (i, &l) in lengths.iter().enumerate() {
            let w = weight(l);
            if i < last
                && let Some(slot) = weights.get_mut(i)
            {
                *slot = w;
            }
            if l > 0
                && let Some(c) = per_weight.get_mut(usize::from(w))
            {
                *c = c.saturating_add(1);
            }
        }
        // The decoder's table: weight w's literals in order, each 2^(w−1) entries, the lowest
        // weight's first; a code is its first entry's index shifted down by the bits it skips.
        let mut start = [0u32; MAX_BITS as usize + 2];
        let mut next = 0u32;
        for w in 1..=max_bits {
            let wi = usize::try_from(w).unwrap_or(0);
            let count = per_weight.get(wi).copied().unwrap_or(0);
            if let Some(s) = start.get_mut(wi) {
                *s = next;
            }
            next = next.saturating_add(count << w.saturating_sub(1));
        }
        if next != 1 << max_bits {
            return Err(Corrupt::Huffman);
        }
        let mut codes = [(0u32, 0u8); LITERALS];
        for (&len, code) in lengths.iter().zip(codes.iter_mut()) {
            if len == 0 {
                continue;
            }
            let w = usize::from(weight(len));
            let at = *start.get(w).ok_or(Corrupt::Huffman)?;
            *code = (at >> (max_bits.saturating_sub(u32::from(len))), len);
            if let Some(s) = start.get_mut(w) {
                *s = at.saturating_add(1 << w.saturating_sub(1));
            }
        }
        Ok(Self {
            codes,
            weights,
            weights_len: last,
        })
    }

    /// The tree description (§4.2.1.1) onto `out`: the weights FSE-compressed when that is
    /// shorter, written directly otherwise (possible for at most 128 weights).
    pub(super) fn description(&self, out: &mut Vec<u8>) -> Result<(), Corrupt> {
        let weights = self
            .weights
            .get(..self.weights_len)
            .ok_or(Corrupt::Huffman)?;
        let start = out.len();
        let direct_len = if weights.len() <= 128 {
            Some(weights.len().div_ceil(2).saturating_add(1))
        } else {
            None
        };
        if compress_weights(weights, out).is_ok()
            && direct_len.is_none_or(|d| out.len().saturating_sub(start) < d)
        {
            return Ok(());
        }
        out.truncate(start);
        if direct_len.is_none() {
            return Err(Corrupt::Huffman);
        }
        out.push(u8::try_from(weights.len().saturating_add(127)).map_err(|_| Corrupt::Huffman)?);
        for pair in weights.chunks(2) {
            let hi = pair.first().copied().unwrap_or(0);
            let lo = pair.get(1).copied().unwrap_or(0);
            out.push((hi << 4) | lo);
        }
        Ok(())
    }

    /// One stream of `literals` onto `out`, written last literal first so the decoder, reading
    /// backward, meets them in order (§4.2.2).
    pub(super) fn stream(&self, literals: &[u8], out: &mut Vec<u8>) -> Result<(), Corrupt> {
        let mut w = Writer::new(out, literals.len().saturating_mul(MAX_BITS as usize));
        // Four codes of at most 11 bits between flushes: with the 7 a flush leaves, within the
        // writer's 64 (`HUF_compress1X_usingCTable`'s unrolled flushes), so each is pushed
        // with no test of the word's room. A literal with no code would push nothing: the
        // shortest length seen is kept, branch-free, and the stream refused once at the end if
        // it is 0, where a test a literal would sit in the loop.
        let (head, fours) = literals.as_rchunks::<4>();
        let mut shortest = u8::MAX;
        let mut code_of = |b: u8, w: &mut Writer<'_>| {
            let (code, len) = self.codes.get(usize::from(b)).copied().unwrap_or((0, 0));
            shortest = shortest.min(len);
            w.push(u64::from(code), u32::from(len));
        };
        for &[a, b, c, d] in fours.iter().rev() {
            code_of(d, &mut w);
            code_of(c, &mut w);
            code_of(b, &mut w);
            code_of(a, &mut w);
            w.flush();
        }
        for &b in head.iter().rev() {
            code_of(b, &mut w);
            w.flush();
        }
        if shortest == 0 {
            return Err(Corrupt::Huffman);
        }
        w.close();
        Ok(())
    }

    /// The streams of `literals` onto `out`: one, or four behind their jump table
    /// (§3.1.1.3.1.6).
    pub(super) fn streams(
        &self,
        literals: &[u8],
        four: bool,
        out: &mut Vec<u8>,
    ) -> Result<(), Corrupt> {
        if !four {
            return self.stream(literals, out);
        }
        let segment = literals.len().div_ceil(4);
        let jump = out.len();
        out.extend_from_slice(&[0; JUMP_TABLE]);
        for i in 0..4usize {
            let from = segment.saturating_mul(i).min(literals.len());
            let to = if i == 3 {
                literals.len()
            } else {
                segment
                    .saturating_mul(i.saturating_add(1))
                    .min(literals.len())
            };
            let begin = out.len();
            self.stream(literals.get(from..to).ok_or(Corrupt::Huffman)?, out)?;
            if i < 3 {
                let size =
                    u16::try_from(out.len().saturating_sub(begin)).map_err(|_| Corrupt::Huffman)?;
                let at = jump.saturating_add(i.saturating_mul(2));
                out.get_mut(at..at.saturating_add(2))
                    .ok_or(Corrupt::Huffman)?
                    .copy_from_slice(&size.to_le_bytes());
            }
        }
        Ok(())
    }
}

/// The weights compressed by FSE with two interleaved states (§4.2.1.2) onto `out`, the header
/// byte its length: the inverse of [`fse_weights`] (the reference's
/// `FSE_compress_usingCTable`). On an error `out` may hold a partial description.
fn compress_weights(weights: &[u8], out: &mut Vec<u8>) -> Result<(), Corrupt> {
    if weights.len() < 2 {
        return Err(Corrupt::Huffman);
    }
    let mut counts = [0u32; WEIGHT_SYMBOLS];
    for &w in weights {
        let c = counts.get_mut(usize::from(w)).ok_or(Corrupt::Huffman)?;
        *c = c.saturating_add(1);
    }
    let max_symbol = counts
        .iter()
        .rposition(|&c| c > 0)
        .ok_or(Corrupt::Huffman)?;
    let counts = counts.get(..=max_symbol).ok_or(Corrupt::Huffman)?;
    if counts.iter().filter(|&&c| c > 0).count() < 2 {
        return Err(Corrupt::Huffman);
    }
    let total = u32::try_from(weights.len()).map_err(|_| Corrupt::Huffman)?;
    let log = table_log(weights.len(), max_symbol, WEIGHTS_MAX_LOG);
    let mut norm = [0i16; SYMBOLS_MAX];
    let norm = normalize(counts, total, log, &mut norm)?;
    let table = EncodeTable::new(norm, log)?;
    let header = out.len();
    out.push(0);
    write_distribution(norm, log, out)?;
    let mut w = Writer::new(out, weights.len().saturating_mul(WEIGHTS_MAX_LOG as usize));
    // From the end: the symbols at even indices are the first state's, odd the second's; the
    // first state is flushed last, so the decoder reads it first.
    let n = weights.len();
    let at = |i: usize| -> Result<u8, Corrupt> { weights.get(i).copied().ok_or(Corrupt::Huffman) };
    let (mut s1, mut s2);
    let mut i;
    // `n` is at least 2 (checked above); the indices below stay within it.
    let back = |k: usize| -> Result<u8, Corrupt> { at(n.checked_sub(k).ok_or(Corrupt::Huffman)?) };
    if n % 2 == 1 {
        s1 = EncodeState::init(&table, back(1)?)?;
        s2 = EncodeState::init(&table, back(2)?)?;
        s1.encode(&table, back(3)?, &mut w)?;
        i = n.saturating_sub(3);
    } else {
        s2 = EncodeState::init(&table, back(1)?)?;
        s1 = EncodeState::init(&table, back(2)?)?;
        i = n.saturating_sub(2);
    }
    w.flush();
    while i > 0 {
        s2.encode(&table, at(i.saturating_sub(1))?, &mut w)?;
        s1.encode(&table, at(i.saturating_sub(2))?, &mut w)?;
        w.flush();
        i = i.saturating_sub(2);
    }
    s2.flush(&table, &mut w);
    s1.flush(&table, &mut w);
    w.close();
    let len = out.len().saturating_sub(header).saturating_sub(1);
    *out.get_mut(header).ok_or(Corrupt::Huffman)? = u8::try_from(len)
        .ok()
        .filter(|&l| l < DIRECT)
        .ok_or(Corrupt::Huffman)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Package-merge kept as the algorithm states it, each item the list of the leaves it
    /// packages: the oracle the array form must equal.
    fn by_lists(counts: &[u32], limit: u32) -> Vec<u8> {
        let mut lengths = vec![0u8; counts.len()];
        let mut present: Vec<(u64, usize)> = counts
            .iter()
            .enumerate()
            .filter(|&(_, &c)| c > 0)
            .map(|(s, &c)| (u64::from(c), s))
            .collect();
        if present.len() == 1 {
            lengths[present[0].1] = 1;
            return lengths;
        }
        present.sort_unstable();
        let leaves: Vec<(u64, Vec<usize>)> = present.iter().map(|&(w, s)| (w, vec![s])).collect();
        let mut row = leaves.clone();
        for _ in 1..limit {
            let packages: Vec<(u64, Vec<usize>)> = row
                .as_chunks::<2>()
                .0
                .iter()
                .map(|[a, b]| (a.0 + b.0, [a.1.clone(), b.1.clone()].concat()))
                .collect();
            let mut merged = Vec::new();
            let (mut i, mut j) = (0, 0);
            while i < leaves.len() || j < packages.len() {
                if j == packages.len() || (i < leaves.len() && leaves[i].0 <= packages[j].0) {
                    merged.push(leaves[i].clone());
                    i += 1;
                } else {
                    merged.push(packages[j].clone());
                    j += 1;
                }
            }
            row = merged;
        }
        for (_, symbols) in row.iter().take(2 * present.len() - 2) {
            for &s in symbols {
                lengths[s] += 1;
            }
        }
        lengths
    }

    proptest! {
        /// For any counts and any limit with room for the literals present, the lengths form a
        /// complete prefix code (Kraft's sum exactly one), keep to the limit, and name exactly
        /// the literals present; and where the minimum-redundancy code keeps to the limit, they
        /// cost what the optimal length-limited code (package-merge as the algorithm states it)
        /// costs, since then no repair runs.
        #[test]
        fn the_lengths_are_complete_within_the_limit_and_optimal_when_they_fit(
            counts in proptest::collection::vec(prop_oneof![Just(0u32), 1u32..4, 1u32..100_000], 2..=256),
            limit in 1u32..=MAX_BITS,
        ) {
            let present = counts.iter().filter(|&&c| c > 0).count();
            prop_assume!(present >= 2 && present <= 1 << limit);
            let mut lengths = [0u8; LITERALS];
            code_lengths(&counts, limit, &mut lengths);
            prop_assert!(lengths.iter().all(|&l| u32::from(l) <= limit));
            for (s, &c) in counts.iter().enumerate() {
                prop_assert_eq!(c > 0, lengths[s] > 0);
            }
            let kraft: u64 = lengths
                .iter()
                .filter(|&&l| l > 0)
                .map(|&l| 1u64 << (MAX_BITS - u32::from(l)))
                .sum();
            prop_assert_eq!(kraft, 1u64 << MAX_BITS);
            let mut unlimited = [0u8; LITERALS];
            code_lengths(&counts, u32::from(u8::MAX), &mut unlimited);
            let deepest = unlimited.iter().copied().max().unwrap_or(0);
            if u32::from(deepest) <= limit {
                let cost = |ls: &[u8]| -> u64 {
                    counts.iter().zip(ls).map(|(&c, &l)| u64::from(c) * u64::from(l)).sum()
                };
                prop_assert_eq!(cost(&lengths[..counts.len()]), cost(&by_lists(&counts, limit)));
            }
        }
    }
}
