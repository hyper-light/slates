//! A guest's stale cache write-back is refused, never applied over another path's newer write (§4.6 "Cache posture";
//! GAPS, "the direct virtio-fs route shares `serve_write`"): the dirty-page write-back rule for a FUSE transport that
//! delivers no invalidation, a virtio-fs device's guest.
//!
//! **What it answers.** A guest kernel caches a file's pages and writes a dirty page back whole, marked
//! `FUSE_WRITE_CACHE`: the program's stores with the rest of the page as the guest fetched it. A virtio-fs device can
//! deliver no invalidation, so the guest is promised no cache lifetime and drops a file's pages when a revalidation shows
//! a change (`FUSE_AUTO_INVAL_DATA`, AUD-29-79). Linux launders a dirty page first, writing it back, so the page that
//! reaches the device carries bytes another path changed since the guest fetched it. Applied whole, it reverted that
//! write: 4,095 of a page's 4,095 other-path bytes in `a_guests_stale_cache_write_back_never_reverts_another_paths_write`
//! (`crates/bridge-virtiofs/tests/device.rs`).
//!
//! **The rule** (every transport's since A-248: [`crate::coherence::judge_writeback`]): a write-back holding stale bytes
//! that differ from the file's bytes now is refused `EIO` whole, and the other path's change survives. Nothing is inferred
//! (CLAUDE.md, banned item 11): the write-back is applied whole or refused whole. Where the states cannot tell (a journal
//! that no longer reaches back, a file lost for room), it is refused too. The guest learns of a refusal at its next
//! `fsync` or `msync` of the file.
//!
//! **The states** are the host mount's ([`PageStates`]), kept per device and fed here, not by invalidation rounds:
//! - a writable `OPEN` or `CREATE` starts its file's states at the journal's head, and the first one of a file has its
//!   reply cleared of `FOPEN_KEEP_CACHE`, so the guest drops what it cached before;
//! - an answered `READ` of a file open for writing makes the bytes it returned fresh as of the head before it;
//! - the guest's own requests' records refresh what they wrote ([`OwnRecords`]);
//! - every other record is another path's change, which makes the bytes it touched stale, consumed before each request
//!   from the point the states were last fed;
//! - the last writable `RELEASE` of a file drops its states.
//!
//! **Bounds.** States exist only for files the guest holds open for writing, and their ranges are charged to the shard's
//! metadata ledger a page at a time (`Bridge::admit_states`, A-155); a refused charge loses the file, whose write-backs
//! are then refused.

use crate::abi::Opcode;
use crate::coherence::{
  MOST_ADDED_BY_AN_UPDATE, OwnRecords, PageStates, RANGE_BYTES, WritableOpens, WritebackVerdict,
  judge_writeback,
};
use crate::request::{ReadIn, Request, WriteIn};
use slates_bridge_core::dax_backing::read_whole;
use slates_bridge_core::{Bridge, ObjectId, OpContext};

/// The dirty-page write-back rule for one guest (see the module doc).
#[derive(Default)]
pub struct GuestWriteback {
  own: OwnRecords,
  writable: WritableOpens,
  states: PageStates,
  /// The journal point the states are fed through; set at the guest's first request.
  fed_through: Option<u64>,
  /// The bytes of metadata the states were charged ([`Bridge::admit_states`]), the guest's until its device ends.
  charged: u64,
  /// Write-backs refused because they held stale bytes another path had changed that differ from the file's: the rule's
  /// non-vacuity count.
  refused_stale: u64,
  /// Write-backs refused because the states could not tell. Zero is the healthy reading.
  refused_unknown: u64,
}

impl GuestWriteback {
  /// A guest that has served nothing.
  pub fn new() -> GuestWriteback {
    GuestWriteback::default()
  }

  /// Serves one request of the guest: a cache write-back by the rule, every other request by `dispatch`, and the
  /// states fed with what the request shows. The reply's length in `reply`.
  pub fn serve(
    &mut self,
    request: &[u8],
    bridge: &mut dyn Bridge,
    cx: &OpContext,
    reply: &mut [u8],
  ) -> usize {
    let before = bridge.seen(cx).journal_seq;
    let started_at = *self.fed_through.get_or_insert(before);
    self.feed(bridge, cx, started_at);
    let len = match crate::bridge::cache_writeback(request) {
      Some(nodeid) => self.judge(request, nodeid, before, bridge, cx, reply),
      None => crate::bridge::dispatch(request, bridge, cx, reply),
    };
    self.note(request, len, before, bridge, cx, reply);
    self.note_own(before, bridge, cx);
    len
  }

  /// Write-backs refused for stale bytes, then for states that could not tell, since this was last read; zero after.
  pub fn take_refused(&mut self) -> (u64, u64) {
    (
      std::mem::take(&mut self.refused_stale),
      std::mem::take(&mut self.refused_unknown),
    )
  }

  /// The metadata bytes the states were charged, for the guest's device to give back at its end.
  pub fn charged(&self) -> u64 {
    self.charged
  }

  /// Consumes every record since the states were last fed: another path's change makes the bytes it touched stale, the
  /// guest's own refreshes them. A journal that no longer reaches back loses every tracked file.
  fn feed(&mut self, bridge: &mut dyn Bridge, cx: &OpContext, since: u64) {
    let head = bridge.seen(cx).journal_seq;
    if head <= since {
      return;
    }
    let mut changes = Vec::new();
    match bridge.data_changes(cx, since, &mut changes) {
      Ok(true) => {
        self.make_room(
          bridge,
          cx,
          changes.len().saturating_mul(MOST_ADDED_BY_AN_UPDATE),
        );
        for change in changes {
          let own = self.own.took(change.seq);
          self
            .states
            .changed(change.ino, change.range, change.seq, own);
        }
        self.states.consumed(head);
      }
      Ok(false) | Err(_) => self.states.lose_all(),
    }
    self.fed_through = Some(head);
  }

  /// Judges a cache write-back of `nodeid` against the states fed through the head by the shared rule
  /// ([`judge_writeback`]): applied whole when none of its stale bytes differs from the file's, refused `EIO` whole when
  /// one does or when it cannot be told.
  fn judge(
    &mut self,
    request: &[u8],
    nodeid: u64,
    head: u64,
    bridge: &mut dyn Bridge,
    cx: &OpContext,
    reply: &mut [u8],
  ) -> usize {
    let write = Request::parse(request)
      .ok()
      .and_then(|parsed| WriteIn::parse(Opcode::Write.to_wire(), parsed.body).ok());
    let verdict = write.map_or(WritebackVerdict::Unknown, |write| {
      let end = write
        .offset
        .saturating_add(u64::try_from(write.data.len()).unwrap_or(u64::MAX));
      let stale = self
        .states
        .stale_after(nodeid, write.offset..end, head, &[]);
      let object = ObjectId {
        inode: nodeid,
        generation: 0,
      };
      judge_writeback(
        stale.as_deref(),
        write.offset,
        write.data,
        |at, len, out| read_whole(bridge, cx, object, (at, len), out).is_ok(),
      )
    });
    match verdict {
      WritebackVerdict::Apply | WritebackVerdict::Identical => {
        crate::bridge::dispatch(request, bridge, cx, reply)
      }
      WritebackVerdict::Conflict => {
        self.refused_stale = self.refused_stale.saturating_add(1);
        crate::bridge::refuse_with_eio(request, reply)
      }
      WritebackVerdict::Unknown => {
        self.refused_unknown = self.refused_unknown.saturating_add(1);
        crate::bridge::refuse_with_eio(request, reply)
      }
    }
  }

  /// What an answered request tells the states: a writable open starts its file's states (and the first clears the
  /// guest's cache of it), a read makes the bytes it returned fresh, a release forgets its handle and, with the file's
  /// last writable one, its states.
  fn note(
    &mut self,
    request: &[u8],
    len: usize,
    before: u64,
    bridge: &mut dyn Bridge,
    cx: &OpContext,
    reply: &mut [u8],
  ) {
    let Ok(parsed) = Request::parse(request) else {
      return;
    };
    let ino = parsed.header.nodeid;
    let answered = reply
      .get(..len)
      .is_some_and(|reply| crate::bridge::reply_error(reply) == 0);
    if !answered {
      return;
    }
    match parsed.opcode {
      Some(Opcode::Open | Opcode::Create) => {
        if crate::bridge::opens_for_writing(parsed.opcode, parsed.body) != Some(true) {
          return;
        }
        let Some(answer) = reply.get_mut(..len) else {
          return;
        };
        let Some(fh) = crate::bridge::granted_handle(parsed.opcode, answer) else {
          return;
        };
        let file = crate::bridge::created_node(parsed.opcode, answer).unwrap_or(ino);
        if self.writable.opened(fh, file) {
          crate::bridge::clear_keep_cache(parsed.opcode, answer);
          self.make_room(bridge, cx, 1);
          self.states.opened(file, bridge.seen(cx).journal_seq);
        }
      }
      Some(Opcode::Read) => {
        if !self.writable.holds(ino) {
          return;
        }
        if let Ok(read) = ReadIn::parse(Opcode::Read.to_wire(), parsed.body) {
          let end = read.offset.saturating_add(u64::from(read.size));
          self.make_room(bridge, cx, MOST_ADDED_BY_AN_UPDATE);
          self.states.fetched(ino, read.offset..end, before);
        }
      }
      Some(Opcode::Release) => {
        let handle = parsed
          .body
          .get(..size_of::<u64>())
          .and_then(|bytes| bytes.try_into().ok())
          .map(u64::from_le_bytes);
        if let Some(file) = handle.and_then(|fh| self.writable.released(fh, ino)) {
          self.states.dropped(file);
        }
      }
      _ => {}
    }
  }

  /// Notes the records the request made as the guest's own, and refreshes what they wrote.
  fn note_own(&mut self, before: u64, bridge: &mut dyn Bridge, cx: &OpContext) {
    let after = bridge.seen(cx).journal_seq;
    let capacity = bridge.journal_capacity(cx);
    self.own.record(before, after, capacity);
    if after <= before {
      return;
    }
    let mut changes = Vec::new();
    match bridge.data_changes(cx, before, &mut changes) {
      Ok(true) => {
        self.make_room(
          bridge,
          cx,
          changes.len().saturating_mul(MOST_ADDED_BY_AN_UPDATE),
        );
        for change in changes {
          self
            .states
            .changed(change.ino, change.range, change.seq, true);
        }
      }
      Ok(false) | Err(_) => self.states.lose_all(),
    }
    self.fed_through = Some(after);
  }

  /// Grows the states' room to hold `more` ranges beyond those they hold, a page of ranges at a time from the shard's
  /// metadata ledger (the host mount's rule, A-155). A refusal leaves the room as it is.
  fn make_room(&mut self, bridge: &mut dyn Bridge, cx: &OpContext, more: usize) {
    let short = self
      .states
      .held()
      .saturating_add(more)
      .saturating_sub(self.states.room());
    if short == 0 {
      return;
    }
    let page = usize::try_from(slates_machine::facts::Facts::page_bytes()).unwrap_or(1);
    let per_page = page.checked_div(RANGE_BYTES).unwrap_or(1).max(1);
    let ranges = short.div_ceil(per_page).saturating_mul(per_page);
    let bytes = u64::try_from(ranges.saturating_mul(RANGE_BYTES)).unwrap_or(u64::MAX);
    if bridge.admit_states(cx, bytes) {
      self.states.grow_room(ranges);
      self.charged = self.charged.saturating_add(bytes);
    }
  }
}
