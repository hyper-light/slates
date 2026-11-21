//! The circular index log: reading and writing frames at positions within its region, and
//! tracking which part of it is still needed.

use hyper_block::DiskError;
use hyper_block::block::BlockFile;
use hyper_block::buf::AlignedBuf;

use crate::frame::{self, FrameHeader, LogRecord};
use crate::layout::{Geometry, MAX_FRAME_BYTES};

/// The live span of the log: from the start frame recovery replays from, to where the next
/// frame goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    /// Byte position of the next frame within the log region.
    pub pos: u64,
    /// LSN of the next frame.
    pub lsn: u64,
    /// Position and LSN of the frame recovery starts from.
    pub start_pos: u64,
    pub start_lsn: u64,
    /// Bytes from `start_pos` to `pos`, going around the end.
    pub used: u64,
}

impl Cursor {
    /// Where a frame of `len` bytes goes, and whether a wrap frame must be written at the
    /// current position first. `None` if it would overwrite the live span.
    pub fn place(&self, len: u64, block: u64, log_size: u64) -> Option<Placement> {
        if len > log_size {
            return None;
        }
        let tail_room = log_size.checked_sub(self.pos)?;
        let (wrap, at, consumed) = if len <= tail_room {
            (false, self.pos, len)
        } else {
            // The rest of this lap is skipped; a wrap frame marks it when a block fits.
            (tail_room >= block, 0, tail_room.checked_add(len)?)
        };
        let free = log_size.checked_sub(self.used)?;
        if consumed > free {
            return None;
        }
        Some(Placement { wrap, at, consumed })
    }

    /// Advances past a frame placed by `place`.
    pub fn advance(&mut self, placement: Placement, len: u64, log_size: u64) {
        let lsns = if placement.wrap { 2 } else { 1 };
        self.lsn = self.lsn.saturating_add(lsns);
        let end = placement.at.saturating_add(len);
        self.pos = if end >= log_size { 0 } else { end };
        self.used = self.used.saturating_add(placement.consumed);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    /// A wrap frame goes at the old position before the frame.
    pub wrap: bool,
    /// Where the frame goes.
    pub at: u64,
    /// Log bytes the frame (and any skipped tail) consumes.
    pub consumed: u64,
}

/// Reads the log's frames through a window of up to `MAX_FRAME_BYTES`. A frame is taken from
/// the window when the window holds it, and the window moves to start at the frame when it
/// does not, so a replay, or a search of every block, reads the log in large sequential reads,
/// each byte at most twice: a window moves either past its end or to a frame that runs past
/// it, which then lies wholly within the next. Before, each frame took a read of its first
/// block and another of the whole frame, and a search of the log read it one block at a time
/// (audit P07).
pub struct Frames<'a, F> {
    file: &'a F,
    geometry: &'a Geometry,
    volume: u128,
    window: AlignedBuf,
    /// The log position of the window's first byte, and the bytes it holds.
    start: u64,
    held: u64,
}

impl<'a, F: BlockFile> Frames<'a, F> {
    pub fn new(file: &'a F, geometry: &'a Geometry, volume: u128) -> Result<Self, DiskError> {
        let size = usize::try_from(MAX_FRAME_BYTES.min(geometry.log_size)).unwrap_or(0);
        Ok(Self {
            file,
            geometry,
            volume,
            window: AlignedBuf::zeroed(size, file.alignment())?,
            start: 0,
            held: 0,
        })
    }

    /// The frame of this volume at `pos`, verified whole, its records and its padded length.
    /// `Ok(None)` when no valid frame of the volume starts there: the end of the log, a torn
    /// frame, or stale bytes from an earlier lap or volume.
    pub fn frame(
        &mut self,
        pos: u64,
    ) -> Result<Option<(FrameHeader, Vec<LogRecord>, u64)>, DiskError> {
        let Some((_, len)) = self.header(pos)? else {
            return Ok(None);
        };
        let volume = self.volume;
        Ok(self
            .bytes(pos, len)?
            .and_then(|bytes| frame::decode(bytes, volume))
            .map(|(h, r)| (h, r, len)))
    }

    /// The header of a frame of this volume that starts at `pos`, not yet verified, and the
    /// frame's padded length, when the frame would lie within the log.
    pub fn header(&mut self, pos: u64) -> Result<Option<(FrameHeader, u64)>, DiskError> {
        let (block, log_size, volume) = (self.geometry.block, self.geometry.log_size, self.volume);
        let block_len = self.geometry.block_usize();
        let Some(first) = self.bytes(pos, block)? else {
            return Ok(None);
        };
        let Some(header) = frame::peek(first) else {
            return Ok(None);
        };
        if header.volume != volume {
            return Ok(None);
        }
        let Some(len) = header
            .frame_len(block_len)
            .and_then(|l| u64::try_from(l).ok())
        else {
            return Ok(None);
        };
        if len > MAX_FRAME_BYTES || pos.checked_add(len).is_none_or(|end| end > log_size) {
            return Ok(None);
        }
        Ok(Some((header, len)))
    }

    /// Log bytes `[pos, pos + len)`, `len` at most the window's size, moving the window to
    /// `pos` when it does not hold them. `None` past the log's end or the file's.
    fn bytes(&mut self, pos: u64, len: u64) -> Result<Option<&[u8]>, DiskError> {
        let log_size = self.geometry.log_size;
        let Some(end) = pos.checked_add(len).filter(|&e| e <= log_size) else {
            return Ok(None);
        };
        let held_end = self.start.saturating_add(self.held);
        if pos < self.start || end > held_end {
            let capacity = u64::try_from(self.window.capacity()).unwrap_or(0);
            if len > capacity {
                return Ok(None);
            }
            let Some(at) = self.geometry.log_offset.checked_add(pos) else {
                return Ok(None);
            };
            // As much of the log from `pos` as the window holds; where the file ends first,
            // just the bytes asked for.
            self.held = 0;
            let mut read = |want: u64| -> Result<bool, DiskError> {
                let want = usize::try_from(want).unwrap_or(0);
                self.window.set_len(want)?;
                match self.file.read_exact_at(self.window.as_mut_slice(), at) {
                    Ok(()) => Ok(true),
                    Err(DiskError::ShortRead { .. }) => Ok(false),
                    Err(e) => Err(e),
                }
            };
            let wide = capacity.min(log_size.saturating_sub(pos));
            let held = if read(wide)? {
                wide
            } else if read(len)? {
                len
            } else {
                return Ok(None);
            };
            self.start = pos;
            self.held = held;
        }
        let from = usize::try_from(pos.saturating_sub(self.start)).unwrap_or(usize::MAX);
        let to = usize::try_from(end.saturating_sub(self.start)).unwrap_or(usize::MAX);
        Ok(self.window.as_slice().get(from..to))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cursor(pos: u64, start: u64, used: u64) -> Cursor {
        Cursor {
            pos,
            lsn: 1,
            start_pos: start,
            start_lsn: 1,
            used,
        }
    }

    #[test]
    fn frames_fit_until_they_would_reach_the_live_span() {
        let (block, size) = (4096, 16 * 4096);
        let c = cursor(0, 0, 0);
        assert_eq!(
            c.place(4096, block, size),
            Some(Placement {
                wrap: false,
                at: 0,
                consumed: 4096
            })
        );
        // Near the end: the tail is skipped with a wrap frame and the frame goes to 0.
        let c = cursor(14 * 4096, 12 * 4096, 2 * 4096);
        assert_eq!(
            c.place(3 * 4096, block, size),
            Some(Placement {
                wrap: true,
                at: 0,
                consumed: 5 * 4096
            })
        );
        // Not enough free space before the live span.
        let c = cursor(4096, 2 * 4096, 15 * 4096);
        assert_eq!(c.place(2 * 4096, block, size), None);
    }

    #[test]
    fn advancing_counts_the_wrap_frame_and_wraps_the_position() {
        let (block, size) = (4096, 16 * 4096);
        let mut c = cursor(14 * 4096, 12 * 4096, 2 * 4096);
        let p = c.place(3 * 4096, block, size).unwrap();
        c.advance(p, 3 * 4096, size);
        assert_eq!(c.pos, 3 * 4096);
        assert_eq!(c.lsn, 3);
        assert_eq!(c.used, 7 * 4096);
    }
}
