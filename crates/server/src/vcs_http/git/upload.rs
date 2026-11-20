//! git's upload-pack behind the HTTP edge (A-625 part 3; §4.17.2 "A fetch" and "Transports" item 1; A-640 part 2's
//! `slates_git::upload`): `GET <repo>/info/refs?service=git-upload-pack` answered with the advertisement, and a `POST
//! <repo>/git-upload-pack` answered as `git upload-pack --stateless-rpc` answers one, its request held whole once its
//! body has ended (upload-pack reads a command request whole, `Asked::Request`) and charged to the request's room.
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
use crate::vcs_http::{Exchange, Failed};

/// Where repositories' stores are lent to upload-pack, afresh for each step of an answer (the module doc).
pub(crate) trait StoreLender {
  /// Where a fetch's pack comes from (A-639's fragments, through A-653's source over the store's stream).
  type Packs: PackSource;

  /// Whether the store of repository `id` is held here.
  fn holds(&self, id: RepositoryId) -> bool;

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
}

/// git's services with upload-pack served from the stores `lender` lends, each request admitted `room` bytes;
/// receive-pack is not its half: it answers `503` for it ([`super::Halves`] pairs the two).
#[derive(Clone, Debug)]
pub(crate) struct UploadServices<L> {
  /// Where the stores are lent.
  lender: L,
  /// The bytes each request may hold: the edge's request room.
  room: u64,
}

impl<L> UploadServices<L> {
  /// Upload-pack over `lender`'s stores, each request admitted `room` bytes.
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
    Ok(Upload {
      lender: self.lender.clone(),
      repository: start.repository.id,
      reader: start.principal.clone(),
      version: start.version,
      advertise: start.advertise,
      admission: Admission::new(self.room),
      request: Vec::new(),
      answer: None,
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

  async fn body_end(&mut self) -> Result<(), Failed> {
    let Self {
      lender,
      repository,
      reader,
      version,
      advertise,
      admission,
      request,
      answer,
    } = self;
    let asked = if *advertise {
      Asked::Advertise
    } else {
      Asked::Request(request.as_slice())
    };
    let made = lender
      .lend((*repository, reader), |served, packs| {
        upload::serve(served, *version, asked, packs, admission)
      })
      .ok_or(Failed(Status::SERVICE_UNAVAILABLE))?;
    *answer = Some(made);
    Ok(())
  }

  async fn reply(&mut self, out: &mut Vec<u8>) -> Result<bool, Failed> {
    let Self {
      lender,
      repository,
      admission,
      answer,
      ..
    } = self;
    let Some(answer) = answer.as_mut() else {
      return Err(Failed(Status::INTERNAL_SERVER_ERROR));
    };
    let written = lender
      .lend_objects(*repository, |served, packs| {
        answer.write(served, packs, admission, out, MAX_LEN)
      })
      .ok_or(Failed(Status::SERVICE_UNAVAILABLE))?;
    Ok(written == Written::More)
  }
}
