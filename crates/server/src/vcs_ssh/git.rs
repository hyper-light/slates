//! git's upload-pack behind the SSH edge (A-651 part 2; §4.17.2 "A fetch" and "Transports" item 2): A-640 part 2's one
//! entry point, `slates_git::upload::serve`, driven over an SSH session's standard input and output, the repository's
//! store lent a step at a time by the HTTP edge's [`StoreLender`] (A-625 part 3), so both edges reach a store one way.
//!
//! **The session.** Over SSH, upload-pack speaks first: the advertisement (`Asked::Advertise`) is written as soon as the
//! command starts. Then each request is read whole as the client's bytes arrive: `v2::request::whole_len` measures the
//! first whole request the held input begins with (A-640 part 2: "or what `request::whole_len` measured of an SSH
//! session's input"), which is consumed, served (`Asked::Request`) and answered. A flush alone ends the session
//! (`Answer::ends_session`), and the command exits 0. A request upload-pack refuses is answered in git's words (`ERR`,
//! or band 3 under `sideband-all`) and then ends the session with git's `die` status, 128, as git's own server dies on
//! such a request (`serve.c` at `e1ec09b2`: "invalid command", "unknown capability") \[C\]; bytes that are no pkt-lines are
//! handed to upload-pack whole, which answers them so. The end of the client's input before a whole request ends the
//! session, 0.
//!
//! **Bounds.** Each request is admitted the edge's room (`Admission`), as the HTTP edge admits one. An answer is written
//! one whole pkt-line's room a step (`pktline::MAX_LEN`, the format's own limit), and the edge steps the command again only
//! once that step's bytes have gone to the channel, so the output held is a packet at most. The input held is at most the
//! channel's window, which the session refuses to exceed; what a request consumed is given back to it.
//!
//! **The store.** A repository whose store the lender does not hold is not served (the site's not-served words). A
//! store that goes away mid-session ends it in the same words. Until the server holds stores on their owner shards
//! (A-662), no lender holds one.

use slates_db::catalog::RepositoryId;
use slates_git::Admission;
use slates_git::http::Version;
use slates_git::pktline::MAX_LEN;
use slates_git::upload::{self, Answer, Asked, PackSource, Written};
use slates_git::v2::request::whole_len;

use super::site::{Command, GIT_DIE_STATUS, Next, Refused, Stepped};
use crate::vcs_http::git::upload::StoreLender;

/// What the session does once the answer being written has ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum After {
  /// Read the next request.
  Next,
  /// End the session: the request was a flush alone.
  End,
  /// End it as git's server dies: the request was refused.
  Die,
}

/// The words a session whose store is not held here ends with.
fn unheld() -> Refused {
  Refused {
    stderr: b"fatal: git-upload-pack is not served yet\n".to_vec(),
    exit_status: GIT_DIE_STATUS,
  }
}

/// One upload-pack session over SSH (the module doc).
pub(crate) struct UploadCommand<L: StoreLender> {
  /// Where the store is lent.
  lender: L,
  /// The repository.
  repository: RepositoryId,
  /// The protocol version the client asked for (`GIT_PROTOCOL`).
  version: Version,
  /// The room each request is admitted.
  room: u64,
  /// The client's input held, not yet consumed.
  input: Vec<u8>,
  /// Whether the client's input has ended.
  input_ended: bool,
  /// What the command wrote that the channel has not taken.
  out: Vec<u8>,
  /// Whether the advertisement was made.
  advertised: bool,
  /// The answer being written.
  answer: Option<Answer<<L::Packs as PackSource>::Pack>>,
  /// What follows it.
  after: After,
}

impl<L: StoreLender> UploadCommand<L> {
  /// A session over the store of `repository` that `lender` lends, at protocol `version`, each request admitted `room`
  /// bytes.
  pub(crate) fn new(lender: L, repository: RepositoryId, version: Version, room: u64) -> Self {
    Self {
      lender,
      repository,
      version,
      room,
      input: Vec::new(),
      input_ended: false,
      out: Vec::new(),
      advertised: false,
      answer: None,
      after: After::Next,
    }
  }

  /// Answers `asked` over the lent store, within a fresh admission of the room; refused when the store is no longer held.
  fn serve(&mut self, asked: Asked<'_>) -> Result<(), Refused> {
    let Self {
      lender,
      repository,
      version,
      room,
      answer,
      after,
      ..
    } = self;
    let mut admission = Admission::new(*room);
    let made = lender
      .lend(*repository, |served, packs| {
        upload::serve(served, *version, asked, packs, &mut admission)
      })
      .ok_or_else(unheld)?;
    *after = if made.ends_session() {
      After::End
    } else if made.refusal().is_some() {
      After::Die
    } else {
      After::Next
    };
    *answer = Some(made);
    Ok(())
  }

  /// Writes one step of the answer, a whole pkt-line's room at most; what follows once it has ended.
  fn write_answer(&mut self) -> Result<Next, Refused> {
    let Self {
      lender,
      repository,
      answer,
      out,
      after,
      ..
    } = self;
    let Some(made) = answer.as_mut() else {
      return Ok(Next::More);
    };
    let written = lender
      .lend(*repository, |served, packs| {
        made.write(served, packs, out, MAX_LEN)
      })
      .ok_or_else(unheld)?;
    if written == Written::More {
      return Ok(Next::More);
    }
    *answer = None;
    Ok(match after {
      After::Next => Next::More,
      After::End => Next::Exited(0),
      After::Die => Next::Exited(GIT_DIE_STATUS),
    })
  }

  /// Serves the next whole request the input begins with: what it consumed, and what follows.
  fn next_request(&mut self) -> Result<Stepped, Refused> {
    let length = match whole_len(&self.input) {
      Ok(Some(length)) => length.min(self.input.len()),
      Ok(None) if self.input_ended => {
        return Ok(Stepped {
          consumed: 0,
          next: Next::Exited(0),
        });
      }
      Ok(None) => {
        return Ok(Stepped {
          consumed: 0,
          next: Next::Input,
        });
      }
      // No pkt-lines: upload-pack answers them as it refuses them, and the session ends.
      Err(_) => self.input.len(),
    };
    let request: Vec<u8> = self.input.drain(..length).collect();
    self.serve(Asked::Request(&request))?;
    Ok(Stepped {
      consumed: length,
      next: Next::More,
    })
  }
}

impl<L: StoreLender> Command for UploadCommand<L> {
  fn input(&mut self, run: &[u8]) -> Result<(), Refused> {
    self.input.extend_from_slice(run);
    Ok(())
  }

  fn input_end(&mut self) {
    self.input_ended = true;
  }

  async fn step(&mut self) -> Result<Stepped, Refused> {
    Ok(Stepped {
      consumed: 0,
      next: Next::Exited(0),
    })
  }

  fn output(&self) -> (&[u8], &[u8]) {
    (&[], &self.out)
  }

  fn sent(&mut self, _stderr: usize, stdout: usize) {
    self.out.drain(..stdout.min(self.out.len()));
  }
}

#[cfg(test)]
#[path = "tests/git.rs"]
mod tests;
