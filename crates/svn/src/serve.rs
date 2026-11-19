//! One ra_svn connection, the server's side, with no I/O (§4.17.4 "5. The wire", piece 7; A-656):
//! the greeting and authentication (A-599's [`handshake::Server`]), then every command the client
//! sends, each answered as `svnserve`'s command loop answers it (`serve.c` `serve` and
//! `svn_ra_svn__handle_commands2` over `main_commands`; Subversion's `1.14.x` at `d434dc14`, 1.15.0
//! alike):
//! - the read commands (A-613's [`reads::answer`]), a long answer's rest resumed a slice at a time
//!   within the caller's room ([`reads::Stream`]);
//! - `reparent`: a trivial auth request, then the session moved to the URL's path, or "'…' is not
//!   the same repository as '…'" (170000) for a URL outside the repository;
//! - `update`, `switch`, `status` and `diff`: the reporter's exchange (A-616's [`Exchange`]), its
//!   drive written within the caller's room and steps;
//! - every other command of `svnserve`'s set, until its piece lands (`commit`, A-655; the lock
//!   verbs; `get-file-revs`, `replay`, `replay-range`, `change-rev-prop`): answered as a command
//!   `svnserve` does not know, "Unknown editor command '…'" (210001), which a stock client reads as
//!   a server without it, and counted ([`Turn::Unserved`]); the connection goes on;
//! - an item that is no command ends the connection, as `svnserve` ends it.
//!
//! **One connection, two transports.** The same connection serves `svn://` over TCP and
//! `svnserve -t` through an SSH tunnel (piece 12): a tunnel's user comes in through the repository
//! its caller names for the client's URL ([`handshake::Repository::tunnel_user`]), and `EXTERNAL`
//! takes it, as `svnserve -t` takes its tunnel's.
//!
//! **What the caller does.** It hands over the client's bytes not yet taken, writes what is
//! appended, and answers what a [`Turn`] asks: resolves the client's URL ([`Connection::open`] or
//! [`Connection::refuse`]), gives a CRAM-MD5 challenge's parts ([`Connection::challenge`]) and a
//! user's key ([`Connection::key`]), and from the session's establishment on hands over the
//! repository's index, green and the principal's guards with every call ([`Sources`]). What the
//! connection waits on its client for, which the caller's waits are learned by, is
//! [`Connection::waiting`].
//!
//! **Bounds.** Every bound is the caller's ([`Limits`]): one item's bytes, the report's room, the
//! nodes a drive holds open at once, each derived from its shard's budget; the deepest list a
//! command holds is `svnserve`'s own deepest prototype ([`COMMAND_DEPTH`]), so a list nested deeper
//! than any command takes ends the connection where `svnserve` would read on to 64 levels.
//!
//! No I/O, no clock, no randomness, no panic.

#[cfg(test)]
mod tests;

use std::ffi::CStr;

use crate::handshake::{
  self, Access, ClientOpen, CramKey, Principal, Repository, Session, Unserved, Verdict,
};
use crate::ra_svn::{Bounds, ItemError, Parsed, Parser, Value};
use crate::reads::{self, Outcome, Stream};
use crate::report::drive::{Guards, Sources};
use crate::report::exchange::{Awaiting, Budget, Exchange, Opened, Turn as ExchangeTurn};
use crate::report::{
  self, Context, ReportFailure, Request, RequestKind, join_fspath, repository_path,
  write_empty_success, write_trivial_auth,
};

/// The deepest list a command holds: `lock-many`'s and `unlock-many`'s, each target a list holding
/// its revision's or token's list, inside the targets' list, inside the parameters, inside the
/// command.
/// Format: `serve.c` `lock_many`'s `"(?c)bl"` with each target `"c(?r)"`, five lists deep.
pub const COMMAND_DEPTH: usize = 5;

/// The longest word a stock parser takes.
/// Format: `marshal.c` `MAX_WORD_LENGTH`, 1.14 and 1.15 alike.
const LONGEST_WORD: usize = 24;

/// What one parsed item costs, by which an item's bytes bound its count.
/// Derived: the size of the item codec's own value, `size_of::<Value>()`.
const PARSED_ITEM_BYTES: usize = size_of::<Value<'static>>();

/// The command that moves a session to another of its repository's paths.
/// Format: `serve.c`'s `main_commands`.
const REPARENT: &str = "reparent";

/// The bounds a connection keeps, each its caller's, derived from its shard's budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
  /// The most bytes one of the client's items may take.
  pub item_bytes: usize,
  /// The most bytes a report may hold.
  pub report_room: usize,
  /// The most nodes a drive holds open at once: the deepest path the repository holds, in names,
  /// and its root.
  pub most_open: usize,
}

/// What one call of [`Connection::receive`] made of the client's bytes, and what the caller does
/// next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Turn<'a> {
  /// The client's bytes end inside its next item: at least `needed` bytes from the input's start.
  NeedMore {
    /// How many bytes from the input's start the item needs at least.
    needed: usize,
  },
  /// The first `consumed` bytes of the input were taken and answered: call again with the rest.
  Took {
    /// The bytes taken.
    consumed: usize,
  },
  /// The output grew by its room or the drive's steps were spent: write the output, then call
  /// again with the same input.
  Wrote,
  /// The client answered the greeting, `consumed` bytes: the caller resolves its URL and calls
  /// [`Connection::open`] or [`Connection::refuse`].
  Opened {
    /// The client's answer.
    open: ClientOpen<'a>,
    /// The bytes taken.
    consumed: usize,
  },
  /// The client chose CRAM-MD5: the caller gives the challenge's parts
  /// ([`Connection::challenge`]) or says it has none ([`Connection::challenge_failed`]).
  ChallengeNeeded {
    /// The bytes taken.
    consumed: usize,
  },
  /// The client's CRAM-MD5 response named `user`: the caller gives that user's key
  /// ([`Connection::key`]).
  KeyNeeded {
    /// The user, as the response named it.
    user: &'a [u8],
    /// The bytes taken.
    consumed: usize,
  },
  /// The client authenticated as `principal`: from here on every call hands over the repository's
  /// sources, the guards the principal's.
  Established {
    /// Who.
    principal: Principal,
    /// The bytes taken.
    consumed: usize,
  },
  /// The client asked for a command of `svnserve`'s set slates does not serve yet: it was answered
  /// "Unknown editor command" (210001), and the connection goes on.
  Unserved {
    /// The command's name.
    command: &'a str,
    /// The bytes taken.
    consumed: usize,
  },
}

/// What the connection waits on its client for, by which its caller learns each wait.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Waiting {
  /// The handshake's answers.
  Handshake,
  /// A command, or the rest of one begun.
  Command,
  /// A report's commands.
  Report,
  /// Nothing: an answer is being written (a drive, a long read's rest).
  Writing,
  /// The client's answer to `close-edit` or `abort-edit`.
  EditAnswer,
  /// Nothing more: the connection has ended.
  Ended,
}

/// Why the connection ends: nothing more is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
  /// The handshake's refusal.
  Handshake(handshake::Refused),
  /// The report's refusal, or the drive's writer's.
  Report(report::Refused),
  /// The client's bytes are no item within the bounds (the codec's refusal).
  Item(ItemError),
  /// An item that is no command, or a command's parameters `svnserve` refuses: "Malformed network
  /// data" (210004), on which `svnserve` ends the connection without a word.
  Malformed,
  /// A call the connection does not take where it stands: the caller's error, not the client's.
  OutOfTurn,
  /// A call after the session's establishment without the repository's sources: the caller's error.
  NoSources,
  /// A repository whose root URL is no ancestor of the client's URL: the caller's error.
  RootUrl,
}

/// The established session, with the path its URL names now.
#[derive(Clone, Debug)]
struct Established {
  /// The session.
  session: Session,
  /// The repository path the session's URL names (`svnserve`'s `fs_path`), moved by `reparent`.
  base: String,
  /// The repository's UUID.
  uuid: String,
}

/// Where the connection stands.
#[derive(Debug)]
enum Phase {
  /// The handshake runs.
  Handshake(Box<handshake::Server>),
  /// Between commands.
  Commands(Box<Established>),
  /// A read's answer has more to write.
  Streaming(Box<Established>, Box<Stream>),
  /// A report command's exchange runs.
  Reporting(Box<Established>, Box<Exchange>),
  /// The connection has ended.
  Ended,
}

/// One connection's server side (module documentation).
#[derive(Debug)]
pub struct Connection {
  /// Where it stands.
  phase: Phase,
  /// Its bounds.
  limits: Limits,
  /// The client's URL, from its answer to the greeting until the caller resolves it.
  url: Vec<u8>,
  /// The path the resolved URL names in its repository, until the session is established.
  base: Option<String>,
}

impl Connection {
  /// A connection whose greeting is appended to `out`.
  ///
  /// # Errors
  /// None in practice: the greeting's words are the grammar's.
  pub fn new(limits: Limits, out: &mut Vec<u8>) -> Result<Self, Refused> {
    let server = handshake::Server::new(limits.item_bytes, out).map_err(Refused::Handshake)?;
    Ok(Self {
      phase: Phase::Handshake(Box::new(server)),
      limits,
      url: Vec::new(),
      base: None,
    })
  }

  /// What the connection waits on its client for now.
  pub fn waiting(&self) -> Waiting {
    match &self.phase {
      Phase::Handshake(_) => Waiting::Handshake,
      Phase::Commands(_) => Waiting::Command,
      Phase::Streaming(..) => Waiting::Writing,
      Phase::Reporting(_, exchange) => match exchange.awaiting() {
        Awaiting::Report => Waiting::Report,
        Awaiting::Drive => Waiting::Writing,
        Awaiting::Answer => Waiting::EditAnswer,
        Awaiting::Nothing => Waiting::Command,
      },
      Phase::Ended => Waiting::Ended,
    }
  }

  /// The established session, once the client has authenticated.
  pub fn session(&self) -> Option<&Session> {
    match &self.phase {
      Phase::Commands(established)
      | Phase::Streaming(established, _)
      | Phase::Reporting(established, _) => Some(&established.session),
      Phase::Handshake(_) | Phase::Ended => None,
    }
  }

  /// Goes on with `input`, the client's bytes not yet taken, appending to `out` within `budget`;
  /// from the session's establishment on, with the repository's `sources`.
  ///
  /// # Errors
  /// [`Refused`] for client bytes that end the connection, and for the caller's own errors (a call
  /// out of turn, or without sources).
  pub fn receive<'a, R: reads::Repository + ?Sized, G: Guards + ?Sized>(
    &mut self,
    sources: Option<&Sources<'_, R, G>>,
    input: &'a [u8],
    out: &mut Vec<u8>,
    budget: Budget,
  ) -> Result<Turn<'a>, Refused> {
    match std::mem::replace(&mut self.phase, Phase::Ended) {
      Phase::Handshake(server) => self.handshake(server, input, out),
      Phase::Commands(established) => {
        self.command(established, sources.ok_or(Refused::NoSources)?, input, out)
      }
      Phase::Streaming(established, stream) => {
        self.stream(established, stream, sources.ok_or(Refused::NoSources)?, out, budget)
      }
      Phase::Reporting(established, exchange) => {
        let sources = sources.ok_or(Refused::NoSources)?;
        self.exchange(established, exchange, sources, input, out, budget)
      }
      Phase::Ended => Err(Refused::OutOfTurn),
    }
  }

  /// Answers the client's URL with `repository`, which names the root of the repository holding it
  /// (`svnserve`'s `find_repos`): the authentication request, or "No access allowed to this
  /// repository" and the connection's end.
  ///
  /// # Errors
  /// [`Refused::OutOfTurn`] unless the client's answer was just read; [`Refused::RootUrl`] for a
  /// root that is no ancestor of the client's URL.
  pub fn open(
    &mut self,
    repository: &Repository<'_>,
    out: &mut Vec<u8>,
  ) -> Result<Access, Refused> {
    let Phase::Handshake(server) = &mut self.phase else {
      return Err(Refused::OutOfTurn);
    };
    let url = std::str::from_utf8(&self.url).map_err(|_| Refused::RootUrl)?;
    let base = repository_path(repository.root_url, url).map_err(|_| Refused::RootUrl)?;
    let access = server.open(repository, out).map_err(Refused::Handshake)?;
    match access {
      Access::Asked => self.base = Some(base),
      Access::Denied => self.phase = Phase::Ended,
    }
    Ok(access)
  }

  /// Answers the client's URL with the command failure `unserved`; the connection ends.
  ///
  /// # Errors
  /// [`Refused::OutOfTurn`] unless the client's answer was just read.
  pub fn refuse(&mut self, unserved: Unserved<'_>, out: &mut Vec<u8>) -> Result<(), Refused> {
    let Phase::Handshake(server) = &mut self.phase else {
      return Err(Refused::OutOfTurn);
    };
    server.refuse(unserved, out).map_err(Refused::Handshake)?;
    self.phase = Phase::Ended;
    Ok(())
  }

  /// Sends the CRAM-MD5 challenge `<nonce.time@host>`, its parts the caller's.
  ///
  /// # Errors
  /// [`Refused::OutOfTurn`] unless the client just chose CRAM-MD5.
  pub fn challenge(
    &mut self,
    nonce: u64,
    time_micros: i64,
    host: &CStr,
    out: &mut Vec<u8>,
  ) -> Result<(), Refused> {
    let Phase::Handshake(server) = &mut self.phase else {
      return Err(Refused::OutOfTurn);
    };
    server
      .challenge(nonce, time_micros, host, out)
      .map_err(Refused::Handshake)
  }

  /// Fails the CRAM-MD5 attempt the client just chose: no challenge could be made.
  ///
  /// # Errors
  /// [`Refused::OutOfTurn`] unless the client just chose CRAM-MD5.
  pub fn challenge_failed(&mut self, out: &mut Vec<u8>) -> Result<(), Refused> {
    let Phase::Handshake(server) = &mut self.phase else {
      return Err(Refused::OutOfTurn);
    };
    server.challenge_failed(out).map_err(Refused::Handshake)
  }

  /// Verifies the CRAM-MD5 response with the named user's key, `None` where the repository holds
  /// none: established, the session's commands follow.
  ///
  /// # Errors
  /// [`Refused::OutOfTurn`] unless a response naming a user was just read.
  pub fn key(&mut self, key: Option<&CramKey>, out: &mut Vec<u8>) -> Result<Verdict, Refused> {
    let Phase::Handshake(server) = &mut self.phase else {
      return Err(Refused::OutOfTurn);
    };
    let verdict = server.key(key, out).map_err(Refused::Handshake)?;
    if matches!(verdict, Verdict::Established(_)) {
      self.establish()?;
    }
    Ok(verdict)
  }

  /// The handshake's next step.
  fn handshake<'a>(
    &mut self,
    mut server: Box<handshake::Server>,
    input: &'a [u8],
    out: &mut Vec<u8>,
  ) -> Result<Turn<'a>, Refused> {
    let progress = server.receive(input, out).map_err(Refused::Handshake)?;
    self.phase = Phase::Handshake(server);
    Ok(match progress {
      handshake::Progress::NeedMore { needed } => Turn::NeedMore { needed },
      handshake::Progress::Opened { open, consumed } => {
        self.url = open.url.to_vec();
        Turn::Opened { open, consumed }
      }
      handshake::Progress::ChallengeNeeded { consumed } => Turn::ChallengeNeeded { consumed },
      handshake::Progress::KeyNeeded { user, consumed } => Turn::KeyNeeded { user, consumed },
      handshake::Progress::Failed { consumed, .. } => Turn::Took { consumed },
      handshake::Progress::Established {
        principal,
        consumed,
      } => {
        self.establish()?;
        Turn::Established {
          principal,
          consumed,
        }
      }
    })
  }

  /// The session established: its commands follow at the path the client's URL named.
  fn establish(&mut self) -> Result<(), Refused> {
    let Phase::Handshake(server) = &self.phase else {
      return Err(Refused::OutOfTurn);
    };
    let session = server.session().cloned().ok_or(Refused::OutOfTurn)?;
    let base = self.base.take().ok_or(Refused::OutOfTurn)?;
    let uuid = String::from_utf8_lossy(&session.uuid).into_owned();
    self.phase = Phase::Commands(Box::new(Established {
      session,
      base,
      uuid,
    }));
    Ok(())
  }

  /// The client's next command, answered.
  fn command<'a, R: reads::Repository + ?Sized, G: Guards + ?Sized>(
    &mut self,
    mut established: Box<Established>,
    sources: &Sources<'_, R, G>,
    input: &'a [u8],
    out: &mut Vec<u8>,
  ) -> Result<Turn<'a>, Refused> {
    let (value, consumed) = match self.parse(input)? {
      Parsed::NeedMore { needed } => {
        self.phase = Phase::Commands(established);
        return Ok(Turn::NeedMore { needed });
      }
      Parsed::Item { value, consumed } => (value, consumed),
    };
    let session = reads::Session {
      base: &established.base,
      uuid: &established.uuid,
    };
    let outcome = reads::answer(sources.index, sources.repository, &session, &value, out)
      .map_err(Refused::Item)?;
    match outcome {
      Outcome::Answered => {
        self.phase = Phase::Commands(established);
        Ok(Turn::Took { consumed })
      }
      Outcome::Streaming(stream) => {
        self.phase = Phase::Streaming(established, Box::new(stream));
        Ok(Turn::Took { consumed })
      }
      Outcome::Close => Err(Refused::Malformed),
      Outcome::Elsewhere => {
        let (name, params) = command_of(&value).ok_or(Refused::Malformed)?;
        let answered = self.elsewhere(&mut established, sources, name, params, out)?;
        Ok(match answered {
          Answered::Here => {
            self.phase = Phase::Commands(established);
            Turn::Took { consumed }
          }
          Answered::Reporting(exchange) => {
            self.phase = Phase::Reporting(established, exchange);
            Turn::Took { consumed }
          }
          Answered::Unserved => {
            self.phase = Phase::Commands(established);
            Turn::Unserved {
              command: name,
              consumed,
            }
          }
        })
      }
    }
  }

  /// A command the read commands hand over: `reparent`, a report command, or one not served yet.
  fn elsewhere<R: reads::Repository + ?Sized, G: Guards + ?Sized>(
    &self,
    established: &mut Established,
    sources: &Sources<'_, R, G>,
    name: &str,
    params: &[Value<'_>],
    out: &mut Vec<u8>,
  ) -> Result<Answered, Refused> {
    if name == REPARENT {
      reparent(established, params, out)?;
      return Ok(Answered::Here);
    }
    let Some(kind) = RequestKind::of(name) else {
      ReportFailure::UnknownCommand {
        name: name.to_owned(),
      }
      .write(out)
      .map_err(Refused::Report)?;
      return Ok(Answered::Unserved);
    };
    let request = Request::read(kind, params).map_err(Refused::Report)?;
    let target = join_fspath(&established.base, &request.target);
    let context = Context {
      session: &established.session,
      session_path: &established.base,
      youngest: sources.index.youngest(),
      readable: sources.guards.may_read(&target),
      room: self.limits.report_room,
      item_bytes: self.limits.item_bytes,
      most_open: self.limits.most_open,
    };
    Ok(
      match Exchange::begin(&context, request, out).map_err(Refused::Report)? {
        Opened::Reporting(exchange) => Answered::Reporting(exchange),
        Opened::Failed(_) => Answered::Here,
      },
    )
  }

  /// A read's answer resumed within the room.
  fn stream<'a, R: reads::Repository + ?Sized, G: Guards + ?Sized>(
    &mut self,
    established: Box<Established>,
    mut stream: Box<Stream>,
    sources: &Sources<'_, R, G>,
    out: &mut Vec<u8>,
    budget: Budget,
  ) -> Result<Turn<'a>, Refused> {
    let session = reads::Session {
      base: &established.base,
      uuid: &established.uuid,
    };
    let progress = stream
      .resume(sources.index, sources.repository, &session, out, budget.room)
      .map_err(Refused::Item)?;
    self.phase = match progress {
      reads::Progress::More => Phase::Streaming(established, stream),
      reads::Progress::Done => Phase::Commands(established),
    };
    Ok(Turn::Wrote)
  }

  /// A report command's exchange, on.
  fn exchange<'a, R: reads::Repository + ?Sized, G: Guards + ?Sized>(
    &mut self,
    established: Box<Established>,
    mut exchange: Box<Exchange>,
    sources: &Sources<'_, R, G>,
    input: &'a [u8],
    out: &mut Vec<u8>,
    budget: Budget,
  ) -> Result<Turn<'a>, Refused> {
    let turn = exchange
      .receive(sources, input, out, budget)
      .map_err(Refused::Report)?;
    Ok(match turn {
      ExchangeTurn::NeedMore { needed } => {
        self.phase = Phase::Reporting(established, exchange);
        Turn::NeedMore { needed }
      }
      ExchangeTurn::Took { consumed } => {
        self.phase = Phase::Reporting(established, exchange);
        Turn::Took { consumed }
      }
      ExchangeTurn::Wrote => {
        self.phase = Phase::Reporting(established, exchange);
        Turn::Wrote
      }
      ExchangeTurn::Answered { consumed } => {
        self.phase = Phase::Commands(established);
        Turn::Took { consumed }
      }
    })
  }

  /// The client's next item within the connection's bounds.
  fn parse<'a>(&self, input: &'a [u8]) -> Result<Parsed<'a>, Refused> {
    let bounds = Bounds {
      item_bytes: self.limits.item_bytes,
      word_bytes: LONGEST_WORD,
      depth: COMMAND_DEPTH,
      nodes: self
        .limits
        .item_bytes
        .checked_div(PARSED_ITEM_BYTES)
        .unwrap_or(0),
    };
    Parser::new(bounds).parse(input).map_err(Refused::Item)
  }
}

/// How a command the read commands handed over was answered.
enum Answered {
  /// Here: `reparent`, or a report command refused at once.
  Here,
  /// A report command begun: its exchange runs.
  Reporting(Box<Exchange>),
  /// As a command not served yet.
  Unserved,
}

/// `reparent`: a trivial auth request, then the session moved to the URL's path, or refused for a
/// URL outside the repository (`serve.c` `reparent`). A URL that is no string, holds a NUL or is no
/// UTF-8 ends the connection, as the report's do.
fn reparent(
  established: &mut Established,
  params: &[Value<'_>],
  out: &mut Vec<u8>,
) -> Result<(), Refused> {
  let Some(Value::String(url)) = params.first() else {
    return Err(Refused::Malformed);
  };
  if url.contains(&0) {
    return Err(Refused::Malformed);
  }
  let url = std::str::from_utf8(url).map_err(|_| Refused::Malformed)?;
  write_trivial_auth(out).map_err(Refused::Report)?;
  match repository_path(&established.session.root_url, url) {
    Ok(base) => {
      established.base = base;
      write_empty_success(out).map_err(Refused::Report)
    }
    Err(failure) => failure.write(out).map_err(Refused::Report),
  }
}

/// A command item's name, lent from the client's bytes, and its parameters.
fn command_of<'v, 'a>(value: &'v Value<'a>) -> Option<(&'a str, &'v [Value<'a>])> {
  let Value::List(parts) = value else {
    return None;
  };
  match parts.as_slice() {
    [Value::Word(name), Value::List(params), ..] => Some((*name, params)),
    _ => None,
  }
}
