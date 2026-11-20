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
//! requests before it are served. It then exits 0 whatever it answered ([`EXIT_STATUS`]), as `hg serve --stdio` does.
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
use slates_hg::serve::stdio::{EXIT_STATUS, PastWindow, Progress, Push, Stdio};

use super::channel_window;
use super::site::{self, Command, HG_ABORT_STATUS, Next, Refused, Service, Stepped};
use crate::vcs_http::hg::HgStores;

/// Format: 7.2's warning to a client that asks for hidden changesets its server's configuration does not let it see
/// (`commands.py` `serve`, 7.2), before the user it names and a line end.
const HIDDEN_REFUSED: &[u8] = b"ignoring request to access hidden changeset by unauthorized user: ";

/// One `hg serve --stdio` session over SSH (the module doc).
pub(crate) struct HgCommand<S> {
  /// Where the store is lent.
  stores: S,
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
  /// Input that arrived after the session ended, which it dropped: given back to the window at the next step.
  dropped: usize,
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
      repository,
      principal,
      room,
      stdio: Stdio::new(window),
      warning,
      dropped: 0,
    }
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
    self.stdio.receive(run).map_err(past_window)
  }

  fn input_end(&mut self) {
    self.stdio.eof();
  }

  async fn step(&mut self) -> Result<Stepped, Refused> {
    let Self {
      stores,
      repository,
      principal,
      room,
      stdio,
      dropped,
      ..
    } = self;
    if let Some(push_room) = stores.push_room(*repository) {
      stdio.admit_pushes(push_room);
    }
    let progress = if stdio.taking_push() {
      stdio.step_push()
    } else {
      let mut admission = Admission::new(*room);
      stores
        .lend(*repository, &mut admission, |store, left| {
          stdio.step(store, left.left())
        })
        .ok_or_else(unheld)?
    };
    if let Some(push) = stdio.take_push() {
      land(stores, (*repository, principal), push, stdio).await?;
    }
    let (consumed, next) = match progress {
      Progress::Stepped { consumed, .. } | Progress::Took { consumed, .. } => {
        (consumed, Next::More)
      }
      Progress::Waiting => (0, Next::Input),
    };
    let next = if stdio.finished() {
      Next::Exited(EXIT_STATUS)
    } else {
      next
    };
    Ok(Stepped {
      consumed: consumed.saturating_add(std::mem::take(dropped)),
      next,
    })
  }

  fn output(&self) -> (&[u8], &[u8]) {
    if self.warning.is_empty() {
      (self.stdio.stderr(), self.stdio.stdout())
    } else {
      (&self.warning, &[])
    }
  }

  fn sent(&mut self, stderr: usize, stdout: usize) {
    if self.warning.is_empty() {
      self.stdio.sent_stderr(stderr);
      self.stdio.sent_stdout(stdout);
    } else {
      self.warning.drain(..stderr.min(self.warning.len()));
    }
  }
}
