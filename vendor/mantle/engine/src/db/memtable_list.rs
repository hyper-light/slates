//! The immutable memtables of one column family: RocksDB's `db/memtable_list.{h,cc}`.
//!
//! RocksDB keeps the list in reference-counted `MemTableListVersion`s, copied on write, so that
//! a reader's `SuperVersion` holds the memtables it read while flushes install new versions.
//! Here the list owns its memtables outright: `memlist` (not yet flushed, newest first) and
//! `history` (flushed, kept for conflict checks, newest first), and [`MemTableList::current`]
//! lends a [`MemTableListVersion`] view of both for as long as the list is not changed. Every
//! change takes `&mut self`; a memtable leaving the list is returned to the caller by value
//! (RocksDB's `to_delete`), whose dropping frees it. Readers that must keep reading while the
//! list changes need a lifetime longer than a borrow — RocksDB's `SuperVersion` — and that is
//! the read path's design (P7–P8), which this list does not presume.
//!
//! Flush results are installed without the MANIFEST: [`MemTableList::try_install_memtable_flush_results`]
//! and [`install_memtable_atomic_flush_results`] do what RocksDB does once `LogAndApply`
//! succeeds (`RemoveMemTablesOrRestoreFlags` [R db/memtable_list.cc:838-920]); writing the
//! version edit first is P6–P7's, which call these after it is durable.
//!
//! The number of memtables not yet flushed is bounded by the write path, which stalls writes at
//! `max_write_buffer_number` (P7, P14); the history by `max_write_buffer_size_to_maintain`.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

use crate::db::dbformat::{LookupKey, MAX_SEQUENCE_NUMBER, SequenceNumber};
use crate::db::memtable::{Hit, MemTable, MergeContext};
use crate::error::Error;

/// A view of a list's memtables, borrowed from the list: `MemTableListVersion`
/// [R db/memtable_list.h].
#[derive(Debug, Clone, Copy)]
pub struct MemTableListVersion<'a> {
    memlist: &'a VecDeque<MemTable>,
    history: &'a VecDeque<MemTable>,
}

/// `GetFromList` [R db/memtable_list.cc:230-268]: the newest memtable holding a final answer.
fn get_from_list(
    list: &VecDeque<MemTable>,
    key: &LookupKey,
    merge_context: &mut MergeContext,
    max_covering_tombstone_seq: &mut SequenceNumber,
) -> Result<Option<Hit>, Error> {
    for mem in list {
        if let Some(hit) = mem.get(key, merge_context, max_covering_tombstone_seq)? {
            return Ok(Some(hit));
        }
    }
    Ok(None)
}

impl<'a> MemTableListVersion<'a> {
    /// `Get` [R db/memtable_list.cc:178-189]: the unflushed memtables, newest first, with the
    /// outcomes of [`MemTable::get`].
    pub fn get(
        &self,
        key: &LookupKey,
        merge_context: &mut MergeContext,
        max_covering_tombstone_seq: &mut SequenceNumber,
    ) -> Result<Option<Hit>, Error> {
        get_from_list(self.memlist, key, merge_context, max_covering_tombstone_seq)
    }

    /// `GetFromHistory` [R db/memtable_list.cc:219-228]: the flushed memtables kept in history.
    pub fn get_from_history(
        &self,
        key: &LookupKey,
        merge_context: &mut MergeContext,
        max_covering_tombstone_seq: &mut SequenceNumber,
    ) -> Result<Option<Hit>, Error> {
        get_from_list(self.history, key, merge_context, max_covering_tombstone_seq)
    }

    /// `GetTotalNumEntries`.
    pub fn total_num_entries(&self) -> u64 {
        self.memlist
            .iter()
            .fold(0u64, |n, m| n.saturating_add(m.num_entries()))
    }

    /// `GetTotalNumDeletes`.
    pub fn total_num_deletes(&self) -> u64 {
        self.memlist
            .iter()
            .fold(0u64, |n, m| n.saturating_add(m.num_deletes()))
    }

    /// `GetEarliestSequenceNumber` [R db/memtable_list.cc:372-381].
    pub fn earliest_sequence_number(&self, include_history: bool) -> SequenceNumber {
        let oldest = if include_history && !self.history.is_empty() {
            self.history.back()
        } else {
            self.memlist.back()
        };
        oldest.map_or(MAX_SEQUENCE_NUMBER, MemTable::earliest_sequence_number)
    }

    /// `GetFirstSequenceNumber` [R db/memtable_list.cc:383-391].
    pub fn first_sequence_number(&self) -> SequenceNumber {
        self.memlist
            .iter()
            .map(MemTable::first_sequence_number)
            .fold(MAX_SEQUENCE_NUMBER, SequenceNumber::min)
    }

    /// `NumNotFlushed`.
    pub fn num_not_flushed(&self) -> usize {
        self.memlist.len()
    }

    /// `NumFlushed`.
    pub fn num_flushed(&self) -> usize {
        self.history.len()
    }

    /// The unflushed memtables, newest first.
    pub fn memtables(&self) -> impl Iterator<Item = &'a MemTable> {
        self.memlist.iter()
    }

    /// The flushed memtables kept in history, newest first.
    pub fn history(&self) -> impl Iterator<Item = &'a MemTable> {
        self.history.iter()
    }
}

/// `MemTableList` [R db/memtable_list.h]: one column family's immutable memtables.
#[derive(Debug)]
pub struct MemTableList {
    memlist: VecDeque<MemTable>,
    history: VecDeque<MemTable>,
    /// `max_write_buffer_size_to_maintain`: history is trimmed to keep the memtables' bytes
    /// below it; 0 keeps none.
    max_write_buffer_size_to_maintain: i64,
    min_write_buffer_number_to_merge: usize,
    num_flush_not_started: usize,
    flush_requested: bool,
    /// `imm_flush_needed`: RocksDB's write path reads it without the DB mutex, so it stays an
    /// atomic. Stored with release and loaded with acquire, RocksDB's orderings: a thread that
    /// sees `true` also sees the list change that set it.
    imm_flush_needed: AtomicBool,
    /// `current_memory_usage_`: the memtables' approximate bytes.
    current_memory_usage: usize,
}

impl MemTableList {
    pub fn new(
        min_write_buffer_number_to_merge: usize,
        max_write_buffer_size_to_maintain: i64,
    ) -> Self {
        Self {
            memlist: VecDeque::new(),
            history: VecDeque::new(),
            max_write_buffer_size_to_maintain,
            min_write_buffer_number_to_merge,
            num_flush_not_started: 0,
            flush_requested: false,
            imm_flush_needed: AtomicBool::new(false),
            current_memory_usage: 0,
        }
    }

    /// `current()`: a view of the memtables.
    pub fn current(&self) -> MemTableListVersion<'_> {
        MemTableListVersion {
            memlist: &self.memlist,
            history: &self.history,
        }
    }

    pub fn num_not_flushed(&self) -> usize {
        self.memlist.len()
    }

    pub fn num_flushed(&self) -> usize {
        self.history.len()
    }

    /// `imm_flush_needed`: a memtable is waiting for a flush to start.
    pub fn imm_flush_needed(&self) -> bool {
        self.imm_flush_needed.load(AtomicOrdering::Acquire)
    }

    fn set_imm_flush_needed(&self, needed: bool) {
        self.imm_flush_needed.store(needed, AtomicOrdering::Release);
    }

    /// `IsFlushPending` [R db/memtable_list.cc:477-484].
    pub fn is_flush_pending(&self) -> bool {
        (self.flush_requested && self.num_flush_not_started > 0)
            || self.num_flush_not_started >= self.min_write_buffer_number_to_merge
    }

    /// `IsFlushPendingOrRunning` [R db/memtable_list.cc:486-493].
    pub fn is_flush_pending_or_running(&self) -> bool {
        self.memlist.len() > self.num_flush_not_started || self.is_flush_pending()
    }

    /// `FlushRequested`.
    pub fn flush_requested(&mut self) {
        self.flush_requested = true;
        if self.num_flush_not_started > 0 {
            self.set_imm_flush_needed(true);
        }
    }

    /// `HasFlushRequested`.
    pub fn has_flush_requested(&self) -> bool {
        self.flush_requested
    }

    /// `GetEarliestMemTableID`: `u64::MAX` when empty.
    pub fn earliest_memtable_id(&self) -> u64 {
        self.memlist.back().map_or(u64::MAX, MemTable::id)
    }

    /// `GetLatestMemTableID(false)`: 0 when empty.
    pub fn latest_memtable_id(&self) -> u64 {
        self.memlist.front().map_or(0, MemTable::id)
    }

    /// An unflushed memtable by id.
    pub fn memtable(&self, id: u64) -> Option<&MemTable> {
        self.memlist.iter().find(|m| m.id() == id)
    }

    /// `ApproximateMemoryUsage`.
    pub fn approximate_memory_usage(&self) -> usize {
        self.current_memory_usage
    }

    /// `HasHistory`.
    pub fn has_history(&self) -> bool {
        !self.history.is_empty()
    }

    /// `Add` [R db/memtable_list.cc:751-767]: `m` becomes the newest unflushed memtable.
    /// Returns the memtables trimmed out of history. A memtable with an id below the newest's is
    /// refused: ids order the list.
    pub fn add(&mut self, m: MemTable) -> Result<Vec<MemTable>, Error> {
        if self.memlist.front().is_some_and(|f| m.id() < f.id()) {
            return Err(Error::InvalidArgument {
                what: "memtable id below the newest in the list",
            });
        }
        self.current_memory_usage = self
            .current_memory_usage
            .saturating_add(m.approximate_memory_usage());
        self.memlist.push_front(m);
        let to_delete = self.trim_history(0);
        self.num_flush_not_started = self.num_flush_not_started.saturating_add(1);
        if self.num_flush_not_started == 1 {
            self.set_imm_flush_needed(true);
        }
        Ok(to_delete)
    }

    /// `MemoryAllocatedBytesExcludingLast` [R db/memtable_list.cc:419-431]: the memtables'
    /// allocated bytes as if the oldest in history were dropped.
    pub fn memory_allocated_bytes_excluding_last(&self) -> usize {
        let total = self
            .memlist
            .iter()
            .chain(self.history.iter())
            .fold(0usize, |n, m| n.saturating_add(m.memory_allocated_bytes()));
        let last = self
            .history
            .back()
            .map_or(0, MemTable::memory_allocated_bytes);
        total.saturating_sub(last)
    }

    /// `HistoryShouldBeTrimmed` [R db/memtable_list.cc:433-448].
    fn history_should_be_trimmed(&self, usage: usize) -> bool {
        let Ok(limit) = usize::try_from(self.max_write_buffer_size_to_maintain) else {
            return false;
        };
        limit > 0
            && !self.history.is_empty()
            && self
                .memory_allocated_bytes_excluding_last()
                .saturating_add(usage)
                >= limit
    }

    /// `TrimHistory` [R db/memtable_list.cc:450-461, :769-785]: drops the oldest flushed
    /// memtables while history keeps too many bytes, returning them.
    pub fn trim_history(&mut self, usage: usize) -> Vec<MemTable> {
        let mut to_delete = Vec::new();
        while self.history_should_be_trimmed(usage) {
            let Some(m) = self.history.pop_back() else {
                break;
            };
            self.release(&m);
            to_delete.push(m);
        }
        to_delete
    }

    /// Takes a departing memtable's bytes off the list's count.
    fn release(&mut self, m: &MemTable) {
        self.current_memory_usage = self
            .current_memory_usage
            .saturating_sub(m.approximate_memory_usage());
    }

    /// `PickMemtablesToFlush` [R db/memtable_list.cc:495-548]: marks the oldest consecutive
    /// memtables not yet flushing, with ids at most `max_memtable_id`, as flushing, and returns
    /// their ids, oldest first.
    pub fn pick_memtables_to_flush(&mut self, max_memtable_id: u64) -> Vec<u64> {
        let mut picked = Vec::new();
        let mut not_started = self.num_flush_not_started;
        for m in self.memlist.iter_mut().rev() {
            if m.id() > max_memtable_id {
                break;
            }
            if !m.flush_in_progress {
                not_started = not_started.saturating_sub(1);
                m.flush_in_progress = true;
                picked.push(m.id());
            } else if !picked.is_empty() {
                // Never pick around a memtable already flushing.
                break;
            }
        }
        self.num_flush_not_started = not_started;
        if not_started == 0 {
            self.set_imm_flush_needed(false);
        }
        // No atomic-flush sequence numbers in the port's single-column-family memtables
        // (`atomic_flush_seqno_` stays unset), so the request is always complete here.
        self.flush_requested = false;
        picked
    }

    /// `RollbackMemtableFlush` [R db/memtable_list.cc:550-605]: returns the memtables `ids`
    /// (and, with `rollback_succeeding_memtables`, the completed ones after them) to waiting.
    pub fn rollback_memtable_flush(&mut self, ids: &[u64], rollback_succeeding_memtables: bool) {
        let mut restored = 0usize;
        if let (true, Some(&first)) = (rollback_succeeding_memtables, ids.first()) {
            let mut after = false;
            for m in self.memlist.iter_mut().rev() {
                if !after {
                    after = m.id() == first;
                    continue;
                }
                if !m.flush_completed {
                    break;
                }
                m.flush_in_progress = false;
                m.flush_completed = false;
                m.file_number = 0;
                restored = restored.saturating_add(1);
            }
        }
        for m in self.memlist.iter_mut() {
            if ids.contains(&m.id()) && m.flush_in_progress {
                m.flush_in_progress = false;
                m.flush_completed = false;
                m.file_number = 0;
                restored = restored.saturating_add(1);
            }
        }
        self.num_flush_not_started = self.num_flush_not_started.saturating_add(restored);
        if !ids.is_empty() {
            self.set_imm_flush_needed(true);
        }
    }

    /// Marks the memtables `ids` flushed into table `file_number`.
    fn mark_flush_completed(&mut self, ids: &[u64], file_number: u64) {
        for m in self.memlist.iter_mut() {
            if ids.contains(&m.id()) {
                m.flush_completed = true;
                m.file_number = file_number;
            }
        }
    }

    /// `Remove` [R db/memtable_list.cc:402-417]: the oldest unflushed memtable leaves, into
    /// history when history is kept, and whatever leaves the list is returned.
    fn remove_oldest(&mut self, to_delete: &mut Vec<MemTable>) {
        let Some(m) = self.memlist.pop_back() else {
            return;
        };
        if self.max_write_buffer_size_to_maintain > 0 {
            self.history.push_front(m);
            to_delete.extend(self.trim_history(0));
        } else {
            self.release(&m);
            to_delete.push(m);
        }
    }

    /// `TryInstallMemtableFlushResults` [R db/memtable_list.cc:607-749], after its MANIFEST
    /// write: marks `ids` flushed into `file_number`, then removes, oldest first, every
    /// memtable whose flush has completed, stopping at the first that has not (results are
    /// committed in creation order). Returns what left the list.
    pub fn try_install_memtable_flush_results(
        &mut self,
        ids: &[u64],
        file_number: u64,
    ) -> Vec<MemTable> {
        self.mark_flush_completed(ids, file_number);
        let mut to_delete = Vec::new();
        while self.memlist.back().is_some_and(|m| m.flush_completed) {
            self.remove_oldest(&mut to_delete);
        }
        to_delete
    }
}

/// `InstallMemtableAtomicFlushResults` [R db/memtable_list.cc:942-1112], after its MANIFEST
/// write: for each list, marks its picked memtables flushed into its file and removes them.
/// `mems` and `file_numbers` are per list; their lengths must match `lists`'.
pub fn install_memtable_atomic_flush_results(
    lists: &mut [&mut MemTableList],
    mems: &[Vec<u64>],
    file_numbers: &[u64],
) -> Result<Vec<MemTable>, Error> {
    if mems.len() != lists.len() || file_numbers.len() != lists.len() {
        return Err(Error::InvalidArgument {
            what: "atomic flush results for a different number of column families",
        });
    }
    let mut to_delete = Vec::new();
    for ((list, ids), &file_number) in lists.iter_mut().zip(mems).zip(file_numbers) {
        list.mark_flush_completed(ids, file_number);
        for _ in ids {
            list.remove_oldest(&mut to_delete);
        }
    }
    Ok(to_delete)
}
