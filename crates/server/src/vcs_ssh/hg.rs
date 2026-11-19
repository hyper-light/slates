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
//! **Bounds.** Each request is admitted the edge's room, as each of upload-pack's is. The input held is at most the
//! channel's window, derived from the same room as the edge derives it (`super::channel_window`), so the session's own
//! bound is the channel's and its refusal of input past it is met only where a caller's room and window disagree. One
//! answer is held at most: the channel takes it before the next request is served.
//!
//! **The store.** A repository whose store the lender does not hold is not served (the site's step 3). A store that
//! goes away mid-session ends the session in the same words, with `abort`'s status. No lender in the daemon holds a
//! Mercurial store until A-662's map is handed to the lender both sites read (GAPS).
//!
//! **Tests** (`tests/hg.rs`, a child of the edge's tests, so that a session also runs over the real edge with their
//! client): the probe stock `hg serve --stdio` 7.2 answered (`crates/hg/fixtures/serve`), after 7.2's handshake,
//! answered byte for byte at every cut down to a byte and over the edge; the hidden warning; a store going away; the
//! window's bookkeeping; the services' start; and the site's refusals in 7.2's recorded words.
//!
//! **Owed** (GAPS): each answer made within the shard's step budget; a `getbundle` is one step today, as over HTTP.

use slates_db::catalog::{Principal, RepositoryId};
use slates_hg::serve::stdio::{EXIT_STATUS, PastWindow, Progress, Stdio};

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
  /// A session over the store of `repository` that `stores` lends, each request admitted `room` bytes and the input
  /// held within the channel's window over that room; `hidden` names the principal when the client asked for hidden
  /// changesets.
  pub(crate) fn new(
    stores: S,
    repository: RepositoryId,
    room: u64,
    hidden: Option<&Principal>,
  ) -> Self {
    let window = usize::try_from(channel_window(room)).unwrap_or(usize::MAX);
    Self {
      stores,
      repository,
      room,
      stdio: Stdio::new(window),
      warning: hidden.map(hidden_refused).unwrap_or_default(),
      dropped: 0,
    }
  }
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
    Ok(Stepped {
      consumed: 0,
      next: Next::Input,
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
