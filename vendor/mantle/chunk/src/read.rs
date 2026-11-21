//! Reading a fragment's bytes back, verified: the record's header against the identity the
//! index expects, and every checksum block returned against the record's table; and the gate
//! that holds client reads at the device's measured depth (docs/design/chunk-store.md §7).

use std::collections::VecDeque;
use std::sync::Mutex;
use std::thread::Thread;

use hyper_block::block::BlockFile;

use crate::error::ChunkError;
use crate::index::Fragment;
use crate::key::ChunkKey;
use crate::layout::Reads;
use crate::record;
use crate::recover::{ReadPool, Span, read_span};
use crate::writer::Shared;

/// What the gate has let through and refused.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReadStats {
    /// The most reads at the device at once since the volume opened.
    pub most_at_device: usize,
    /// The most bytes the reads at the device buffered at once since the volume opened.
    pub most_bytes: u64,
    /// Reads refused with `Busy` since the volume opened.
    pub refused: u64,
}

#[derive(Debug, Default)]
struct Turns {
    at_device: usize,
    /// Bytes the reads at the device buffer.
    bytes: u64,
    /// The reads waiting, in the order they arrived: each one's ticket, the bytes it will
    /// buffer, and its thread, woken alone when its turn comes.
    waiting: VecDeque<(u64, u64, Thread)>,
    /// Tickets handed out, and turns handed to waiting reads, in the order they arrived.
    issued: u64,
    admitted: u64,
    stats: ReadStats,
}

impl Turns {
    /// Whether a read that buffers `cost` bytes may go to the device now: a turn is free, and
    /// its bytes fit, or it goes alone.
    fn fits(&self, reads: &Reads, cost: u64) -> bool {
        self.at_device < reads.depth
            && (self.at_device == 0 || self.bytes.saturating_add(cost) <= reads.bytes)
    }

    fn admit(&mut self, cost: u64) {
        self.at_device = self.at_device.saturating_add(1);
        self.bytes = self.bytes.saturating_add(cost);
        self.stats.most_at_device = self.stats.most_at_device.max(self.at_device);
        self.stats.most_bytes = self.stats.most_bytes.max(self.bytes);
    }
}

/// Holds client reads at `Reads::depth` at the device and their buffers at `Reads::bytes`,
/// lets `Reads::waiting` more wait in the order they arrived, and refuses the rest with
/// `Busy`. A read is let through before it takes anything, so what reads hold is bounded by
/// the reads let through, not by how many arrive (audit S07): their device buffers by
/// `Reads::bytes`, or one read's when it goes alone, and their outputs, the bytes each caller
/// asked for, by at most `Reads::depth` of them at once. The outputs are not counted against
/// `Reads::bytes`, which is the device's measure of bytes in flight rather than a budget of
/// memory; what a node lets its callers ask for is its own admission. A wait lasts while the
/// reads ahead take, each bounded by the operating system's I/O timeout.
#[derive(Debug)]
pub(crate) struct Gate {
    reads: Reads,
    turns: Mutex<Turns>,
}

/// A read's turn at the device and the bytes it buffers, given back when dropped: to the reads
/// that have waited longest, in order, as many as then fit.
pub(crate) struct Turn<'a> {
    gate: &'a Gate,
    cost: u64,
}

impl Gate {
    pub fn new(reads: Reads) -> Self {
        Self {
            reads,
            turns: Mutex::new(Turns::default()),
        }
    }

    /// Bytes of payload a read goes past rather than read twice (`Reads::gap`).
    pub fn gap(&self) -> u64 {
        self.reads.gap
    }

    /// The depth, bytes and waiting the gate holds reads to, which a volume's
    /// [`crate::Reader`] holds its own reads to as well.
    pub fn reads(&self) -> Reads {
        self.reads
    }

    /// A turn for a read that buffers `cost` bytes: at once if it fits and none waits, after
    /// the reads that came first if one may wait, and otherwise `Busy`.
    pub fn enter(&self, cost: u64) -> Result<Turn<'_>, ChunkError> {
        let mut turns = self.turns.lock().map_err(|_| ChunkError::Fenced)?;
        if turns.waiting.is_empty() && turns.fits(&self.reads, cost) {
            turns.admit(cost);
        } else if turns.waiting.len() < self.reads.waiting {
            let ticket = turns.issued;
            turns.issued = ticket.checked_add(1).ok_or(ChunkError::Busy)?;
            turns
                .waiting
                .push_back((ticket, cost, std::thread::current()));
            // A turn given back admits the reads at the head that fit, counts them at the
            // device, and wakes only them. A wake-up before the park is kept by it.
            while turns.admitted <= ticket {
                drop(turns);
                std::thread::park();
                turns = self.turns.lock().map_err(|_| ChunkError::Fenced)?;
            }
        } else {
            turns.stats.refused = turns.stats.refused.saturating_add(1);
            return Err(ChunkError::Busy);
        }
        Ok(Turn { gate: self, cost })
    }

    pub fn stats(&self) -> Result<ReadStats, ChunkError> {
        Ok(self.turns.lock().map_err(|_| ChunkError::Fenced)?.stats)
    }
}

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        let Ok(mut turns) = self.gate.turns.lock() else {
            return;
        };
        turns.at_device = turns.at_device.saturating_sub(1);
        turns.bytes = turns.bytes.saturating_sub(self.cost);
        let mut woken = Vec::new();
        // In the order they arrived: a read at the head that does not fit holds the rest.
        while let Some(&(_, cost, _)) = turns.waiting.front() {
            if !turns.fits(&self.gate.reads, cost) {
                break;
            }
            let Some((_, cost, reader)) = turns.waiting.pop_front() else {
                break;
            };
            turns.admit(cost);
            turns.admitted = turns.admitted.saturating_add(1);
            woken.push(reader);
        }
        drop(turns);
        for reader in woken {
            reader.unpark();
        }
    }
}

/// How a read of `fragment`'s payload `[from, to)` goes to the device: the record's header
/// and checksum table, which verify it, and the checksum blocks that hold the range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Plan {
    /// Bytes of the header and checksum table.
    pub prefix: u64,
    /// The payload offsets of the first checksum block the range needs and the end of the
    /// last.
    pub blocks: (u64, u64),
    /// Whether the header and table are read apart from the blocks: when the blocks start
    /// more than the device's gap into the payload, which a single read would read past
    /// (audit P01).
    pub apart: bool,
}

impl Plan {
    /// Plans a read of `[from, to)` of `fragment`'s payload whose checksum blocks are
    /// `1 << shift` bytes, reading past no more than `gap` bytes of payload it was not asked
    /// for.
    pub fn new(shift: u8, fragment: &Fragment, from: u64, to: u64, gap: u64) -> Option<Self> {
        let block_size = 1u64.checked_shl(u32::from(shift))?;
        let prefix = u64::try_from(record::prefix_len(fragment.payload_len, shift)?).ok()?;
        let first = from.checked_div(block_size)?.checked_mul(block_size)?;
        let end = to
            .div_ceil(block_size)
            .checked_mul(block_size)?
            .min(u64::from(fragment.payload_len));
        Some(Self {
            prefix,
            blocks: (first, end),
            apart: first > gap,
        })
    }

    /// Bytes the read buffers from the device.
    pub fn bytes(&self) -> u64 {
        let (first, end) = self.blocks;
        if self.apart {
            self.prefix.saturating_add(end.saturating_sub(first))
        } else {
            self.prefix.saturating_add(end)
        }
    }
}

/// Appends payload bytes `[from, to)` of `fragment` to `out`, read through the caller's own
/// buffers. A device error or any failed check is `Corrupt`: the caller reads another copy.
pub(crate) fn fragment<F: BlockFile>(
    shared: &Shared<F>,
    reads: &mut ReadPool,
    key: &ChunkKey,
    fragment: &Fragment,
    from: u64,
    to: u64,
    out: &mut Vec<u8>,
) -> Result<(), ChunkError> {
    let corrupt = |detail: &str| ChunkError::Corrupt {
        key: *key,
        detail: detail.to_owned(),
    };
    let geometry = &shared.geometry;
    let shift = shared.checksum_shift;
    let plan = Plan::new(shift, fragment, from, to, shared.reads.gap())
        .ok_or_else(|| corrupt("record size"))?;
    let (payload_from, last_block_end) = plan.blocks;
    let base = geometry
        .segment_offset(fragment.segment)
        .ok_or_else(|| corrupt("segment"))?;
    let read = |reads: &mut ReadPool, offset: u64, len: u64| match read_span(
        &shared.file,
        reads,
        geometry,
        base,
        offset,
        len,
    ) {
        Ok(Some(span)) => Ok(span),
        Ok(None) => Err(corrupt("record extends past the end of the volume")),
        Err(ChunkError::Device(e)) => Err(corrupt(&format!("read failed: {e}"))),
        Err(e) => Err(e),
    };
    let record_at = u64::from(fragment.offset);
    // The header and table, with the blocks when they are near enough to read past the
    // payload before them; the blocks apart otherwise.
    let head = if plan.apart {
        read(reads, record_at, plan.prefix)?
    } else {
        read(reads, record_at, plan.bytes())?
    };
    let apart = if plan.apart {
        let at = record_at
            .checked_add(plan.prefix)
            .and_then(|at| at.checked_add(payload_from))
            .ok_or_else(|| corrupt("offset"));
        match at.and_then(|at| read(reads, at, last_block_end.saturating_sub(payload_from))) {
            Ok(span) => Some(span),
            Err(e) => {
                reads.give(head);
                return Err(e);
            }
        }
    } else {
        None
    };
    let checked = check(
        shared,
        key,
        fragment,
        &plan,
        (from, to),
        (head.bytes(), apart.as_ref().map(Span::bytes)),
        out,
    );
    reads.give(head);
    if let Some(span) = apart {
        reads.give(span);
    }
    checked
}

/// Verifies a read of `fragment` against the index and its checksums, from the header and
/// table in `head` and the checksum blocks in `apart`, or after the table in `head` when
/// read with it, and appends payload bytes `[from, to)` to `out`.
pub(crate) fn check<F>(
    shared: &Shared<F>,
    key: &ChunkKey,
    fragment: &Fragment,
    plan: &Plan,
    (from, to): (u64, u64),
    (head, apart): (&[u8], Option<&[u8]>),
    out: &mut Vec<u8>,
) -> Result<(), ChunkError> {
    let corrupt = |detail: &str| ChunkError::Corrupt {
        key: *key,
        detail: detail.to_owned(),
    };
    let block_size = 1u64
        .checked_shl(u32::from(shared.checksum_shift))
        .unwrap_or(u64::MAX);
    let (payload_from, last_block_end) = plan.blocks;
    let first_block = payload_from.checked_div(block_size).unwrap_or(0);
    let prefix =
        record::decode_prefix(head).ok_or_else(|| corrupt("record header did not verify"))?;
    let h = &prefix.header;
    if h.key != *key
        || h.volume != shared.volume
        || h.segment != fragment.segment
        || h.incarnation != fragment.incarnation
        || h.sequence != fragment.sequence
        || h.chunk_offset != fragment.chunk_offset
        || h.payload_len != fragment.payload_len
    {
        return Err(corrupt("record identity does not match the index"));
    }
    let blocks = match apart {
        Some(bytes) => bytes,
        None => {
            let start = usize::try_from(plan.prefix.saturating_add(payload_from))
                .map_err(|_| corrupt("offset"))?;
            let stop = usize::try_from(plan.prefix.saturating_add(last_block_end))
                .map_err(|_| corrupt("offset"))?;
            head.get(start..stop)
                .ok_or_else(|| corrupt("short record"))?
        }
    };
    let first = u32::try_from(first_block).map_err(|_| corrupt("offset"))?;
    if !record::verify(&prefix, first, blocks) {
        return Err(corrupt("payload checksum mismatch"));
    }
    let skip = usize::try_from(from.saturating_sub(payload_from)).map_err(|_| corrupt("offset"))?;
    let take = usize::try_from(to.saturating_sub(from)).map_err(|_| corrupt("offset"))?;
    let wanted = blocks
        .get(skip..skip.saturating_add(take))
        .ok_or_else(|| corrupt("short record"))?;
    out.extend_from_slice(wanted);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::mpsc;

    fn waiting(gate: &Gate) -> usize {
        gate.turns.lock().unwrap().waiting.len()
    }

    /// The gate holds its depth at the device, lets as many more wait, in the order they came,
    /// and refuses the rest.
    #[test]
    fn the_gate_holds_its_depth_and_lets_reads_wait_in_order() {
        let gate = Arc::new(Gate::new(Reads {
            depth: 2,
            waiting: 2,
            bytes: u64::MAX,
            gap: 0,
        }));
        let first = gate.enter(1).unwrap();
        let second = gate.enter(1).unwrap();
        let (admitted, order) = mpsc::channel();
        let waiters: Vec<_> = (0..2)
            .map(|n| {
                let (theirs, admitted) = (Arc::clone(&gate), admitted.clone());
                let waiter = std::thread::spawn(move || {
                    let _turn = theirs.enter(1).unwrap();
                    admitted.send(n).unwrap();
                });
                // The next one arrives only once this one waits.
                while waiting(&gate) < n + 1 {
                    std::thread::yield_now();
                }
                waiter
            })
            .collect();
        assert!(matches!(gate.enter(1), Err(ChunkError::Busy)));
        drop(first);
        assert_eq!(order.recv().unwrap(), 0);
        drop(second);
        assert_eq!(order.recv().unwrap(), 1);
        for waiter in waiters {
            waiter.join().unwrap();
        }
        let stats = gate.stats().unwrap();
        assert_eq!((stats.most_at_device, stats.refused), (2, 1));
        let turns = gate.turns.lock().unwrap();
        assert_eq!((turns.at_device, turns.waiting.len()), (0, 0));
    }

    /// The gate holds the reads' buffers to its bytes as it holds them to its depth: a read
    /// whose bytes do not fit waits with a turn free, the reads behind it wait in order, and a
    /// read larger than the bytes goes only alone (audit S07).
    #[test]
    fn the_gate_holds_its_bytes_and_takes_a_larger_read_alone() {
        let gate = Arc::new(Gate::new(Reads {
            depth: 4,
            waiting: 4,
            bytes: 100,
            gap: 0,
        }));
        let first = gate.enter(60).unwrap();
        let (admitted, order) = mpsc::channel();
        let costs = [50u64, 10, 500];
        let waiters: Vec<_> = costs
            .iter()
            .enumerate()
            .map(|(n, &cost)| {
                let (theirs, admitted) = (Arc::clone(&gate), admitted.clone());
                let waiter = std::thread::spawn(move || {
                    let turn = theirs.enter(cost).unwrap();
                    admitted.send((n, turn.cost)).unwrap();
                    // Held until the test has seen who is at the device.
                    std::thread::park();
                    drop(turn);
                });
                while waiting(&gate) < n + 1 {
                    std::thread::yield_now();
                }
                waiter
            })
            .collect();
        // Behind the one that does not fit, a read that would fit waits too.
        assert_eq!(gate.turns.lock().unwrap().at_device, 1);
        drop(first);
        // The first two fit together; the large one waits for the device to empty.
        let mut seen = vec![order.recv().unwrap(), order.recv().unwrap()];
        seen.sort_unstable();
        assert_eq!(seen, vec![(0, 50), (1, 10)]);
        assert_eq!(waiting(&gate), 1);
        waiters[0].thread().unpark();
        waiters[1].thread().unpark();
        assert_eq!(order.recv().unwrap(), (2, 500));
        assert_eq!(gate.turns.lock().unwrap().at_device, 1);
        waiters[2].thread().unpark();
        for waiter in waiters {
            waiter.join().unwrap();
        }
        let stats = gate.stats().unwrap();
        assert_eq!(stats.most_bytes, 500);
        let turns = gate.turns.lock().unwrap();
        assert_eq!(
            (turns.at_device, turns.bytes, turns.waiting.len()),
            (0, 0, 0)
        );
    }

    /// However many readers come at once, no more than the depth are at the device, their
    /// bytes stay within the budget but for a read alone, and every turn taken is given back.
    #[test]
    fn many_readers_never_pass_the_depth_or_the_bytes() {
        let gate = Gate::new(Reads {
            depth: 3,
            waiting: 5,
            bytes: 1_000,
            gap: 0,
        });
        std::thread::scope(|s| {
            for t in 0..16u64 {
                let gate = &gate;
                s.spawn(move || {
                    for i in 0..2_000u64 {
                        // Mostly reads that fit, now and then one larger than the budget.
                        let cost = if (t + i) % 97 == 0 {
                            5_000
                        } else {
                            100 + (t * 37 + i) % 400
                        };
                        match gate.enter(cost) {
                            Ok(turn) => drop(turn),
                            Err(ChunkError::Busy) => std::thread::yield_now(),
                            Err(e) => panic!("{e}"),
                        }
                    }
                });
            }
        });
        let stats = gate.stats().unwrap();
        assert!(stats.most_at_device <= 3, "{stats:?}");
        assert!(stats.most_bytes <= 5_000, "{stats:?}");
        let turns = gate.turns.lock().unwrap();
        assert_eq!(
            (turns.at_device, turns.bytes, turns.waiting.len()),
            (0, 0, 0)
        );
    }
}
