//! The merge a scan reads a leaf segment's trunk sources through (docs/design/engine-structure.md
//! §5): a cursor for each branch, given newest first as `Trunk::segment_at` lists them. Each key
//! reads from the newest source holding it, a deletion too, and every source holding it moves
//! past it.

use crate::branch::{Branch, Op, RunCursor};
use crate::error::Error;
use crate::store::Store;
use crate::trunk::Source;
use hyper_block::block::BlockFile;

/// One source's head: a branch's cursor.
#[derive(Debug)]
enum Head<'a> {
    Branch(RunCursor, &'a Branch),
}

impl Head<'_> {
    fn key(&self) -> Option<&[u8]> {
        match self {
            Head::Branch(c, _) => c.valid().then(|| c.key()),
        }
    }

    fn op(&self) -> Op {
        match self {
            Head::Branch(c, _) => c.op(),
        }
    }

    fn value(&self) -> &[u8] {
        match self {
            Head::Branch(c, _) => c.value(),
        }
    }

    fn next<F: BlockFile>(&mut self, store: &mut Store<F>) -> Result<(), Error> {
        match self {
            Head::Branch(c, b) => c.next(b, store),
        }
    }

    async fn next_async<F: BlockFile>(&mut self, store: &mut Store<F>) -> Result<(), Error> {
        match self {
            Head::Branch(c, b) => c.next_async(b, store).await,
        }
    }

    /// Gives the head's pages back to `store`.
    fn give_back<F: BlockFile>(self, store: &mut Store<F>) {
        match self {
            Head::Branch(c, _) => c.give_back(store),
        }
    }
}

/// A merge of a segment's sources over `[from, end)`, its buffers kept between segments.
#[derive(Debug, Default)]
pub struct ScanMerge<'a> {
    heads: Vec<Head<'a>>,
    end: Vec<u8>,
    bounded: bool,
    /// The head whose entry is current, if any.
    current: Option<usize>,
    /// The key `next` moves past.
    past: Vec<u8>,
    /// A range filter check's buffer.
    scratch: Vec<u8>,
    /// Sources passed over at open by their range filters, and sources opened.
    skipped: u64,
    opened: u64,
    /// The nanoseconds each source of the last open took to open, in the sources' order, none
    /// for one passed over: what a seek's extra sources cost it, measured.
    open_ns: Vec<u64>,
}

impl<'a> ScanMerge<'a> {
    /// A merge holding no sources.
    pub fn new() -> Self {
        Self::default()
    }

    /// Opens `sources` (newest first) over `[from, end)`, the heads of a merge open before given
    /// back first. With `filter`, a source whose range filter rules `[from, end)` out is passed
    /// over unread. A scan the caller bounds
    /// asks for it; a leaf's end alone almost never rules a branch above it out, so an open scan
    /// does not pay the checks.
    pub fn open<F: BlockFile>(
        &mut self,
        store: &mut Store<F>,
        sources: &[Source<'a>],
        from: &[u8],
        end: Option<&[u8]>,
        filter: bool,
        timed: bool,
    ) -> Result<(), Error> {
        self.close(store);
        self.end.clear();
        self.bounded = end.is_some();
        if let Some(e) = end {
            self.end.extend_from_slice(e);
        }
        self.open_ns.clear();
        for s in sources {
            let t = timed.then(std::time::Instant::now);
            if filter && end.is_some() {
                let scratch = &mut self.scratch;
                let held = match *s {
                    Source::Branch(b) => b.range.may_hold(from, end, scratch),
                };
                if !held {
                    self.skipped = self.skipped.saturating_add(1);
                    self.open_ns.push(0);
                    continue;
                }
            }
            self.opened = self.opened.saturating_add(1);
            let head = match *s {
                Source::Branch(b) => Head::Branch(b.seek(store, from)?, b),
            };
            self.heads.push(head);
            self.open_ns.push(t.map_or(0, |t| {
                u64::try_from(t.elapsed().as_nanos()).unwrap_or(u64::MAX)
            }));
        }
        self.pick();
        Ok(())
    }

    async fn open_async<F: BlockFile>(
        &mut self,
        store: &mut Store<F>,
        sources: &[Source<'a>],
        from: &[u8],
        end: Option<&[u8]>,
        filter: bool,
        timed: bool,
    ) -> Result<(), Error> {
        self.close(store);
        self.end.clear();
        self.bounded = end.is_some();
        if let Some(e) = end {
            self.end.extend_from_slice(e);
        }
        self.open_ns.clear();
        for s in sources {
            let t = timed.then(std::time::Instant::now);
            if filter && end.is_some() {
                let scratch = &mut self.scratch;
                let held = match *s {
                    Source::Branch(b) => b.range.may_hold(from, end, scratch),
                };
                if !held {
                    self.skipped = self.skipped.saturating_add(1);
                    self.open_ns.push(0);
                    continue;
                }
            }
            self.opened = self.opened.saturating_add(1);
            let head = match *s {
                Source::Branch(b) => Head::Branch(RunCursor::new(store)?, b),
            };
            // The guard owns the head before any await, including the first demand miss.
            self.heads.push(head);
            let head = self.heads.last_mut().ok_or(Error::InvalidArgument {
                what: "a scan head absent after it was opened",
            })?;
            match head {
                Head::Branch(cursor, branch) => cursor.seek_async(branch, store, from).await?,
            }
            self.open_ns.push(t.map_or(0, |t| {
                u64::try_from(t.elapsed().as_nanos()).unwrap_or(u64::MAX)
            }));
        }
        self.pick();
        Ok(())
    }

    /// The merge, its heads given back to `store` first, with its buffers kept for sources of
    /// another borrow and its counts reset: a shard keeps one merge between scans.
    pub fn recycle<'b, F: BlockFile>(mut self, store: &mut Store<F>) -> ScanMerge<'b> {
        self.close(store);
        ScanMerge {
            heads: crate::util::reuse(self.heads),
            end: self.end,
            bounded: false,
            current: None,
            past: self.past,
            scratch: self.scratch,
            skipped: 0,
            opened: 0,
            open_ns: self.open_ns,
        }
    }

    /// The nanoseconds each source of the last [`Self::open`] took, in the sources' order.
    pub fn open_ns(&self) -> &[u64] {
        &self.open_ns
    }

    /// Sources passed over by their range filters, and sources opened, since the merge was made.
    pub fn counts(&self) -> (u64, u64) {
        (self.skipped, self.opened)
    }

    /// Gives every head's pages and span back to `store`.
    pub fn close<F: BlockFile>(&mut self, store: &mut Store<F>) {
        for h in self.heads.drain(..) {
            h.give_back(store);
        }
        self.current = None;
    }

    /// The head at the least key before the end, the newest of those tied.
    fn pick(&mut self) {
        let mut best: Option<(usize, &[u8])> = None;
        for (i, h) in self.heads.iter().enumerate() {
            let Some(k) = h.key() else { continue };
            // Strictly less only: an equal key keeps the earlier, newer head.
            if best.is_none_or(|(_, b)| k < b) {
                best = Some((i, k));
            }
        }
        self.current = best
            .filter(|&(_, k)| !self.bounded || k < self.end.as_slice())
            .map(|(i, _)| i);
    }

    /// The current entry: key, operation, value; none past the end.
    pub fn entry(&self) -> Option<(&[u8], Op, &[u8])> {
        let h = self.heads.get(self.current?)?;
        Some((h.key()?, h.op(), h.value()))
    }

    /// With one source, its entries from the current one pushed to `out` in a run, a deletion
    /// passed over, until `rows` rows are pushed, an entry at or past `before` (the least key
    /// the scan's memtables hold next, which the caller merges) or the merge's end, or the
    /// source's end: the number pushed, or none, nothing moved, when the merge holds more than
    /// one source. Nothing is decided by comparing sources, so the run leaves the merge where the
    /// caller's step would have, the entry it stopped at current. A 10M seek spent 43 ns a row
    /// after its placing, most of it in the shard's merge of one source with memtables holding
    /// nothing before the segment's end (research/61 §3).
    pub fn push_run<F: BlockFile>(
        &mut self,
        store: &mut Store<F>,
        rows: usize,
        before: Option<&[u8]>,
        out: &mut crate::rows::Rows,
    ) -> Result<Option<usize>, Error> {
        let mut pushed = 0usize;
        let [head] = self.heads.as_mut_slice() else {
            return Ok(None);
        };
        let end = if self.bounded {
            Some(self.end.as_slice())
        } else {
            None
        };
        // At most a step an entry the source holds: each step moves the cursor on or ends it.
        while pushed < rows {
            let Some(key) = head.key() else {
                break;
            };
            if end.is_some_and(|e| key >= e) || before.is_some_and(|b| key >= b) {
                break;
            }
            if head.op() == Op::Put {
                out.push(key, head.value());
                pushed = pushed.saturating_add(1);
            }
            head.next(store)?;
        }
        self.pick();
        Ok(Some(pushed))
    }

    /// Moves every head holding the current key past it.
    pub fn next<F: BlockFile>(&mut self, store: &mut Store<F>) -> Result<(), Error> {
        if self.current == Some(0)
            && let [head] = self.heads.as_mut_slice()
        {
            // The only head needs no copied key to decide which sources move.
            head.next(store)?;
            self.pick();
            return Ok(());
        }
        let Some(key) = self
            .current
            .and_then(|i| self.heads.get(i))
            .and_then(Head::key)
        else {
            return Ok(());
        };
        self.past.clear();
        self.past.extend_from_slice(key);
        for h in &mut self.heads {
            if h.key() == Some(self.past.as_slice()) {
                h.next(store)?;
            }
        }
        self.pick();
        Ok(())
    }

    async fn next_async<F: BlockFile>(&mut self, store: &mut Store<F>) -> Result<(), Error> {
        if self.current == Some(0)
            && let [head] = self.heads.as_mut_slice()
        {
            // The only head needs no copied key to decide which sources move.
            head.next_async(store).await?;
            self.pick();
            return Ok(());
        }
        let Some(key) = self
            .current
            .and_then(|i| self.heads.get(i))
            .and_then(Head::key)
        else {
            return Ok(());
        };
        self.past.clear();
        self.past.extend_from_slice(key);
        for h in &mut self.heads {
            if h.key() == Some(self.past.as_slice()) {
                h.next_async(store).await?;
            }
        }
        self.pick();
        Ok(())
    }
}

/// A scan page's borrowed merge, returning its heads and reusable buffers on every
/// exit. Source references stay borrowed, so no topology snapshot or replay is needed.
pub(crate) struct BorrowedScanMerge<'s, 'a, F: BlockFile> {
    merge: Option<ScanMerge<'a>>,
    store: &'s mut Store<F>,
    returned: &'s mut ScanMerge<'static>,
}

impl<'s, 'a, F: BlockFile> BorrowedScanMerge<'s, 'a, F> {
    pub(crate) fn new(returned: &'s mut ScanMerge<'static>, store: &'s mut Store<F>) -> Self {
        let merge = std::mem::take(returned).recycle(store);
        Self {
            merge: Some(merge),
            store,
            returned,
        }
    }

    pub(crate) async fn open(
        &mut self,
        sources: &[Source<'a>],
        from: &[u8],
        end: Option<&[u8]>,
        filter: bool,
        timed: bool,
    ) -> Result<(), Error> {
        if !self.store.has_issuer() {
            return Err(Error::InvalidArgument {
                what: "an async scan without an attached issuer",
            });
        }
        let merge = self.merge.as_mut().ok_or(Error::InvalidArgument {
            what: "an async scan after its merge was returned",
        })?;
        merge
            .open_async(self.store, sources, from, end, filter, timed)
            .await
    }

    pub(crate) fn entry(&self) -> Option<(&[u8], Op, &[u8])> {
        self.merge.as_ref().and_then(ScanMerge::entry)
    }

    pub(crate) fn open_ns(&self) -> &[u64] {
        self.merge.as_ref().map_or(&[], ScanMerge::open_ns)
    }

    pub(crate) fn counts(&self) -> (u64, u64) {
        self.merge.as_ref().map_or((0, 0), ScanMerge::counts)
    }

    pub(crate) fn close(&mut self) {
        if let Some(merge) = self.merge.as_mut() {
            merge.close(self.store);
        }
    }

    pub(crate) async fn next(&mut self) -> Result<(), Error> {
        let merge = self.merge.as_mut().ok_or(Error::InvalidArgument {
            what: "an async scan after its merge was returned",
        })?;
        merge.next_async(self.store).await
    }
}

impl<F: BlockFile> Drop for BorrowedScanMerge<'_, '_, F> {
    fn drop(&mut self) {
        if let Some(merge) = self.merge.take() {
            *self.returned = merge.recycle(self.store);
        }
    }
}
