//! git's upload-pack behind the HTTP edge (A-625 part 3; §4.17.2 "A fetch" and "Transports" item 1; A-640 part 2's
//! `slates_git::upload`): `GET <repo>/info/refs?service=git-upload-pack` answered with the advertisement, and a `POST
//! <repo>/git-upload-pack` answered as `git upload-pack --stateless-rpc` answers one, its request held whole once its
//! body has ended (upload-pack reads a command request whole, `Asked::Request`) and charged to the request's room: the
//! edge's request room, or what the repository's shard could admit when the request began where that is less
//! ([`StoreLender::room`], A-713). Its answer is admitted what the repository's shard admits it, never one region
//! (A-855: its walk's tables grow with the repository); one refused past that room before its pack begins is held and
//! made again once the shard gives more (`Upload::wait_for_room`), never cut. A fetch fits it whatever the repository's
//! size (A-745, §4.17.1 "A fetch's answer"): it
//! is answered in turns (`slates_git::upload::serve_in_turns`), its pack's objects chosen a unit at a time within the
//! shard's step quantum as the answer is written (`Answer::write_within`, the pack source's `turn`), its sets kept as
//! runs or bits and its pack framed in packets sized from the room.
//!
//! **git's keepalive** (A-722's on the fetch). An answer's step may write nothing: its pack's objects still being
//! chosen after the `packfile` section's head is written, or its source framing nothing new. Once the answer has been
//! silent for the keepalive interval (`DaemonConfig::git_keepalive_ns`, git's own five seconds), the step writes git's
//! keepalive where the answer may carry one (`Answer::write_keepalive`: inside a banded pack's section, as git's
//! `create_pack_file` writes `0005\1` while `pack-objects` is silent), so a client reading with a deadline is never left
//! waiting on a pack still being planned. The head is written at once (A-722's `StatusMode::AtHead`).
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
/// Counter: a git fetch over HTTP answered refused (a pack the store could not finish, a request refused), its typed
/// refusal named in the log. Format: a counter name.
pub(crate) const UPLOAD_REFUSED: &str = "vcs_http.git.upload_refused";

/// Counter: a git fetch over HTTP whose negotiation ran past its room before its pack began, answered again within the
/// wider room its repository's shard gave it later (A-855: a refusal costs time, never an answer). Format: a counter
/// name.
pub(crate) const ANSWERED_AGAIN: &str = "vcs_http.git.answered_again";

/// Counter: a fetch's answer made again once the let-go window a read of it met was brought back (A-864's readers;
/// main 01a20182's r52). Format: a counter name.
pub(crate) const READ_THROUGH_AGAIN: &str = "vcs_http.git.read_through_again";

/// How long a fetch held past its room waits for its repository's shard to give it more (A-855): the daemon's
/// observation budget, the longest it waits on any party before judging it, git's keepalive written meanwhile; past it
/// the refusal is answered as it was before.
const ROOM_PATIENCE_NS: u64 = crate::daemon::OBSERVE_BUDGET_NS;

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

  /// The answer made over `hold` wrote `bytes` more (A-724 piece 3d): what its fetch is learned to use once it ends. A
  /// lender that learns nothing does nothing.
  fn wrote(&mut self, _hold: &Self::Hold, _bytes: u64) {}

  /// A fetch's answer wrote `bytes` past its first write that waited for the peer, taken in `ns` (A-859 part A,
  /// `crate::vcs_store::upload::Pacing`): what the shard's link price is learned from. A lender that learns nothing does
  /// nothing.
  fn paced(&mut self, _taught: (u64, u64)) {}

  /// Where an answer's `pack` reads next and the let-go window its last turn stopped at (A-864 piece 2): none by
  /// default, for a source that reads no let-go window.
  fn pack_reads(_pack: &<Self::Packs as PackSource>::Pack) -> PackReads {
    PackReads::default()
  }

  /// Brings back, before an answer's next turn, the let-go window its last turn stopped at and those its next turn
  /// reads first (A-864 piece 2). Nothing by default.
  async fn read_ahead(&mut self, _reads: PackReads, _resident: &mut Resident) {}

  /// Lets go again the windows of `resident` the answer's cursor has passed (A-864 piece 3): those of its pack before
  /// where `next` reads, and those of a pack it has left; every one with no `next` (the answer ended). Nothing by
  /// default.
  fn let_go_behind(
    &mut self,
    _resident: &mut Resident,
    _next: Option<slates_git::store::packed::Waited>,
  ) {
  }

  /// Brings back the let-go window `waited` names (A-864's readers): whether it is back. False by default, for a lender
  /// whose store lets nothing go.
  async fn bring_back(&mut self, _waited: slates_git::store::packed::Waited) -> bool {
    false
  }

  /// Brings back the let-go window `waited` names and records it in `resident` so it is let go
  /// again once the fetch ends (or is dropped): defaults to [`Self::bring_back`].
  async fn bring_back_tracked(
    &mut self,
    waited: slates_git::store::packed::Waited,
    _resident: &mut Resident,
  ) -> bool {
    self.bring_back(waited).await
  }

  /// Makes repository `id`'s store ready for a fetch's answer before it is made (A-718): the shard's lender encodes every
  /// version not yet encoded as its fragments, a turn at a time, yielding between turns, so the answer copies them. A
  /// lender whose store needs nothing made does nothing.
  async fn encode(&mut self, _id: RepositoryId) {}
}

/// Whether `request`, upload-pack's at `version`, may be a fetch, so its store's versions are derived and encoded before
/// it is answered (A-718): every request before protocol v2 (its body is a fetch's negotiation), and a v2 request naming
/// `fetch` or naming no command this crate reads (its answer refuses it). A v2 `ls-refs`, `object-info` or `bundle-uri`
/// reads neither.
pub(crate) fn fetches(version: Version, request: &[u8]) -> bool {
  !matches!(version, Version::V2)
    || slates_git::v2::request::command_of(request)
      .is_none_or(|command| command == slates_git::v2::request::Command::Fetch)
}

/// A window an answer's read-through brought back (A-864 piece 3): its pack, where it ends there, and its identity, let
/// go again once the answer's cursor has passed it; or, for a window a read stopped at, once the cursor has moved off the
/// object that read was for (`pinned`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Brought {
  /// The pack.
  pub(crate) pack: slates_vcs::ContentId,
  /// Where the window ends in the pack.
  pub(crate) end: u64,
  /// The window.
  pub(crate) window: slates_vcs::ContentId,
  /// For a window a read stopped at, where the stream read next when it stopped: the object under way, whose read may
  /// need the window wherever it lies in the pack (a delta's base, earlier in it). Held while the stream still reads
  /// there (goal 93, r130's shallow clone: a base brought back and let go behind the cursor in one turn was met again
  /// and the answer refused `Unreadable`).
  pub(crate) pinned: Option<slates_git::store::packed::Waited>,
}

/// The windows an answer brought back and has not let go (A-864 piece 3): a turn's look-ahead, and the windows the
/// object under way stopped at (`Brought::pinned`), the cursor passing each. The pinned windows are at most those one
/// object's read lies in: its delta chain's entries, at most the deepest chain a push carried, each at most the largest
/// object and so its bytes' windows and one more.
pub(crate) type Resident = Vec<Brought>;

/// Where an answer's pack reads next and the let-go window its last turn stopped at (A-864 piece 2,
/// `slates_git::upload::Streamed::pack_reads`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PackReads {
  /// Where its next turn begins to read a pushed pack.
  pub(crate) next: Option<slates_git::store::packed::Waited>,
  /// The let-go window its last turn stopped at.
  pub(crate) stopped: Option<slates_git::store::packed::Waited>,
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
  /// How long an answer may write nothing before git's keepalive is written (`DaemonConfig::git_keepalive_ns`).
  keepalive_ns: u64,
  /// How long an answer held past its room waits for more ([`ROOM_PATIENCE_NS`]).
  room_patience_ns: u64,
}

impl<L> UploadServices<L> {
  /// Upload-pack over `lender`'s stores, each request admitted `room`, the edge's request room, or what its stores'
  /// shard could admit when it began where that is less; git's keepalive written after `keepalive_ns` of a silent
  /// answer (the module doc).
  pub(crate) fn new(lender: L, room: u64, keepalive_ns: u64) -> Self {
    Self {
      lender,
      room,
      keepalive_ns,
      room_patience_ns: ROOM_PATIENCE_NS,
    }
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
    // A-855: the request's own bytes are held to the edge's request room; its answer, whose walk keeps tables of a bit
    // or a word an object and a few words a want (A-745 rule 1), to what its repository's shard admits it (A-724's
    // share), never one region: git/git's clone of master and 1,019 tags needs more than a region's 256 KiB.
    let room = self.room.min(shard);
    Ok(Upload {
      lender: self.lender.clone(),
      repository: start.repository.id,
      reader: start.principal.clone(),
      version: start.version,
      advertise: start.advertise,
      request_room: Admission::new(room),
      admission: Admission::new(shard),
      request: Vec::new(),
      answer: None,
      generation: None,
      keepalive_ns: self.keepalive_ns,
      last_written_ns: slates_rt::futures::now_ns(),
      admitted: shard,
      held_since: None,
      room_patience_ns: self.room_patience_ns,
      pacing: crate::vcs_store::upload::Pacing::default(),
      resident: Resident::new(),
      last_away: None,
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
  /// The request's room: its bytes as they arrive (A-745: a request's room bounds the request, and its answer is
  /// admitted on its own).
  request_room: Admission,
  /// The answer's room: all upload-pack keeps answering the request, the room its repository's shard admitted it when it
  /// began (A-855: the shard's, never the edge's request room), a request's own bytes never taken from it, as the SSH
  /// edge admits each answer afresh (`vcs_ssh::git`'s `serve`).
  admission: Admission,
  /// The request, as its body arrived.
  request: Vec<u8>,
  /// The answer, once made.
  answer: Option<Answer<<L::Packs as PackSource>::Pack>>,
  /// The generation a request's answer began on, held until the answer is written (A-745 rule 4).
  generation: Option<L::Hold>,
  /// How long the answer may write nothing before git's keepalive is written.
  keepalive_ns: u64,
  /// When the exchange last wrote: its head (written at once), or the answer's last bytes.
  last_written_ns: u64,
  /// The room the answer was made within (A-855): an answer held past it is made again once its repository's shard can
  /// give more.
  admitted: u64,
  /// Since when the answer has been held past its room, if it is.
  held_since: Option<u64>,
  /// How long it waits held for more room ([`ROOM_PATIENCE_NS`]).
  room_patience_ns: u64,
  /// What its answer's writes have taught of its link (A-859 part A).
  pacing: crate::vcs_store::upload::Pacing,
  /// The windows its answer brought back and has not let go (A-864 piece 3).
  resident: Resident,
  /// The let-go window its answer was last held for, until it writes again (A-864's readers' progress).
  last_away: Option<slates_git::store::packed::Waited>,
}

impl<L: StoreLender> Exchange for Upload<L> {
  /// A write of the answer (A-859 part A): learned into the shard's link price once a write of it has waited for the
  /// peer; an advertisement's teaches nothing, its bytes too few to fill a socket.
  fn sent(&mut self, sent: crate::vcs_store::upload::Sent) {
    if self.advertise || self.answer.is_none() {
      return;
    }
    if let Some(taught) = self.pacing.sent(sent) {
      self.lender.paced(taught);
    }
  }

  async fn body(&mut self, run: &[u8]) -> Result<(), Failed> {
    self
      .request_room
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
      resident,
      ..
    } = self;
    let unavailable = Failed(Status::SERVICE_UNAVAILABLE);
    // A-864's readers: an answer refused where a read of it met a let-go window is made again once the window is back,
    // nothing of it written yet; the same window met twice with none brought back between ends the retries.
    let mut last = None;
    let made = if *advertise {
      loop {
        let (made, away) = lender
          .lend((*repository, reader), |served, packs| {
            let made = upload::serve_in_turns(served, *version, Asked::Advertise, packs, admission);
            let away = made.refusal().and(served.repository.waiting());
            (made, away)
          })
          .ok_or(unavailable)?;
        match away {
          Some(waited)
            if last != Some(waited) && lender.bring_back_tracked(waited, resident).await =>
          {
            last = Some(waited)
          }
          _ => break made,
        }
      }
    } else {
      // A-718: the store's versions derived and encoded before a fetch is answered; a protocol v2 command other than
      // `fetch` reads neither (goal 93, r405: `git ls-remote` after a whole push waited 29 to 54 s for them).
      if fetches(*version, request) {
        lender.encode(*repository).await;
      }
      let held = lender.hold(*repository, reader).ok_or(unavailable)?;
      // A-855: a negotiation run past its room is held, not answered, until the room grows (`Upload::wait_for_room`).
      let made = loop {
        let (made, away) = lender
          .lend_held(&held, |served, packs| {
            let made = upload::serve_in_turns(
              served,
              *version,
              Asked::Request(request.as_slice()),
              packs,
              admission,
            )
            .holding();
            let away = made.refusal().and(served.repository.waiting());
            (made, away)
          })
          .ok_or(unavailable)?;
        match away {
          Some(waited)
            if last != Some(waited) && lender.bring_back_tracked(waited, resident).await =>
          {
            last = Some(waited)
          }
          _ => break made,
        }
      };
      *generation = Some(held);
      made
    };
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
      keepalive_ns,
      last_written_ns,
      resident,
      last_away,
      ..
    } = self;
    let Some(made) = answer.as_mut() else {
      return Err(Failed(Status::INTERNAL_SERVER_ERROR));
    };
    read_ahead_of(lender, made, resident).await;
    let before = out.len();
    let step = |served: &Served<'_>, packs: &mut L::Packs| {
      let mut spent = packs.turn();
      made.write_within(served, packs, admission, out, MAX_LEN, &mut spent)
    };
    let written = match generation.as_ref() {
      Some(held) => lender.lend_held(held, step),
      None => lender.lend_objects(*repository, step),
    }
    .ok_or(Failed(Status::SERVICE_UNAVAILABLE))?;
    if let Some(held) = generation.as_ref() {
      lender.wrote(
        held,
        u64::try_from(out.len().saturating_sub(before)).unwrap_or(u64::MAX),
      );
    }
    let now = slates_rt::futures::now_ns();
    if out.len() > before {
      *last_away = None;
    }
    if out.len() > before
      || (now.saturating_sub(*last_written_ns) >= *keepalive_ns && made.write_keepalive(out))
    {
      *last_written_ns = now;
    }
    if made.held().is_some() {
      self.wait_for_room().await?;
      return Ok(true);
    }
    if let Some(waited) = made.held_away() {
      self.read_through_held(waited).await?;
      return Ok(true);
    }
    if written != Written::More {
      // The answer is written: its generation is given back, and every window it brought back let go again.
      *generation = None;
      lender.let_go_behind(resident, None);
      // A refused answer (a pack the store could not finish ends with git's abort words, A-639) is counted and named in
      // the log by its typed refusal, so a client's "possible repository corruption" has its cause on the server.
      if let Some(refusal) = made.refusal() {
        slates_machine::log_line!(
          "slates-server: a git fetch of repository {repository:?} was answered refused: {refusal:?}"
        );
        let _ = crate::state::with_state_counted(|state| state.count(UPLOAD_REFUSED, 1));
      }
    }
    Ok(written == Written::More)
  }
}

/// Before an answer's next turn (A-864 pieces 2 and 3): the windows its pack reads next brought back first, off the
/// turn, so the turn never meets one let go; those its cursor has passed let go again.
async fn read_ahead_of<L: StoreLender>(
  lender: &mut L,
  made: &Answer<<L::Packs as PackSource>::Pack>,
  resident: &mut Resident,
) {
  if let Some(pack) = made.pack_source() {
    let reads = L::pack_reads(pack);
    lender.read_ahead(reads, resident).await;
    if reads.next.is_some() {
      lender.let_go_behind(resident, reads.next);
    }
  }
}

impl<L: StoreLender> Drop for Upload<L> {
  fn drop(&mut self) {
    if !self.resident.is_empty() {
      self.lender.let_go_behind(&mut self.resident, None);
    }
  }
}

impl<L: StoreLender> Upload<L> {
  /// A turn of an answer held for a let-go window its read met before its pack (A-864's readers; main 01a20182's r52):
  /// the window brought back and tracked in `self.resident` so it is released when the fetch ends;
  /// when the negotiation paused in place, resumes it without restarting; otherwise rebuilds the
  /// answer over the same generation passing over what it wrote ([`Answer::skip_written`]), counted
  /// [`READ_THROUGH_AGAIN`].
  async fn read_through_held(
    &mut self,
    waited: slates_git::store::packed::Waited,
  ) -> Result<(), Failed> {
    let Self {
      lender,
      version,
      request,
      answer,
      generation,
      admitted,
      admission,
      resident,
      last_away,
      ..
    } = self;
    let Some(made) = answer.as_mut() else {
      return Err(Failed(Status::INTERNAL_SERVER_ERROR));
    };
    let Some(held) = generation.as_ref() else {
      made.give_up();
      return Ok(());
    };
    if *last_away == Some(waited) || !lender.bring_back_tracked(waited, resident).await {
      made.give_up();
      return Ok(());
    }
    *last_away = Some(waited);
    if made.resume_away() {
      return Ok(());
    }
    let skip = made.written();
    let mut again_room = Admission::new(*admitted);
    let again = lender
      .lend_held(held, |served, packs| {
        upload::serve_in_turns(
          served,
          *version,
          Asked::Request(request.as_slice()),
          packs,
          &mut again_room,
        )
        .holding()
        .skip_written(skip)
      })
      .ok_or(Failed(Status::SERVICE_UNAVAILABLE))?;
    *made = again;
    *admission = again_room;
    let _ = crate::state::with_state_counted(|state| state.count(READ_THROUGH_AGAIN, 1));
    Ok(())
  }

  /// A turn of an answer held past its room (A-855, main's ruling of 2026-11-21: a refusal costs time, never an
  /// answer). Where the repository's shard can give more room than the answer was made within, it is made again within
  /// it over the same generation, passing over what it wrote, which the answer made again writes alike byte for byte
  /// (`Answer::skip_written`), and counted [`ANSWERED_AGAIN`]; where it cannot yet, the turn waits a keepalive's interval
  /// (the next turn writes git's keepalive), at most [`ROOM_PATIENCE_NS`] in all since it was held, past which the
  /// refusal is answered as it was before (`Answer::give_up`).
  async fn wait_for_room(&mut self) -> Result<(), Failed> {
    let Self {
      lender,
      repository,
      version,
      admission,
      request,
      answer,
      generation,
      keepalive_ns,
      admitted,
      held_since,
      room_patience_ns,
      ..
    } = self;
    let unavailable = Failed(Status::SERVICE_UNAVAILABLE);
    let Some(made) = answer.as_mut() else {
      return Err(Failed(Status::INTERNAL_SERVER_ERROR));
    };
    let now = slates_rt::futures::now_ns();
    let room = lender.room(*repository).unwrap_or(0);
    if room > *admitted
      && let Some(held) = generation.as_ref()
    {
      let skip = made.written();
      let mut wider = Admission::new(room);
      let again = lender
        .lend_held(held, |served, packs| {
          upload::serve_in_turns(
            served,
            *version,
            Asked::Request(request.as_slice()),
            packs,
            &mut wider,
          )
          .holding()
          .skip_written(skip)
        })
        .ok_or(unavailable)?;
      *made = again;
      *admission = wider;
      *admitted = room;
      *held_since = None;
      let _ = crate::state::with_state_counted(|state| state.count(ANSWERED_AGAIN, 1));
      return Ok(());
    }
    let since = *held_since.get_or_insert(now);
    let waited = now.saturating_sub(since);
    if waited >= *room_patience_ns {
      made.give_up();
      return Ok(());
    }
    // A sleep the runtime refuses (no timer here) ends the wait early: the next turn asks again.
    let _ =
      slates_rt::futures::sleep((*keepalive_ns).min(room_patience_ns.saturating_sub(waited))).await;
    Ok(())
  }
}
