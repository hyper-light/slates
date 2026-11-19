//! Mercurial's HTTP version 1 served (A-650 part 1; §4.17.3 "The wire surface"; 7.2's `wireprotoserver.py`
//! `handlewsgirequest`, `_callhttp`, `_httpresponsetype` and `httpv1protocolhandler` \[C\]): a request's command and
//! arguments read through A-569's codecs ([`crate::http`]), the command answered as the session answers it (its
//! dispatch over the HTTP transport, [`session`]), and the answer framed as 7.2 frames it over HTTP.
//!
//! **A request.** Its command is the query's last `cmd` field; its arguments are read by 7.2's spec for the command
//! from the query and either the body's front (`X-HgArgs-Post`) or the numbered `X-HgArg-<N>` headers. One naming no
//! command of 7.2's table is answered as 7.2 answers a wire request it cannot serve ("requested wire protocol command
//! is not available over HTTP"); a request that is no wire request at all (hgweb's pages) is its caller's to route.
//!
//! **An answer** (`_callhttp`). A string is `application/mercurial-0.1`, as it is. An error is `application/hg-error`,
//! its words the body, ended by a line end as 7.2 forwards an abort (`exc.message + b'\n'`), or 7.2's own words where
//! 7.2 has them. A stream (`getbundle`'s) is `application/mercurial-0.2` to a client that reads `0.2` and names an
//! engine this crate writes: a byte giving the engine's name's length, the name, then the stream as that engine
//! compresses it; to any other client, `application/mercurial-0.1` compressed by zlib, which every client inflates
//! (`_httpresponsetype`). The abort part 7.2 answers a refused bundle2 request with goes uncompressed to a `0.2`
//! client, as 7.2 sends it. Every answer here is a `200`: hgweb's statuses (401, 403, 404, 405) are decided by the
//! caller from the request's head, before this module is asked.
//!
//! **Compression is the caller's** ([`Compressor`]): the engine among those the client reads, in the client's order,
//! and the level within the engine's range, which §4.17.3 learns from the connection's delivery rate and the
//! repository's own compression speed and ratio. An engine the client does not read is refused, never sent. Every
//! byte of an answer is charged to the request's admission before it is made, a compressed stream's at its engine's
//! documented bound ([`compression::bound`]).
//!
//! **A push** (A-680 part 2b; 7.2's `wireprotov1server.unbundle` and `_callhttp`). [`unbundle_request`] reads
//! `unbundle`'s `heads` argument by its spec and takes the body after any arguments there as the bundle; the caller
//! lands the push (`crate::push`'s steps over the repository it holds) and [`pushed`] frames what it came to as 7.2
//! frames each answer of `unbundle`: a bundle2 reply (accepted, or holding its error part) in
//! `application/mercurial-0.1` as it is, with no engine whatever the client reads (`streamreslegacy`); a refusal before
//! the bundle2 was processed as `pusherr` in `0.1` (`0`, a line end, the words, a line end); a bundle1 in 7.2's
//! `bundle2required` words, out of band.
//!
//! **Stricter than 7.2, on purpose.** A named argument the request lacks, or post arguments that are not a length
//! within the body, is answered out of band, where 7.2's `KeyError` and `int()` end in a `500`; so is a `heads` item
//! that is not hexadecimal, where 7.2's `bin` fails.
//!
//! **Differs from 7.2, on purpose.** Over HTTP 7.2 captures what its server prints while it takes a push (`adding
//! changesets`, `added 1 changesets with 1 changes to 1 files`; `exchange.unbundle`'s `captureoutput` for a `remote:http`
//! URL) into `output` parts of the reply, which its client prints after `remote:`. They are its progress words, not the
//! push's answer: no part a client acts on, so a reply here carries none of them, as a push over SSH here prints none.

use super::Refusal;
use super::getbundle::Store;
use super::session::{self, Answer, BUNDLE2_REQUIRED, Refused, Transport, dispatch, spec_of};
use crate::compression::{self, Engine};
use crate::http::{self, MEDIA_TYPE_ERROR, MEDIA_TYPE_V1, MEDIA_TYPE_V2, ProtocolCaps, Request};
use crate::push::Outcome;
use crate::push::check::{self, HeadsArgument, HeadsArgumentRefusal};
use crate::push::reply::Reply;
use crate::read::to_u64;
use crate::{Admission, PastAdmission};

/// Format: the protocol capability a client that reads `application/mercurial-0.2` sends (`X-HgProto`).
const READS_V2: &[u8] = b"0.2";

/// Format: the line end that ends an error's words, as 7.2 forwards an abort over HTTP (`exc.message + b'\n'`).
const LINE_END: u8 = b'\n';

/// Format: the bytes before an `application/mercurial-0.2` body's engine name: the one byte giving its length
/// (`genversion2`).
const V2_LENGTH_BYTE: u64 = 1;

/// Format: the commands 7.2's table marks `push` (`wireprotov1server`'s `permission=b'push'`); every other command it
/// holds is `pull`.
const PUSH_COMMANDS: [&[u8]; 2] = [b"pushkey", b"unbundle"];

/// Format: the command a push's bundle is sent with, and the argument its spec names (`unbundle`, `heads`).
pub const UNBUNDLE: &[u8] = b"unbundle";
/// Format: see [`UNBUNDLE`].
const HEADS: &[u8] = b"heads";

/// Format: the bytes `pusherr` frames its words with (`0` and a line end before them, a line end after).
const PUSH_ERROR_FRAMING: u64 = 3;

/// What a command asks of its caller's principal, as 7.2's table states it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Permission {
  /// A read: every command but the two that push.
  Pull,
  /// A push: `pushkey` and `unbundle`.
  Push,
}

/// The permission `command` needs, `None` for a name 7.2's table lacks: a request naming such a command, or none, is
/// no wire request, and 7.2 hands it to hgweb.
pub fn permission(command: &[u8]) -> Option<Permission> {
  spec_of(command).map(|_| {
    if PUSH_COMMANDS.contains(&command) {
      Permission::Push
    } else {
      Permission::Pull
    }
  })
}

/// The command a request's query names, decoded: its last `cmd` field, as hgweb reads it; `None` where it names none.
/// The query's decoded fields are charged to `admission`, and freed.
pub fn command(query: &[u8], admission: &mut Admission) -> Result<Option<Vec<u8>>, http::Refusal> {
  let mut buffer = Vec::new();
  let request = Request {
    query,
    headers: &[],
    body: &[],
  };
  let fields = http::fields(&request, admission, &mut buffer)?;
  Ok(fields.command().map(<[u8]>::to_vec))
}

/// A stream answer's media type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Media {
  /// `application/mercurial-0.1`: zlib, the one engine every client inflates.
  V1,
  /// `application/mercurial-0.2`: the engine its header names.
  V2,
}

/// A stream answer's compression: its engine and the level it compresses at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Compression {
  /// The engine.
  pub engine: Engine,
  /// The level, within the engine's [`Engine::levels`].
  pub level: i32,
}

/// What chooses a stream answer's compression (§4.17.3 "Learned tunables", the response compression's row).
pub trait Compressor {
  /// The compression for a stream of `length` bytes answered in `media`, its engine one of `engines` (the client's
  /// order: those it names that this crate writes, or zlib alone for `application/mercurial-0.1`).
  fn choose(&mut self, media: Media, engines: &[Engine], length: u64) -> Compression;
}

/// An HTTP answer: its media type and body, and the refusal an error or an abort reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answered {
  /// The media type: the reply's `Content-Type`.
  pub media_type: &'static [u8],
  /// The body.
  pub body: Vec<u8>,
  /// Why the request was refused, where the answer is an error or an abort.
  pub refused: Option<Refused>,
}

/// Answers `request` from `store` (the module doc): `capabilities` advertising `header_line` as `httpheader`, a stream
/// compressed as `compressor` chooses, everything held charged to `admission`.
pub fn answer(
  store: &impl Store,
  request: &Request<'_>,
  header_line: u64,
  compressor: &mut impl Compressor,
  admission: &mut Admission,
) -> Answered {
  let mut buffer = Vec::new();
  let fields = match http::fields(request, admission, &mut buffer) {
    Ok(fields) => fields,
    Err(refusal) => return error(Refused::Http(refusal)),
  };
  let named = fields
    .command()
    .and_then(|command| spec_of(command).map(|spec| (command, spec)));
  let Some((command, spec)) = named else {
    return not_available();
  };
  let arguments = match fields.arguments(spec, admission) {
    Ok(arguments) => arguments,
    Err(refusal) => return error(Refused::Http(refusal)),
  };
  let transport = Transport::Http { header_line };
  let (answered, refused) = dispatch(store, transport, command, spec, &arguments, admission);
  match answered {
    Answer::Bytes(body) => Answered {
      media_type: MEDIA_TYPE_V1,
      body,
      refused,
    },
    Answer::OutOfBand(words) => Answered {
      media_type: MEDIA_TYPE_ERROR,
      body: ended(words),
      refused,
    },
    Answer::Stream {
      stream,
      prefer_uncompressed,
    } => match streamed(&stream, prefer_uncompressed, request, compressor, admission) {
      Ok((media_type, body)) => Answered {
        media_type,
        body,
        refused,
      },
      Err(refused) => error(refused),
    },
  }
}

/// What an HTTP `unbundle` brings (the module doc's push): its `heads` argument, read by `unbundle`'s spec from the query
/// and the `X-HgArg` headers or the body's front, and its bundle, the body after any arguments there; or, out of band,
/// the answer to arguments that cannot be read. Everything read is charged to `admission`.
pub fn unbundle_request<'r>(
  request: &Request<'r>,
  admission: &mut Admission,
) -> Result<(HeadsArgument, &'r [u8]), Answered> {
  let mut buffer = Vec::new();
  let fields = http::fields(request, admission, &mut buffer)
    .map_err(|refusal| error(Refused::Http(refusal)))?;
  let spec = spec_of(UNBUNDLE).ok_or_else(not_available)?;
  let arguments = fields
    .arguments(spec, admission)
    .map_err(|refusal| error(Refused::Http(refusal)))?;
  let value = arguments
    .get(spec, HEADS)
    .ok_or_else(|| error(Refused::MissingArgument { name: HEADS }))?;
  let heads = check::heads_argument(value, admission).map_err(|refusal| {
    error(match refusal {
      HeadsArgumentRefusal::NotHex { at } => Refused::Serve(Refusal::NodeList { at }),
      HeadsArgumentRefusal::PastAdmission(past) => Refused::from(past),
    })
  })?;
  let bundle = http::payload(request).map_err(|refusal| error(Refused::Http(refusal)))?;
  Ok((heads, bundle))
}

/// The answer to a push over HTTP (the module doc's push): a reply in `application/mercurial-0.1` as it is, its length
/// charged to `admission` before it is written; a refusal before the bundle2 was processed as `pusherr`; a bundle1 in
/// 7.2's `bundle2required` words. A refused push reports [`Refused::Push`]; its words are in its answer.
pub fn pushed(outcome: &Outcome, admission: &mut Admission) -> Answered {
  match outcome {
    Outcome::Accepted(reply) => replied(reply, None, admission),
    Outcome::Refused(reply) => replied(reply, Some(Refused::Push), admission),
    Outcome::Unprocessed(words) => push_error(words, admission),
    Outcome::Bundle1 => Answered {
      media_type: MEDIA_TYPE_ERROR,
      body: BUNDLE2_REQUIRED.to_vec(),
      refused: Some(Refused::Serve(Refusal::NoBundle2)),
    },
  }
}

/// A push's reply in `0.1`, as it is, charged first; `refused` reports a reply holding an error part.
fn replied(reply: &Reply, refused: Option<Refused>, admission: &mut Admission) -> Answered {
  let unencodable = || {
    error(Refused::Serve(Refusal::Unencodable {
      what: "a push's reply",
    }))
  };
  let Ok(length) = reply.encoded_len() else {
    return unencodable();
  };
  if let Err(past) = admission.charge(length) {
    return error(Refused::from(past));
  }
  let mut body = Vec::new();
  match reply.encode(&mut body) {
    Ok(()) => Answered {
      media_type: MEDIA_TYPE_V1,
      body,
      refused,
    },
    Err(_) => unencodable(),
  }
}

/// 7.2's `pusherr` in `0.1` for `words`, charged first.
fn push_error(words: &[u8], admission: &mut Admission) -> Answered {
  if let Err(past) = admission.charge(to_u64(words.len()).saturating_add(PUSH_ERROR_FRAMING)) {
    return error(Refused::from(past));
  }
  let mut body = Vec::new();
  http::encode_push_error(words, &mut body);
  Answered {
    media_type: MEDIA_TYPE_V1,
    body,
    refused: Some(Refused::Push),
  }
}

/// The error answer for `refused`: its words, ended by a line end.
fn error(refused: Refused) -> Answered {
  Answered {
    media_type: MEDIA_TYPE_ERROR,
    body: ended(session::message(&refused)),
    refused: Some(refused),
  }
}

/// The answer to a request naming no command 7.2's table holds: 7.2's words, as 7.2 writes them.
fn not_available() -> Answered {
  let refused = Refused::NotAvailable;
  Answered {
    media_type: MEDIA_TYPE_ERROR,
    body: session::message(&refused),
    refused: Some(refused),
  }
}

/// `words` ended by a line end, unless they end with one.
fn ended(mut words: Vec<u8>) -> Vec<u8> {
  if words.last() != Some(&LINE_END) {
    words.push(LINE_END);
  }
  words
}

/// A stream's media type and body (the module doc): `0.2` in the chosen engine to a client that reads it and names one
/// this crate writes (uncompressed where 7.2 prefers that), else `0.1` in zlib.
fn streamed(
  stream: &[u8],
  prefer_uncompressed: bool,
  request: &Request<'_>,
  compressor: &mut impl Compressor,
  admission: &mut Admission,
) -> Result<(&'static [u8], Vec<u8>), Refused> {
  let caps = http::protocol_caps(request, admission)?;
  let length = to_u64(stream.len());
  if caps.has(READS_V2) {
    if prefer_uncompressed {
      let plain = Compression {
        engine: Engine::Uncompressed,
        level: 0,
      };
      return framed(stream, Media::V2, plain, admission);
    }
    let engines = readable(&caps, admission)?;
    if !engines.is_empty() {
      let choice = compressor.choose(Media::V2, &engines, length);
      return chosen(stream, Media::V2, choice, &engines, admission);
    }
  }
  let zlib = [Engine::Zlib];
  let choice = compressor.choose(Media::V1, &zlib, length);
  chosen(stream, Media::V1, choice, &zlib, admission)
}

/// The engines a `0.2` client reads that this crate writes, in its order (`clientcompressionsupport`), charged.
fn readable(caps: &ProtocolCaps, admission: &mut Admission) -> Result<Vec<Engine>, PastAdmission> {
  admission.charge_items::<Engine>(caps.engines().count())?;
  Ok(caps.engines().filter_map(Engine::for_wire_name).collect())
}

/// The stream framed in `choice`, refused where its engine is not one of `engines`.
fn chosen(
  stream: &[u8],
  media: Media,
  choice: Compression,
  engines: &[Engine],
  admission: &mut Admission,
) -> Result<(&'static [u8], Vec<u8>), Refused> {
  if !engines.contains(&choice.engine) {
    return Err(Refused::EngineNotRead {
      engine: choice.engine,
    });
  }
  framed(stream, media, choice, admission)
}

/// The stream compressed in `choice` behind `media`'s header, its bound charged first.
fn framed(
  stream: &[u8],
  media: Media,
  choice: Compression,
  admission: &mut Admission,
) -> Result<(&'static [u8], Vec<u8>), Refused> {
  let name = choice.engine.wire_name();
  let header = match media {
    Media::V1 => 0,
    Media::V2 => V2_LENGTH_BYTE.saturating_add(to_u64(name.len())),
  };
  admission
    .charge(header.saturating_add(compression::bound(choice.engine, to_u64(stream.len()))))?;
  let mut body = Vec::new();
  let media_type = match media {
    Media::V1 => MEDIA_TYPE_V1,
    Media::V2 => {
      http::encode_v2_header(name, &mut body).map_err(|_| Refused::EngineNotRead {
        engine: choice.engine,
      })?;
      MEDIA_TYPE_V2
    }
  };
  compression::compress(choice.engine, choice.level, stream, &mut body)
    .map_err(Refused::Compression)?;
  Ok((media_type, body))
}

#[cfg(test)]
#[path = "http_tests.rs"]
mod tests;
