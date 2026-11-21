//! The sequences section (RFC 8878 §3.1.1.3.2): its header, each symbol type's table by mode,
//! the codes' baselines and extra bits (§3.1.1.3.2.1.1), and decoding the interleaved bitstream
//! (§3.1.1.3.2.1.2).

use super::Corrupt;
use super::bits::Backward;
use super::fse::{
    LITERALS_LENGTH_DEFAULT, LITERALS_LENGTH_DEFAULT_LOG, MATCH_LENGTH_DEFAULT,
    MATCH_LENGTH_DEFAULT_LOG, OFFSET_DEFAULT, OFFSET_DEFAULT_LOG, SYMBOLS_MAX, Spread,
    read_distribution,
};

/// Literals length codes 0 to 35 (§3.1.1.3.2.1.1, Table 16): baseline and extra bits.
pub(super) const LITERALS_LENGTH_CODES: [(u32, u8); 36] = [
    (0, 0),
    (1, 0),
    (2, 0),
    (3, 0),
    (4, 0),
    (5, 0),
    (6, 0),
    (7, 0),
    (8, 0),
    (9, 0),
    (10, 0),
    (11, 0),
    (12, 0),
    (13, 0),
    (14, 0),
    (15, 0),
    (16, 1),
    (18, 1),
    (20, 1),
    (22, 1),
    (24, 2),
    (28, 2),
    (32, 3),
    (40, 3),
    (48, 4),
    (64, 6),
    (128, 7),
    (256, 8),
    (512, 9),
    (1024, 10),
    (2048, 11),
    (4096, 12),
    (8192, 13),
    (16384, 14),
    (32768, 15),
    (65536, 16),
];
/// Match length codes 0 to 52 (§3.1.1.3.2.1.1, Table 17): baseline and extra bits.
pub(super) const MATCH_LENGTH_CODES: [(u32, u8); 53] = [
    (3, 0),
    (4, 0),
    (5, 0),
    (6, 0),
    (7, 0),
    (8, 0),
    (9, 0),
    (10, 0),
    (11, 0),
    (12, 0),
    (13, 0),
    (14, 0),
    (15, 0),
    (16, 0),
    (17, 0),
    (18, 0),
    (19, 0),
    (20, 0),
    (21, 0),
    (22, 0),
    (23, 0),
    (24, 0),
    (25, 0),
    (26, 0),
    (27, 0),
    (28, 0),
    (29, 0),
    (30, 0),
    (31, 0),
    (32, 0),
    (33, 0),
    (34, 0),
    (35, 1),
    (37, 1),
    (39, 1),
    (41, 1),
    (43, 2),
    (47, 2),
    (51, 3),
    (59, 3),
    (67, 4),
    (83, 4),
    (99, 5),
    (131, 7),
    (259, 8),
    (515, 9),
    (1027, 10),
    (2051, 11),
    (4099, 12),
    (8195, 13),
    (16387, 14),
    (32771, 15),
    (65539, 16),
];
/// The largest offset code the decoder supports, the reference decoder's N (§3.1.1.3.2.1.1).
const MAX_OFFSET_CODE: usize = 31;
/// The largest accuracy logs a sequences table may state (§3.1.1.3.2.1, FSE_Compressed_Mode).
pub(super) const LITERALS_LENGTH_MAX_LOG: u32 = 9;
pub(super) const MATCH_LENGTH_MAX_LOG: u32 = 9;
pub(super) const OFFSET_MAX_LOG: u32 = 8;

/// One sequence: literals to copy, then a match (§3.1.1.4). `offset_value` is the raw value,
/// 1 to 3 naming repeat offsets (§3.1.1.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Sequence {
    pub(super) literals: u32,
    pub(super) offset_value: u32,
    pub(super) match_length: u32,
}

/// One cell of a sequence code's decoding table: the code's baseline and extra bits beside the
/// state's next baseline and bits, as the reference's `ZSTD_seqSymbol` holds them (zstd 1.5.7
/// `lib/decompress/zstd_decompress_internal.h`), so a sequence takes one lookup per code.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SeqCell {
    base: u32,
    extra: u8,
    bits: u8,
    next: u16,
}

/// A sequence code's decoding table of `1 << log` cells.
#[derive(Debug, Default, PartialEq, Eq)]
struct SeqTable {
    log: u32,
    cells: Vec<SeqCell>,
}

impl Clone for SeqTable {
    fn clone(&self) -> Self {
        Self {
            log: self.log,
            cells: self.cells.clone(),
        }
    }

    fn clone_from(&mut self, source: &Self) {
        self.log = source.log;
        self.cells.clone_from(&source.cells);
    }
}

impl SeqTable {
    /// Makes this the table of a normalized distribution, its cells with `kind`'s baselines
    /// and extra bits: spread and finished in one pass, as `ZSTD_buildFSETable` builds it. Every
    /// cell is written, so a table of the same size is not cleared first.
    fn build(&mut self, norm: &[i16], log: u32, kind: Kind) -> Result<(), Corrupt> {
        if norm.len() > kind.max_symbols() {
            return Err(Corrupt::Sequences);
        }
        let mut spread = Spread::new();
        let size = spread.spread(norm, log)?;
        self.log = log;
        if self.cells.len() != size {
            self.cells.resize(size, SeqCell::default());
        }
        for (cell, made) in self.cells.iter_mut().zip(spread.cells()) {
            let (symbol, bits, next) = made?;
            let (base, extra) = kind.code(symbol)?;
            *cell = SeqCell {
                base,
                extra,
                bits,
                next,
            };
        }
        Ok(())
    }

    /// Makes this the one-cell table of RLE_Mode (§3.1.1.3.2.1): every state decodes `symbol`
    /// and stays.
    fn rle(&mut self, symbol: u8, kind: Kind) -> Result<(), Corrupt> {
        let (base, extra) = kind.code(symbol)?;
        self.log = 0;
        self.cells.clear();
        self.cells.push(SeqCell {
            base,
            extra,
            bits: 0,
            next: 0,
        });
        Ok(())
    }

    /// The cell of `state`: every state is below the table's size (`fse::Table::at`), and the
    /// mask keeps the lookup in the table all the same.
    #[inline(always)]
    fn at(&self, state: u32) -> SeqCell {
        let mask = self.cells.len().wrapping_sub(1);
        let index = usize::try_from(state).unwrap_or(usize::MAX) & mask;
        self.cells.get(index).copied().unwrap_or_default()
    }
}

/// Where a symbol type's table comes from, kept for Repeat_Mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Source {
    /// None yet: Repeat_Mode is refused.
    #[default]
    Unset,
    /// The predefined distribution's table (§3.1.1.3.2.2).
    Predefined,
    /// The slot's own table, from an RLE symbol or a description.
    Own,
}

/// One symbol type's table: the predefined one, or one the stream gave, rebuilt in place.
#[derive(Debug, Default)]
pub(super) struct Slot {
    source: Source,
    own: SeqTable,
}

impl Clone for Slot {
    fn clone(&self) -> Self {
        Self {
            source: self.source,
            own: self.own.clone(),
        }
    }

    fn clone_from(&mut self, source: &Self) {
        self.source = source.source;
        self.own.clone_from(&source.own);
    }
}

impl Slot {
    /// Forgets the table, keeping its cells' allocation.
    pub(super) fn unset(&mut self) {
        self.source = Source::Unset;
    }

    fn table<'a>(&'a self, predefined: &'a SeqTable) -> Result<&'a SeqTable, Corrupt> {
        match self.source {
            Source::Unset => Err(Corrupt::Sequences),
            Source::Predefined => Ok(predefined),
            Source::Own => Ok(&self.own),
        }
    }
}

/// The three tables a block's sequences are decoded with, kept for Repeat_Mode.
#[derive(Debug, Default)]
pub(super) struct Tables {
    literals_length: Slot,
    offset: Slot,
    match_length: Slot,
}

impl Clone for Tables {
    fn clone(&self) -> Self {
        Self {
            literals_length: self.literals_length.clone(),
            offset: self.offset.clone(),
            match_length: self.match_length.clone(),
        }
    }

    fn clone_from(&mut self, source: &Self) {
        self.literals_length.clone_from(&source.literals_length);
        self.offset.clone_from(&source.offset);
        self.match_length.clone_from(&source.match_length);
    }
}

impl Tables {
    /// Forgets every table, keeping the allocations, as a frame without a dictionary starts.
    pub(super) fn unset(&mut self) {
        self.literals_length.unset();
        self.offset.unset();
        self.match_length.unset();
    }
}

/// The predefined distributions' tables (§3.1.1.3.2.2), built once for a decoder's life.
#[derive(Debug)]
pub(super) struct Predefined {
    literals_length: SeqTable,
    offset: SeqTable,
    match_length: SeqTable,
}

impl Predefined {
    pub(super) fn new() -> Result<Self, Corrupt> {
        let table = |norm: &[i16], log: u32, kind: Kind| -> Result<SeqTable, Corrupt> {
            let mut seq = SeqTable::default();
            seq.build(norm, log, kind)?;
            Ok(seq)
        };
        Ok(Self {
            literals_length: table(
                &LITERALS_LENGTH_DEFAULT,
                LITERALS_LENGTH_DEFAULT_LOG,
                Kind::LiteralsLength,
            )?,
            offset: table(&OFFSET_DEFAULT, OFFSET_DEFAULT_LOG, Kind::Offset)?,
            match_length: table(
                &MATCH_LENGTH_DEFAULT,
                MATCH_LENGTH_DEFAULT_LOG,
                Kind::MatchLength,
            )?,
        })
    }
}

/// Which symbol type a table is for.
#[derive(Clone, Copy)]
enum Kind {
    LiteralsLength,
    Offset,
    MatchLength,
}

impl Kind {
    fn max_symbols(self) -> usize {
        match self {
            Self::LiteralsLength => LITERALS_LENGTH_CODES.len(),
            Self::Offset => MAX_OFFSET_CODE + 1,
            Self::MatchLength => MATCH_LENGTH_CODES.len(),
        }
    }

    /// Code `symbol`'s baseline and extra bits (§3.1.1.3.2.1.1).
    #[inline(always)]
    fn code(self, symbol: u8) -> Result<(u32, u8), Corrupt> {
        match self {
            Self::LiteralsLength => LITERALS_LENGTH_CODES
                .get(usize::from(symbol))
                .copied()
                .ok_or(Corrupt::Sequences),
            Self::MatchLength => MATCH_LENGTH_CODES
                .get(usize::from(symbol))
                .copied()
                .ok_or(Corrupt::Sequences),
            // Offset code N: 2^N plus N extra bits.
            Self::Offset => {
                if usize::from(symbol) > MAX_OFFSET_CODE {
                    return Err(Corrupt::Sequences);
                }
                Ok((1u32 << symbol, symbol))
            }
        }
    }

    fn max_log(self) -> u32 {
        match self {
            Self::LiteralsLength => LITERALS_LENGTH_MAX_LOG,
            Self::Offset => OFFSET_MAX_LOG,
            Self::MatchLength => MATCH_LENGTH_MAX_LOG,
        }
    }
}

/// Sets `slot` to the table one mode gives (§3.1.1.3.2.1, Table 15), reading its description
/// from `bytes`; returns the bytes it took. Repeat_Mode keeps the slot's table.
fn table_for(kind: Kind, mode: u8, bytes: &[u8], slot: &mut Slot) -> Result<usize, Corrupt> {
    match mode {
        0 => {
            slot.source = Source::Predefined;
            Ok(0)
        }
        1 => {
            let symbol = *bytes.first().ok_or(Corrupt::Sequences)?;
            if usize::from(symbol) >= kind.max_symbols() {
                return Err(Corrupt::Sequences);
            }
            slot.own.rle(symbol, kind)?;
            slot.source = Source::Own;
            Ok(1)
        }
        2 => {
            let mut norm = [0i16; SYMBOLS_MAX];
            let (symbols, log, used) =
                read_distribution(bytes, kind.max_symbols(), kind.max_log(), &mut norm)?;
            slot.own
                .build(norm.get(..symbols).ok_or(Corrupt::Sequences)?, log, kind)?;
            slot.source = Source::Own;
            Ok(used)
        }
        _ => {
            if slot.source == Source::Unset {
                return Err(Corrupt::Sequences);
            }
            Ok(0)
        }
    }
}

/// Reads the sequences section `section`, handing each sequence to `apply` as it is decoded,
/// and updates `tables` for the next block. A section with no sequences leaves the tables as they were (§3.1.1.3.2.1); a
/// corrupt one may leave them changed, and the frame is abandoned with it.
pub(super) fn decode(
    section: &[u8],
    tables: &mut Tables,
    predefined: &Predefined,
    mut apply: impl FnMut(Sequence) -> Result<(), Corrupt>,
) -> Result<(), Corrupt> {
    let byte0 = *section.first().ok_or(Corrupt::Sequences)?;
    let (count, mut at) = match byte0 {
        0 => {
            return if section.len() == 1 {
                Ok(())
            } else {
                Err(Corrupt::Sequences)
            };
        }
        1..=127 => (usize::from(byte0), 1),
        128..=254 => {
            let byte1 = *section.get(1).ok_or(Corrupt::Sequences)?;
            // ((byte0 - 128) << 8) + byte1 (§3.1.1.3.2.1): at most 0x7EFF.
            ((usize::from(byte0 & 0x7F) << 8) | usize::from(byte1), 2)
        }
        255 => {
            let byte1 = *section.get(1).ok_or(Corrupt::Sequences)?;
            let byte2 = *section.get(2).ok_or(Corrupt::Sequences)?;
            // byte1 + (byte2 << 8) + 0x7F00: at most 0x17EFF.
            (
                (usize::from(byte1) | (usize::from(byte2) << 8)).saturating_add(0x7F00),
                3,
            )
        }
    };
    let modes = *section.get(at).ok_or(Corrupt::Sequences)?;
    at = at.checked_add(1).ok_or(Corrupt::Sequences)?;
    if modes & 0b11 != 0 {
        return Err(Corrupt::Sequences);
    }
    for (kind, mode, slot) in [
        (
            Kind::LiteralsLength,
            modes >> 6,
            &mut tables.literals_length,
        ),
        (Kind::Offset, (modes >> 4) & 0b11, &mut tables.offset),
        (
            Kind::MatchLength,
            (modes >> 2) & 0b11,
            &mut tables.match_length,
        ),
    ] {
        let used = table_for(
            kind,
            mode,
            section.get(at..).ok_or(Corrupt::Sequences)?,
            slot,
        )?;
        at = at.checked_add(used).ok_or(Corrupt::Sequences)?;
    }
    let ll = tables.literals_length.table(&predefined.literals_length)?;
    let of = tables.offset.table(&predefined.offset)?;
    let ml = tables.match_length.table(&predefined.match_length)?;
    let stream = section.get(at..).ok_or(Corrupt::Sequences)?;
    let mut bits = Backward::new(stream)?;
    let mut ll_state = bits.read(ll.log);
    let mut of_state = bits.read(of.log);
    let mut ml_state = bits.read(ml.log);
    for i in 0..count {
        let of_cell = of.at(of_state);
        let ml_cell = ml.at(ml_state);
        let ll_cell = ll.at(ll_state);
        // Offset bits first, then match length, then literals length (§3.1.1.3.2.1.2). A
        // baseline with its extra bits read stays within u32: at most 2^31 + 2^31 - 1 for an
        // offset, 65539 + 2^16 - 1 for a length. A reload leaves at least 57 bits: the offset's
        // and match length's extra bits (at most 31 + 16), then the literals length's and the
        // three states' (at most 9 + 9 + 8) while the extra bits total 31 or fewer, as they
        // nearly always do; past that a second refill is tested for.
        bits.reload();
        let offset_value = of_cell
            .base
            .wrapping_add(bits.read_ensured(u32::from(of_cell.extra)));
        let match_length = ml_cell
            .base
            .wrapping_add(bits.read_ensured(u32::from(ml_cell.extra)));
        let extra = u32::from(of_cell.extra)
            .wrapping_add(u32::from(ml_cell.extra))
            .wrapping_add(u32::from(ll_cell.extra));
        if extra > 31 {
            bits.ensure(42);
        }
        let literals = ll_cell
            .base
            .wrapping_add(bits.read_ensured(u32::from(ll_cell.extra)));
        apply(Sequence {
            literals,
            offset_value,
            match_length,
        })?;
        // States update for every sequence but the last: literals length, match length, offset.
        // A next state is below its table's size (`fse::Table::at`).
        if i.saturating_add(1) < count {
            ll_state =
                u32::from(ll_cell.next).wrapping_add(bits.read_ensured(u32::from(ll_cell.bits)));
            ml_state =
                u32::from(ml_cell.next).wrapping_add(bits.read_ensured(u32::from(ml_cell.bits)));
            of_state =
                u32::from(of_cell.next).wrapping_add(bits.read_ensured(u32::from(of_cell.bits)));
        }
    }
    if !bits.finished() {
        return Err(Corrupt::Sequences);
    }
    Ok(())
}

/// Reads a formatted dictionary's three FSE tables from `bytes` at `at`: offsets, match lengths,
/// literals lengths, in that order (§5), each a table description; returns where they end.
pub(super) fn dictionary_tables(
    bytes: &[u8],
    at: usize,
    tables: &mut Tables,
) -> Result<usize, Corrupt> {
    let mut at = at;
    for (kind, slot) in [
        (Kind::Offset, &mut tables.offset),
        (Kind::MatchLength, &mut tables.match_length),
        (Kind::LiteralsLength, &mut tables.literals_length),
    ] {
        let mut norm = [0i16; SYMBOLS_MAX];
        let (symbols, log, used) = read_distribution(
            bytes.get(at..).ok_or(Corrupt::Dictionary)?,
            kind.max_symbols(),
            kind.max_log(),
            &mut norm,
        )?;
        slot.own
            .build(norm.get(..symbols).ok_or(Corrupt::Dictionary)?, log, kind)?;
        slot.source = Source::Own;
        at = at.checked_add(used).ok_or(Corrupt::Dictionary)?;
    }
    Ok(at)
}
