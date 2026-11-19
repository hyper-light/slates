//! One push over one connection, sans-io (A-620; §4.17.2 "A push"; git's `cmd_receive_pack`): the
//! advertisement, then the client's request and pack as their bytes arrive, then the answer, each
//! step's bytes written to whatever [`Out`] the caller hands it. A-625's HTTP edge and A-594's SSH
//! channel drive it, bytes in through [`Session::input`] and [`Session::end`], bytes out to their
//! own sending half. A connection that opens with the advertisement (SSH, git's own transports)
//! calls [`Session::advertise`] first; smart HTTP answers `info/refs` with it and serves the push's
//! `POST` with a session that never advertises, as `git-http-backend` runs `receive-pack
//! --stateless-rpc` for each.
//!
//! The session answers as git's receive-pack does: once the request is whole when it holds only
//! deletes (no pack follows them), once the pack's trailer is read otherwise, and at the client's
//! end of input when the pack is cut short (a short pack, answered `unpack <words>`). A request cut
//! short is refused, as receive-pack dies on it. Bytes after the push's end are not the push's: the
//! session takes none of them.

use super::{
  Advertised, HiddenRefs, Judge, NO_INDEX, PackRefused, Received, Refusal, Repository, Request,
  RequestReader, checked_pack, outcomes, visible, write_advertisement, write_report,
};
use crate::Admission;
use crate::object_name::ObjectId;
use crate::out::Out;
use crate::unpack::{self, NoIndex, Unpacked};

/// What a session reads next.
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
  /// Nothing: the push is answered, or was refused.
  Done,
}

/// One push over one connection, sans-io.
pub struct Session<'s, R: ?Sized, H: ?Sized, J: ?Sized> {
  /// What the server advertises.
  advertised: &'s Advertised<'s>,
  /// The repository pushed to.
  repository: &'s R,
  /// The refs hidden from this pusher.
  hidden: &'s H,
  /// Each command's verdict and append.
  judge: &'s mut J,
  /// What comes next.
  state: State<'s>,
}

impl<'s, R, H, J> Session<'s, R, H, J>
where
  R: Repository + ?Sized,
  H: HiddenRefs + ?Sized,
  J: Judge + ?Sized,
{
  /// A session of a push to `repository`, advertising `advertised`, hiding what `hidden` hides, its
  /// commands judged and appended through `judge`.
  pub fn new(
    advertised: &'s Advertised<'s>,
    repository: &'s R,
    hidden: &'s H,
    judge: &'s mut J,
  ) -> Self {
    Self {
      advertised,
      repository,
      hidden,
      judge,
      state: State::Request(RequestReader::new(advertised)),
    }
  }

  /// Writes the advertisement of `refs` (in git's order, by name) a pusher may see: every one
  /// `hidden` does not hide.
  pub fn advertise(&self, refs: &[(ObjectId, &[u8])], out: &mut impl Out) -> Result<(), Refusal> {
    write_advertisement(&visible(refs, self.hidden), self.advertised, out)
  }

  /// Takes the client's next bytes. When they end the push, answers it into `out` and gives what
  /// was received; until then, and once done, `None`. A refusal ends the session, nothing written
  /// for it, as receive-pack dies without an answer.
  pub fn input(
    &mut self,
    bytes: &[u8],
    out: &mut impl Out,
    admission: &mut Admission,
  ) -> Result<Option<Received>, Refusal> {
    let taken = self.take(bytes, out, admission);
    if taken.is_err() {
      self.state = State::Done;
    }
    taken
  }

  /// [`Session::input`]'s reading, before a refusal ends the session.
  fn take(
    &mut self,
    bytes: &[u8],
    out: &mut impl Out,
    admission: &mut Admission,
  ) -> Result<Option<Received>, Refusal> {
    let mut rest = bytes;
    if let State::Request(reader) = &mut self.state {
      let took = reader.read(rest, admission)?;
      if !reader.is_whole() {
        return Ok(None);
      }
      rest = rest.get(took..).unwrap_or_default();
      let State::Request(reader) = core::mem::replace(&mut self.state, State::Done) else {
        return Ok(None);
      };
      let request = reader.request()?;
      if !request.wants_pack() {
        return self.answer(request, None, out, admission).map(Some);
      }
      let reader = unpack::Reader::new(self.advertised.format, &NO_INDEX);
      self.state = State::Pack { request, reader };
    }
    let State::Pack { reader, .. } = &mut self.state else {
      return Ok(None);
    };
    let read = reader.read(rest, admission).map(|_| ());
    if read.is_ok() && !reader.is_whole() {
      return Ok(None);
    }
    self.answer_pack(read, out, admission)
  }

  /// The client's bytes have ended. A request cut short is refused; a pack cut short is answered as
  /// git answers a short pack; a session already done does nothing. The session is done after it.
  pub fn end(
    &mut self,
    out: &mut impl Out,
    admission: &mut Admission,
  ) -> Result<Option<Received>, Refusal> {
    match &self.state {
      State::Request(_) => {
        let State::Request(reader) = core::mem::replace(&mut self.state, State::Done) else {
          return Ok(None);
        };
        reader.request().map(|_| None)
      }
      State::Pack { .. } => self.answer_pack(Ok(()), out, admission),
      State::Done => Ok(None),
    }
  }

  /// Whether the push is answered, or was refused.
  pub fn is_done(&self) -> bool {
    matches!(self.state, State::Done)
  }

  /// Answers the push whose pack has ended or been refused (`read`).
  fn answer_pack(
    &mut self,
    read: Result<(), unpack::Refusal>,
    out: &mut impl Out,
    admission: &mut Admission,
  ) -> Result<Option<Received>, Refusal> {
    let State::Pack { request, reader } = core::mem::replace(&mut self.state, State::Done) else {
      return Ok(None);
    };
    let unpacked = checked_pack(reader, read, self.repository, admission);
    self
      .answer(request, Some(unpacked), out, admission)
      .map(Some)
  }

  /// Each command's outcome, written to `out` as receive-pack answers it.
  fn answer(
    &mut self,
    request: Request,
    unpacked: Option<Result<Unpacked, PackRefused>>,
    out: &mut impl Out,
    admission: &mut Admission,
  ) -> Result<Received, Refusal> {
    let outcomes = outcomes(
      &request,
      unpacked.as_ref(),
      self.repository,
      self.hidden,
      &mut *self.judge,
      admission,
    )?;
    let received = Received {
      request,
      unpacked,
      outcomes,
    };
    write_report(&received, out)?;
    Ok(received)
  }
}
