//! git's upload-pack behind the HTTP edge (A-625 part 3; §4.17.2 "A fetch" and "Transports" item 1; A-640 part 2's
//! `slates_git::upload`): `GET <repo>/info/refs?service=git-upload-pack` answered with the advertisement, and a `POST
//! <repo>/git-upload-pack` answered as `git upload-pack --stateless-rpc` answers one, its request held whole once its
//! body has ended (upload-pack reads a command request whole, `Asked::Request`) and charged to the request's room: the
//! edge's request room, or what the repository's shard could admit when the request began where that is less
//! ([`StoreLender::room`], A-713). A fetch fits it whatever the repository's size (A-745, §4.17.1 "A fetch's answer"): it
//! is answered in turns (`slates_git::upload::serve_in_turns`), its pack's objects chosen a unit at a time within the
//! shard's step quantum as the answer is written (`Answer::write_within`, the pack source's `turn`), its sets kept as
//! runs or bits and its pack framed in packets sized from the room.
//!
//! **The store, lent a step at a time.** A repository's store is lent afresh for each step of an answer ([`StoreLender`];
//! A-640 part 2: "It is lent afresh for each step of an answer, so a shard's state is never held across a wait"), and the
//! answer is written a slice at a time, one whole pkt-line's room a step (`slates_git::pktline::MAX_LEN`, the format's
//! own limit, so a band-1 packet goes out whole), each step one reply run of the edge, which yields between runs: a clone
//! of any size passes in bounded memory and never holds its shard. A request upload-pack refuses is answered too, in
//! git's words (`ERR`, or band 3 under `sideband-all`), so the reply is `200` with those words, as `http-backend`'s is.
//!
//! **What each step is lent** (A-682). The step that answers the request, the advertisement or a command, is lent the
//! refs its asker may read and the branch `HEAD` names, read when it runs ([`StoreLender::lend`]); every later step only
//! writes the answer, handing the store to the pack's source, which names what it sends by position, so it is lent the
//! objects alone ([`StoreLender::lend_objects`]) and costs its room and one packet whatever the repository's refs. The
//! daemon's lender is the owner shard's map of stores (`crate::vcs_store::upload::ShardStores`, A-662's map, with A-639's
//! stream as the pack's source); a repository whose store it does not hold is `503`.

use slates_db::catalog::{Principal, RepositoryId};
use slates_git::Admission;
use slates_git::http::{Service, Version};
use slates_git::pktline::MAX_LEN;
use slates_git::upload::{self, Answer, Asked, PackSource, Served, Written};
use slates_http::reply::Status;

use super::{GitServices, ServiceRefused, Start};
use crate::vcs_http::{ClientGone, Exchange, Failed};

/// Where repositories' stores are lent to upload-pack, afresh for each step of an answer (the module doc).
pub(crate) trait StoreLender {
  /// Where a fetch's pack comes from (A-639's fragments, through A-653's source over the store's stream).
  type Packs: PackSource;

  /// Whether the store of repository `id` is held here.
  fn holds(&self, id: RepositoryId) -> bool;

  /// What the shard the stores live on could admit now (A-713), as a push is admitted (A-704): a request is admitted the
  /// edge's request room, or this where it is less (A-745). None when the shard's state is not lent.
  fn room(&mut self, repository: RepositoryId) -> Option<u64>;

  /// Runs `step` over the store of the repository `asked` names as upload-pack answers a request of the principal it
  /// names, with the pack source: the refs that principal may read, in name order and each once, and the branch `HEAD`
  /// names, read as the step runs; none when the store is not held here.
  fn lend<R>(
    &mut self,
    asked: (RepositoryId, &Principal),
    step: impl FnOnce(&Served<'_>, &mut Self::Packs) -> R,
  ) -> Option<R>;

  /// Runs `step` over repository `id`'s store for an answer's next bytes, with the pack source: its objects alone, no ref
  /// read (the refs and `HEAD` lent empty), since such a step hands the store to the pack's source alone (the module
  /// doc); none when the store is not held here.
  fn lend_objects<R>(
    &mut self,
    id: RepositoryId,
    step: impl FnOnce(&Served<'_>, &mut Self::Packs) -> R,
  ) -> Option<R>;

  /// A request's hold on the generation of a repository its answer began on (A-745, §4.17.1 "A fetch's answer" rule 4):
  /// what every step of that answer is lent, whatever lands, is re-based or rebuilt meanwhile; given back by its drop.
  type Hold;

  /// Holds repository `id` as it stands now for a request of `reader`, with the refs that principal may read and the
  /// branch `HEAD` names as they stand (as [`StoreLender::lend`] reads them); none when the store is not held here.
  fn hold(&mut self, id: RepositoryId, reader: &Principal) -> Option<Self::Hold>;

  /// Runs `step` over the generation `hold` holds, with the pack source; none when it can no longer be lent.
  fn lend_held<R>(
    &mut self,
    hold: &Self::Hold,
    step: impl FnOnce(&Served<'_>, &mut Self::Packs) -> R,
  ) -> Option<R>;

  /// Makes repository `id`'s store ready for a fetch's answer before it is made (A-718): the shard's lender encodes every
  /// version not yet encoded as its fragments, a turn at a time, yielding between turns, so the answer copies them. A
  /// lender whose store needs nothing made does nothing.
  async fn encode(&mut self, _id: RepositoryId) {}
}

/// git's services with upload-pack served from the stores `lender` lends, each request admitted the room its stores'
/// shard could admit ([`StoreLender::room`]); receive-pack is not its half: it answers `503` for it ([`super::Halves`]
/// pairs the two).
#[derive(Clone, Debug)]
pub(crate) struct UploadServices<L> {
  /// Where the stores are lent.
  lender: L,
  /// The room one request may hold: the edge's (A-745, §4.17.1 "A fetch's answer" rule 2: a request's room bounds a
  /// slice, never a repository).
  room: u64,
}

impl<L> UploadServices<L> {
  /// Upload-pack over `lender`'s stores, each request admitted `room`, the edge's request room, or what its stores'
  /// shard could admit when it began where that is less.
  pub(crate) fn new(lender: L, room: u64) -> Self {
    Self { lender, room }
  }
}

impl<L: StoreLender + Clone> GitServices for UploadServices<L> {
  type Exchange = Upload<L>;

  fn start(&mut self, start: &Start<'_>) -> Result<Upload<L>, ServiceRefused> {
    if start.service != Service::UploadPack || !self.lender.holds(start.repository.id) {
      return Err(ServiceRefused::Unavailable);
    }
    let shard = self
      .lender
      .room(start.repository.id)
      .ok_or(ServiceRefused::Unavailable)?;
    let room = self.room.min(shard);
    Ok(Upload {
      lender: self.lender.clone(),
      repository: start.repository.id,
      reader: start.principal.clone(),
      version: start.version,
      advertise: start.advertise,
      admission: Admission::new(room),
      request: Vec::new(),
      answer: None,
      generation: None,
    })
  }
}

/// One upload-pack exchange: the advertisement, or one request held whole, answered once the body has ended and its
/// answer written a step at a time (the module doc).
pub(crate) struct Upload<L: StoreLender> {
  /// Where the store is lent.
  lender: L,
  /// The repository.
  repository: RepositoryId,
  /// Who asks: the refs the request's step is lent are the ones this principal may read.
  reader: Principal,
  /// The protocol version the client asked for.
  version: Version,
  /// Whether the exchange is the advertisement.
  advertise: bool,
  /// The request's room: its bytes, and all upload-pack keeps answering it.
  admission: Admission,
  /// The request, as its body arrived.
  request: Vec<u8>,
  /// The answer, once made.
  answer: Option<Answer<<L::Packs as PackSource>::Pack>>,
  /// The generation a request's answer began on, held until the answer is written (A-745 rule 4).
  generation: Option<L::Hold>,
}

impl<L: StoreLender> Exchange for Upload<L> {
  async fn body(&mut self, run: &[u8]) -> Result<(), Failed> {
    self
      .admission
      .charge(u64::try_from(run.len()).unwrap_or(u64::MAX))
      .map_err(|_| Failed(Status::CONTENT_TOO_LARGE))?;
    self.request.extend_from_slice(run);
    Ok(())
  }

  async fn body_end(&mut self, _gone: ClientGone<'_>) -> Result<(), Failed> {
    let Self {
      lender,
      repository,
      reader,
      version,
      advertise,
      admission,
      request,
      answer,
      generation,
    } = self;
    let unavailable = Failed(Status::SERVICE_UNAVAILABLE);
    let made = if *advertise {
      lender.lend((*repository, reader), |served, packs| {
        upload::serve_in_turns(served, *version, Asked::Advertise, packs, admission)
      })
    } else {
      lender.encode(*repository).await;
      let held = lender.hold(*repository, reader).ok_or(unavailable)?;
      let made = lender.lend_held(&held, |served, packs| {
        upload::serve_in_turns(
          served,
          *version,
          Asked::Request(request.as_slice()),
          packs,
          admission,
        )
      });
      *generation = Some(held);
      made
    }
    .ok_or(unavailable)?;
    *answer = Some(made);
    Ok(())
  }

  async fn reply(&mut self, out: &mut Vec<u8>, _gone: ClientGone<'_>) -> Result<bool, Failed> {
    let Self {
      lender,
      repository,
      admission,
      answer,
      generation,
      ..
    } = self;
    let Some(made) = answer.as_mut() else {
      return Err(Failed(Status::INTERNAL_SERVER_ERROR));
    };
    let step = |served: &Served<'_>, packs: &mut L::Packs| {
      let mut spent = packs.turn();
      made.write_within(served, packs, admission, out, MAX_LEN, &mut spent)
    };
    let written = match generation.as_ref() {
      Some(held) => lender.lend_held(held, step),
      None => lender.lend_objects(*repository, step),
    }
    .ok_or(Failed(Status::SERVICE_UNAVAILABLE))?;
    if written != Written::More {
      // The answer is written: its generation is given back.
      *generation = None;
    }
    Ok(written == Written::More)
  }
}
