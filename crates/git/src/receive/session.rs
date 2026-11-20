//! One push over one connection, sans-io (A-620; §4.17.2 "A push"; git's `cmd_receive_pack`): the
//! advertisement, then the client's request and pack as their bytes arrive, then the checks, then
//! the verdicts and the answer. A-625's HTTP edge and A-594's SSH channel drive it: bytes in
//! through [`Session::input`] and [`Session::end`], the answer out through [`Session::answer`] to
//! whatever [`Out`] the caller hands it. A connection that opens with the advertisement (SSH, git's
//! own transports) calls [`Session::advertise`] first; smart HTTP answers `info/refs` with it and
//! serves the push's `POST` with a session that never advertises, as `git-http-backend` runs
//! `receive-pack --stateless-rpc` for each.
//!
//! **Nothing is held across calls.** The repository a thin pack's bases and the connectivity check
//! read is handed to each call that reads, and the judge to the call that decides. The session owns
//! its advertisement (a copy) and its hidden refs, so a caller keeps it across its own steps, as a
//! connection's task keeps it between the bytes it is handed (A-654). Between the two
//! the push waits checked ([`Session::checked`]): the caller admits its objects to its store's
//! quarantine there, which needs the store the repository's view borrowed (A-638), and then
//! decides it.
//!
//! The push is checked as git's receive-pack would answer it: once the request is whole when it
//! holds only deletes (no pack follows them), once the pack's trailer is read otherwise, and at the
//! client's end of input when the pack is cut short (a short pack, answered `unpack <words>`). A
//! request cut short is refused, as receive-pack dies on it; a refusal ends the session with
//! nothing written. Bytes after the push's end are not the push's: the session takes none of them.

use super::{
  Advertised, Checked, HiddenRefs, Judge, NO_INDEX, Received, Refusal, Repository, Request,
  RequestReader, check, checked_pack, decide, visible, write_advertisement, write_report,
};
use crate::Admission;
use crate::object_name::ObjectId;
use crate::out::Out;
use crate::unpack::{self, NoIndex};

/// What a session reads or does next.
enum State<'s> {
  /// The request, as its bytes arrive.
  Request(RequestReader<'s>),
  /// The pack that follows a whole request.
  Pack {
    /// The request.
    request: Request,
    /// The pack as its bytes arrive.
    reader: unpack::Reader<'static, NoIndex>,
  },
  /// The push, read and checked, waiting for its verdicts.
  Checked(Checked),
  /// Nothing: the push is answered, or was refused.
  Done,
}

/// One push over one connection, sans-io.
pub struct Session<'s, H> {
  /// What the server advertises.
  advertised: Advertised<'s>,
  /// The refs hidden from this pusher.
  hidden: H,
  /// What comes next.
  state: State<'s>,
}

impl<'s, H: HiddenRefs> Session<'s, H> {
  /// A session of a push to a server that advertises `advertised`, hiding what `hidden` hides.
  pub fn new(advertised: &Advertised<'s>, hidden: H) -> Self {
    Self {
      advertised: *advertised,
      hidden,
      state: State::Request(RequestReader::new(advertised)),
    }
  }

  /// Writes the advertisement of `refs` (in git's order, by name) a pusher may see: every one
  /// `hidden` does not hide.
  pub fn advertise(&self, refs: &[(ObjectId, &[u8])], out: &mut impl Out) -> Result<(), Refusal> {
    write_advertisement(&visible(refs, &self.hidden), &self.advertised, out)
  }

  /// Takes the client's next bytes, a thin pack's bases and the connectivity check read from
  /// `repository`: whether the push is now checked ([`Session::checked`]). Once checked, and once
  /// done, it takes no more. A refusal ends the session.
  pub fn input<R: Repository + ?Sized>(
    &mut self,
    bytes: &[u8],
    repository: &R,
    admission: &mut Admission,
  ) -> Result<bool, Refusal> {
    let taken = self.take(bytes, repository, admission);
    if taken.is_err() {
      self.state = State::Done;
    }
    taken
  }

  /// [`Session::input`]'s reading, before a refusal ends the session.
  fn take<R: Repository + ?Sized>(
    &mut self,
    bytes: &[u8],
    repository: &R,
    admission: &mut Admission,
  ) -> Result<bool, Refusal> {
    let mut rest = bytes;
    if let State::Request(reader) = &mut self.state {
      let took = reader.read(rest, admission)?;
      if !reader.is_whole() {
        return Ok(false);
      }
      rest = rest.get(took..).unwrap_or_default();
      let State::Request(reader) = core::mem::replace(&mut self.state, State::Done) else {
        return Ok(false);
      };
      let request = reader.request()?;
      if !request.wants_pack() {
        let checked = check(request, None, repository, &self.hidden, admission)?;
        self.state = State::Checked(checked);
        return Ok(true);
      }
      let reader = unpack::Reader::new(self.advertised.format, &NO_INDEX);
      self.state = State::Pack { request, reader };
    }
    let State::Pack { reader, .. } = &mut self.state else {
      return Ok(false);
    };
    let read = reader.read(rest, admission).map(|_| ());
    if read.is_ok() && !reader.is_whole() {
      return Ok(false);
    }
    self.check_pack(read, repository, admission)
  }

  /// The client's bytes have ended: whether the push is now checked. A request cut short is
  /// refused, ending the session; a pack cut short is checked as git answers a short pack.
  pub fn end<R: Repository + ?Sized>(
    &mut self,
    repository: &R,
    admission: &mut Admission,
  ) -> Result<bool, Refusal> {
    match &self.state {
      State::Request(_) => {
        let State::Request(reader) = core::mem::replace(&mut self.state, State::Done) else {
          return Ok(false);
        };
        reader.request().map(|_| false)
      }
      State::Pack { .. } => {
        let checked = self.check_pack(Ok(()), repository, admission);
        if checked.is_err() {
          self.state = State::Done;
        }
        checked
      }
      State::Checked(_) | State::Done => Ok(false),
    }
  }

  /// The push, read and checked, until it is answered: its objects for the caller's quarantine,
  /// and each command's refusal by the checks.
  pub fn checked(&self) -> Option<&Checked> {
    match &self.state {
      State::Checked(checked) => Some(checked),
      _ => None,
    }
  }

  /// Decides the checked push through `judge` and writes the answer to `out`: what was received,
  /// or `None` when no push waits checked. The session is done after it.
  pub fn answer<J: Judge + ?Sized>(
    &mut self,
    judge: &mut J,
    out: &mut impl Out,
    admission: &mut Admission,
  ) -> Result<Option<Received>, Refusal> {
    let received = self.decide(judge, admission)?;
    if let Some(received) = &received {
      write_report(received, out)?;
    }
    Ok(received)
  }

  /// Decides the checked push through `judge`, writing nothing: what was received, or `None` when
  /// no push waits checked. The session is done after it. Its answer is [`write_report`]'s, or
  /// [`super::write_report_held`]'s and [`super::write_report_end`]'s around what git sends after
  /// its report, `post-receive`'s words (A-665 part 3).
  pub fn decide<J: Judge + ?Sized>(
    &mut self,
    judge: &mut J,
    admission: &mut Admission,
  ) -> Result<Option<Received>, Refusal> {
    let State::Checked(checked) = core::mem::replace(&mut self.state, State::Done) else {
      return Ok(None);
    };
    decide(checked, judge, admission).map(Some)
  }

  /// Whether the push is answered, or was refused.
  pub fn is_done(&self) -> bool {
    matches!(self.state, State::Done)
  }

  /// Checks the push whose pack has ended or been refused (`read`).
  fn check_pack<R: Repository + ?Sized>(
    &mut self,
    read: Result<(), unpack::Refusal>,
    repository: &R,
    admission: &mut Admission,
  ) -> Result<bool, Refusal> {
    let State::Pack { request, reader } = core::mem::replace(&mut self.state, State::Done) else {
      return Ok(false);
    };
    let unpacked = checked_pack(reader, read, repository, admission);
    let checked = check(request, Some(unpacked), repository, &self.hidden, admission)?;
    self.state = State::Checked(checked);
    Ok(true)
  }
}
