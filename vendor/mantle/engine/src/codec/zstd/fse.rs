//! FSE (RFC 8878 §4.1): reading a table description (§4.1.1), building the decoding table from
//! the normalized distribution, and the predefined distributions of the sequence codes
//! (§3.1.1.3.2.2).

use super::Corrupt;
use super::bits::{Backward, Forward, Writer};

/// The smallest accuracy log a description states: its low 4 bits plus 5 (§4.1.1).
const MIN_ACCURACY_LOG: u32 = 5;

/// One cell of a decoding table: the symbol it decodes and how to reach the next state, the
/// next state being `baseline + read(bits)` (§4.1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Cell {
    pub(super) symbol: u8,
    pub(super) bits: u8,
    pub(super) baseline: u16,
}

/// The most symbols a distribution describes: the match length codes' 53 (§3.1.1.3.2.1.1).
pub(super) const SYMBOLS_MAX: usize = 64;

/// A decoding table of `1 << accuracy_log` cells. Rebuilding one reuses its cells' allocation,
/// as cloning into one does.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Table {
    pub(super) accuracy_log: u32,
    pub(super) cells: Vec<Cell>,
}

impl Clone for Table {
    fn clone(&self) -> Self {
        Self {
            accuracy_log: self.accuracy_log,
            cells: self.cells.clone(),
        }
    }

    fn clone_from(&mut self, source: &Self) {
        self.accuracy_log = source.accuracy_log;
        self.cells.clone_from(&source.cells);
    }
}

impl Table {
    /// Makes this the decoding table of a normalized distribution (§4.1.1); every cell is
    /// written, so a table of the same size is not cleared first.
    pub(super) fn set_distribution(
        &mut self,
        norm: &[i16],
        accuracy_log: u32,
    ) -> Result<(), Corrupt> {
        let mut spread = Spread::new();
        let size = spread.spread(norm, accuracy_log)?;
        self.accuracy_log = accuracy_log;
        if self.cells.len() != size {
            self.cells.resize(size, Cell::default());
        }
        for (cell, made) in self.cells.iter_mut().zip(spread.cells()) {
            let (symbol, bits, baseline) = made?;
            *cell = Cell {
                symbol,
                bits,
                baseline,
            };
        }
        Ok(())
    }

    /// The cell of `state`. Every state a [`State`] holds is below the table's size: the first
    /// is `accuracy_log` bits, and each next one, `baseline + read(bits)`, is below
    /// `((state + 1) << bits) - size`, at most `size - 1` as `state << bits < 2 * size` (§4.1).
    /// The mask keeps the lookup in the table for a table of any size all the same.
    #[inline(always)]
    fn at(&self, state: u32) -> Cell {
        let mask = self.cells.len().wrapping_sub(1);
        let index = usize::try_from(state).unwrap_or(usize::MAX) & mask;
        self.cells.get(index).copied().unwrap_or_default()
    }
}

/// The most cells a decoding table has: 2^9, the sequence codes' largest accuracy log
/// (§3.1.1.3.2.1); Huffman weights' tables are 2^6 at most (§4.2.1.2).
pub(super) const CELLS_MAX: usize = 1 << 9;

/// A distribution spread over its table's cells (§4.1.1), as `ZSTD_buildFSETable` spreads it
/// (zstd 1.5.7 `lib/decompress/zstd_decompress_block.c`): each cell's symbol, and each symbol's
/// next state, which [`Spread::cells`] hands out in cell order.
pub(super) struct Spread {
    pub(super) symbols: [u8; CELLS_MAX],
    next: [u32; SYMBOLS_MAX],
    log: u32,
    size: u32,
}

impl Spread {
    pub(super) fn new() -> Self {
        Self {
            symbols: [0; CELLS_MAX],
            next: [0; SYMBOLS_MAX],
            log: 0,
            size: 0,
        }
    }

    /// Spreads `norm` over `1 << log` cells and returns their count: "less than 1" symbols take
    /// one cell each from the end, the rest are spread by the fixed step. Without "less than 1"
    /// symbols the step never lands on a taken cell, so the symbols are laid out in order and
    /// placed by the step with no skipping, as the reference's fast path does.
    pub(super) fn spread(&mut self, norm: &[i16], log: u32) -> Result<usize, Corrupt> {
        let size = 1usize.checked_shl(log).ok_or(Corrupt::Distribution)?;
        if size > CELLS_MAX || norm.len() > SYMBOLS_MAX {
            return Err(Corrupt::Distribution);
        }
        self.log = log;
        self.size = u32::try_from(size).map_err(|_| Corrupt::Distribution)?;
        let mut high = size.checked_sub(1).ok_or(Corrupt::Distribution)?;
        for (s, (&count, n)) in norm.iter().zip(self.next.iter_mut()).enumerate() {
            if count == -1 {
                *self.symbols.get_mut(high).ok_or(Corrupt::Distribution)? =
                    u8::try_from(s).map_err(|_| Corrupt::Distribution)?;
                high = high.checked_sub(1).ok_or(Corrupt::Distribution)?;
                *n = 1;
            } else {
                *n = u32::try_from(count).map_err(|_| Corrupt::Distribution)?;
            }
        }
        // §4.1.1: the spread's step, `(tableSize >> 1) + (tableSize >> 3) + 3`; `size` is at most
        // 2^9, so the sum is far from overflowing.
        let step = (size >> 1).wrapping_add(size >> 3).wrapping_add(3);
        let mask = size.wrapping_sub(1);
        let mut position = 0usize;
        if high == mask {
            let mut laid = [0u8; CELLS_MAX];
            let mut at = 0usize;
            for (s, &count) in norm.iter().enumerate() {
                let n = usize::try_from(count.max(0)).map_err(|_| Corrupt::Distribution)?;
                let end = at.checked_add(n).ok_or(Corrupt::Distribution)?;
                laid.get_mut(at..end)
                    .ok_or(Corrupt::Distribution)?
                    .fill(u8::try_from(s).map_err(|_| Corrupt::Distribution)?);
                at = end;
            }
            // The counts must fill the table exactly (§4.1.1's reader checks that).
            if at != size {
                return Err(Corrupt::Distribution);
            }
            for &symbol in laid.get(..size).ok_or(Corrupt::Distribution)? {
                *self
                    .symbols
                    .get_mut(position)
                    .ok_or(Corrupt::Distribution)? = symbol;
                position = position.wrapping_add(step) & mask;
            }
        } else {
            for (s, &count) in norm.iter().enumerate() {
                let symbol = u8::try_from(s).map_err(|_| Corrupt::Distribution)?;
                for _ in 0..count.max(0) {
                    *self
                        .symbols
                        .get_mut(position)
                        .ok_or(Corrupt::Distribution)? = symbol;
                    position = position.wrapping_add(step) & mask;
                    // Skip the cells the "less than 1" symbols hold.
                    while position > high {
                        position = position.wrapping_add(step) & mask;
                    }
                }
            }
        }
        // Every cell is reached once by the spread only if the counts sum to the table: §4.1.1's
        // reader checks that, so a position not back at 0 means a description it did not check.
        if position != 0 {
            return Err(Corrupt::Distribution);
        }
        Ok(size)
    }

    /// The cells in order, each with its symbol and its state's bits and baseline (the next
    /// state being `baseline + read(bits)`), borrowed from the spread rather than copied out.
    pub(super) fn cells(&mut self) -> impl Iterator<Item = Result<(u8, u8, u16), Corrupt>> + '_ {
        let Self {
            symbols,
            next,
            log,
            size,
        } = self;
        let (log, size) = (*log, *size);
        symbols
            .iter()
            .take(usize::try_from(size).unwrap_or(0))
            .map(move |&symbol| {
                let (bits, baseline) = transition(next, log, size, symbol)?;
                Ok((symbol, bits, baseline))
            })
    }
}

/// The next cell of `symbol`'s, from its next state: the state's bits and baseline. Lower states
/// read one more bit, to reach the next power of two.
#[inline(always)]
fn transition(
    next: &mut [u32; SYMBOLS_MAX],
    log: u32,
    size: u32,
    symbol: u8,
) -> Result<(u8, u16), Corrupt> {
    let n = next
        .get_mut(usize::from(symbol))
        .ok_or(Corrupt::Distribution)?;
    let state = *n;
    *n = state.checked_add(1).ok_or(Corrupt::Distribution)?;
    let bits = log
        .checked_sub(
            31u32
                .checked_sub(state.leading_zeros())
                .ok_or(Corrupt::Distribution)?,
        )
        .ok_or(Corrupt::Distribution)?;
    let baseline = state
        .checked_shl(bits)
        .and_then(|v| v.checked_sub(size))
        .ok_or(Corrupt::Distribution)?;
    Ok((
        u8::try_from(bits).map_err(|_| Corrupt::Distribution)?,
        u16::try_from(baseline).map_err(|_| Corrupt::Distribution)?,
    ))
}

/// A decoder's state over one table: its first value read, and each next one from the cell's
/// baseline and bits (§4.1).
#[derive(Clone, Copy, Debug)]
pub(super) struct State {
    pub(super) value: u32,
}

impl State {
    #[inline(always)]
    pub(super) fn init(table: &Table, bits: &mut Backward<'_>) -> Self {
        Self {
            value: bits.read(table.accuracy_log),
        }
    }

    #[inline(always)]
    pub(super) fn symbol(self, table: &Table) -> u8 {
        table.at(self.value).symbol
    }

    /// Steps to the next state, its bits read without a refill: a [`Backward::reload`] or
    /// [`Backward::ensure`] since must cover them.
    #[inline(always)]
    pub(super) fn update_ensured(&mut self, table: &Table, bits: &mut Backward<'_>) {
        let cell = table.at(self.value);
        // A baseline is below 2^9 and the bits read below 2^9 (§4.1).
        self.value = u32::from(cell.baseline).wrapping_add(bits.read_ensured(u32::from(cell.bits)));
    }
}

/// Reads a table description (§4.1.1) of at most `max_symbols` symbols and accuracy log
/// `max_log` into `norm`, returning the symbols it described, its accuracy log and the bytes it
/// took.
pub(super) fn read_distribution(
    bytes: &[u8],
    max_symbols: usize,
    max_log: u32,
    out: &mut [i16; SYMBOLS_MAX],
) -> Result<(usize, u32, usize), Corrupt> {
    let max_symbols = max_symbols.min(SYMBOLS_MAX);
    let mut bits = Forward::new(bytes);
    let accuracy_log = bits
        .peek(4)
        .checked_add(MIN_ACCURACY_LOG)
        .ok_or(Corrupt::Distribution)?;
    bits.skip(4)?;
    if accuracy_log > max_log {
        return Err(Corrupt::Distribution);
    }
    let size = 1i32 << accuracy_log;
    // §4.1.1: values range to "remaining probabilities + 1".
    let mut remaining = size.checked_add(1).ok_or(Corrupt::Distribution)?;
    let mut threshold = size;
    let mut width = accuracy_log.checked_add(1).ok_or(Corrupt::Distribution)?;
    let mut len = 0usize;
    // Whether the last probability read was 0, which a repeat flag follows.
    let mut last_zero = false;
    // Appends one probability; `len` stays below `max_symbols`, checked before each.
    let mut push = |len: &mut usize, p: i16| -> Result<(), Corrupt> {
        *out.get_mut(*len).ok_or(Corrupt::Distribution)? = p;
        *len = len.checked_add(1).ok_or(Corrupt::Distribution)?;
        Ok(())
    };
    while remaining > 1 {
        if len >= max_symbols {
            return Err(Corrupt::Distribution);
        }
        // Each 2-bit flag after a zero repeats that many more zeros; 3 continues.
        if last_zero {
            loop {
                let repeat = bits.peek(2);
                bits.skip(2)?;
                for _ in 0..repeat {
                    if len >= max_symbols {
                        return Err(Corrupt::Distribution);
                    }
                    push(&mut len, 0)?;
                }
                if repeat != 3 {
                    break;
                }
            }
            if len >= max_symbols {
                return Err(Corrupt::Distribution);
            }
        }
        // Table 20: the low values take one bit less.
        let max = threshold
            .checked_mul(2)
            .and_then(|t| t.checked_sub(1))
            .and_then(|t| t.checked_sub(remaining))
            .ok_or(Corrupt::Distribution)?;
        let narrow = width.checked_sub(1).ok_or(Corrupt::Distribution)?;
        let low = i32::try_from(bits.peek(narrow)).map_err(|_| Corrupt::Distribution)?;
        let value = if low < max {
            bits.skip(narrow)?;
            low
        } else {
            let full = i32::try_from(bits.peek(width)).map_err(|_| Corrupt::Distribution)?;
            bits.skip(width)?;
            if full >= threshold {
                full.checked_sub(max).ok_or(Corrupt::Distribution)?
            } else {
                full
            }
        };
        let probability = value.checked_sub(1).ok_or(Corrupt::Distribution)?;
        // "Less than 1" counts as one point.
        remaining = remaining
            .checked_sub(probability.abs())
            .ok_or(Corrupt::Distribution)?;
        push(
            &mut len,
            i16::try_from(probability).map_err(|_| Corrupt::Distribution)?,
        )?;
        last_zero = probability == 0;
        while remaining < threshold {
            width = width.checked_sub(1).ok_or(Corrupt::Distribution)?;
            threshold >>= 1;
        }
    }
    if remaining != 1 {
        return Err(Corrupt::Distribution);
    }
    // At least two symbols carry probability (§4.1.1).
    let described = out.get(..len).ok_or(Corrupt::Distribution)?;
    if described.iter().filter(|&&p| p != 0).count() < 2 {
        return Err(Corrupt::Distribution);
    }
    Ok((len, accuracy_log, bits.bytes_used()))
}

/// The predefined distribution of literals length codes, accuracy log 6 (§3.1.1.3.2.2.1).
pub(super) const LITERALS_LENGTH_DEFAULT: [i16; 36] = [
    4, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 2, 1, 1, 1, 1, 1,
    -1, -1, -1, -1,
];
pub(super) const LITERALS_LENGTH_DEFAULT_LOG: u32 = 6;
/// The predefined distribution of match length codes, accuracy log 6 (§3.1.1.3.2.2.2).
pub(super) const MATCH_LENGTH_DEFAULT: [i16; 53] = [
    1, 4, 3, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1, -1, -1,
];
pub(super) const MATCH_LENGTH_DEFAULT_LOG: u32 = 6;
/// The predefined distribution of offset codes, accuracy log 5 (§3.1.1.3.2.2.3).
pub(super) const OFFSET_DEFAULT: [i16; 29] = [
    1, 1, 1, 1, 1, 1, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1,
];
pub(super) const OFFSET_DEFAULT_LOG: u32 = 5;

/// The accuracy log of an FSE table for `total` symbols whose largest is `max_symbol`, at most
/// `max_log`: the reference's `FSE_optimalTableLog` (zstd 1.5.7 lib/compress/fse_compress.c),
/// which [`optimal_log`] computes with its `minus` of 2.
pub(super) fn table_log(total: usize, max_symbol: usize, max_log: u32) -> u32 {
    optimal_log(total, max_symbol, max_log, 2)
}

/// `FSE_optimalTableLog_internal` (zstd 1.5.7 lib/compress/fse_compress.c): `max_log`, lowered
/// to `highbit(total − 1) − minus` (a difference below zero, unsigned there, lowers nothing),
/// raised to the fewest bits the symbols need (`FSE_minTableLog`: the smaller of
/// `highbit(total) + 1` and `highbit(max_symbol) + 2`), and kept from 5 (`FSE_MIN_TABLELOG`) to
/// `max_log`. The Huffman code's depth is this with a `minus` of 1 (`HUF_optimalTableLog`'s
/// evaluation without the depth search, which the reference uses below `btultra`).
pub(super) fn optimal_log(total: usize, max_symbol: usize, max_log: u32, minus: u32) -> u32 {
    let high = |x: usize| (usize::BITS - 1).saturating_sub(x.leading_zeros());
    let mut log = max_log;
    if let Some(from_source) = high(total.saturating_sub(1)).checked_sub(minus) {
        log = log.min(from_source);
    }
    let fewest = high(total)
        .saturating_add(1)
        .min(high(max_symbol).saturating_add(2));
    log.max(fewest).clamp(MIN_ACCURACY_LOG, max_log)
}

/// Scales `counts` (each symbol's occurrences, `total` in all) to a distribution summing to
/// `1 << log` in which every present symbol keeps at least one cell: each its rounded share,
/// the rounding's error then taken from or given to the largest, or past half of the largest's
/// share spread by [`by_remainders`]. Symbols too rare for a full
/// cell keep one (the format's "less than 1", -1, is an encoding choice this does not make).
pub(super) fn normalize<'a>(
    counts: &[u32],
    total: u32,
    log: u32,
    out: &'a mut [i16; SYMBOLS_MAX],
) -> Result<&'a [i16], Corrupt> {
    let size = 1i64.checked_shl(log).ok_or(Corrupt::Distribution)?;
    let total = i64::from(total);
    if total == 0 || counts.len() > SYMBOLS_MAX {
        return Err(Corrupt::Distribution);
    }
    let mut sum = 0i64;
    let mut largest = 0usize;
    for (s, (&c, slot)) in counts.iter().zip(out.iter_mut()).enumerate() {
        let c = i64::from(c);
        let share = if c == 0 {
            0
        } else {
            // Rounded, never below one cell.
            (c.saturating_mul(size).saturating_add(total / 2))
                .checked_div(total)
                .unwrap_or(0)
                .max(1)
        };
        if c > i64::from(*counts.get(largest).unwrap_or(&0)) {
            largest = s;
        }
        sum = sum.saturating_add(share);
        *slot = i16::try_from(share).map_err(|_| Corrupt::Distribution)?;
    }
    let fix = size.saturating_sub(sum);
    let slot = out.get_mut(largest).ok_or(Corrupt::Distribution)?;
    let share = i64::from(*slot);
    // The largest absorbs the rounding unless the cells to give back reach half its share: many
    // rare symbols each kept at one cell can ask more than it holds. The reference then turns to
    // a second method (`FSE_normalizeCount`'s test, `-correction >= normalizedCounter[largest] >> 1`,
    // falling to `FSE_normalizeM2`); here that is the largest remainders' distribution.
    if fix >= 0 || fix.saturating_neg() < share >> 1 {
        *slot = i16::try_from(share.saturating_add(fix)).map_err(|_| Corrupt::Distribution)?;
        return out.get(..counts.len()).ok_or(Corrupt::Distribution);
    }
    by_remainders(counts, total, size, out)
}

/// [`normalize`]'s second method: each present symbol its share rounded down, at least one cell;
/// the cells left over go one each to the symbols whose shares lost the most to the rounding, and
/// cells owed are taken one each from those that lost the least and hold more than one, until the
/// distribution sums to `size`. Exact in integers, so every host normalizes alike; refused only
/// when the present symbols outnumber the cells.
fn by_remainders<'a>(
    counts: &[u32],
    total: i64,
    size: i64,
    out: &'a mut [i16; SYMBOLS_MAX],
) -> Result<&'a [i16], Corrupt> {
    let mut remainder = [0i64; SYMBOLS_MAX];
    let mut order = [0u8; SYMBOLS_MAX];
    let mut present = 0usize;
    let mut sum = 0i64;
    for (s, ((&c, slot), r)) in counts
        .iter()
        .zip(out.iter_mut())
        .zip(remainder.iter_mut())
        .enumerate()
    {
        *slot = 0;
        if c == 0 {
            continue;
        }
        let scaled = i64::from(c).saturating_mul(size);
        let share = scaled
            .checked_div(total)
            .ok_or(Corrupt::Distribution)?
            .max(1);
        *r = scaled.checked_rem(total).ok_or(Corrupt::Distribution)?;
        *slot = i16::try_from(share).map_err(|_| Corrupt::Distribution)?;
        sum = sum.saturating_add(share);
        *order.get_mut(present).ok_or(Corrupt::Distribution)? =
            u8::try_from(s).map_err(|_| Corrupt::Distribution)?;
        present = present.saturating_add(1);
    }
    if i64::try_from(present).map_err(|_| Corrupt::Distribution)? > size {
        return Err(Corrupt::Distribution);
    }
    let order = order.get_mut(..present).ok_or(Corrupt::Distribution)?;
    // Largest remainder first, the larger count on a tie, then the lower symbol.
    let key = |s: u8| {
        let i = usize::from(s);
        (
            std::cmp::Reverse(remainder.get(i).copied().unwrap_or(0)),
            std::cmp::Reverse(counts.get(i).copied().unwrap_or(0)),
            s,
        )
    };
    order.sort_unstable_by_key(|&s| key(s));
    // Each floor loses less than a cell, so one pass gives what is left over.
    for &s in order.iter() {
        if sum >= size {
            break;
        }
        let slot = out.get_mut(usize::from(s)).ok_or(Corrupt::Distribution)?;
        *slot = slot.checked_add(1).ok_or(Corrupt::Distribution)?;
        sum = sum.saturating_add(1);
    }
    // Cells owed (the one-cell minimums) come from the symbols that lost the least, a cell a
    // pass while any holds more than one; the present symbols fit the table, so each pass takes
    // at least one and at most `size` passes run.
    let mut passes = size;
    while sum > size && passes > 0 {
        passes = passes.saturating_sub(1);
        for &s in order.iter().rev() {
            if sum <= size {
                break;
            }
            let slot = out.get_mut(usize::from(s)).ok_or(Corrupt::Distribution)?;
            if *slot > 1 {
                *slot = slot.saturating_sub(1);
                sum = sum.saturating_sub(1);
            }
        }
    }
    if sum != size {
        return Err(Corrupt::Distribution);
    }
    out.get(..counts.len()).ok_or(Corrupt::Distribution)
}

/// Writes a distribution's table description (§4.1.1), the inverse of [`read_distribution`]:
/// the accuracy log less 5 in 4 bits, then each symbol's probability plus one in the reader's
/// variable width, zero runs after a zero as 2-bit repeat flags; appended to `out`.
pub(super) fn write_distribution(norm: &[i16], log: u32, out: &mut Vec<u8>) -> Result<(), Corrupt> {
    let mut w = Writer::new(out, norm.len().saturating_mul(16).saturating_add(4));
    w.add(
        u64::from(
            log.checked_sub(MIN_ACCURACY_LOG)
                .ok_or(Corrupt::Distribution)?,
        ),
        4,
    );
    let size = 1i32.checked_shl(log).ok_or(Corrupt::Distribution)?;
    let mut remaining = size.checked_add(1).ok_or(Corrupt::Distribution)?;
    let mut threshold = size;
    let mut width = log.checked_add(1).ok_or(Corrupt::Distribution)?;
    let mut s = 0usize;
    let mut previous_zero = false;
    while remaining > 1 && s < norm.len() {
        if previous_zero {
            let start = s;
            while norm.get(s) == Some(&0) {
                s = s.saturating_add(1);
            }
            let mut run = s.saturating_sub(start);
            while run >= 3 {
                w.add(3, 2);
                run = run.saturating_sub(3);
            }
            w.add(u64::try_from(run).unwrap_or(0), 2);
        }
        let count = i32::from(*norm.get(s).ok_or(Corrupt::Distribution)?);
        s = s.saturating_add(1);
        let max = threshold
            .checked_mul(2)
            .and_then(|t| t.checked_sub(1))
            .and_then(|t| t.checked_sub(remaining))
            .ok_or(Corrupt::Distribution)?;
        remaining = remaining
            .checked_sub(count.abs())
            .ok_or(Corrupt::Distribution)?;
        // The value read is the probability plus one; values at or above the threshold are the
        // ones Table 20 moves up by `max`.
        let mut value = count.checked_add(1).ok_or(Corrupt::Distribution)?;
        if value >= threshold {
            value = value.checked_add(max).ok_or(Corrupt::Distribution)?;
        }
        let bits = if value < max {
            width.saturating_sub(1)
        } else {
            width
        };
        w.add(
            u64::try_from(value).map_err(|_| Corrupt::Distribution)?,
            bits,
        );
        previous_zero = count == 0;
        if remaining < 1 {
            return Err(Corrupt::Distribution);
        }
        while remaining < threshold {
            width = width.checked_sub(1).ok_or(Corrupt::Distribution)?;
            threshold >>= 1;
        }
    }
    if remaining != 1 {
        return Err(Corrupt::Distribution);
    }
    w.finish();
    Ok(())
}

/// How one symbol is encoded from any state: the reference's `symbolTT`, the bits a state of it
/// emits found from `delta_bits`, the next state from `delta_state` (`FSE_buildCTable`).
#[derive(Clone, Copy, Debug, Default)]
struct SymbolTransform {
    delta_bits: u32,
    delta_state: i32,
}

/// An encoding table, the decoder's [`Table`] run backwards. Rebuilding one reuses its
/// allocations.
#[derive(Clone, Debug, Default)]
pub(super) struct EncodeTable {
    pub(super) accuracy_log: u32,
    /// The next state, indexed by a symbol's range of states.
    states: Vec<u16>,
    symbols: Vec<SymbolTransform>,
}

impl EncodeTable {
    /// The encoding table of `norm` at `log`.
    pub(super) fn new(norm: &[i16], log: u32) -> Result<Self, Corrupt> {
        let mut table = Self::default();
        table.set(norm, log)?;
        Ok(table)
    }

    /// Makes this the encoding table of `norm` at `log`, its cells spread exactly as the
    /// decoder's (`FSE_buildCTable`).
    pub(super) fn set(&mut self, norm: &[i16], log: u32) -> Result<(), Corrupt> {
        let mut spread = Spread::new();
        let size = spread.spread(norm, log)?;
        // Each symbol's states, in the order the spread placed them, numbered from the table size:
        // a symbol's run starts where the runs of the symbols before it end.
        let mut next = [0usize; SYMBOLS_MAX];
        let mut running = 0usize;
        for (&n, slot) in norm.iter().zip(next.iter_mut()) {
            *slot = running;
            running = running.saturating_add(if n == -1 {
                1
            } else {
                usize::try_from(n.max(0)).unwrap_or(0)
            });
        }
        self.states.clear();
        self.states.resize(size, 0);
        for (u, &symbol) in spread
            .symbols
            .get(..size)
            .ok_or(Corrupt::Distribution)?
            .iter()
            .enumerate()
        {
            let at = next
                .get_mut(usize::from(symbol))
                .ok_or(Corrupt::Distribution)?;
            *self.states.get_mut(*at).ok_or(Corrupt::Distribution)? =
                u16::try_from(size.saturating_add(u)).map_err(|_| Corrupt::Distribution)?;
            *at = at.saturating_add(1);
        }
        self.symbols.clear();
        self.symbols.resize(norm.len(), SymbolTransform::default());
        let mut total = 0i32;
        let size32 = i64::try_from(size).unwrap_or(0);
        for (t, &n) in self.symbols.iter_mut().zip(norm) {
            match n {
                0 => {
                    // Never encoded; the reference's value keeps its maths defined.
                    t.delta_bits = u32::try_from(
                        (i64::from(log).saturating_add(1) << 16).saturating_sub(size32),
                    )
                    .unwrap_or(0);
                }
                -1 | 1 => {
                    t.delta_bits =
                        u32::try_from((i64::from(log) << 16).saturating_sub(size32)).unwrap_or(0);
                    t.delta_state = total.saturating_sub(1);
                    total = total.saturating_add(1);
                }
                _ => {
                    let n32 = u32::try_from(n).map_err(|_| Corrupt::Distribution)?;
                    let high = u32::BITS
                        .saturating_sub(1)
                        .saturating_sub((n32.saturating_sub(1)).leading_zeros());
                    let max_bits_out = log.saturating_sub(high);
                    let min_state_plus = i64::from(n32) << max_bits_out;
                    t.delta_bits = u32::try_from(
                        (i64::from(max_bits_out) << 16).saturating_sub(min_state_plus),
                    )
                    .unwrap_or(0);
                    t.delta_state = total.saturating_sub(i32::from(n));
                    total = total.saturating_add(i32::from(n));
                }
            }
        }
        self.accuracy_log = log;
        Ok(())
    }

    fn transform(&self, symbol: u8) -> Result<SymbolTransform, Corrupt> {
        self.symbols
            .get(usize::from(symbol))
            .copied()
            .ok_or(Corrupt::Distribution)
    }

    fn state_at(&self, index: i64) -> Result<u32, Corrupt> {
        let i = usize::try_from(index).map_err(|_| Corrupt::Distribution)?;
        Ok(u32::from(*self.states.get(i).ok_or(Corrupt::Distribution)?))
    }
}

/// An encoder's state over one table (`FSE_CState_t`): its value in `[size, 2·size)`.
#[derive(Clone, Copy, Debug)]
pub(super) struct EncodeState {
    value: u32,
}

impl EncodeState {
    /// The state after the stream's last symbol, written first (`FSE_initCState2`).
    pub(super) fn init(table: &EncodeTable, symbol: u8) -> Result<Self, Corrupt> {
        let t = table.transform(symbol)?;
        let bits_out = t.delta_bits.saturating_add(1 << 15) >> 16;
        let value = (i64::from(bits_out) << 16).saturating_sub(i64::from(t.delta_bits));
        let index = (value >> bits_out).saturating_add(i64::from(t.delta_state));
        Ok(Self {
            value: table.state_at(index)?,
        })
    }

    /// Encodes `symbol`, writing the bits the decoder reads to return here (`FSE_encodeSymbol`).
    pub(super) fn encode(
        &mut self,
        table: &EncodeTable,
        symbol: u8,
        w: &mut Writer<'_>,
    ) -> Result<(), Corrupt> {
        let t = table.transform(symbol)?;
        let bits_out = self.value.saturating_add(t.delta_bits) >> 16;
        // At most the table's accuracy log, 9: the caller flushes (`FSE_encodeSymbol`).
        w.add_held(u64::from(self.value), bits_out);
        let index = i64::from(self.value >> bits_out).saturating_add(i64::from(t.delta_state));
        self.value = table.state_at(index)?;
        Ok(())
    }

    /// Writes the final state, which the decoder reads first (`FSE_flushCState`).
    pub(super) fn flush(self, table: &EncodeTable, w: &mut Writer<'_>) {
        w.add(u64::from(self.value), table.accuracy_log);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    proptest::proptest! {
        /// Any counts normalize, at the log `table_log` picks, to a distribution summing to the
        /// table's size with a cell or more for every present symbol and none for an absent one:
        /// flat counts over many symbols included, where the largest alone cannot absorb the
        /// rounding.
        #[test]
        fn every_distribution_normalizes_completely(
            counts in proptest::collection::vec(
                proptest::prop_oneof![proptest::strategy::Just(0u32), 1u32..4, 1u32..100_000],
                2..=SYMBOLS_MAX,
            ),
            max_log in MIN_ACCURACY_LOG..=9u32,
        ) {
            let total: u32 = counts.iter().sum();
            let Some(max_symbol) = counts.iter().rposition(|&c| c > 0) else {
                return Ok(());
            };
            let present = counts.iter().filter(|&&c| c > 0).count();
            let log = table_log(total as usize, max_symbol, max_log);
            let counts = &counts[..=max_symbol];
            let mut out = [0i16; SYMBOLS_MAX];
            let normalized = normalize(counts, total, log, &mut out);
            if present > 1 << log {
                proptest::prop_assert!(normalized.is_err());
                return Ok(());
            }
            let norm = normalized.unwrap();
            proptest::prop_assert_eq!(norm.iter().map(|&n| i64::from(n)).sum::<i64>(), 1i64 << log);
            for (&c, &n) in counts.iter().zip(norm) {
                proptest::prop_assert_eq!(c == 0, n == 0);
                proptest::prop_assert!(n >= 0);
            }
        }
    }
}
