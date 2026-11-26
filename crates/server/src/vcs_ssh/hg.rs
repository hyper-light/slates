//! Mercurial's `hg -R <path> serve --stdio` behind the SSH edge (A-679; §4.17.3 "The wire surface" and piece 4; §4.17.2
//! "Transports" item 2): A-650 part 2's one entry point, `slates_hg::serve::stdio`'s [`Stdio`], driven over an SSH
//! session's standard input and output, the repository's store lent a step at a time by the lender the HTTP site reads
//! its stores from ([`HgStores`]), so both edges reach a Mercurial store one way, as git's two reach theirs through one
//! `StoreLender` (`super::git`).
//!
//! **The session** is 7.2's `_runsshserver` \[C: Mercurial 7.2 `wireprotoserver.py`\]: the client speaks first (7.2's
//! handshake is `hello`, then `between` with the null pair), each request is served once it is whole and the last
//! answer has gone to the channel, an error's words go on standard error before its empty line on standard output, and
//! the session ends at an empty line, at a request it cannot read, or at the end of the client's input once the whole
//! requests before it are served (if an incomplete request remains unconsumed when client input ends, an out-of-band
//! SSH error refusal is emitted before exiting rather than silent empty output). It then exits 0 whatever it answered
//! (`EXIT_STATUS`), as `hg serve --stdio` does, but where an answer was refused past its stream's start
//! (`Stdio::exit_status`, A-745).
//! The input a step consumed is given back to the channel's window. Bytes that arrive after the session ended, which it
//! drops as an exited process reads none, are given back at the next step, so the window is never left short.
//!
//! **Hidden changesets.** A client asking for them (`--hidden`, the fifth word 7.2's dispatch allows a `serve --stdio`)
//! is answered as 7.2 answers a user its `experimental.server.allow-hidden-access` does not name: the warning `ignoring
//! request to access hidden changeset by unauthorized user: <user>` on standard error, then the served view as for any
//! client \[C: 7.2 `commands.py` `serve`; recorded from stock 7.2 on 2026-11-19\]. No read right grants hidden access
//! (§4.17.3 "Access": "an administrator's mode"), so none is granted here. 7.2 names the account it runs as; here the
//! principal's record is named, as `acl`'s words name it (A-657 part 4), until §4.13's Mercurial identity names the
//! principal.
//!
//! **Bounds.** Each request is admitted the edge's room, as each of upload-pack's is: the served view is made within
//! it and the session's step is handed what is left, as the HTTP site's answers are. The input held is at most the
//! channel's window, derived from the same room as the edge derives it (`super::channel_window`), so the session's own
//! bound is the channel's and its refusal of input past it is met only where a caller's room and window disagree. One
//! answer is held at most: the channel takes it before the next request is served.
//!
//! **Pushes** (A-696; 7.2's `wireprotov1server.unbundle` over `sshv1protocolhandler` \[C\]). The session reads an
//! `unbundle`, judges its `heads` argument against the served view, asks for the bundle and takes its chunks as they
//! arrive, a chunk a step with no view made for it ([`Stdio::step_push`]). Once the bundle is whole the push is handed
//! to the lender ([`HgStores::push`]), the driver the HTTP site's pushes go through too (A-680 part 2b:
//! `crate::repository::hg_push::unbundle`, on the repository's owner shard the connection moved to), by the principal
//! the client's key proved, within what is left of the request's room once the bundle is held; what the push came to
//! is framed as 7.2 frames it over SSH ([`Stdio::pushed`]). The site lets a principal start `serve --stdio` holding
//! any right on the repository (its step 2); a push's names are judged by the repository's policy in the driver, as
//! over HTTP, so a principal who may only read is refused there in hgweb's words.
//!
//! **The store.** A repository whose store the lender does not hold is not served (the site's step 3). A step whose
//! store went away mid-session, or whose view cannot be made, ends the session in the same words, with `abort`'s
//! status. In the daemon the lender is the HTTP site's, `crate::vcs_store::hg_view::ServedViews` (A-680 part 2a): the
//! served view over the repository's store on its owner shard, made afresh for each step.
//!
//! **Tests** (`tests/hg.rs`, a child of the edge's tests, so that a session also runs over the real edge with their
//! client): the probe stock `hg serve --stdio` 7.2 answered (`crates/hg/fixtures/serve`), after 7.2's handshake,
//! answered byte for byte at every cut down to a byte and over the edge; the hidden warning; a store going away; the
//! window's bookkeeping; the services' start; the site's refusals in 7.2's recorded words; a push handed to the lender
//! as the client sent it, by the key's principal; and stock 7.2's push over SSH (`crates/hg/fixtures/wire`'s
//! `ssh-push`) replayed against a daemon's shard through the daemon's lender, every answer after the handshake 7.2's
//! byte for byte.
//!
//! **Owed** (GAPS): each answer made within the shard's step budget; a `getbundle` is one step today, as over HTTP.

use slates_db::catalog::{Principal, RepositoryId};
use slates_hg::Admission;
use slates_hg::serve::stdio::{PastWindow, Progress, Push, Stdio};

use super::channel_window;
use super::site::{self, ClientGone, Command, HG_ABORT_STATUS, Next, Refused, Service, Stepped};
use crate::vcs_http::hg::HgStores;

/// Format: 7.2's warning to a client that asks for hidden changesets its server's configuration does not let it see
/// (`commands.py` `serve`, 7.2), before the user it names and a line end.
const HIDDEN_REFUSED: &[u8] = b"ignoring request to access hidden changeset by unauthorized user: ";

/// Format: out-of-band SSH refusal message when client input ends mid-request (for example a `known` command missing
/// its `* <count>` varargs line or a truncated push bundle), framed via `slates_hg::ssh::respond_error`.
const INCOMPLETE_REQUEST: &[u8] = b"unreadable request: incomplete request before end of input";

/// One `hg serve --stdio` session over SSH (the module doc).
pub(crate) struct HgCommand<S: HgStores> {
  /// Where the store is lent.
  stores: S,
  /// A streamed `getbundle` answer's hold on the generation it began on (A-745 rule 4).
  generation: Option<S::Hold>,
  /// The repository.
  repository: RepositoryId,
  /// Who the session's pushes are by: the principal the client's key proved.
  principal: Principal,
  /// The room each request is admitted.
  room: u64,
  /// The session.
  stdio: Stdio,
  /// 7.2's warning to a client asking for hidden changesets, not yet taken by the channel: sent before anything else.
  warning: Vec<u8>,
  /// Bytes currently buffered inside [`Self::stdio`] that have not yet been consumed by a completed step.
  pending_bytes: usize,
  /// Whether the client closed standard input; applied once all complete buffered requests are drained.
  eof_pending: bool,
  /// Out-of-band stderr refusal emitted on an incomplete request at EOF, drained before exiting.
  refusal_stderr: Vec<u8>,
  /// Out-of-band stdout empty-line marker paired with [`Self::refusal_stderr`].
  refusal_stdout: Vec<u8>,
  /// Input that arrived after the session ended, which it dropped: given back to the window at the next step.
  dropped: usize,
  /// A streamed answer's measured step cost, across its turns (A-783's paced turn).
  costs: crate::vcs_store::TurnCosts,
}

impl<S: HgStores> HgCommand<S> {
  /// A session by `principal` over the store of `repository` that `stores` lends, each request admitted `room` bytes and
  /// the input held within the channel's window over that room; `hidden` when the client asked for hidden changesets.
  pub(crate) fn new(
    stores: S,
    (repository, principal): (RepositoryId, Principal),
    room: u64,
    hidden: bool,
  ) -> Self {
    let window = usize::try_from(channel_window(room)).unwrap_or(usize::MAX);
    let warning = if hidden {
      hidden_refused(&principal)
    } else {
      Vec::new()
    };
    Self {
      stores,
      generation: None,
      repository,
      principal,
      room,
      stdio: Stdio::new(window),
      warning,
      pending_bytes: 0,
      eof_pending: false,
      refusal_stderr: Vec::new(),
      refusal_stdout: Vec::new(),
      dropped: 0,
      costs: crate::vcs_store::TurnCosts::ZERO,
    }
  }

  /// Whether an EOF-triggered out-of-band refusal is still waiting to be drained to the channel.
  fn has_refusal(&self) -> bool {
    !self.refusal_stderr.is_empty() || !self.refusal_stdout.is_empty()
  }

  /// Whether a deferred client EOF can now be applied to [`Self::stdio`] without interrupting a reply or stream.
  fn can_finish_at_eof(&self, progress: &Progress) -> bool {
    matches!(progress, Progress::Waiting)
      && self.eof_pending
      && !self.stdio.ended()
      && self.stdio.stdout().is_empty()
      && self.stdio.stderr().is_empty()
      && !self.stdio.streaming()
  }

  /// Advances [`Self::stdio`] by one step and lands any push bundle that step completed.
  async fn advance_stdio(&mut self) -> Result<Progress, Refused> {
    if let Some(push_room) = self.stores.push_room(self.repository) {
      self.stdio.admit_pushes(push_room);
    }
    let progress = if self.stdio.taking_push() {
      self.stdio.step_push()
    } else if let (true, Some(generation)) = (self.stdio.streaming(), self.generation.as_ref()) {
      streamed_turn(
        &mut self.stores,
        (generation, self.repository, self.room),
        &mut self.stdio,
        &mut self.costs,
      )
      .await?
    } else {
      answered_step(
        &mut self.stores,
        (self.repository, self.room),
        &mut self.stdio,
        &mut self.generation,
      )?
    };
    if !self.stdio.streaming() {
      self.generation = None;
    }
    if let Some(push) = self.stdio.take_push() {
      land(
        &mut self.stores,
        (self.repository, &self.principal),
        push,
        &mut self.stdio,
      )
      .await?;
    }
    Ok(progress)
  }

  /// Applies a deferred EOF once `stdio` is waiting with no buffered reply, emitting a typed SSH out-of-band refusal
  /// if unconsumed request bytes or an unfinished push remained before EOF.
  fn finish_at_eof(&mut self) -> Result<usize, Refused> {
    self.eof_pending = false;
    let incomplete = self.pending_bytes > 0 || self.stdio.taking_push();
    self.stdio.eof();
    let final_progress = if self.stdio.taking_push() {
      self.stdio.step_push()
    } else {
      answered_step(
        &mut self.stores,
        (self.repository, self.room),
        &mut self.stdio,
        &mut self.generation,
      )?
    };
    let consumed = consumed_of(&final_progress);
    self.pending_bytes = self.pending_bytes.saturating_sub(consumed);
    if incomplete && self.stdio.stdout().is_empty() && self.stdio.stderr().is_empty() {
      slates_hg::ssh::respond_error(
        INCOMPLETE_REQUEST,
        &mut self.refusal_stdout,
        &mut self.refusal_stderr,
      );
    }
    Ok(consumed)
  }
}

/// The input bytes consumed by a single [`Progress`] transition.
fn consumed_of(progress: &Progress) -> usize {
  match progress {
    Progress::Stepped { consumed, .. } | Progress::Took { consumed, .. } => *consumed,
    Progress::Streamed { .. } | Progress::Waiting => 0,
  }
}

/// Lands `push` on `repository` by `principal` through `stores` (the module doc's pushes) and frames what it came to;
/// refused with the not-served words where the lender no longer lends the store or no view of it could be made.
async fn land<S: HgStores>(
  stores: &mut S,
  (repository, principal): (RepositoryId, &Principal),
  push: Push,
  stdio: &mut Stdio,
) -> Result<(), Refused> {
  let mut left = push.admission;
  let outcome = stores
    .push(
      repository,
      (principal, &push.heads, &push.bundle),
      &mut left,
    )
    .await
    .ok_or_else(unheld)?;
  stdio.pushed(&outcome, &mut left);
  Ok(())
}

/// 7.2's warning to `principal`, who asked for hidden changesets (the module doc).
fn hidden_refused(principal: &Principal) -> Vec<u8> {
  let mut words = HIDDEN_REFUSED.to_vec();
  words.extend_from_slice(format!("{principal:?}").as_bytes());
  words.push(b'\n');
  words
}

/// A request's step: its answer admitted the room a fetch's turn is (A-745 rule 2), as over HTTP (a `lookup` reads each
/// head's manifest for its `.hgtags`, past the edge's room in hg's own history; the input stays bounded by the
/// channel's window). A stream begun is held in this same turn, with no wait between, so every later turn reads what
/// this one read.
fn answered_step<S: HgStores>(
  stores: &mut S,
  (repository, room): (RepositoryId, u64),
  stdio: &mut Stdio,
  generation: &mut Option<S::Hold>,
) -> Result<Progress, Refused> {
  // A-740: a shard stuck in a step names it.
  let _doing = slates_rt::futures::doing("vcs_ssh::hg::answered_step");
  let answer_room = stores
    .fetch_turn(repository)
    .map_or(room, |(turn_room, _)| turn_room);
  let mut admission = Admission::new(answer_room);
  let progress = stores
    .lend(repository, &mut admission, |store, left| {
      stdio.step(store, left.left())
    })
    .ok_or_else(unheld)?;
  if stdio.streaming() {
    let mut held = Admission::new(room);
    *generation = Some(stores.hold(repository, &mut held).ok_or_else(unheld)?);
  }
  Ok(progress)
}

/// A stream's turn (A-745, §4.17.1 "A fetch's answer"): what its next step asks of the node's changeset store answered
/// first; then a `getbundle`'s answer made a turn at a time, the view lent for the turn, each slice within the shard's
/// room, the turn paced (A-783, [`crate::vcs_store::paced_turn`]) and ending once a room's worth waits to be
/// sent or the store is asked.
async fn streamed_turn<S: HgStores>(
  stores: &mut S,
  (generation, repository, room): (&S::Hold, RepositoryId, u64),
  stdio: &mut Stdio,
  costs: &mut crate::vcs_store::TurnCosts,
) -> Result<Progress, Refused> {
  // A-740: a shard stuck in a turn names it.
  let _doing = slates_rt::futures::doing("vcs_ssh::hg::streamed_turn");
  let (turn_room, quantum) = stores.fetch_turn(repository).unwrap_or((room, 0));
  let started = slates_rt::futures::now_ns();
  let before_total = stdio.stdout().len();
  loop {
    if let Some(wants) = stdio.store_wants()
      && !stores
        .answer_wants((generation, repository, room), wants)
        .await
    {
      return Err(unheld());
    }
    let mut admission = Admission::new(turn_room);
    let start = slates_rt::futures::now_ns();
    let bound = usize::try_from(turn_room).unwrap_or(usize::MAX);
    let mut paced = crate::vcs_store::paced_turn(quantum, start);
    let before = stdio.stdout().len();
    let turned = stores
      .lend_held(generation, &mut admission, |store, _| {
        loop {
          if !paced.more(crate::vcs_store::ANSWER_STEP, costs) {
            break Progress::Streamed { more: true };
          }
          match stdio.stream(store, turn_room) {
            Progress::Streamed { more: true }
              if stdio.stdout().len() < bound && stdio.store_wants().is_none() => {}
            other => break other,
          }
        }
      })
      .ok_or_else(unheld)?;
    paced.close(slates_rt::futures::now_ns(), costs);
    crate::vcs_store::hg_view::turn_taken();
    // A-724 piece 3d: what the turn wrote, learned as the fetch's demand once its answer ends.
    stores.wrote(
      generation,
      u64::try_from(stdio.stdout().len().saturating_sub(before)).unwrap_or(u64::MAX),
    );
    let elapsed = slates_rt::futures::now_ns().saturating_sub(started);
    if !matches!(turned, Progress::Streamed { more: true })
      || stdio.stdout().len() > before_total
      || stdio.store_wants().is_none()
      || (quantum > 0 && elapsed >= quantum)
    {
      return Ok(turned);
    }
  }
}

/// The words a session ends with when its store is no longer held here: the site's not-served words (its step 3).
fn unheld() -> Refused {
  site::not_served(Service::Mercurial { hidden: false })
}

/// The session's refusal of input past its window, in Mercurial's form: met only where a caller's room and window
/// disagree (the module doc's bounds).
fn past_window(past: PastWindow) -> Refused {
  Refused {
    stderr: format!(
      "abort: the session's input passed its window: {} of {} bytes\n",
      past.held, past.capacity
    )
    .into_bytes(),
    exit_status: HG_ABORT_STATUS,
  }
}

impl<S: HgStores> Command for HgCommand<S> {
  fn input(&mut self, run: &[u8]) -> Result<(), Refused> {
    if self.stdio.ended() {
      self.dropped = self.dropped.saturating_add(run.len());
      return Ok(());
    }
    self.stdio.receive(run).map_err(past_window)?;
    self.pending_bytes = self.pending_bytes.saturating_add(run.len());
    Ok(())
  }

  fn input_end(&mut self) {
    self.eof_pending = true;
  }

  async fn step(&mut self, _gone: ClientGone<'_>) -> Result<Stepped, Refused> {
    let progress = self.advance_stdio().await?;
    let mut consumed = consumed_of(&progress);
    self.pending_bytes = self.pending_bytes.saturating_sub(consumed);
    if self.can_finish_at_eof(&progress) {
      consumed = consumed.saturating_add(self.finish_at_eof()?);
    }
    let next = if self.stdio.finished() && self.warning.is_empty() {
      Next::Exited(self.stdio.exit_status())
    } else if matches!(progress, Progress::Waiting) && !self.stdio.finished() {
      Next::Input
    } else {
      Next::More
    };
    Ok(Stepped {
      consumed: consumed.saturating_add(std::mem::take(&mut self.dropped)),
      next,
    })
  }

  fn output(&self) -> (&[u8], &[u8]) {
    if !self.warning.is_empty() {
      (&self.warning, &[])
    } else if self.has_refusal() {
      (&self.refusal_stderr, &self.refusal_stdout)
    } else {
      (self.stdio.stderr(), self.stdio.stdout())
    }
  }

  fn sent(&mut self, stderr: usize, stdout: usize) {
    if !self.warning.is_empty() {
      self.warning.drain(..stderr.min(self.warning.len()));
    } else if self.has_refusal() {
      self
        .refusal_stderr
        .drain(..stderr.min(self.refusal_stderr.len()));
      self
        .refusal_stdout
        .drain(..stdout.min(self.refusal_stdout.len()));
    } else {
      self.stdio.sent_stderr(stderr);
      self.stdio.sent_stdout(stdout);
    }
  }
}
