//! Reading every stored byte on a schedule to find damage before a read does
//! (docs/design/chunk-store.md §9).
//!
//! Latent sector errors are found by nothing but reads, and most are found by scrubbing
//! rather than by the workload (Bairavasundaram et al., SIGMETRICS 2007, §6). The scrubber
//! verifies every live fragment segment by segment, at a rate that finishes the volume
//! within its period (seven days is practice, NetApp scrubbing every two weeks and Ceph
//! every one: docs/research/11 §12.2), in a staggered order that reads a little of every
//! region of the volume before any region twice (Schroeder, Damouras and Gill, FAST 2010,
//! §5.2.3). It records each chunk that fails in a bounded list for repair. A failure also
//! marks the volume at risk: errors cluster in space and time (Bairavasundaram et al. 2007,
//! §5), so the whole volume is then scrubbed at once rather than on schedule.

use std::collections::BTreeSet;
use std::ops::Range;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hyper_block::DiskError;
use hyper_block::block::BlockFile;

use crate::error::ChunkError;
use crate::frame::SegmentState;
use crate::key::ChunkKey;
use crate::recover::{Identity, ReadPool, verify_at};
use crate::writer::Shared;

/// Damaged chunks remembered for repair. More than 80% of disks with latent sector errors had
/// fewer than 50, and errors cluster within 10 MB (docs/research/03 BGPS07-F2, F3), so a
/// volume with thousands of damaged chunks has failed in many places: past this many it is
/// failing, to be drained whole rather than repaired chunk by chunk, and further damage is
/// counted rather than listed (docs/research/11 §12.4).
pub const MAX_DAMAGED: usize = 4096;

/// Bytes of data area one scrub step reads, and steps in a region. Schroeder, Damouras and
/// Gill stagger 128 MiB regions read in 1 MiB steps, one step of every region before the
/// next (docs/design/chunk-store.md §9); each round reads 1/128 of the volume, spread over
/// all of it.
const STEP: u64 = 1 << 20;
const REGION_STEPS: u64 = 128;

/// What scrubbing has found.
#[derive(Debug, Default)]
pub struct Findings {
    /// Chunks with a fragment that failed verification, awaiting repair.
    pub damaged: BTreeSet<ChunkKey>,
    /// Damaged chunks found once the list was full, counted but not listed.
    pub dropped: u64,
    /// The list filled: the volume is to be drained whole. It stays so until reopened.
    pub failing: bool,
    /// When the volume first showed damage; it stays at risk until reopened.
    pub at_risk_since: Option<Instant>,
    /// Fragments verified and bytes read, over the volume's life so far.
    pub fragments: u64,
    pub bytes: u64,
    /// Full passes completed.
    pub passes: u64,
}

impl Findings {
    fn record(&mut self, key: ChunkKey) {
        if self.damaged.len() >= MAX_DAMAGED && !self.damaged.contains(&key) {
            self.failing = true;
            self.dropped = self.dropped.saturating_add(1);
            return;
        }
        self.damaged.insert(key);
        self.at_risk_since.get_or_insert_with(Instant::now);
    }
}

pub(crate) type SharedFindings = Arc<Mutex<Findings>>;

/// What one scrub step found: records verified, their payload bytes, and records damaged.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Step {
    pub records: u64,
    pub bytes: u64,
    pub damaged: u64,
}

/// Verifies every live record that starts in `window` of the data area, the segments laid
/// end to end.
pub(crate) fn scrub_window<F: BlockFile>(
    shared: &Shared<F>,
    reads: &mut ReadPool,
    findings: &SharedFindings,
    window: Range<u64>,
) -> Result<Step, ChunkError> {
    let size = shared.geometry.segment_size;
    let mut step = Step::default();
    let mut at = window.start;
    while at < window.end {
        let segment = at.checked_div(size).ok_or(ChunkError::Fenced)?;
        let base = segment.checked_mul(size).ok_or(ChunkError::Fenced)?;
        let end = window.end.min(base.saturating_add(size));
        let from = u32::try_from(at.saturating_sub(base)).unwrap_or(u32::MAX);
        let to = u32::try_from(end.saturating_sub(base)).unwrap_or(u32::MAX);
        let segment = u32::try_from(segment).map_err(|_| ChunkError::Fenced)?;
        scrub_range(shared, reads, findings, segment, from..to, &mut step)?;
        at = end;
    }
    let mut f = findings.lock().map_err(|_| ChunkError::Fenced)?;
    f.fragments = f.fragments.saturating_add(step.records);
    f.bytes = f.bytes.saturating_add(step.bytes);
    Ok(step)
}

/// Verifies the live records that start in `offsets` of `segment`, read by where they lie
/// and each checked against the index before it counts.
fn scrub_range<F: BlockFile>(
    shared: &Shared<F>,
    reads: &mut ReadPool,
    findings: &SharedFindings,
    segment: u32,
    offsets: Range<u32>,
    step: &mut Step,
) -> Result<(), ChunkError> {
    let Some(incarnation) = incarnation(shared, segment)? else {
        return Ok(());
    };
    let placed = shared
        .index
        .read()
        .map_err(|_| ChunkError::Fenced)?
        .placed_in(segment, offsets);
    let identity = Identity {
        volume: shared.volume,
        checksum_shift: shared.checksum_shift,
    };
    for offset in placed {
        if shared.stopping.load(Ordering::Acquire) {
            break;
        }
        let at = u64::from(offset);
        let read = verify_at(
            &shared.file,
            reads,
            &shared.geometry,
            identity,
            segment,
            at,
            None,
        );
        match read {
            Ok(Some(v)) if v.prefix.header.incarnation == incarnation => {
                let h = &v.prefix.header;
                let index = shared.index.read().map_err(|_| ChunkError::Fenced)?;
                // A record moved or deleted since the offsets were read no longer counts.
                if index
                    .live(
                        &h.key,
                        h.chunk_offset,
                        (segment, offset),
                        h.incarnation,
                        h.sequence,
                    )
                    .is_some()
                {
                    step.records = step.records.saturating_add(1);
                    step.bytes = step.bytes.saturating_add(u64::from(h.payload_len));
                }
            }
            // A live record's place holds nothing intact, another incarnation's record, or
            // cannot be read: a latent sector error reads as an I/O error.
            Ok(_) | Err(ChunkError::Device(DiskError::Io { .. })) => {
                if let Some(key) = damaged_owner(shared, segment, offset, incarnation)? {
                    step.damaged = step.damaged.saturating_add(1);
                    findings.lock().map_err(|_| ChunkError::Fenced)?.record(key);
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// The incarnation of `segment` if it holds live records.
fn incarnation<F: BlockFile>(shared: &Shared<F>, segment: u32) -> Result<Option<u64>, ChunkError> {
    let usage = shared.usage.read().map_err(|_| ChunkError::Fenced)?;
    Ok(usage
        .get(segment)
        .filter(|s| s.state != SegmentState::Free && s.live > 0)
        .map(|s| s.incarnation))
}

/// The chunk whose record at `offset` of `segment` failed to verify, if that record is still
/// live in the same incarnation: one moved or deleted since is not damage.
pub(crate) fn damaged_owner<F: BlockFile>(
    shared: &Shared<F>,
    segment: u32,
    offset: u32,
    incarnation: u64,
) -> Result<Option<ChunkKey>, ChunkError> {
    if self::incarnation(shared, segment)? != Some(incarnation) {
        return Ok(None);
    }
    let index = shared.index.read().map_err(|_| ChunkError::Fenced)?;
    if !index.is_placed(segment, offset) {
        return Ok(None);
    }
    Ok(index
        .owner(segment, offset)
        .filter(|(_, f)| f.incarnation == incarnation)
        .map(|(key, _)| key))
}

/// Scrubs the whole volume once, in the staggered order; returns the damaged records found.
pub(crate) fn scrub_all<F: BlockFile>(
    shared: &Shared<F>,
    reads: &mut ReadPool,
    findings: &SharedFindings,
) -> Result<u64, ChunkError> {
    let data = data_bytes(shared);
    let mut damaged = 0u64;
    for p in 0..positions(data) {
        if shared.stopping.load(Ordering::Acquire) {
            break;
        }
        if let Some(window) = window(p, data) {
            damaged =
                damaged.saturating_add(scrub_window(shared, reads, findings, window)?.damaged);
        }
    }
    let mut f = findings.lock().map_err(|_| ChunkError::Fenced)?;
    f.passes = f.passes.saturating_add(1);
    Ok(damaged)
}

/// The background scrubber: one window of the staggered order at a time, pausing after each
/// so a pass over the volume takes the configured period, and without pausing while the
/// volume is at risk.
pub(crate) struct Scrubber<F> {
    pub shared: Arc<Shared<F>>,
    pub findings: SharedFindings,
    pub wake: std::sync::mpsc::Receiver<()>,
    pub period: Duration,
    /// The scrubber thread's own read buffers.
    pub reads: ReadPool,
}

impl<F: BlockFile> Scrubber<F> {
    pub fn run(mut self) {
        let data = data_bytes(&self.shared);
        let count = positions(data);
        let (mut p, mut pass_bytes) = (0u64, 0u64);
        loop {
            if self.shared.stopping.load(Ordering::Acquire) {
                return;
            }
            let volume_bytes = self.shared.usage.read().map_or(0, |usage| usage.live());
            if volume_bytes == 0 || count == 0 {
                // Nothing stored: wait a period, or until woken.
                if self.wait(self.period) {
                    return;
                }
                continue;
            }
            let position = p;
            p = p.saturating_add(1).checked_rem(count).unwrap_or(0);
            let Some(window) = window(position, data) else {
                continue;
            };
            let bytes = match scrub_window(&self.shared, &mut self.reads, &self.findings, window) {
                Ok(step) => step.bytes,
                // The volume is fenced or closing: stop until woken.
                Err(_) => {
                    if self.wait(self.period) {
                        return;
                    }
                    continue;
                }
            };
            pass_bytes = pass_bytes.saturating_add(bytes);
            if position.saturating_add(1) == count {
                if let Ok(mut f) = self.findings.lock() {
                    f.passes = f.passes.saturating_add(1);
                }
                // A pass that verified nothing paced nothing: it waits a period, so the loop
                // never spins over windows with nothing live.
                if std::mem::take(&mut pass_bytes) == 0 && self.wait(self.period) {
                    return;
                }
            }
            let at_risk = self
                .findings
                .lock()
                .map(|f| f.at_risk_since.is_some())
                .unwrap_or(false);
            let pause = if at_risk {
                Duration::ZERO
            } else {
                pace(bytes, volume_bytes, self.period)
            };
            if self.wait(pause) {
                return;
            }
        }
    }

    /// Waits `pause`, or until woken; true if the volume is closing.
    fn wait(&self, pause: Duration) -> bool {
        match self.wake.recv_timeout(pause) {
            Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return true,
        }
        self.shared.stopping.load(Ordering::Acquire)
    }
}

/// Bytes of the data area: every segment, end to end.
fn data_bytes<F: BlockFile>(shared: &Shared<F>) -> u64 {
    u64::from(shared.geometry.segments).saturating_mul(shared.geometry.segment_size)
}

/// Regions of `data` bytes of data area.
fn regions(data: u64) -> u64 {
    data.div_ceil(STEP.saturating_mul(REGION_STEPS))
}

/// Positions in a staggered pass over `data` bytes: every step of every region.
fn positions(data: u64) -> u64 {
    regions(data).saturating_mul(REGION_STEPS)
}

/// The bytes position `p` of a staggered pass covers: step `p / regions` of region
/// `p % regions`. `None` past the end of the data area.
fn window(p: u64, data: u64) -> Option<Range<u64>> {
    let regions = regions(data);
    let start = p
        .checked_rem(regions)?
        .checked_mul(REGION_STEPS)?
        .checked_add(p.checked_div(regions)?)?
        .checked_mul(STEP)?;
    if start < data {
        Some(start..start.saturating_add(STEP).min(data))
    } else {
        None
    }
}

/// The pause after scrubbing `bytes`, so that `volume_bytes` take `period` to scrub.
pub(crate) fn pace(bytes: u64, volume_bytes: u64, period: Duration) -> Duration {
    if volume_bytes == 0 {
        return period;
    }
    // bytes / volume_bytes of the period, computed in nanoseconds without overflow.
    let share = u128::from(bytes)
        .saturating_mul(period.as_nanos())
        .checked_div(u128::from(volume_bytes))
        .unwrap_or(0);
    Duration::from_nanos(u64::try_from(share).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pacing_spreads_the_volume_over_the_period() {
        let week = Duration::from_secs(7 * 24 * 3600);
        assert_eq!(pace(1, 7, week), Duration::from_secs(24 * 3600));
        assert_eq!(pace(0, 7, week), Duration::ZERO);
        assert_eq!(pace(5, 0, week), week);
    }

    #[test]
    fn findings_are_bounded() {
        let mut f = Findings::default();
        for n in 0..(MAX_DAMAGED as u128 + 10) {
            f.record(ChunkKey {
                block: n,
                epoch: 0,
                index: 0,
            });
        }
        assert_eq!(f.damaged.len(), MAX_DAMAGED);
        assert_eq!(f.dropped, 10);
        assert!(f.failing);
        assert!(f.at_risk_since.is_some());
    }

    /// A pass covers the data area once, and its first round one step of every region.
    #[test]
    fn the_staggered_order_covers_the_volume_and_spreads_each_round() {
        let region = STEP * REGION_STEPS;
        for data in [
            STEP,
            STEP + 1,
            region - 1,
            region,
            region + STEP / 2,
            20 * region + 7,
        ] {
            let mut windows: Vec<Range<u64>> = (0..positions(data))
                .filter_map(|p| window(p, data))
                .collect();
            let first: Vec<u64> = windows
                .iter()
                .take(regions(data) as usize)
                .map(|w| w.start / region)
                .collect();
            assert_eq!(first, (0..regions(data)).collect::<Vec<_>>(), "{data}");
            windows.sort_by_key(|w| w.start);
            let mut at = 0;
            for w in &windows {
                assert_eq!(w.start, at, "{data}");
                at = w.end;
            }
            assert_eq!(at, data);
        }
        assert_eq!(window(1, 3 * region), Some(region..region + STEP));
        assert_eq!(window(3, 3 * region), Some(STEP..2 * STEP));
    }
}
