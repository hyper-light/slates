//! A push's reply, written as 7.2's server writes it (A-641; §4.17.3 "Push" step 7 and "Refusals"), a bundle2 stream
//! with no stream parameters whose parts are numbered from 0 in the order they are added (`bundle20.addpart`).
//!
//! **Accepted** ([`Reply::accepted`]): the reply parts of the parts a push's handlers ran, in their order, written only
//! when the client asked for a reply (`replycaps`; otherwise the stream holds no part): `reply:changegroup` (advisory,
//! its `in-reply-to` and `return` advisory; `return` the change in heads 7.2's `changegroup.apply` counts,
//! [`heads_changed`]), `reply:pushkey` (mandatory, its two parameters advisory), `reply:obsmarkers` (mandatory, its
//! `in-reply-to` and `new` advisory), and `output` (advisory) where a hook's words were captured, after the reply of the
//! part whose handler ran the hook, or last for the transaction's own hooks.
//!
//! **Refused** ([`Reply::refused`]): a new stream holding the `output` parts captured before the refusal, then one
//! error part (`wireprotov1server.unbundle`): `error:abort` (its `message` mandatory, its `hint` advisory),
//! `error:pushraced` (its `message` mandatory), `error:pushkey` (its `in-reply-to` mandatory; `namespace`, `key`,
//! `new`, `old` and `ret` advisory, each only when given) and `error:unsupportedcontent` (its `parttype` and `params`
//! mandatory, each only when given, the parameters joined by NUL). Every error part is mandatory.
//!
//! **Stricter than 7.2.** A parameter's value is at most 255 bytes, its length one byte (`internals.bundle2`); 7.2's
//! writer fails on a longer one, which a race's words naming a long bookmark would be, and its server's answer with it.
//! Here such a value is cut to the 255 bytes a parameter holds.

use crate::bundle2::{Bundle2, EncodeRefusal, Event, Param, Part};
use crate::decimal::Decimal;

/// Format: a part parameter's longest value, its length one byte (`internals.bundle2`).
const PARAM_VALUE_MAX: usize = 255;

/// Format: the part types 7.2's server writes in a push's reply, their case its mandatory bit (`newpart(...,
/// mandatory=False)` lower case, the default upper case).
const REPLY_CHANGEGROUP: &[u8] = b"reply:changegroup";
/// Format: see [`REPLY_CHANGEGROUP`].
const REPLY_PUSHKEY: &[u8] = b"REPLY:PUSHKEY";
/// Format: see [`REPLY_CHANGEGROUP`].
const REPLY_OBSMARKERS: &[u8] = b"REPLY:OBSMARKERS";
/// Format: see [`REPLY_CHANGEGROUP`].
const OUTPUT: &[u8] = b"output";
/// Format: see [`REPLY_CHANGEGROUP`].
const ERROR_ABORT: &[u8] = b"ERROR:ABORT";
/// Format: see [`REPLY_CHANGEGROUP`].
const ERROR_PUSHRACED: &[u8] = b"ERROR:PUSHRACED";
/// Format: see [`REPLY_CHANGEGROUP`].
const ERROR_PUSHKEY: &[u8] = b"ERROR:PUSHKEY";
/// Format: see [`REPLY_CHANGEGROUP`].
const ERROR_UNSUPPORTED: &[u8] = b"ERROR:UNSUPPORTEDCONTENT";

/// Format: the separator of `error:unsupportedcontent`'s `params` (`b'\0'.join(exc.params)`).
const PARAMS_SEPARATOR: u8 = 0;

/// Format: the words of 7.2's `hooklib.reject_new_heads` before the branch's name and after it
/// (`b'Changes on branch %r resulted in multiple heads'`).
const MULTIPLE_HEADS_BEFORE: &[u8] = b"Changes on branch ";
/// Format: see [`MULTIPLE_HEADS_BEFORE`].
const MULTIPLE_HEADS_AFTER: &[u8] = b" resulted in multiple heads";

/// Format: the parameter names 7.2's server writes in a push's reply.
const IN_REPLY_TO: &[u8] = b"in-reply-to";
/// Format: see [`IN_REPLY_TO`].
const HINT: &[u8] = b"hint";
/// Format: see [`IN_REPLY_TO`].
const PARTTYPE: &[u8] = b"parttype";

/// Format: the lower-case hexadecimal digits CPython's bytes repr writes (`Py_hexdigits`), and the bits of one digit.
const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";
/// Format: see [`HEX_DIGITS`].
const NIBBLE: u32 = 4;
/// Format: see [`HEX_DIGITS`].
const LOW_NIBBLE: u8 = 0x0f;

/// Format: the first and last bytes CPython's bytes repr writes as themselves (`' '` to `'~'`).
const PRINTABLE: core::ops::RangeInclusive<u8> = b' '..=b'~';

/// One part of an accepted push's reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Replied {
  /// `reply:changegroup`: the changegroup part's id, and the change in heads ([`heads_changed`]).
  Changegroup {
    /// The changegroup part's id.
    in_reply_to: u32,
    /// The `return`.
    heads: i64,
  },
  /// `reply:pushkey`: the pushkey part's id, and whether the key moved (`return` 1 or 0).
  Pushkey {
    /// The pushkey part's id.
    in_reply_to: u32,
    /// Whether it moved.
    moved: bool,
  },
  /// `reply:obsmarkers`: the obsmarkers part's id, and how many of its markers were new.
  Obsmarkers {
    /// The obsmarkers part's id.
    in_reply_to: u32,
    /// The new markers.
    new: u64,
  },
  /// `output`: words a hook printed, captured; after the part whose handler ran it, or none for the transaction's.
  Output {
    /// The part whose handler ran the hook.
    in_reply_to: Option<u32>,
    /// The words.
    words: Vec<u8>,
  },
}

/// Why a push was refused, as the error part 7.2's server answers it with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused {
  /// `error:abort`: the message and the hint.
  Abort {
    /// The message.
    message: Vec<u8>,
    /// The hint.
    hint: Option<Vec<u8>>,
  },
  /// `error:pushraced`: the race's words.
  PushRaced {
    /// The words.
    message: Vec<u8>,
  },
  /// `error:pushkey`: a mandatory pushkey that did not move, its part's id and the parameters it was sent with.
  Pushkey {
    /// The part's id.
    in_reply_to: u32,
    /// Its `namespace`.
    namespace: Option<Vec<u8>>,
    /// Its `key`.
    key: Option<Vec<u8>>,
    /// Its `new`.
    new: Option<Vec<u8>>,
    /// Its `old`.
    old: Option<Vec<u8>>,
    /// Its `ret`.
    ret: Option<Vec<u8>>,
  },
  /// `error:unsupportedcontent`: the part's type and the parameters 7.2 does not know.
  Unsupported {
    /// The part's type, in lower case.
    parttype: Option<Vec<u8>>,
    /// The parameters.
    params: Vec<Vec<u8>>,
  },
}

/// One part of a reply as it is written: its type, its parameters (the mandatory first) and its payload.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Written {
  /// The type, its case its mandatory bit.
  kind: &'static [u8],
  /// The parameters, in order.
  params: Vec<(&'static [u8], Vec<u8>)>,
  /// How many of `params` are mandatory.
  mandatory: usize,
  /// The payload, written as one chunk where it is not empty (`bundlepart._payloadchunks` for bytes).
  payload: Vec<u8>,
}

/// A push's reply, ready to be written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reply {
  /// Its parts, in order.
  parts: Vec<Written>,
}

impl Reply {
  /// The reply to an accepted push: `replied`'s parts in order where the client asked for a reply, none otherwise.
  pub fn accepted(replied: &[Replied], wanted: bool) -> Reply {
    let parts = if wanted {
      replied.iter().map(written).collect()
    } else {
      Vec::new()
    };
    Reply { parts }
  }

  /// The reply to a refused push: the `output` among `salvaged`, in order, then `refused`'s error part.
  pub fn refused(salvaged: &[Replied], refused: &Refused) -> Reply {
    let mut parts: Vec<Written> = salvaged
      .iter()
      .filter(|part| matches!(part, Replied::Output { .. }))
      .map(written)
      .collect();
    parts.push(error(refused));
    Reply { parts }
  }

  /// The stream the reply is, its parts borrowing it.
  fn bundle(&self) -> Bundle2<'_> {
    let mut parts = Vec::with_capacity(self.parts.len());
    let mut events = Vec::new();
    for (index, written) in self.parts.iter().enumerate() {
      parts.push(Part {
        kind: written.kind,
        id: u32::try_from(index).unwrap_or(u32::MAX),
        params: written
          .params
          .iter()
          .map(|(key, value)| Param {
            key,
            value: value.get(..PARAM_VALUE_MAX).unwrap_or(value),
          })
          .collect(),
        mandatory_params: written.mandatory,
        depth: 0,
      });
      events.push(Event::Open(index));
      if !written.payload.is_empty() {
        events.push(Event::Chunk(&written.payload));
      }
      events.push(Event::Close);
    }
    Bundle2 {
      params: Vec::new(),
      parts,
      events,
    }
  }

  /// The written reply's length, for a caller that charges it first.
  pub fn encoded_len(&self) -> Result<u64, EncodeRefusal> {
    self.bundle().encoded_len()
  }

  /// Appends the reply to `out`.
  pub fn encode(&self, out: &mut Vec<u8>) -> Result<(), EncodeRefusal> {
    self.bundle().encode(out)
  }
}

/// The text of a number, as 7.2's `'%i'` writes it.
fn number(value: i64) -> Vec<u8> {
  Decimal::signed(value).as_bytes().to_vec()
}

/// The text of a part id, as 7.2's `bytestr(id)` writes it.
fn id_text(id: u32) -> Vec<u8> {
  Decimal::unsigned(u64::from(id)).as_bytes().to_vec()
}

/// An accepted push's reply part.
fn written(replied: &Replied) -> Written {
  match replied {
    Replied::Changegroup { in_reply_to, heads } => advisory_only(
      REPLY_CHANGEGROUP,
      vec![
        (IN_REPLY_TO, id_text(*in_reply_to)),
        (b"return", number(*heads)),
      ],
    ),
    Replied::Pushkey { in_reply_to, moved } => advisory_only(
      REPLY_PUSHKEY,
      vec![
        (IN_REPLY_TO, id_text(*in_reply_to)),
        (b"return", number(i64::from(*moved))),
      ],
    ),
    Replied::Obsmarkers { in_reply_to, new } => advisory_only(
      REPLY_OBSMARKERS,
      vec![
        (IN_REPLY_TO, id_text(*in_reply_to)),
        (b"new", Decimal::unsigned(*new).as_bytes().to_vec()),
      ],
    ),
    Replied::Output { in_reply_to, words } => Written {
      kind: OUTPUT,
      params: in_reply_to
        .iter()
        .map(|id| (IN_REPLY_TO, id_text(*id)))
        .collect(),
      mandatory: 0,
      payload: words.clone(),
    },
  }
}

/// A part of `kind` with only advisory parameters and no payload.
fn advisory_only(kind: &'static [u8], params: Vec<(&'static [u8], Vec<u8>)>) -> Written {
  Written {
    kind,
    params,
    mandatory: 0,
    payload: Vec::new(),
  }
}

/// A refused push's error part.
fn error(refused: &Refused) -> Written {
  let mut params: Vec<(&'static [u8], Vec<u8>)> = Vec::new();
  let (kind, mandatory) = match refused {
    Refused::Abort { message, hint } => {
      params.push((b"message", message.clone()));
      params.extend(hint.iter().map(|hint| (HINT, hint.clone())));
      (ERROR_ABORT, 1)
    }
    Refused::PushRaced { message } => {
      params.push((b"message", message.clone()));
      (ERROR_PUSHRACED, 1)
    }
    Refused::Pushkey {
      in_reply_to,
      namespace,
      key,
      new,
      old,
      ret,
    } => {
      params.push((IN_REPLY_TO, id_text(*in_reply_to)));
      let given: [(&'static [u8], &Option<Vec<u8>>); 5] = [
        (b"namespace", namespace),
        (b"key", key),
        (b"new", new),
        (b"old", old),
        (b"ret", ret),
      ];
      for (name, value) in given {
        params.extend(value.iter().map(|value| (name, value.clone())));
      }
      (ERROR_PUSHKEY, 1)
    }
    Refused::Unsupported { parttype, params: unknown } => {
      params.extend(parttype.iter().map(|kind| (PARTTYPE, kind.clone())));
      if !unknown.is_empty() {
        params.push((b"params", unknown.join(&PARAMS_SEPARATOR)));
      }
      (ERROR_UNSUPPORTED, params.len())
    }
  };
  Written {
    kind,
    params,
    mandatory,
    payload: Vec::new(),
  }
}

/// The `return` of `reply:changegroup` (7.2's `changegroup.apply`): the heads the push added less those it removed and
/// less the added ones that close their branch, plus one, or minus one where that is negative, so it is never 0.
pub fn heads_changed(removed: u64, added: u64, added_closing: u64) -> i64 {
  let signed = |count: u64| i64::try_from(count).unwrap_or(i64::MAX);
  let delta = signed(added)
    .saturating_sub(signed(removed))
    .saturating_sub(signed(added_closing));
  if delta < 0 {
    delta.saturating_sub(1)
  } else {
    delta.saturating_add(1)
  }
}

/// The words of 7.2's `hooklib.reject_new_heads` for `branch` (`b'Changes on branch %r resulted in multiple heads'`):
/// the branch as Python writes a bytes repr without its `b` (`pycompat.bytestr.__repr__`; CPython's `PyBytes_Repr`).
/// Its message for a push that would leave a branch more than one open head (§4.17.3 decision 1).
pub fn multiple_heads(branch: &[u8]) -> Vec<u8> {
  let mut words = MULTIPLE_HEADS_BEFORE.to_vec();
  python_repr(branch, &mut words);
  words.extend_from_slice(MULTIPLE_HEADS_AFTER);
  words
}

/// Appends CPython's repr of the bytes `value` without its `b`: in single quotes, or in double quotes where it holds a
/// single quote and no double one; the quote and the backslash escaped, tab, newline and return named, and every other
/// byte outside space to `~` as `\x` and two lower-case hexadecimal digits.
fn python_repr(value: &[u8], out: &mut Vec<u8>) {
  let quote = if value.contains(&b'\'') && !value.contains(&b'"') {
    b'"'
  } else {
    b'\''
  };
  out.push(quote);
  for &byte in value {
    match byte {
      b'\\' => out.extend_from_slice(b"\\\\"),
      b'\t' => out.extend_from_slice(b"\\t"),
      b'\n' => out.extend_from_slice(b"\\n"),
      b'\r' => out.extend_from_slice(b"\\r"),
      _ if byte == quote => out.extend_from_slice(&[b'\\', quote]),
      _ if PRINTABLE.contains(&byte) => out.push(byte),
      _ => {
        out.extend_from_slice(b"\\x");
        let high = byte.checked_shr(NIBBLE).unwrap_or_default();
        out.extend(HEX_DIGITS.get(usize::from(high)));
        out.extend(HEX_DIGITS.get(usize::from(byte & LOW_NIBBLE)));
      }
    }
  }
  out.push(quote);
}
