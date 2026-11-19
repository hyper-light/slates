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
//! handed to upload-pack whole, which answers them so. The end of the client's input before a request's first packet is
//! whole ends the session, 0; its end inside a request is refused in git's words with `die`'s status, as git's reader is
//! gentle on the end of its input only until it has peeked a request's first packet (`serve.c` `process_request` clears
//! `PACKET_READ_GENTLE_ON_EOF` then, and `pkt-line.c` `get_packet_data` dies at a later short read) \[C\].
//!
//! **Protocols.** A session is served at protocol v2, which git's client speaks over SSH by default (slates-git's
//! `upload` module doc: since git 2.26), asking for it through `GIT_PROTOCOL`, the session channel's one `env`. A
//! session at v0 or v1 is refused at its start, before any advertisement: an `ERR` line naming v2, the way upload-pack
//! refuses a request, then `die`'s status. slates-git answers those protocols statelessly, a whole request at a time as
//! smart HTTP sends them (A-640 part 3), but a v0 client over SSH negotiates in rounds on one stream, sending its wants
//! once and its haves round by round, so a stateless answer to its second round would have lost its wants. That session
//! is owed (GAPS).
//!
//! **Bounds.** Each request is admitted the edge's room (`Admission`), as the HTTP edge admits one. An answer is written
//! one whole pkt-line's room a step (`pktline::MAX_LEN`, the format's own limit), and the edge steps the command again only
//! once that step's bytes have gone to the channel, so the output held is a packet at most. The input held is at most the
//! channel's window, which the session refuses to exceed; what a request consumed is given back to it.
//!
//! **The store.** A repository whose store the lender does not hold is not served (the site's not-served words). A
//! store that goes away mid-session ends it in the same words. A-662 holds each repository's store on its owner shard,
//! and the daemon's lender lends it from that map (`crate::vcs_store::upload::ShardStores`, A-682): each request's step
//! with the refs the session's principal may read, each step after it with the objects alone.

use slates_db::catalog::{Principal, RepositoryId};
use slates_git::Admission;
use slates_git::http::Version;
use slates_git::pktline::{self, MAX_LEN};
use slates_git::upload::{self, Answer, Asked, PackSource, Written};
use slates_git::v2::request::whole_len;

use super::site::{Command, GIT_DIE_STATUS, Next, Refused, Stepped};
use crate::vcs_http::git::StoreLender;

/// Format: git's error packet's prefix (gitprotocol-pack: an `ERR` pkt-line, whose reason the client shows).
const ERR: &[u8] = b"ERR ";

/// Why a session at protocol v0 or v1 is refused (the module doc's protocols), as its client shows it.
const VERSION_REFUSED: &[u8] =
  b"upload-pack serves protocol v2 alone over SSH: ask for it with git -c protocol.version=2";

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

/// The words git's reader dies with when its input ends inside a request (`pkt-line.c` `get_packet_data`).
fn hung_up() -> Refused {
  Refused {
    stderr: b"fatal: the remote end hung up unexpectedly\n".to_vec(),
    exit_status: GIT_DIE_STATUS,
  }
}

/// One upload-pack session over SSH (the module doc).
pub(crate) struct UploadCommand<L: StoreLender> {
  /// Where the store is lent.
  lender: L,
  /// The repository.
  repository: RepositoryId,
  /// The session's principal: the refs each request's step is lent are the ones it may read.
  reader: Principal,
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
  /// The admission of the request it answers, lent to each step of its writing (A-653 continued (4)).
  admission: Admission,
  /// What follows it.
  after: After,
}

impl<L: StoreLender> UploadCommand<L> {
  /// A session of `reader` over the store of `repository` that `lender` lends, at protocol `version`, each request
  /// admitted `room` bytes.
  pub(crate) fn new(
    lender: L,
    (repository, reader): (RepositoryId, Principal),
    version: Version,
    room: u64,
  ) -> Self {
    Self {
      lender,
      repository,
      reader,
      version,
      room,
      input: Vec::new(),
      input_ended: false,
      out: Vec::new(),
      advertised: false,
      answer: None,
      admission: Admission::new(room),
      after: After::Next,
    }
  }

  /// Answers `asked` over the lent store, within a fresh admission of the room, kept for the answer's writing; refused
  /// when the store is no longer held.
  fn serve(&mut self, asked: Asked<'_>) -> Result<(), Refused> {
    let Self {
      lender,
      repository,
      reader,
      version,
      room,
      answer,
      admission,
      after,
      ..
    } = self;
    *admission = Admission::new(*room);
    let made = lender
      .lend((*repository, reader), |served, packs| {
        upload::serve(served, *version, asked, packs, admission)
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
      admission,
      out,
      after,
      ..
    } = self;
    let Some(made) = answer.as_mut() else {
      return Ok(Next::More);
    };
    let written = lender
      .lend_objects(*repository, |served, packs| {
        made.write(served, packs, admission, out, MAX_LEN)
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

  /// Refuses a session at protocol v0 or v1 at its start (the module doc's protocols): one `ERR` packet, written as
  /// upload-pack writes a refusal, then git's `die` status. A line too long for a packet, which these words never are,
  /// is refused in the same words on standard error.
  fn refuse_version(&mut self) -> Result<Stepped, Refused> {
    let line = [ERR, VERSION_REFUSED].concat();
    pktline::write(pktline::Packet::Data(&line), &mut self.out).map_err(|_too_long| Refused {
      stderr: [b"fatal: ".as_slice(), VERSION_REFUSED, b"\n"].concat(),
      exit_status: GIT_DIE_STATUS,
    })?;
    Ok(Stepped {
      consumed: 0,
      next: Next::Exited(GIT_DIE_STATUS),
    })
  }

  /// Serves the next whole request the input begins with: what it consumed, and what follows.
  fn next_request(&mut self) -> Result<Stepped, Refused> {
    let length = match whole_len(&self.input) {
      Ok(Some(length)) => length.min(self.input.len()),
      Ok(None) if self.input_ended => return self.ended(),
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

  /// The client's input ended before a whole request (the module doc): the session ended, 0, while the request's first
  /// packet is short; refused with git's `die` status once it is whole, in its reader's words. (git judges each line as
  /// it reads it, so where a line before the end is one it refuses, it dies in that refusal's words instead, as at a
  /// delimiter before any command: "no command requested"; the status is the same.) `whole_len` has read the first
  /// packet's length already, so it is whole or short here.
  fn ended(&self) -> Result<Stepped, Refused> {
    if matches!(pktline::read(&self.input, 0), Ok(Some(_))) {
      return Err(hung_up());
    }
    Ok(Stepped {
      consumed: 0,
      next: Next::Exited(0),
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
    if self.answer.is_some() {
      let next = self.write_answer()?;
      return Ok(Stepped { consumed: 0, next });
    }
    if !self.advertised {
      self.advertised = true;
      if !matches!(self.version, Version::V2) {
        return self.refuse_version();
      }
      self.serve(Asked::Advertise)?;
      return Ok(Stepped {
        consumed: 0,
        next: Next::More,
      });
    }
    self.next_request()
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
