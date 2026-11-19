//! An answer, written out a slice at a time (A-640 parts 2 and 3): the bytes [`super::serve`] encoded, then, for a
//! fetch that carries a pack, the pack its source writes, framed a packet at a time as the protocol frames it
//! (`Framing`): protocol v2's packfile section on band 1, then its flush ("The packfile section ... is multiplexed
//! using side-band-64k" [gitprotocol-v2]); protocol v0's side-band-64k or side-band on band 1, then a flush
//! (`create_pack_file`); or, under v0 without side-band, the pack's bytes as they are.
//!
//! A step writes at most the room its caller gives it, and any bytes past that wait for the next step, so the caller's
//! room bounds what a step writes and one packet bounds what it asks of the pack's source. A packet is filled to the
//! most data its band carries, the format's own limit (`sideband::MAX_BAND_DATA` for side-band-64k, 995 bytes for
//! side-band's 1000-byte packets), so a pack goes out in the fewest packets whatever room the caller gives. A source
//! whose call worked and wrote nothing yet (A-639's stream counting the pack before its header) ends the step there,
//! and the next step asks it again, so a step is bounded by one call of the source's. A source that refuses mid-pack
//! ends the response with git's band-3 words for a pack that could not be finished: nothing follows a band-3 packet,
//! and the client stops with them. Without side-band there is no band to say it on, so the pack stops short, as git's
//! does, and the client finds it cut.

use super::{ABORT, PackSource, Refusal, Refused, Served};
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
  /// The pack's next data, as its source wrote it.
  data: Vec<u8>,
  /// Packets framed and not yet written whole.
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
      .charge(to_u64(MAX_BAND_DATA).saturating_add(to_u64(pktline::MAX_LEN)))
      .map_err(Refusal::PastAdmission)?;
    Ok(Answer {
      pack: Some(Streaming {
        source,
        framing,
        at,
        data: Vec::with_capacity(MAX_BAND_DATA),
        framed: Vec::with_capacity(pktline::MAX_LEN),
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
  /// step: whether any are left. A pack that cannot be finished ends with its band-3 words, and
  /// [`Answer::refusal`] then says why.
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
    let mut left = room.saturating_sub(put);
    loop {
      let put = put_from(&streaming.framed, streaming.framed_written, out, left);
      streaming.framed_written = streaming.framed_written.saturating_add(put);
      left = left.saturating_sub(put);
      let unwritten = streaming.framed_written < streaming.framed.len();
      if unwritten || (left == 0 && !streaming.closed) {
        return Written::More;
      }
      if streaming.closed {
        return Written::End;
      }
      match streaming.frame_next(served, packs) {
        Ok(true) => {}
        Ok(false) => return Written::More,
        Err(refusal) => self.refusal = Some(refusal),
      }
    }
  }
}

impl<K> Streaming<K> {
  /// Frames the pack's next packet: its next data, and the flush after its last; or, when the source refuses, the
  /// band-3 words that end the response: whether anything was framed. A source that worked and wrote nothing yet
  /// frames nothing, and the step ends there, so a step is bounded by one call of the source's; the next step asks
  /// again. Refused with why the pack ended early.
  fn frame_next<P: PackSource<Pack = K>>(
    &mut self,
    served: &Served<'_>,
    packs: &mut P,
  ) -> Result<bool, Refusal> {
    self.framed.clear();
    self.framed_written = 0;
    self.data.clear();
    let room = self.framing.data();
    match packs.write(served, &mut self.source, &mut self.data, room) {
      Ok(true) if self.data.is_empty() => Ok(false),
      Ok(more) => match self.frame_data(more) {
        Ok(()) => Ok(true),
        Err(refusal) => self.end_early(refusal).map(|()| true),
      },
      Err(refused) => self.end_early(Refusal::Pack(refused)).map(|()| true),
    }
  }

  /// Frames the data the source wrote, and what ends the pack when it was the last.
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
    self.closed = !more;
    Ok(())
  }

  /// Ends the response with git's band-3 words for a pack that could not be finished, in place of anything framed and
  /// not written, or, with no band to say it on, ends it there: refused with `refusal`.
  fn end_early(&mut self, refusal: Refusal) -> Result<(), Refusal> {
    self.framed.clear();
    self.framed_written = 0;
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
