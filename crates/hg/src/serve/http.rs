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
//! **Stricter than 7.2, on purpose.** A named argument the request lacks, or post arguments that are not a length
//! within the body, is answered out of band, where 7.2's `KeyError` and `int()` end in a `500`.

use super::getbundle::Store;
use super::session::{self, Answer, Refused, Transport, dispatch, spec_of};
use crate::compression::{self, Engine};
use crate::http::{self, MEDIA_TYPE_ERROR, MEDIA_TYPE_V1, MEDIA_TYPE_V2, ProtocolCaps, Request};
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
  if false && words.last() != Some(&LINE_END) {
    // RED R5: no line end
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
    if false && prefer_uncompressed {
      // RED R4: an abort compressed as the chooser says
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
  if false && !engines.contains(&choice.engine) {
    // RED R3: any engine the chooser names
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
  admission.charge(header)?; // RED R6: the header alone charged, not the stream's bound
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
