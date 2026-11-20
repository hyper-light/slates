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
//!
//! A pack git's `pack-objects` would die of as it begins (A-640 part 4: a `sparse:oid` filter's blob not resolved or
//! not read) is answered as git's server answers it: what came before the pack, then the dying process's lines as git
//! formats them (`vfreportf`: cut to 4096 bytes, a control byte but a tab and a newline shown as `?`) and relays them
//! (a band-2 packet each read of at most 128 bytes, writes made one after another read together, [`Said`]), then git's
//! band-3 words, and no flush; with no band, what came before the pack alone. What `pack-objects` writes as it reads
//! the filter's name and lives on (a warning) is relayed the same way before the pack.

use super::{ABORT, PackPlan, PackRefused, PackSource, Refusal, Refused, Served};
use crate::negotiation::Counters;
use crate::negotiation::{Answer as NegotiatedAnswer, Filter, Negotiating};
use crate::out::Out;
use crate::pktline::{self, Packet};
use crate::sideband::{self, Band, MAX_BAND_DATA};
use crate::store::StoreGraph;
use crate::v2::fetch::{Event, Response};
use crate::{Admission, to_u64};

/// Format: what an error line begins with outside a multiplexed response (`packet_writer_error`).
const ERR: &[u8] = b"ERR ";

/// Format: what git's `die` writes before a dying process's words (`usage.c` `die_message_builtin`), which git's server
/// relays on band 2 when `pack-objects` dies (A-640 part 4).
const FATAL: &[u8] = b"fatal: ";

/// Format: what git's `warning` writes before its words (`usage.c` `warn_builtin`).
pub(super) const WARNING: &[u8] = b"warning: ";

/// Format: what git's `error` writes before its words (`usage.c` `error_builtin`).
pub(super) const ERROR: &[u8] = b"error: ";

/// Format: the most bytes one of git's reported lines takes, its newline last (`usage.c` `vfreportf`'s `msg[4096]`).
const REPORTED: usize = 4096;

/// Format: the most bytes git's server relays of `pack-objects`' error stream in one band-2 packet, a read's worth
/// (`upload-pack.c` `create_pack_file`'s `progress[128]`).
const RELAYED: usize = 128;

/// Format: what git's report shows in place of a control byte other than a tab and a newline (`vfreportf`).
const SHOWN: u8 = b'?';

/// Format: the control bytes git's report keeps: a tab and a newline (`vfreportf`).
const KEPT_CONTROLS: &[u8] = b"\t\n";

/// Format: what ends each of git's reported lines (`vfreportf`).
const NEWLINE: &[u8] = b"\n";

/// Format: the flush packet, four zero digits [gitprotocol-common: "0000 (flush-pkt)"].
const FLUSH: &[u8] = b"0000";

/// Format: the length of the band-3 packet of git's words for a pack that could not be finished: the words, after the
/// packet's length and band byte (a band packet's most less the most data it carries).
const ABORT_PACKET: usize = ABORT
  .len()
  .saturating_add(pktline::MAX_LEN.saturating_sub(MAX_BAND_DATA));

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

  /// Whether the answer has bands to say more than the pack on: every framing but v0's bare pack.
  pub(super) fn is_banded(self) -> bool {
    !matches!(self, Framing::Raw)
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
  /// The pack's objects still being chosen, before its source begins (A-745).
  choosing: Option<Box<Choosing>>,
}

/// A fetch's pack whose objects are still being chosen (A-745, §4.17.1 "A fetch's answer" rule 3): the negotiation's
/// choice, stepped within the owner's quantum by [`Answer::write_within`], and what the pack's source begins with once it
/// ends: the request's filter, `thin-pack`, `ofs-delta`, the framing and where the response stands.
pub(super) struct Choosing {
  /// The choice.
  negotiating: Negotiating<u64, u64>,
  /// The request's filter.
  filter: Option<Filter>,
  /// `thin-pack`.
  thin: bool,
  /// `ofs-delta`.
  ofs_delta: bool,
  /// How the pack's data will be framed.
  framing: Framing,
  /// How many bytes of the response come before the pack.
  at: u64,
}

impl Choosing {
  /// The choice `negotiating` makes, and what the pack begins with after it.
  pub(super) fn new(
    negotiating: Negotiating<u64, u64>,
    filter: Option<Filter>,
    thin: bool,
    ofs_delta: bool,
    framing: Framing,
    at: u64,
  ) -> Choosing {
    Choosing {
      negotiating,
      filter,
      thin,
      ofs_delta,
      framing,
      at,
    }
  }
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
      choosing: None,
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

  /// The answer git's server gives when `pack-objects` dies as it begins (A-640 part 4: a `sparse:oid` filter's blob
  /// that is not resolved or not read): `head`, everything written before the pack; then, where the answer has a band
  /// to say them on (`banded`), what the dying process writes to its error stream (`said`, its writes while it read
  /// the filter's name; `Refusal::warning`'s line; then `refusal`'s words as its `fatal:` line) as git's server relays
  /// it ([`relay`]), then git's band-3 words, and no flush, as none follows git's. With no band, `head` alone: git's
  /// server writes those lines to its own error stream, and the client finds the pack missing. The packets are charged
  /// to `admission` first. The `fatal:` line joins `said`'s last run when git read nothing since, as it dies at once
  /// of a name that names nothing after it listed an abbreviation's candidates.
  pub(super) fn died(
    mut head: Vec<u8>,
    mut said: Said,
    refusal: Refusal,
    banded: bool,
    admission: &mut Admission,
  ) -> Result<Answer<K>, Refusal> {
    if banded {
      if let Some(words) = refusal.warning() {
        said.write(&reported(WARNING, &words));
        // git frees the blob it read, past its limit, before it dies.
        said.read();
      }
      said.write(&reported(FATAL, &refusal.words()));
      relay(&said, &mut head, admission)?;
      admission
        .charge(sideband::written_len(to_u64(ABORT.len())))
        .map_err(Refusal::PastAdmission)?;
      sideband::write(Band::Error, ABORT, &mut head);
    }
    Ok(Answer {
      refusal: Some(refusal),
      ..Answer::whole(head)
    })
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

  /// An answer of `head`, the sections before a pack, then the pack whose objects `choosing` is still choosing: the two
  /// packet buffers the pack is framed through charged to `admission` now, as [`Answer::streaming`] charges them.
  pub(super) fn choosing(
    head: Vec<u8>,
    choosing: Choosing,
    admission: &mut Admission,
  ) -> Result<Answer<K>, Refusal> {
    admission
      .charge(to_u64(MAX_BAND_DATA).saturating_add(to_u64(FRAMED_MOST)))
      .map_err(Refusal::PastAdmission)?;
    Ok(Answer {
      choosing: Some(Box::new(choosing)),
      ..Answer::whole(head)
    })
  }

  /// Steps the choice of the pack's objects within `spent` ([`Answer::write_within`]), the repository lent for the step;
  /// once it ends, the pack's source begun from the plan it made.
  fn choose<P: PackSource<Pack = K>>(
    &mut self,
    served: &Served<'_>,
    packs: &mut P,
    admission: &mut Admission,
    spent: &mut dyn FnMut() -> bool,
  ) {
    let Some(choosing) = self.choosing.as_mut() else {
      return;
    };
    let graph = StoreGraph::new(served.repository, served.store, served.refs);
    let stepped = choosing.negotiating.step(&graph, admission, spent);
    if let Some(fault) = graph.fault() {
      return self.abandon(Refusal::Store(fault));
    }
    let outcome = match stepped {
      Err(refusal) => return self.abandon(Refusal::Negotiation(refusal)),
      Ok(None) => return,
      Ok(Some(outcome)) => outcome,
    };
    self.counters = Some(outcome.counters);
    let Some(choosing) = self.choosing.take() else {
      return;
    };
    let NegotiatedAnswer::Pack(pack) = outcome.answer else {
      return;
    };
    let plan = PackPlan {
      runs: &pack.runs,
      objects: &pack.objects,
      filter: choosing.filter.as_ref(),
      thin: choosing.thin,
      ofs_delta: choosing.ofs_delta,
    };
    match packs.begin(served, &plan, admission) {
      Ok(source) => {
        self.pack = Some(Streaming {
          source,
          framing: choosing.framing,
          at: choosing.at,
          data: Vec::with_capacity(MAX_BAND_DATA),
          framed: Vec::with_capacity(FRAMED_MOST),
          framed_written: 0,
          closed: false,
        });
      }
      Err(refused) => {
        self.choosing = Some(choosing);
        self.abandon(Refusal::Pack(refused));
      }
    }
  }

  /// Ends the response of a pack whose choice was refused, or whose source could not begin, after the sections before
  /// it: git's band-3 words where the framing has a band to say them on, else nothing, so the pack is missing; refused
  /// with `refusal`. Their packet is within what [`Answer::choosing`] charged.
  fn abandon(&mut self, refusal: Refusal) {
    if let Some(choosing) = self.choosing.take()
      && choosing.framing.is_banded()
    {
      sideband::write(Band::Error, ABORT, &mut self.head);
    }
    self.refusal = Some(refusal);
  }

  /// This answer, with how its negotiation went.
  pub(super) fn counted(self, counters: Counters) -> Answer<K> {
    Answer {
      counters: Some(counters),
      ..self
    }
  }

  /// The pack's source, for an answer that carries a pack: what it keeps while it writes, such as a stream's counters
  /// (A-718: which of the store's paths wrote the pack, kept fragments copied or objects encoded afresh).
  pub fn pack_source(&self) -> Option<&K> {
    self.pack.as_ref().map(|pack| &pack.source)
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
  /// step and what the step holds for a while charged to the request's `admission` (the one it was made within),
  /// `packs` called at most once: whether any are left. A pack that cannot be finished ends with its band-3 words, and
  /// [`Answer::refusal`] then says why.
  pub fn write<P: PackSource<Pack = K>>(
    &mut self,
    served: &Served<'_>,
    packs: &mut P,
    admission: &mut Admission,
    out: &mut impl Out,
    room: usize,
  ) -> Written {
    self.write_within(served, packs, admission, out, room, &mut || false)
  }

  /// [`Answer::write`], a pack whose objects are still being chosen ([`super::serve_in_turns`]) stepped first until
  /// `spent` says the owner's quantum is spent, at least one unit a step, the repository lent for the step alone; the
  /// pack's source begun once the last object is chosen. A choice refused, or a source that cannot begin, after the
  /// sections before the pack were written ends the response with git's band-3 words for a pack that could not be
  /// finished, as a pack refused midway ends.
  pub fn write_within<P: PackSource<Pack = K>>(
    &mut self,
    served: &Served<'_>,
    packs: &mut P,
    admission: &mut Admission,
    out: &mut impl Out,
    room: usize,
    spent: &mut dyn FnMut() -> bool,
  ) -> Written {
    let put = put_from(&self.head, self.head_written, out, room);
    self.head_written = self.head_written.saturating_add(put);
    if self.head_written < self.head.len() {
      return Written::More;
    }
    if self.choosing.is_some() {
      self.choose(served, packs, admission, spent);
      return if self.head_written < self.head.len()
        || self.choosing.is_some()
        || self.pack.is_some()
      {
        Written::More
      } else {
        Written::End
      };
    }
    let Some(streaming) = self.pack.as_mut() else {
      return Written::End;
    };
    let left = streaming.put_framed(out, room.saturating_sub(put));
    if left > 0 && streaming.wants_data() {
      if let Err(refusal) = streaming.frame_next(served, packs, admission) {
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
    admission: &mut Admission,
  ) -> Result<(), Refusal> {
    self.framed.clear();
    self.framed_written = 0;
    let room = self.framing.data().saturating_sub(self.data.len());
    match packs.write(served, &mut self.source, admission, &mut self.data, room) {
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
    if self.framing.is_banded() {
      sideband::write(Band::Error, ABORT, &mut self.framed);
    }
    self.closed = true;
    Err(refusal)
  }
}

/// What git's `pack-objects` writes to its error stream before its pack or its death, kept as git's server reads it
/// (A-640 part 4; `create_pack_file` reads at most `RELAYED` bytes a read): writes made one after another reach the
/// pipe together, so a read takes as much of them as it holds; git reading an object between two writes lets its
/// server read the first before the second comes, so no read spans two runs of writes. Stock git's own recordings show
/// both (`fixtures/fetch-sparse-*`): a refname warning and its advice read together; an abbreviation's `error:` line
/// read alone before git reads the candidates it lists, then its advice and the `fatal:` line together; a refname
/// warning before git reads the object it names, and the `fatal:` line after it, read apart.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Said {
  /// The runs of writes made with no object read between them, each whole, in order.
  runs: Vec<Vec<u8>>,
  /// Whether git read an object since the last write, so the next write begins a run.
  read_since: bool,
}

impl Said {
  /// Adds a write: to the last run, unless git read an object since it.
  pub(super) fn write(&mut self, bytes: &[u8]) {
    match self.runs.last_mut() {
      Some(run) if !self.read_since => run.extend_from_slice(bytes),
      _ => self.runs.push(bytes.to_vec()),
    }
    self.read_since = false;
  }

  /// Marks that git reads an object, or frees one, before its next write.
  pub(super) fn read(&mut self) {
    self.read_since = true;
  }

  /// Each read git's server makes of the writes: at most `RELAYED` bytes, never across two runs.
  pub(super) fn reads(&self) -> impl Iterator<Item = &[u8]> {
    self.runs.iter().flat_map(|run| run.chunks(RELAYED))
  }
}

/// Puts what `said` holds after `out` on band 2 as git's server relays it: a packet each read ([`Said::reads`]); the
/// packets charged to `admission` first.
pub(super) fn relay(
  said: &Said,
  out: &mut Vec<u8>,
  admission: &mut Admission,
) -> Result<(), Refusal> {
  let len = said
    .reads()
    .map(|read| sideband::written_len(to_u64(read.len())))
    .fold(0, u64::saturating_add);
  admission.charge(len).map_err(Refusal::PastAdmission)?;
  for read in said.reads() {
    sideband::write(Band::Progress, read, out);
  }
  Ok(())
}

/// The line git's `vfreportf` writes for `words` after `prefix`: the words cut to what its buffer holds, each control
/// byte but a tab and a newline shown as `?`, and a newline last.
pub(super) fn reported(prefix: &[u8], words: &[u8]) -> Vec<u8> {
  let most = REPORTED
    .saturating_sub(prefix.len())
    .saturating_sub(NEWLINE.len());
  let kept = words.get(..most).unwrap_or(words);
  let mut line = Vec::with_capacity(
    prefix
      .len()
      .saturating_add(kept.len())
      .saturating_add(NEWLINE.len()),
  );
  line.extend_from_slice(prefix);
  line.extend(
    kept
      .iter()
      .map(|byte| if shown_as_is(*byte) { *byte } else { SHOWN }),
  );
  line.extend_from_slice(NEWLINE);
  line
}

/// Whether git's report shows `byte` as it is: any byte but a control byte (git's `iscntrl`: below a space, and DEL),
/// a tab and a newline excepted.
fn shown_as_is(byte: u8) -> bool {
  !byte.is_ascii_control() || KEPT_CONTROLS.contains(&byte)
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
