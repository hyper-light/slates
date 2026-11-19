//! An answer, written out a slice at a time (A-640 parts 2 and 3): the bytes [`super::serve`] encoded, then, for a
//! fetch that carries a pack, the pack its source writes, framed a packet at a time as the protocol frames it
//! (`Framing`): protocol v2's packfile section on band 1, then its flush ("The packfile section ... is multiplexed
//! using side-band-64k" [gitprotocol-v2]); protocol v0's side-band-64k or side-band on band 1, then a flush
//! (`create_pack_file`); or, under v0 without side-band, the pack's bytes as they are.
//!
//! A step writes at most the room its caller gives it, and any bytes past that wait for the next step, so the caller's
//! room bounds what a step writes. A step calls the pack's source at most once, asking for what the packet being
//! filled still has room for, so one call of the source's bounds a step's work (one turn of A-639's stream, §4.3's
//! step budget). A packet is framed once it is full, to the most data its band carries, the format's own limit
//! (`sideband::MAX_BAND_DATA` for side-band-64k, 995 bytes for side-band's 1000-byte packets), or once the pack has
//! ended; data a call wrote that leaves it short waits in it for the next step's call. So a pack goes out in the fewest
//! packets, the same packets whatever room the caller gives and however the source divides its work, and a call that
//! worked and wrote nothing yet (A-639's stream counting the pack before its header) only ends its step. A source that
//! refuses mid-pack ends the response with git's band-3 words for a pack that could not be finished, after the data it
//! wrote before refusing, as git sends what `pack-objects` wrote before it failed: nothing follows a band-3 packet, and
//! the client stops with them. Without side-band there is no band to say it on, so the pack stops short, as git's
//! does, and the client finds it cut.

use super::{ABORT, PackRefused, PackSource, Refusal, Refused, Served};
use crate::negotiation::Counters;
use crate::out::Out;
use crate::pktline::{self, Packet};
use crate::sideband::{self, Band, MAX_BAND_DATA};
use crate::v2::fetch::{Event, Response};
use crate::{Admission, to_u64};

/// Format: what an error line begins with outside a multiplexed response (`packet_writer_error`).
const ERR: &[u8] = b"ERR ";

/// Format: the flush packet, four zero digits [gitprotocol-common: "0000 (flush-pkt)"].
const FLUSH: &[u8] = b"0000";

/// Format: the length of the band-3 packet of git's words for a pack that could not be finished: the words, after the
/// packet's length and band byte (a band packet's most less the most data it carries).
const ABORT_PACKET: usize =
  ABORT.len().saturating_add(pktline::MAX_LEN.saturating_sub(MAX_BAND_DATA));

/// Format: the most one framing holds: a packet at the format's longest, then the flush after the pack's last data or,
/// after the data a source wrote before it refused, git's band-3 words, whichever is longer.
const FRAMED_MOST: usize = pktline::MAX_LEN.saturating_add(longer(FLUSH.len(), ABORT_PACKET));

/// The longer of two lengths, for the bounds above.
const fn longer(first: usize, second: usize) -> usize {
  if first > second { first } else { second }
}

/// How an answer frames its pack's data.
#[derive(Clone, Copy, Debug)]
pub(super) enum Framing {
  /// Protocol v2's packfile section, through its grammar: band 1, then the flush.
  V2(Response),
  /// Protocol v0's side-band-64k or side-band: band 1 in packets of at most `data` bytes of the pack, then a flush.
  Band {
    /// The most bytes of the pack one packet carries.
    data: usize,
  },
  /// Protocol v0 without side-band: the pack's bytes as they are, and nothing after them.
  Raw,
}

impl Framing {
  /// The most bytes of the pack one packet carries.
  fn data(self) -> usize {
    match self {
      Framing::V2(_) | Framing::Raw => MAX_BAND_DATA,
      Framing::Band { data } => data,
    }
  }
}

/// What a step left to write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Written {
  /// More of the answer is left: step it again.
  More,
  /// The answer is whole: every byte of it is written.
  End,
}

/// One exchange's answer, made by [`super::serve`] and written a slice at a time by [`Answer::write`]. `K` is what its
/// pack's source keeps while it writes the pack.
pub struct Answer<K> {
  /// The bytes encoded when the answer was made: the whole answer, or everything before its pack's data.
  head: Vec<u8>,
  /// How many of them are written.
  head_written: usize,
  /// The pack, for an answer that carries one.
  pack: Option<Streaming<K>>,
  /// Why the exchange was refused, if it was.
  refusal: Option<Refusal>,
  /// How the negotiation went, for a `fetch`.
  counters: Option<Counters>,
  /// Whether the request was a flush alone, which ends the client's session.
  ends_session: bool,
}

/// A pack being framed.
struct Streaming<K> {
  /// What its source keeps.
  source: K,
  /// How the pack's data is framed.
  framing: Framing,
  /// How many bytes of the response come before what is framed next.
  at: u64,
  /// The data of the packet being filled, as its source wrote it, until the packet is full or the pack has ended.
  data: Vec<u8>,
  /// The packet framed and not yet written whole, with the flush after it when it is the pack's last.
  framed: Vec<u8>,
  /// How much of them is written.
  framed_written: usize,
  /// Whether the response's last packet is framed: the flush after the pack, or the band-3 words that end it.
  closed: bool,
}

impl<K> Answer<K> {
  /// An answer whose bytes are all in `head`.
  pub(super) fn whole(head: Vec<u8>) -> Answer<K> {
    Answer {
      head,
      head_written: 0,
      pack: None,
      refusal: None,
      counters: None,
      ends_session: false,
    }
  }

  /// The answer to a flush alone: nothing, and the session's end.
  pub(super) fn end() -> Answer<K> {
    Answer {
      ends_session: true,
      ..Answer::whole(Vec::new())
    }
  }

  /// The answer refusing an exchange: its words in an `ERR` line, or on band 3 in a multiplexed response.
  pub(super) fn refused(refused: Refused) -> Answer<K> {
    let head = error_line(&refused.refusal, refused.multiplexed);
    Answer {
      refusal: Some(refused.refusal),
      ..Answer::whole(head)
    }
  }

  /// An answer of `head`, then the pack `source` writes, framed as `framing` frames it; under v2 its grammar is where
  /// `head`'s `at` bytes brought it, the packfile section's header last. The two packet buffers are charged to
  /// `admission` first.
  pub(super) fn streaming(
    head: Vec<u8>,
    framing: Framing,
    at: u64,
    source: K,
    admission: &mut Admission,
  ) -> Result<Answer<K>, Refusal> {
    admission
      .charge(to_u64(MAX_BAND_DATA).saturating_add(to_u64(FRAMED_MOST)))
      .map_err(Refusal::PastAdmission)?;
    Ok(Answer {
      pack: Some(Streaming {
        source,
        framing,
        at,
        data: Vec::with_capacity(MAX_BAND_DATA),
        framed: Vec::with_capacity(FRAMED_MOST),
        framed_written: 0,
        closed: false,
      }),
      ..Answer::whole(head)
    })
  }

  /// This answer, with how its negotiation went.
  pub(super) fn counted(self, counters: Counters) -> Answer<K> {
    Answer {
      counters: Some(counters),
      ..self
    }
  }

  /// Why the exchange was refused, if it was: the answer then carries the words the client is told.
  pub fn refusal(&self) -> Option<&Refusal> {
    self.refusal.as_ref()
  }

  /// How a `fetch`'s negotiation went: wants answered in one round, commits walked. `None` for any other exchange.
  pub fn counters(&self) -> Option<Counters> {
    self.counters
  }

  /// Whether the request was a flush alone, with which a client ends its session (an SSH session's last request).
  pub fn ends_session(&self) -> bool {
    self.ends_session
  }

  /// Writes the answer's next bytes to `out`, at most `room` of them, the pack's from `packs` with `served` lent for the
  /// step, `packs` called at most once: whether any are left. A pack that cannot be finished ends with its band-3
  /// words, and [`Answer::refusal`] then says why.
  pub fn write<P: PackSource<Pack = K>>(
    &mut self,
    served: &Served<'_>,
    packs: &mut P,
    out: &mut impl Out,
    room: usize,
  ) -> Written {
    let put = put_from(&self.head, self.head_written, out, room);
    self.head_written = self.head_written.saturating_add(put);
    if self.head_written < self.head.len() {
      return Written::More;
    }
    let Some(streaming) = self.pack.as_mut() else {
      return Written::End;
    };
    let left = streaming.put_framed(out, room.saturating_sub(put));
    if left > 0 && streaming.wants_data() {
      if let Err(refusal) = streaming.frame_next(served, packs) {
        self.refusal = Some(refusal);
      }
      streaming.put_framed(out, left);
    }
    streaming.written()
  }
}

impl<K> Streaming<K> {
  /// Puts what is framed and not yet written into `out`, at most `room` bytes of it: the room left.
  fn put_framed(&mut self, out: &mut impl Out, room: usize) -> usize {
    let put = put_from(&self.framed, self.framed_written, out, room);
    self.framed_written = self.framed_written.saturating_add(put);
    room.saturating_sub(put)
  }

  /// Whether everything framed is written and the pack is not closed, so its source is what a step asks next.
  fn wants_data(&self) -> bool {
    self.framed_written >= self.framed.len() && !self.closed
  }

  /// What a step left: the end once the pack is closed and everything framed is written.
  fn written(&self) -> Written {
    if self.closed && self.framed_written >= self.framed.len() {
      Written::End
    } else {
      Written::More
    }
  }

  /// Asks the source once for the pack's next data, as much as the packet being filled has room for, and frames the
  /// packet once it is full or the pack has ended, the flush after its last; data that leaves it short waits in it for
  /// the next step's call, so every packet but the last carries its band's most however the source divides its work.
  /// When the source refuses, frames the band-3 words that end the response, refused with why the pack ended early.
  fn frame_next<P: PackSource<Pack = K>>(
    &mut self,
    served: &Served<'_>,
    packs: &mut P,
  ) -> Result<(), Refusal> {
    self.framed.clear();
    self.framed_written = 0;
    let room = self.framing.data().saturating_sub(self.data.len());
    match packs.write(served, &mut self.source, &mut self.data, room) {
      Ok(true) if self.data.len() < self.framing.data() => Ok(()),
      Ok(more) => self
        .frame_data(more)
        .or_else(|refusal| self.end_early(refusal)),
      Err(refused) => self.end_refused(refused),
    }
  }

  /// Frames the packet's data, and what ends the pack when it was the last; the data is taken.
  fn frame_data(&mut self, more: bool) -> Result<(), Refusal> {
    match self.framing {
      Framing::V2(mut response) => {
        if !self.data.is_empty() {
          let wrote = response
            .write(&Event::Pack(&self.data), self.at, &mut self.framed)
            .map_err(Refusal::Wire)?;
          self.at = self.at.saturating_add(wrote);
        }
        if !more {
          let wrote = response
            .write(&Event::Flush, self.at, &mut self.framed)
            .map_err(Refusal::Wire)?;
          self.at = self.at.saturating_add(wrote);
        }
        self.framing = Framing::V2(response);
      }
      Framing::Band { .. } => {
        sideband::write(Band::Data, &self.data, &mut self.framed);
        if !more {
          self.framed.extend_from_slice(FLUSH);
        }
      }
      Framing::Raw => self.framed.extend_from_slice(&self.data),
    }
    self.data.clear();
    self.closed = !more;
    Ok(())
  }

  /// Ends the pack early because its source refused: the data it wrote before refusing goes out first, in its packet,
  /// as git sends what `pack-objects` wrote before it failed, then git's band-3 words. Refused with the source's
  /// refusal, or with why that data could not be framed.
  fn end_refused(&mut self, refused: PackRefused) -> Result<(), Refusal> {
    match self.frame_data(true) {
      Ok(()) => self.abort(Refusal::Pack(refused)),
      Err(refusal) => self.end_early(refusal),
    }
  }

  /// Ends the response with git's band-3 words for a pack that could not be finished, in place of anything framed and
  /// not written: refused with `refusal`.
  fn end_early(&mut self, refusal: Refusal) -> Result<(), Refusal> {
    self.framed.clear();
    self.framed_written = 0;
    self.abort(refusal)
  }

  /// Puts git's band-3 words for a pack that could not be finished after what is framed, or, with no band to say them
  /// on, nothing, so the pack stops short: the response ends there, refused with `refusal`.
  fn abort(&mut self, refusal: Refusal) -> Result<(), Refusal> {
    if !matches!(self.framing, Framing::Raw) {
      sideband::write(Band::Error, ABORT, &mut self.framed);
    }
    self.closed = true;
    Err(refusal)
  }
}

/// Puts the bytes of `bytes` from `from` on into `out`, at most `room` of them: how many it put.
fn put_from(bytes: &[u8], from: usize, out: &mut impl Out, room: usize) -> usize {
  let rest = bytes.get(from..).unwrap_or_default();
  let chunk = rest.get(..room).unwrap_or(rest);
  out.put(chunk);
  chunk.len()
}

/// The line a refusal is answered with: `ERR <words>`, or its words on band 3 in a multiplexed response
/// (`packet_writer_error`), cut to what one packet carries.
fn error_line(refusal: &Refusal, multiplexed: bool) -> Vec<u8> {
  let words = refusal.words();
  let mut line = Vec::new();
  if multiplexed {
    sideband::write(
      Band::Error,
      words.get(..MAX_BAND_DATA).unwrap_or(&words),
      &mut line,
    );
    return line;
  }
  let text = [ERR, &words].concat();
  // The text is cut to what a packet's data holds, so the write cannot be refused; were it refused, the answer would be
  // empty rather than malformed.
  if pktline::write(
    Packet::Data(text.get(..pktline::MAX_DATA).unwrap_or(&text)),
    &mut line,
  )
  .is_err()
  {
    line.clear();
  }
  line
}
