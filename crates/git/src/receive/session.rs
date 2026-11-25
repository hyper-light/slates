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
//! pack's checks are made a turn at a time ([`Session::check_turn`], A-719): every object checked
//! as git checks a push, then the connectivity walk, each turn for the caller's quantum, so no step
//! of a large push runs past its shard's window. A
//! request cut short is refused, as receive-pack dies on it; a refusal ends the session with
//! nothing written. Bytes after the push's end are not the push's: the session takes none of them.

use super::{
  Advertised, Checked, Checking, HiddenRefs, InMemory, Judge, NO_INDEX, Received, Refusal,
  Repository, Request, RequestReader, check, decide, finished_pack, visible, write_advertisement,
  write_report,
};
use crate::Admission;
use crate::object_name::ObjectId;
use crate::out::Out;
use crate::store::hook_view::Quantum;
use crate::unpack::{self, Contents, Held, Keep, NoIndex};

/// Outcome of [`Session::input_deferred`]: whether more pack bytes are needed, the pack reached
/// its trailer or an unpack error (caller must settle any in-flight batches before calling
/// [`Session::finish_deferred`]), or a delete-only request completed immediately.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeferredInput {
  /// More pack bytes expected; caller may dispatch `batch` once it reaches the scatter threshold.
  More,
  /// Pack reached its trailer (`Ok(())`) or an unpack refusal (`Err(refusal)`); caller must flush
  /// and settle all pending batches and then call [`Session::finish_deferred`].
  PackEnded(Result<(), unpack::Refusal>),
  /// Request carried no pack (delete-only) and is already checked.
  Ready,
}

/// Worker-verified payload for one [`unpack::Unverified`] pack entry ([`verify_unpacked`]).
#[derive(Debug)]
pub struct VerifiedBody {
  /// Verified Git object id (SHA-1DC or SHA-256).
  pub id: ObjectId,
  /// Plain BLAKE3 content identity (`slates_vcs::ContentId::of(&content)`).
  pub content_id: slates_vcs::ContentId,
  /// Uncompressed object bytes.
  pub content: Vec<u8>,
  /// Strict `fsck` refusal, if any (applied only when `!repository.holds(&id)`).
  pub(crate) fsck_refusal: Option<crate::objects::FsckId>,
  /// Any `.gitmodules` / `.gitattributes` entries referenced by a pushed tree.
  pub(crate) special: Vec<super::fsck::SpecialBlob>,
  /// Pre-parsed commit or tag metadata for `Push::admit_precomputed`.
  pub meta: Option<crate::store::ParsedMeta>,
}

/// Result of verifying one [`unpack::Unverified`] entry on a worker shard ([`verify_unpacked`]).
#[derive(Debug)]
pub struct VerifiedObject {
  /// Pack position (`at`).
  pub at: usize,
  /// Pack byte offset.
  pub offset: u64,
  /// Stored byte length in the pack.
  pub stored: u64,
  /// Object kind.
  pub kind: crate::object_name::Kind,
  /// Delta chain depth (`0` for whole).
  pub depth: u64,
  /// Admission hold covering `content`.
  pub hold: crate::Hold,
  /// Verification result (`Err` on SHA-1 collision attack).
  pub outcome: Result<VerifiedBody, unpack::Refusal>,
}

/// One settled, pre-verified pack object ready for O(1) owner-shard quarantine admission.
#[derive(Debug)]
pub struct SettledObject {
  /// Precomputed placement metadata for [`crate::store::Push::admit_precomputed`].
  pub placed: crate::store::PrecomputedPlaced,
  /// Uncompressed object content (moved into `Recent` cache on the owner shard).
  pub content: Vec<u8>,
  /// Admission hold to release after admission.
  pub hold: crate::Hold,
}

/// Pure stateless worker function executed across shards via `xshard::scatter_compute`: computes
/// SHA-1DC/SHA-256 object naming, plain BLAKE3 content identity, strict `fsck`, and commit/tag
/// metadata in parallel.
pub fn verify_unpacked(
  format: crate::object_name::ObjectFormat,
  room: u64,
  object: unpack::Unverified,
) -> VerifiedObject {
  let outcome = match crate::object_name::name_of(format, object.kind, &object.content) {
    Ok(id) => {
      let content_id = slates_vcs::ContentId::of(&object.content);
      let mut special = Vec::new();
      let fsck_refusal = if object.kind != crate::object_name::Kind::Blob {
        super::fsck::check_object(object.kind, &object.content, format, &mut special)
      } else {
        None
      };
      let meta = crate::store::parse_meta(format, object.kind, &object.content, room);
      Ok(VerifiedBody {
        id,
        content_id,
        content: object.content,
        fsck_refusal,
        special,
        meta,
      })
    }
    Err(attack) => Err(unpack::Refusal::Collision {
      at: object.offset,
      attack,
    }),
  };
  VerifiedObject {
    at: object.at,
    offset: object.offset,
    stored: object.stored,
    kind: object.kind,
    depth: object.depth,
    hold: object.hold,
    outcome,
  }
}

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
  /// The pack read whole, its waiting deltas resolved a turn at a time ([`Session::resolve_turn_into`], A-724's
  /// starvation fix), before its checks.
  Resolving {
    /// The request.
    request: Request,
    /// The pack, whole, its deltas being resolved.
    reader: unpack::Reader<'static, NoIndex>,
  },
  /// The push, read whole, its checks made a turn at a time ([`Session::check_turn`], A-719).
  Checking(Checking),
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
  /// The objects of a pack read into the session's own keeper ([`Session::input`]); none of a pack
  /// read into the caller's ([`Session::input_into`]).
  held: Held,
  /// The first object of the pack git's checks refuse, found as each object was kept (A-719 part 3:
  /// its bytes in hand, so the checks after the pack read none of them again for this).
  refused: Option<(ObjectId, crate::objects::FsckId)>,
  /// Any `.gitmodules` and `.gitattributes` entries referenced by pushed trees, checked once the pack
  /// is unpacked.
  special: Vec<super::fsck::SpecialBlob>,
  /// How many of the client's bytes were the pack's, each charged to the admission as it arrived: what the caller keeps
  /// of them is its own (a server's push holds the pack as its objects' one copy, A-723 and main's ruling B part 2), so
  /// the session keeps none ([`Session::pack_len`]).
  pack_len: u64,
}

impl<'s, H: HiddenRefs> Session<'s, H> {
  /// A session of a push to a server that advertises `advertised`, hiding what `hidden` hides.
  pub fn new(advertised: &Advertised<'s>, hidden: H) -> Self {
    Self {
      advertised: *advertised,
      hidden,
      state: State::Request(RequestReader::new(advertised)),
      held: Held::default(),
      refused: None,
      special: Vec::new(),
      pack_len: 0,
    }
  }

  /// Writes the advertisement of `refs` (in git's order, by name) a pusher may see: every one
  /// `hidden` does not hide.
  pub fn advertise(&self, refs: &[(ObjectId, &[u8])], out: &mut impl Out) -> Result<(), Refusal> {
    write_advertisement(&visible(refs, &self.hidden), &self.advertised, out)
  }

  /// Takes the client's next bytes, a thin pack's bases and a push of deletes' checks read from
  /// `repository`, the pack's objects kept in the session's own memory: whether the push is now
  /// read whole, checked or with its pack's checks to make ([`Session::check_turn`]). Once read
  /// whole, and once done, it takes no more. A refusal ends the session.
  pub fn input<R: Repository + ?Sized>(
    &mut self,
    bytes: &[u8],
    repository: &R,
    admission: &mut Admission,
  ) -> Result<bool, Refusal> {
    let mut held = core::mem::take(&mut self.held);
    let mut site = InMemory {
      repository,
      held: &mut held,
    };
    let taken = self.input_into(bytes, &mut site, admission);
    self.held = held;
    taken
  }

  /// The repository's hash format advertised by this session.
  pub fn format(&self) -> crate::object_name::ObjectFormat {
    self.advertised.format
  }

  /// [`Session::input_into`] in deferred-naming mode: unpacks entries into `batch` without running
  /// synchronous SHA-1DC, `fsck`, or BLAKE3 on the owner shard so the caller can pipeline
  /// [`verify_unpacked`] across all shards.
  pub fn input_deferred<S: Keep + Repository>(
    &mut self,
    bytes: &[u8],
    site: &mut S,
    admission: &mut Admission,
    batch: &mut unpack::DeferredBatch,
  ) -> Result<DeferredInput, Refusal> {
    let taken = self.take_deferred(bytes, site, admission, batch);
    if taken.is_err() {
      self.state = State::Done;
    }
    taken
  }

  /// [`Session::input_deferred`]'s inner body before a refusal ends the session.
  fn take_deferred<S: Keep + Repository>(
    &mut self,
    bytes: &[u8],
    site: &mut S,
    admission: &mut Admission,
    batch: &mut unpack::DeferredBatch,
  ) -> Result<DeferredInput, Refusal> {
    let mut rest = bytes;
    if let State::Request(reader) = &mut self.state {
      let took = reader.read(rest, admission)?;
      if !reader.is_whole() {
        return Ok(DeferredInput::More);
      }
      rest = rest.get(took..).unwrap_or_default();
      let State::Request(reader) = core::mem::replace(&mut self.state, State::Done) else {
        return Ok(DeferredInput::More);
      };
      let request = reader.request()?;
      if !request.wants_pack() {
        let checked = check(request, None, (&*site, &*site), &self.hidden, admission)?;
        self.state = State::Checked(checked);
        return Ok(DeferredInput::Ready);
      }
      let reader = unpack::Reader::new(self.advertised.format, &NO_INDEX);
      self.state = State::Pack { request, reader };
    }
    let State::Pack { reader, .. } = &mut self.state else {
      return Ok(DeferredInput::More);
    };
    admission
      .charge(crate::to_u64(rest.len()))
      .map_err(Refusal::PastAdmission)?;
    self.pack_len = self.pack_len.saturating_add(crate::to_u64(rest.len()));
    let mut fscking = Fscking {
      site: &mut *site,
      format: self.advertised.format,
      refused: &mut self.refused,
      special: &mut self.special,
    };
    let read = reader
      .read_deferred(rest, admission, &mut fscking, batch)
      .map(|_| ());
    if read.is_ok() && !reader.is_whole() {
      return Ok(DeferredInput::More);
    }
    Ok(DeferredInput::PackEnded(read))
  }

  /// Settles a batch of worker-verified [`VerifiedObject`] entries in exact pack order into the
  /// active pack reader and session fsck state: returns `(settled, first_collision_error)` while
  /// ensuring any holds past a collision error are released back to `admission`.
  pub fn settle_deferred<R: Repository + ?Sized>(
    &mut self,
    verified: Vec<VerifiedObject>,
    repository: &R,
    admission: &mut Admission,
  ) -> (Vec<SettledObject>, Option<unpack::Refusal>) {
    let reader = match &mut self.state {
      State::Pack { reader, .. } | State::Resolving { reader, .. } => reader,
      _ => {
        for item in verified {
          admission.release(item.hold);
        }
        return (Vec::new(), None);
      }
    };
    let mut settled = Vec::with_capacity(verified.len());
    let mut first_error = None;
    for item in verified {
      if first_error.is_some() {
        admission.release(item.hold);
        continue;
      }
      let body = match item.outcome {
        Ok(body) => body,
        Err(refusal) => {
          admission.release(item.hold);
          first_error = Some(refusal);
          continue;
        }
      };
      let _ = reader.settle_named(item.at, body.id);
      if self.refused.is_none()
        && item.kind != crate::object_name::Kind::Blob
        && !repository.holds(&body.id)
      {
        if let Some(finding) = body.fsck_refusal {
          self.refused = Some((body.id, finding));
        } else {
          self.special.extend(body.special);
        }
      }
      let len = crate::to_u64(body.content.len());
      settled.push(SettledObject {
        placed: crate::store::PrecomputedPlaced {
          offset: item.offset,
          kind: item.kind,
          id: body.id,
          identity: body.content_id,
          len,
          depth: item.depth,
          meta: body.meta,
        },
        content: body.content,
        hold: item.hold,
      });
    }
    (settled, first_error)
  }

  /// Completes deferred pack intake once every in-flight batch has been settled via
  /// [`Session::settle_deferred`], entering [`State::Resolving`] or [`State::Checking`].
  pub fn finish_deferred<S: Keep + Repository>(
    &mut self,
    read: Result<(), unpack::Refusal>,
    site: &mut S,
    admission: &mut Admission,
  ) -> Result<bool, Refusal> {
    let finished = self.check_pack(read, site, admission);
    if finished.is_err() {
      self.state = State::Done;
    }
    finished
  }

  /// [`Session::input`], the pack read into `site` (A-719 part 3): its keeper, which a server's
  /// quarantine is, and the repository its thin bases and a push of deletes' checks are read from.
  pub fn input_into<S: Keep + Repository>(
    &mut self,
    bytes: &[u8],
    site: &mut S,
    admission: &mut Admission,
  ) -> Result<bool, Refusal> {
    let taken = self.take(bytes, site, admission);
    if taken.is_err() {
      self.state = State::Done;
    }
    taken
  }

  /// [`Session::input_into`]'s reading, before a refusal ends the session.
  fn take<S: Keep + Repository>(
    &mut self,
    bytes: &[u8],
    site: &mut S,
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
        let checked = check(request, None, (&*site, &*site), &self.hidden, admission)?;
        self.state = State::Checked(checked);
        return Ok(true);
      }
      let reader = unpack::Reader::new(self.advertised.format, &NO_INDEX);
      self.state = State::Pack { request, reader };
    }
    let State::Pack { reader, .. } = &mut self.state else {
      return Ok(false);
    };
    admission
      .charge(crate::to_u64(rest.len()))
      .map_err(Refusal::PastAdmission)?;
    self.pack_len = self.pack_len.saturating_add(crate::to_u64(rest.len()));
    let mut fscking = Fscking {
      site: &mut *site,
      format: self.advertised.format,
      refused: &mut self.refused,
      special: &mut self.special,
    };
    let read = reader.read_into(rest, admission, &mut fscking).map(|_| ());
    if read.is_ok() && !reader.is_whole() {
      return Ok(false);
    }
    self.check_pack(read, site, admission)
  }

  /// The client's bytes have ended: whether the push is now read whole, checked or with its pack's
  /// checks to make. A request cut short is refused, ending the session; a pack cut short is
  /// checked as git answers a short pack.
  pub fn end<R: Repository + ?Sized>(
    &mut self,
    repository: &R,
    admission: &mut Admission,
  ) -> Result<bool, Refusal> {
    let mut held = core::mem::take(&mut self.held);
    let mut site = InMemory {
      repository,
      held: &mut held,
    };
    let ended = self.end_into(&mut site, admission);
    self.held = held;
    ended
  }

  /// [`Session::end`], the pack read into `site` ([`Session::input_into`]).
  pub fn end_into<S: Keep + Repository>(
    &mut self,
    site: &mut S,
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
        let checked = self.check_pack(Ok(()), site, admission);
        if checked.is_err() {
          self.state = State::Done;
        }
        checked
      }
      State::Resolving { .. } | State::Checking(_) | State::Checked(_) | State::Done => Ok(false),
    }
  }

  /// The push, read and checked, until it is answered: each object's place, and each command's
  /// refusal by the checks.
  pub fn checked(&self) -> Option<&Checked> {
    match &self.state {
      State::Checked(checked) => Some(checked),
      _ => None,
    }
  }

  /// The checked push, taken to be decided a command at a time ([`super::Deciding`], A-719); the
  /// session is done after it. None while no push waits checked.
  pub fn take_checked(&mut self) -> Option<Checked> {
    match core::mem::replace(&mut self.state, State::Done) {
      State::Checked(checked) => Some(checked),
      other => {
        self.state = other;
        None
      }
    }
  }

  /// The bytes of the objects the session read into its own memory ([`Session::input`]).
  pub fn kept(&self) -> &dyn Contents {
    &self.held
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

  /// [`Session::answer`] for a push read into the caller's site, its objects' bytes lent to the
  /// judge from `contents` where the caller lends them (a server's judge reads its own store).
  pub fn answer_with<J: Judge + ?Sized>(
    &mut self,
    judge: &mut J,
    contents: Option<&dyn Contents>,
    out: &mut impl Out,
    admission: &mut Admission,
  ) -> Result<Option<Received>, Refusal> {
    let received = self.decide_with(judge, contents, admission)?;
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
    decide(checked, Some(&self.held), judge, admission).map(Some)
  }

  /// [`Session::decide`] for a push read into the caller's site ([`Session::answer_with`]).
  pub fn decide_with<J: Judge + ?Sized>(
    &mut self,
    judge: &mut J,
    contents: Option<&dyn Contents>,
    admission: &mut Admission,
  ) -> Result<Option<Received>, Refusal> {
    let State::Checked(checked) = core::mem::replace(&mut self.state, State::Done) else {
      return Ok(None);
    };
    decide(checked, contents, judge, admission).map(Some)
  }

  /// How many of the client's bytes so far were the pack's: the pack is the last of each call's bytes, so a caller that
  /// keeps it takes the bytes past what it kept from the end of the call's (A-723: its record keeps them durably,
  /// compressed and with their deltas, so a push's objects are never recorded inflated).
  pub fn pack_len(&self) -> u64 {
    self.pack_len
  }

  /// Whether the push is answered, or was refused.
  pub fn is_done(&self) -> bool {
    matches!(self.state, State::Done)
  }

  /// Finishes the pack that has ended or been refused (`read`) into `site`, its checks to make a
  /// turn at a time.
  fn check_pack<S: Keep + Repository>(
    &mut self,
    read: Result<(), unpack::Refusal>,
    site: &mut S,
    admission: &mut Admission,
  ) -> Result<bool, Refusal> {
    let State::Pack { request, reader } = core::mem::replace(&mut self.state, State::Done) else {
      return Ok(false);
    };
    // A pack read whole resolves its waiting deltas a turn at a time from here (A-724's starvation fix: in one poll, a
    // push's last input step took 1.4 s for git/git v1.5.0); one cut short or refused is finished now.
    if read.is_ok() && reader.is_whole() {
      self.state = State::Resolving { request, reader };
      return Ok(true);
    }
    self.finish_pack(request, reader, read, site, admission);
    Ok(true)
  }

  /// The pack finished into `site` (its deltas all resolved, or the read refused), and its checks begun.
  fn finish_pack<S: Keep + Repository>(
    &mut self,
    request: Request,
    reader: unpack::Reader<'static, NoIndex>,
    read: Result<(), unpack::Refusal>,
    site: &mut S,
    admission: &mut Admission,
  ) {
    let mut fscking = Fscking {
      site,
      format: self.advertised.format,
      refused: &mut self.refused,
      special: &mut self.special,
    };
    let unpacked = finished_pack(reader, read, &mut fscking, admission);
    let unpacked = match (unpacked, self.refused) {
      (Ok(kept), Some((id, finding))) => Err(super::PackRefused {
        objects: u32::try_from(kept.objects.len()).ok(),
        why: super::PackRefusal::Fsck { id, finding },
      }),
      (unpacked, _) => unpacked,
    };
    let special = core::mem::take(&mut self.special);
    self.state = State::Checking(Checking::fscked_with_special(
      request,
      Some(unpacked),
      special,
    ));
  }

  /// Whether the client asked its answer multiplexed (`side-band-64k`), once its request is read: what lets the edge
  /// send git's keepalive while the push is answered (A-722's keepalive, git's `receive.keepAlive`).
  pub fn side_band(&self) -> bool {
    self
      .request()
      .is_some_and(|request| request.requested.side_band_64k)
  }

  /// The push's request, once it is read whole and until the push is checked and taken ([`Session::take_checked`]) or
  /// refused.
  pub fn request(&self) -> Option<&Request> {
    match &self.state {
      State::Pack { request, .. } | State::Resolving { request, .. } => Some(request),
      State::Checking(checking) => Some(checking.request()),
      State::Checked(checked) => Some(&checked.request),
      State::Request(_) | State::Done => None,
    }
  }

  /// Whether the pack's checks are still to make: its waiting deltas still resolving, or its checks under way.
  pub fn is_checking(&self) -> bool {
    matches!(self.state, State::Resolving { .. } | State::Checking(_))
  }

  /// Whether the pack's waiting deltas are still resolving ([`Session::resolve_turn_into`]).
  pub fn is_resolving(&self) -> bool {
    matches!(self.state, State::Resolving { .. })
  }

  /// One turn of resolving the whole pack's waiting deltas into `site` (A-724's starvation fix), for `quantum` and at
  /// least one unit: whether they are all resolved and the pack's checks begun. Answers so at once where nothing is
  /// resolving. A delta that cannot resolve ends the reading as a refused pack, its checks begun, as a short pack's are.
  pub fn resolve_turn_into<S: Keep + Repository>(
    &mut self,
    site: &mut S,
    admission: &mut Admission,
    quantum: Quantum<'_>,
  ) -> Result<bool, Refusal> {
    let State::Resolving { reader, .. } = &mut self.state else {
      return Ok(true);
    };
    let mut fscking = Fscking {
      site: &mut *site,
      format: self.advertised.format,
      refused: &mut self.refused,
      special: &mut self.special,
    };
    let turned = reader.resolve_turn(admission, &mut super::Thin(&mut fscking), quantum);
    let read = match turned {
      Ok(false) => return Ok(false),
      Ok(true) => Ok(()),
      Err(refusal) => Err(refusal),
    };
    let State::Resolving { request, reader } = core::mem::replace(&mut self.state, State::Done)
    else {
      return Ok(true);
    };
    self.finish_pack(request, reader, read, site, admission);
    Ok(true)
  }

  /// [`Session::resolve_turn_into`] in deferred-naming mode: applies ready waiting deltas into
  /// `batch` without running synchronous SHA-1DC/BLAKE3/fsck so the caller can scatter verification
  /// across all daemon shards and settle each batch via [`Session::settle_deferred`]. Transitions
  /// `State::Resolving` to `State::Checking` once all waiting deltas finish (or on unpack refusal).
  pub fn resolve_turn_deferred<S: Keep + Repository>(
    &mut self,
    site: &mut S,
    admission: &mut Admission,
    batch: &mut unpack::DeferredBatch,
    (target_bytes, quantum): (u64, Quantum<'_>),
  ) -> Result<unpack::DeferredResolve, Refusal> {
    let State::Resolving { reader, .. } = &mut self.state else {
      return Ok(unpack::DeferredResolve::Done);
    };
    let mut fscking = Fscking {
      site: &mut *site,
      format: self.advertised.format,
      refused: &mut self.refused,
      special: &mut self.special,
    };
    let turned = reader.resolve_turn_deferred(
      admission,
      &mut super::Thin(&mut fscking),
      batch,
      (target_bytes, quantum),
    );
    let read = match turned {
      Ok(unpack::DeferredResolve::Batch) => return Ok(unpack::DeferredResolve::Batch),
      Ok(unpack::DeferredResolve::Away) => return Ok(unpack::DeferredResolve::Away),
      Ok(unpack::DeferredResolve::Done) => Ok(()),
      Err(refusal) => {
        batch.release_all(admission);
        Err(refusal)
      }
    };
    let State::Resolving { request, reader } = core::mem::replace(&mut self.state, State::Done)
    else {
      return Ok(unpack::DeferredResolve::Done);
    };
    self.finish_pack(request, reader, read, site, admission);
    Ok(unpack::DeferredResolve::Done)
  }

  /// Transitions `State::Resolving` to `State::Checking` with a worker-detected collision refusal
  /// encountered while settling a deferred resolution batch.
  pub fn finish_resolving_with_error<S: Keep + Repository>(
    &mut self,
    err: unpack::Refusal,
    site: &mut S,
    admission: &mut Admission,
  ) {
    let State::Resolving { request, reader } = core::mem::replace(&mut self.state, State::Done)
    else {
      return;
    };
    self.finish_pack(request, reader, Err(err), site, admission);
  }

  /// One turn of the pack's checks against `repository`, the objects read back from the session's
  /// own memory, for `quantum` and at least one unit of work (A-719): whether the push is now
  /// checked ([`Session::checked`]). A push already checked answers so at once; one with no checks
  /// under way, `false`. A refusal ends the session.
  pub fn check_turn<R: Repository + ?Sized>(
    &mut self,
    repository: &R,
    admission: &mut Admission,
    quantum: Quantum<'_>,
  ) -> Result<bool, Refusal> {
    let mut held = core::mem::take(&mut self.held);
    if self.is_resolving() {
      let mut site = InMemory {
        repository,
        held: &mut held,
      };
      let resolved = self.resolve_turn_into(&mut site, admission, quantum);
      self.held = held;
      // A turn resolving the pack's deltas is this call's turn; its checks begin with the next.
      return resolved.map(|_| false);
    }
    let turned = self.check_turn_with((repository, &held), admission, quantum);
    self.held = held;
    turned
  }

  /// [`Session::check_turn`], the objects read back from `contents`, the caller's site.
  pub fn check_turn_with<R: Repository + ?Sized, C: Contents>(
    &mut self,
    (repository, contents): (&R, &C),
    admission: &mut Admission,
    quantum: Quantum<'_>,
  ) -> Result<bool, Refusal> {
    let checking = match &mut self.state {
      State::Checking(checking) => checking,
      State::Checked(_) => return Ok(true),
      _ => return Ok(false),
    };
    let turned = checking.turn((repository, contents), admission, quantum);
    let done = match turned {
      Ok(done) => done,
      Err(refusal) => {
        self.state = State::Done;
        return Err(refusal);
      }
    };
    if !done {
      return Ok(false);
    }
    let State::Checking(checking) = core::mem::replace(&mut self.state, State::Done) else {
      return Ok(false);
    };
    let checked = checking.checked((repository, contents), &self.hidden, admission)?;
    self.state = State::Checked(checked);
    Ok(true)
  }
}

/// The caller's site as the session reads a pack into it, every object checked as git checks a push
/// as it is kept (A-719 part 3; `super::fsck`), the first refused noted and any special dotfiles
/// referenced by trees recorded to check once the pack is unpacked.
struct Fscking<'f, S: ?Sized> {
  /// The caller's site.
  site: &'f mut S,
  /// The hash the pack's objects are in.
  format: crate::object_name::ObjectFormat,
  /// The first object refused, if any.
  refused: &'f mut Option<(ObjectId, crate::objects::FsckId)>,
  /// Any `.gitmodules` and `.gitattributes` entries referenced by pushed trees.
  special: &'f mut Vec<super::fsck::SpecialBlob>,
}

impl<S: Repository + ?Sized> Repository for Fscking<'_, S> {
  fn object(&self, id: &ObjectId) -> Option<(crate::object_name::Kind, &[u8])> {
    self.site.object(id)
  }

  fn holds(&self, id: &ObjectId) -> bool {
    self.site.holds(id)
  }

  fn away(&self) -> Option<u64> {
    self.site.away()
  }
}

impl<S: Contents + Repository + ?Sized> Contents for Fscking<'_, S> {
  fn content(&self, at: usize, id: &ObjectId) -> Option<std::borrow::Cow<'_, [u8]>> {
    self.site.content(at, id)
  }

  fn away(&self) -> Option<u64> {
    Contents::away(&*self.site).or_else(|| Repository::away(&*self.site))
  }
}

impl<S: Keep + Repository + ?Sized> Keep for Fscking<'_, S> {
  fn keep(
    &mut self,
    at: usize,
    object: unpack::Object,
    hold: crate::Hold,
    admission: &mut Admission,
  ) -> Result<(), unpack::Refusal> {
    if self.refused.is_none()
      && object.kind != crate::object_name::Kind::Blob
      && !self.site.holds(&object.id)
      && let Some(finding) =
        super::fsck::check_object(object.kind, &object.content, self.format, self.special)
    {
      *self.refused = Some((object.id, finding));
    }
    self.site.keep(at, object, hold, admission)
  }

  fn release(&mut self, hold: crate::Hold, admission: &mut Admission) {
    self.site.release(hold, admission);
  }
}
