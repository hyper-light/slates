//! git's receive-pack behind the SSH edge (A-683, A-654's SSH call; §4.17.2 "A push" and "Transports" item 2): A-654
//! part 2's driver ([`ReceivePack`], the one both edges call) run over an SSH session's standard input and output on
//! the repository's owner shard, as the HTTP site runs it (`crate::vcs_http::git`'s `receive`, A-654 part 3).
//!
//! **The shard, lent a step at a time** (the HTTP site's [`ShardLender`]; in the daemon its `ThisShard`, the state of
//! the shard the connection runs on). Each of the driver's calls is one step on the repository's owner shard and holds
//! nothing of the shard after it returns; the command keeps the driver between the edge's turns and is lent the shard's
//! state afresh for each step, so a push never holds its shard across the edge's awaits, as the driver asks of its
//! callers. Both edges lend the shard through the one seam, so a push meets its shard alike over SSH and over HTTPS.
//!
//! **The session**, as git's own receive-pack runs over SSH \[C: git v2.50.1 `builtin/receive-pack.c`,
//! `cmd_receive_pack`, lines 2558-2649; the copy `docs/wip/research/vcs-servers.md` §5.9 pins\]. receive-pack speaks
//! first: its advertisement is written as the command starts, after a `version 1` line for a client that asked for
//! protocol v1 through `GIT_PROTOCOL`, while a client that asked for v2 is answered at v0, as git answers it ("push
//! support for protocol v2 has not been implemented yet, so ignore the request to use v2 and fallback to using v0").
//! The client's bytes are handed to the driver as they arrive, its request and then its pack, the driver keeping only
//! what the push holds (A-620's session). Once the driver has read the push whole and checked it, it decides it and
//! writes git's report, and the command exits 0 whatever each command's outcome, as git's does: a refused ref is the
//! report's to say. A request of no command, a flush alone (git's push of nothing to update), is answered with nothing,
//! as git's is. The client's end of input checks a push whose pack was cut short, which the report answers with git's
//! `unpack` words; the end of a request cut short is refused, as git's reader refuses it.
//!
//! **The answer, a step at a time** (A-665 part 3), as the HTTP site answers it: each of the command's steps is one of
//! the driver's answer steps, so a turn of a hook's view sealed is a step and the edge yields the shard between steps,
//! and a hook runs off the shard's step, what it answered handed back in a lend of its own. The daemon's presenter shows
//! no hook's view yet (`crate::git_push::Unpresented`), so a push whose repository configures a git hook is refused
//! `hook not run`; owed with the presenter that shows one: this edge's signal of its client gone, which ends a hook's
//! run.
//!
//! **Refusals**, each on standard error with git's `die` status, as git's receive-pack dies: a repository gone since
//! the site found it, in the site's words for a missing one (git's own after `enter_repo`, line 2547); a store not
//! held here, or a shard whose state is not lent (no state on this thread, the shard fenced, or its state lent
//! already), in the site's words for a service not served, as the HTTP site answers both `503`; a policy git's matcher
//! cannot read, in the site's words for it; and an exchange the driver refused, in git's own words where git dies with
//! fixed ones \[C: git v2.50.1 `pkt-line.c` lines 158, 371, 433, 450 and 458, sha256 `a8a3cf58e3026ddd…`;
//! `receive-pack.c` lines 2052, 2111, 2143 and 2164\], without the bytes git quotes after some, which the driver has
//! read and the command no longer holds, and in slates' words where git has none (a push past its room, memory the
//! system refused).
//!
//! **Bounds.** The client's input held is at most the channel's window, which the session refuses to exceed; each step
//! hands all of it to the driver, which keeps what it must within the push's room (the edge's request room, as each of
//! upload-pack's requests is admitted), and the window is given back. The step that ends the pack checks the whole push
//! and the next decides it, each within that room: the driver's own steps. The advertisement and the report are written
//! whole: a line a ref the pusher may read, and a line a command.
//!
//! **Where.** The driver runs where the connection is served, on the shard whose own partition the site found the
//! repository on (`crate::vcs_http::daemon_edge::ShardRepositories`): the repository's owner, since the edge moves a
//! connection whose `exec` names a repository another shard holds to that shard before the site dispatches it (A-651
//! part 4, as the HTTP edge moves one), and the command, made there, is lent that shard's state.

use slates_db::catalog::{Principal, RepositoryId};
use slates_git::http::Version;
use slates_git::pktline::{self, HEADER_LEN, Packet};
use slates_git::receive::Refusal;

use super::site::{Command, GIT_DIE_STATUS, Next, Refused, Stepped};
use crate::git_push::{Answering, PushFailed, ReceivePack, Unpresented};
use crate::state::ShardState;
use crate::vcs_http::git::ShardLender;

/// Format: the line git's receive-pack writes before its advertisement to a client that asked for protocol v1
/// (`receive-pack.c` line 2571).
const VERSION_ONE: &[u8] = b"version 1\n";

/// Format: git's words when its input ends inside a packet (`pkt-line.c` line 371).
const HUNG_UP: &str = "the remote end hung up unexpectedly";

/// The site's words for a service not served (`super::site`'s `Unserved`), which a store not held here and a shard not
/// lent are too.
const NOT_SERVED: &str = "git-receive-pack is not served yet";

/// One receive-pack session over SSH (the module doc), its shard's state lent by `S`.
pub(crate) struct ReceiveCommand<S> {
  /// Where the shard's state is lent.
  lender: S,
  /// Who pushes.
  principal: Principal,
  /// The repository.
  repository: RepositoryId,
  /// The repository's name, for a refusal's words.
  name: String,
  /// The protocol version the client asked for (`GIT_PROTOCOL`).
  version: Version,
  /// The push's room: its request, its pack, and what reading and answering them builds.
  room: u64,
  /// The push, once started.
  push: Option<ReceivePack>,
  /// Whether the driver has read the push whole and checked it, so it waits for its answer.
  checked: bool,
  /// The client's input held, not yet handed to the driver.
  input: Vec<u8>,
  /// Whether the client's input has ended.
  input_ended: bool,
  /// What the command wrote that the channel has not taken.
  out: Vec<u8>,
}

impl<S: ShardLender> ReceiveCommand<S> {
  /// A push of `principal`'s to the repository `repository` named `name`, at protocol `version`, admitted `room` bytes,
  /// its shard's state lent by `lender`.
  pub(crate) fn new(
    lender: S,
    principal: Principal,
    (repository, name): (RepositoryId, String),
    version: Version,
    room: u64,
  ) -> Self {
    Self {
      lender,
      principal,
      repository,
      name,
      version,
      room,
      push: None,
      checked: false,
      input: Vec::new(),
      input_ended: false,
      out: Vec::new(),
    }
  }

  /// Starts the push (the module doc's session): `version 1` for a v1 client, then the driver's advertisement.
  fn start(&mut self) -> Result<Stepped, Refused> {
    if self.version == Version::V1 {
      pktline::write(Packet::Data(VERSION_ONE), &mut self.out)
        .map_err(|_too_long| died(&self.exchange_failed()))?;
    }
    let Self {
      lender,
      principal,
      repository,
      room,
      out,
      ..
    } = self;
    let started = lent(lender, |state| {
      ReceivePack::start(
        state,
        (&*principal, *repository),
        (*room, true),
        out,
        Unpresented,
      )
    })?;
    self.push = Some(started.map_err(|failed| refused(failed, &self.name))?);
    Ok(Stepped {
      consumed: 0,
      next: self.waiting(),
    })
  }

  /// What the push waits for while it reads: another step when input is held, has ended, or the push is checked; the
  /// client's input otherwise.
  fn waiting(&self) -> Next {
    if self.checked || self.input_ended || !self.input.is_empty() {
      Next::More
    } else {
      Next::Input
    }
  }

  /// The words of a failure of the exchange's own writing, which a short line never meets.
  fn exchange_failed(&self) -> String {
    format!("{}: the advertisement could not be written", self.name)
  }
}

impl<S: ShardLender> Command for ReceiveCommand<S> {
  fn input(&mut self, run: &[u8]) -> Result<(), Refused> {
    self.input.extend_from_slice(run);
    Ok(())
  }

  fn input_end(&mut self) {
    self.input_ended = true;
  }

  async fn step(&mut self) -> Result<Stepped, Refused> {
    let Some(push) = self.push.as_mut() else {
      return self.start();
    };
    if self.checked {
      let next = answer_step(&mut self.lender, push, &mut self.out, &self.name).await?;
      return Ok(Stepped { consumed: 0, next });
    }
    if !self.input.is_empty() {
      let consumed = self.input.len();
      self.checked = lent(&mut self.lender, |state| push.input(state, &self.input))?
        .map_err(|failed| refused(failed, &self.name))?;
      self.input.clear();
      return Ok(Stepped {
        consumed,
        next: self.waiting(),
      });
    }
    if self.input_ended {
      self.checked = ended(&mut self.lender, push, &self.name)?;
      return Ok(Stepped {
        consumed: 0,
        next: Next::More,
      });
    }
    Ok(Stepped {
      consumed: 0,
      next: Next::Input,
    })
  }

  fn output(&self) -> (&[u8], &[u8]) {
    (&[], &self.out)
  }

  fn sent(&mut self, _stderr: usize, stdout: usize) {
    self.out.drain(..stdout.min(self.out.len()));
  }
}

/// Runs `step` over the shard's state `lender` lends (the module doc's lend); refused in the not-served words when it is
/// not lent, as the HTTP site answers `503`.
fn lent<S: ShardLender, R>(
  lender: &mut S,
  step: impl FnOnce(&mut ShardState) -> R,
) -> Result<R, Refused> {
  lender.lend(step).ok_or_else(|| died(NOT_SERVED))
}

/// One step of the checked push's answer (the module doc's answer): exited 0 once it is answered whole; another step
/// after a turn of a hook's view sealed, the edge yielding the shard between its steps; another step after a hook run off
/// the shard's step, with no signal of the client gone (owed, as on the HTTP site), and what it answered handed back.
async fn answer_step<S: ShardLender>(
  lender: &mut S,
  push: &mut ReceivePack,
  out: &mut Vec<u8>,
  name: &str,
) -> Result<Next, Refused> {
  let answering = lent(lender, |state| push.answer_step(state, &mut *out))?
    .map_err(|failed| refused(failed, name))?;
  match answering {
    Answering::Answered(_) => Ok(Next::Exited(0)),
    Answering::Sealing => Ok(Next::More),
    Answering::Hook(call) => {
      let answer = call.run(core::future::pending()).await;
      lent(lender, |state| push.hook_ran(state, answer, &mut *out))?
        .map_err(|failed| refused(failed, name))?;
      Ok(Next::More)
    }
  }
}

/// The client's input ended before the push was checked: checked now (a pack cut short, which the report answers), or
/// refused as git's reader refuses a request cut short. An end that leaves no push to check is that refusal too.
fn ended<S: ShardLender>(
  lender: &mut S,
  push: &mut ReceivePack,
  name: &str,
) -> Result<bool, Refused> {
  let checked = lent(lender, |state| push.end(state))?.map_err(|failed| refused(failed, name))?;
  if checked {
    Ok(true)
  } else {
    Err(died(HUNG_UP))
  }
}

/// `message` as git's `die` writes it, with its status.
fn died(message: &str) -> Refused {
  Refused {
    stderr: format!("fatal: {message}\n").into_bytes(),
    exit_status: GIT_DIE_STATUS,
  }
}

/// The driver's refusal `failed` of the push to repository `name`, in the words the module doc gives each.
fn refused(failed: PushFailed, name: &str) -> Refused {
  died(&match failed {
    PushFailed::NotFound | PushFailed::NotGit => {
      format!("'{name}' does not appear to be a git repository")
    }
    PushFailed::NoStore => NOT_SERVED.to_owned(),
    PushFailed::Policy(_) => format!("{name}: the repository's policy cannot be read"),
    PushFailed::Exchange(refusal) => exchange_words(&refusal),
    PushFailed::Unchecked => format!("{name}: the push was answered before it was read whole"),
  })
}

/// The words of an exchange the driver refused (the module doc): git's own where git dies with fixed ones.
fn exchange_words(refusal: &Refusal) -> String {
  match refusal {
    Refusal::PktLine(line) => pkt_line_words(line),
    Refusal::Truncated { .. } => HUNG_UP.to_owned(),
    Refusal::UnexpectedPacket { .. } => "protocol error: got an unexpected packet".to_owned(),
    Refusal::Command { .. } => "protocol error: expected old/new/ref".to_owned(),
    Refusal::Shallow { .. } => "protocol error: expected shallow sha".to_owned(),
    Refusal::ObjectFormat { .. } => "error: unsupported object format".to_owned(),
    Refusal::LineTooLong { .. } => "protocol error: impossibly long line".to_owned(),
    Refusal::Capabilities { .. } => {
      "protocol error: capabilities after the first command's".to_owned()
    }
    Refusal::PushCert { .. } => {
      "protocol error: a push certificate, which this server does not advertise".to_owned()
    }
    Refusal::Advertisement(_) => "the advertisement could not be written".to_owned(),
    Refusal::PastAdmission(past) => past_room(past),
    Refusal::AllocationRefused { bytes } => format!("out of memory: {bytes} bytes refused"),
  }
}

/// The words of a packet git's pkt-line reader refuses (`pkt-line.c` lines 371, 433, 450 and 458): git's length is the
/// packet's less its header where the packet is too long, and a length shorter than the header is 3, the one such
/// length that is no special packet.
fn pkt_line_words(refusal: &pktline::Refusal) -> String {
  match refusal {
    pktline::Refusal::NotHex { .. } => "protocol error: bad line length character".to_owned(),
    pktline::Refusal::ShorterThanHeader { .. } => format!(
      "protocol error: bad line length {}",
      HEADER_LEN.saturating_sub(1)
    ),
    pktline::Refusal::TooLong { len, .. } => format!(
      "protocol error: bad line length {}",
      len.saturating_sub(u64::try_from(HEADER_LEN).unwrap_or(u64::MAX))
    ),
    pktline::Refusal::Truncated { .. } => HUNG_UP.to_owned(),
    pktline::Refusal::PastAdmission(past) => past_room(past),
  }
}

/// The words of a push past its room (slates' own: git has no room).
fn past_room(past: &slates_git::PastAdmission) -> String {
  format!(
    "the push needs {} bytes where its room had {} left",
    past.asked, past.left
  )
}
