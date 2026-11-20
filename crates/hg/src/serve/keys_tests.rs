//! A-758: the `pushkey` command's push and answer, read back through the push's own reader and reply.

use super::*;
use crate::push::read_bundle;
use crate::push::reply::{Refused as ReplyRefused, Replied, Reply};

/// Shape: room enough for every request these tests make.
const ROOM: u64 = 1 << 16;

/// A bookmark's request, `old` and `new` as given.
fn bookmark<'a>(old: &'a [u8], new: &'a [u8]) -> Request<'a> {
  Request {
    namespace: b"bookmarks",
    key: b"moved-bm",
    old,
    new,
  }
}

/// A-758 (one write path): do write a bookmark's request as its push and read it back with the push's own reader. Expect
/// a reply asked for, and one advisory pushkey part carrying the request as sent.
#[test]
fn a_pushkey_is_written_as_a_push_the_push_reader_takes() {
  let asked = bookmark(b"", b"0123456789abcdef0123456789abcdef01234567");
  let written = bundle(&asked, &mut Admission::new(ROOM)).unwrap();
  let read = read_bundle(&written, &mut Admission::new(ROOM)).unwrap();
  let pushkeys: Vec<_> = read
    .parts
    .iter()
    .filter_map(|taken| match &taken.asks {
      crate::push::Asks::Pushkey(ask) => Some((
        taken.id,
        taken.mandatory,
        ask.namespace.clone(),
        ask.key.clone(),
      )),
      _ => None,
    })
    .collect();
  assert_eq!(
    (read.reply_caps().is_some(), pushkeys),
    (
      true,
      vec![(
        PUSHKEY_ID,
        false,
        b"bookmarks".to_vec(),
        b"moved-bm".to_vec()
      )]
    )
  );
}

/// A-758 (the module doc's namespaces; 7.2's `pushkey.py` `_get`): do write requests in `obsolete` and in a namespace
/// nothing serves. Expect each refused before anything is written, and answered `0` with words naming the namespace.
#[test]
fn a_namespace_no_key_is_moved_in_is_refused_by_name() {
  let refusals = [&b"obsolete"[..], b"nonesuch"].map(|namespace| {
    let asked = Request {
      namespace,
      ..bookmark(b"", b"")
    };
    let refusal = bundle(&asked, &mut Admission::new(ROOM)).unwrap_err();
    let answer = refused(refusal, &asked, &mut Admission::new(ROOM)).unwrap();
    (refusal, String::from_utf8(answer).unwrap())
  });
  assert_eq!(
    refusals,
    [
      (
        Refusal::UnknownNamespace,
        "0\npushkey: no key of namespace 'obsolete' is moved by this server\n".to_owned()
      ),
      (
        Refusal::UnknownNamespace,
        "0\npushkey: no key of namespace 'nonesuch' is moved by this server\n".to_owned()
      ),
    ]
  );
}

/// A-758 (`wireprotov1server.pushkey`'s `b'%d\n%s'`): do answer a reply whose key moved, one whose key did not, a
/// refused push and one refused before its bundle was read. Expect `1`, `0`, `0` with the abort's words, and `0` with
/// the refusal's words, each line ended.
#[test]
fn a_pushkey_is_answered_one_where_its_key_moved_and_zero_with_words_where_not() {
  let replied = |moved| {
    Outcome::Accepted(Reply::accepted(
      &[Replied::Pushkey {
        in_reply_to: PUSHKEY_ID,
        moved,
      }],
      true,
    ))
  };
  let abort = ReplyRefused::Abort {
    message: b"push refused: bookmark moved-bm is not yours".to_vec(),
    hint: None,
  };
  let answers = [
    replied(true),
    replied(false),
    Outcome::refused(&abort),
    Outcome::Unprocessed(b"remote repository changed while pushing".to_vec()),
  ]
  .map(|outcome| String::from_utf8(answer(&outcome, &mut Admission::new(ROOM)).unwrap()).unwrap());
  assert_eq!(
    answers,
    [
      "1\n",
      "0\n",
      "0\npush refused: bookmark moved-bm is not yours\n",
      "0\nremote repository changed while pushing\n",
    ]
  );
}
