//! git's receive-pack behind the HTTP edge (A-654 part 3; §4.17.2 "A push" and "Transports" item 1; gitprotocol-http's
//! smart service): `GET <repo>/info/refs?service=git-receive-pack` answered with the push driver's advertisement, and a
//! `POST <repo>/git-receive-pack` answered as `git receive-pack --stateless-rpc` answers one, through the one driver
//! both edges call (`crate::git_push::ReceivePack`, A-654 part 2): the hook gate, the quarantine, the policy through
//! A-644 part 2's check point, the verdicts over the shard's greens and A-635's append with A-662's store, git's report.
//!
//! **The shard, lent a step at a time** ([`ShardLender`]). Each of the driver's calls is one step on the repository's
//! owner shard, where the site moved the connection (A-625 part 3), and holds nothing of the shard after it returns; the
//! exchange keeps the driver between the edge's turns and is lent the shard's state afresh for each step, so a push never
//! holds its shard across a wait. The request's body goes to the driver a run at a time as it arrives, charged to the
//! push's room, what its shard could admit (A-704); once the body has ended the push is answered a step at a time (A-665 part 3): the shard yielded
//! between the turns of a hook's view's sealing, and a hook run off the shard's step, its answer handed back in a step
//! of its own. What the push wrote is given back a pkt-line's room at a time (`slates_git::pktline::MAX_LEN`), as
//! upload-pack's answer is, so no reply run is longer. A hook runs until it ends or the client goes: the edge's signal of
//! its client gone ([`ClientGone`], the connection's socket closing while the push is answered) ends its run, and what
//! the hook would have judged is refused (A-665 part 4). The daemon's presenter shows no hook's view yet
//! (`crate::git_push::Unpresented`), so a push whose repository configures a git hook is refused `hook not run`.
//!
//! **Versions.** receive-pack answers protocol version 0 whatever the client asks, as git's own does: `receive-pack.c`
//! (git 2.50.1, `cmd_receive_pack`) ignores a request for version 2 ("push support for protocol v2 has not been
//! implemented yet") and writes `version 1` before its advertisement to a client asking version 1, as this one does; the
//! site writes the `# service=` prelude for every version but 2, as `http-backend.c`'s `get_info_refs` does. Stock git's
//! client asks version 0 for a push (`remote-curl.c`'s `discover_refs` falls back from version 2 for any service but
//! upload-pack).
//!
//! **Refusals.** A step that fails before any reply is answered with a status, and a reply it cuts short ends the
//! connection: a request git's protocol does not allow `400`, one past the push's room `413`, memory the system refused
//! or a store not held here `503`, a repository gone `404`, a policy git's matcher cannot read and an advertisement the
//! writer refuses `500`. A command's own refusal is no failure: the report answers it, `200`, as `http-backend`'s does. A
//! request that asks nothing (a flush alone, the probe stock git's client posts before a large push to learn whether it
//! may, `remote-curl.c`'s `probe_rpc`) is answered with no report, as git's receive-pack writes none for it.

use slates_git::http::{Service, Version};
use slates_git::pktline::{self, MAX_LEN, Packet};
use slates_git::receive::Refusal;
use slates_http::reply::Status;

use super::{GitServices, ServiceRefused, Start};
use crate::git_push::{Answering, PushFailed, ReceivePack, Unpresented};
use crate::state::ShardState;
use crate::vcs_http::{ClientGone, Exchange, Failed};

/// Format: git's keepalive under `side-band-64k`: a pkt-line of band 1 alone, no data (`receive-pack.c`'s `"0005\1"`),
/// which a client's demultiplexer reads and passes over.
const KEEPALIVE: &[u8] = b"0005\x01";

/// Format: the line git's receive-pack writes before its advertisement to a client asking protocol version 1
/// (`receive-pack.c`, `cmd_receive_pack`).
const VERSION_1: &[u8] = b"version 1\n";

/// Where receive-pack is lent its repository's owner shard's state, afresh for each step of a push (the module doc); the
/// SSH site's receive-pack is lent it the same way (`crate::vcs_ssh`'s `receive`, A-683).
pub(crate) trait ShardLender {
  /// Runs `step` over the shard's state; none when it is not lent (no state on this thread, the shard fenced, or its
  /// state lent already).
  fn lend<R>(&mut self, step: impl FnOnce(&mut ShardState) -> R) -> Option<R>;
}

/// The daemon's lender: the state of the shard the exchange runs on, the repository's own once the site has moved its
/// connection there (A-625 part 3).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ThisShard;

impl ShardLender for ThisShard {
  fn lend<R>(&mut self, step: impl FnOnce(&mut ShardState) -> R) -> Option<R> {
    crate::state::with_state(step)
  }
}

/// git's receive-pack through the push driver on the shard `lender` lends, each push admitted what its shard could
/// admit (`crate::vcs_share::room`, A-704, A-724), never the edge's request room. upload-pack is not its half: it answers `503` for it
/// ([`super::Halves`] pairs the two).
#[derive(Clone, Debug)]
pub(crate) struct ReceiveServices<L> {
  /// Where the shard's state is lent.
  lender: L,
  /// How long a push's answer may write nothing before git's keepalive is written (`DaemonConfig::git_keepalive_ns`).
  keepalive_ns: u64,
}

impl<L> ReceiveServices<L> {
  /// receive-pack over the state `lender` lends, git's keepalive written after `keepalive_ns` of a silent answer.
  pub(crate) fn new(lender: L, keepalive_ns: u64) -> Self {
    Self {
      lender,
      keepalive_ns,
    }
  }
}

impl<L: ShardLender + Clone> GitServices for ReceiveServices<L> {
  type Exchange = Receive<L>;

  fn start(&mut self, start: &Start<'_>) -> Result<Receive<L>, ServiceRefused> {
    if start.service != Service::ReceivePack {
      return Err(ServiceRefused::Unavailable);
    }
    let mut given = Vec::new();
    if start.advertise && start.version == Version::V1 {
      pktline::write(Packet::Data(VERSION_1), &mut given)
        .map_err(|_| ServiceRefused::Unwritable)?;
    }
    let who = (start.principal, start.repository.id);
    let asked = start.advertise;
    let push = self
      .lender
      .lend(|state| ReceivePack::start(state, who, asked, &mut given, Unpresented))
      .ok_or(ServiceRefused::Unavailable)?
      .map_err(refused)?;
    Ok(Receive {
      lender: self.lender.clone(),
      push,
      advertise: start.advertise,
      given,
      sent: 0,
      answering: false,
      keepalive_ns: self.keepalive_ns,
      last_written_ns: slates_rt::futures::now_ns(),
    })
  }
}

/// One receive-pack exchange: the advertisement, or one push whose request's body is read as it arrives and answered
/// once it has ended (the module doc).
pub(crate) struct Receive<L> {
  /// Where the shard's state is lent.
  lender: L,
  /// The push, between the edge's turns.
  push: ReceivePack,
  /// Whether the exchange is the advertisement.
  advertise: bool,
  /// What is given back: the advertisement, written as the push started, or the report, written as it is answered.
  given: Vec<u8>,
  /// How much of it the edge has been given.
  sent: usize,
  /// Whether the push is still being answered as its reply is given: a push under side-band-64k whose answer wrote
  /// nothing for its keepalive interval, answered in the reply's turns, git's keepalive written between (A-722: a long push
  /// answered nothing until its report, and a client's read gave up on the silence, the load-only edge tests).
  answering: bool,
  /// How long the answer may write nothing before git's keepalive is written.
  keepalive_ns: u64,
  /// When the exchange last wrote: its head, or an interim keepalive while its body was read.
  last_written_ns: u64,
}

impl<L: ShardLender> Exchange for Receive<L> {
  /// git's keepalive while the push's pack is still being read (A-722; the module doc): once its commands show
  /// side-band-64k, whenever the keepalive interval has passed since the exchange last wrote.
  fn interim(&mut self, out: &mut Vec<u8>) {
    if self.advertise || !self.push.side_band() {
      return;
    }
    let now = slates_rt::futures::now_ns();
    if now.saturating_sub(self.last_written_ns) >= self.keepalive_ns {
      out.extend_from_slice(KEEPALIVE);
      self.last_written_ns = now;
    }
  }

  async fn body(&mut self, run: &[u8]) -> Result<(), Failed> {
    // An advertisement's request is a `GET`, whose body asks nothing: `http-backend` reads none for it.
    if self.advertise {
      return Ok(());
    }
    let Self { lender, push, .. } = self;
    lender
      .lend(|state| push.input(state, run))
      .ok_or(Failed(Status::SERVICE_UNAVAILABLE))?
      .map(|_| ())
      .map_err(failed)
  }

  async fn body_end(&mut self, gone: ClientGone<'_>) -> Result<(), Failed> {
    if self.advertise {
      return Ok(());
    }
    let Self { lender, push, .. } = self;
    lender
      .lend(|state| push.end(state))
      .ok_or(Failed(Status::SERVICE_UNAVAILABLE))?
      .map_err(failed)?;
    // A push under side-band-64k is answered here until its answer would have been silent for git's keepalive interval,
    // then in the reply's turns with the keepalive between (the module doc); one without is answered here whole, as
    // nothing can be written to it but its report. Either way a refusal before any reply still answers with a status.
    let silent_until = self
      .push
      .side_band()
      .then(|| slates_rt::futures::now_ns().saturating_add(self.keepalive_ns));
    self.answering = !self.answer_until(gone, silent_until).await?;
    Ok(())
  }

  async fn reply(&mut self, out: &mut Vec<u8>, gone: ClientGone<'_>) -> Result<bool, Failed> {
    if self.sent >= self.given.len() && self.answering {
      let silent_until = slates_rt::futures::now_ns().saturating_add(self.keepalive_ns);
      let answered = self.answer_until(gone, Some(silent_until)).await?;
      self.answering = !answered;
      if !answered && self.sent >= self.given.len() {
        self.given.extend_from_slice(KEEPALIVE);
      }
    }
    let rest = self.given.get(self.sent..).unwrap_or_default();
    let run = rest.get(..MAX_LEN).unwrap_or(rest);
    out.extend_from_slice(run);
    self.sent = self.sent.saturating_add(run.len());
    Ok(self.answering || self.sent < self.given.len())
  }
}

impl<L: ShardLender> Receive<L> {
  /// Answers the push a step at a time until it is answered (whether it is), or, once `silent_until` passes on the
  /// shard's clock with nothing new to give, until its next turn (A-722's keepalive): each step takes its repository's
  /// turn on the shard and keeps it through the yield after, so a repository's pushes share one turn a round (A-724,
  /// `crate::vcs_share::turns`); a hook runs off the shard, its repository's other pushes taking their turns meanwhile.
  async fn answer_until(
    &mut self,
    gone: ClientGone<'_>,
    silent_until: Option<u64>,
  ) -> Result<bool, Failed> {
    // The client's going ends a hook's run alone, and no hook runs off unix.
    #[cfg(not(unix))]
    let _ = gone;
    let Self {
      lender,
      push,
      given,
      sent,
      ..
    } = self;
    let repository = push.repository();
    loop {
      let turn = crate::vcs_share::turns::take(repository).await;
      let step = lender
        .lend(|state| push.answer_step(state, given))
        .ok_or(Failed(Status::SERVICE_UNAVAILABLE))?
        .map_err(failed)?;
      match step {
        Answering::Answered(_) => return Ok(true),
        Answering::Turned => {
          let due = silent_until.is_some_and(|until| slates_rt::futures::now_ns() >= until);
          if due || *sent < given.len() && silent_until.is_some() {
            return Ok(false);
          }
          turn.yield_holding().await;
        }
        #[cfg(unix)]
        Answering::Hook(call) => {
          // A hook runs off the shard: its repository's other pushes take their turns meanwhile.
          drop(turn);
          let answer = call.run(gone.gone()).await;
          lender
            .lend(|state| push.hook_ran(state, answer, given))
            .ok_or(Failed(Status::SERVICE_UNAVAILABLE))?
            .map_err(failed)?;
        }
      }
    }
  }
}

/// Why the driver did not start a push, as the site refuses it (the module doc, "Refusals").
fn refused(failure: PushFailed) -> ServiceRefused {
  match failure {
    PushFailed::NotFound | PushFailed::NotGit => ServiceRefused::NoRepository,
    PushFailed::NoStore => ServiceRefused::Unavailable,
    PushFailed::Policy(_) => ServiceRefused::Unreadable,
    PushFailed::Exchange(_) | PushFailed::Unchecked => ServiceRefused::Unwritable,
  }
}

/// The status a push step that failed before any reply is answered with (the module doc, "Refusals").
fn failed(failure: PushFailed) -> Failed {
  Failed(match failure {
    PushFailed::NotFound | PushFailed::NotGit => Status::NOT_FOUND,
    PushFailed::NoStore => Status::SERVICE_UNAVAILABLE,
    PushFailed::Policy(_) => Status::INTERNAL_SERVER_ERROR,
    PushFailed::Unchecked => Status::BAD_REQUEST,
    PushFailed::Exchange(refusal) => exchange_status(refusal),
  })
}

/// The status of an exchange receive-pack refused: the client's request for what git's protocol does not allow, the
/// push's room or the system's memory for what it would take, the server for what it could not write.
fn exchange_status(refusal: Refusal) -> Status {
  match refusal {
    Refusal::PktLine(_)
    | Refusal::Truncated { .. }
    | Refusal::UnexpectedPacket { .. }
    | Refusal::Command { .. }
    | Refusal::Shallow { .. }
    | Refusal::Capabilities { .. }
    | Refusal::ObjectFormat { .. }
    | Refusal::PushCert { .. } => Status::BAD_REQUEST,
    Refusal::PastAdmission(_) => Status::CONTENT_TOO_LARGE,
    Refusal::AllocationRefused { .. } => Status::SERVICE_UNAVAILABLE,
    Refusal::Advertisement(_) | Refusal::LineTooLong { .. } => Status::INTERNAL_SERVER_ERROR,
  }
}
