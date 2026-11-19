//! A-610: the locks API, held to the requests the stock git-lfs 3.8.1 client sent, the answers it accepted, git-lfs
//! v3.8.1's locking.md, and hostile bodies and queries.

use serde_json::Value;

use super::{
  Field, ListQuery, Lock, QueryRefusal, Refusal, read_create, read_list_query, read_unlock,
  read_verify, write_conflict, write_list, write_lock, write_verify,
};
use crate::Admission;
use crate::lfs::json::{self, JsonStr, NumberRefusal};
use crate::lfs::tests::{CONFLICT, LFS_PREFIX, LOCKING_MD, exchanges, json_examples};
use crate::lfs::{ErrorBody, Timestamp};

/// Room enough for any body these tests read.
const ROOM: u64 = 1 << 16;

/// A string's text.
fn text(string: Option<JsonStr<'_>>) -> Option<String> {
  string.map(|value| value.chars().collect())
}

/// The lock time every recorded lock has, 2026-11-19T00:00:00Z (Python's `calendar.timegm`).
const LOCKED_AT: i64 = 1_795_046_400;

/// The locks a recorded answer gives under `key`, borrowing their strings from `body`.
fn locks_of<'a>(body: &'a Value, key: &str) -> Vec<Lock<'a>> {
  let list: Vec<&'a Value> = match &body[key] {
    Value::Array(locks) => locks.iter().collect(),
    lock @ Value::Object(_) => vec![lock],
    _ => Vec::new(),
  };
  list
    .into_iter()
    .map(|lock| Lock {
      id: lock["id"].as_str().unwrap(),
      path: lock["path"].as_str().unwrap(),
      locked_at: Timestamp::from_unix(LOCKED_AT).unwrap(),
      owner: lock["owner"]["name"].as_str(),
    })
    .collect()
}

/// A-610, golden: do read every request to create a lock the stock client sent, the conflicting one too. Expect the
/// path it named and the ref it pushes, and nothing charged past the nesting's one word.
#[test]
fn every_create_request_the_stock_client_sent_is_read() {
  let mut paths = Vec::new();
  for exchange in exchanges() {
    if exchange.method != "POST" || exchange.path != format!("{LFS_PREFIX}/locks") {
      continue;
    }
    let mut admission = Admission::new(ROOM);
    let request = read_create(exchange.request.as_bytes(), &mut admission).unwrap();
    assert_eq!(text(request.ref_name).as_deref(), Some("refs/heads/main"));
    assert_eq!(ROOM - admission.left(), 8);
    paths.push(text(Some(request.path)).unwrap());
  }
  assert_eq!(paths, ["one.bin", "two.bin", "one.bin"]);
}

/// A-610, golden: do read every verification and unlock request the stock client sent. Expect the verifications' ref
/// and, on the second page, the cursor the first answer gave; and the unlocks' ref and `force`, false then true.
#[test]
fn every_verify_and_unlock_request_the_stock_client_sent_is_read() {
  let (mut cursors, mut forces) = (Vec::new(), Vec::new());
  for exchange in exchanges() {
    let room = &mut Admission::new(ROOM);
    if exchange.path == format!("{LFS_PREFIX}/locks/verify") {
      let request = read_verify(exchange.request.as_bytes(), room).unwrap();
      assert_eq!(text(request.ref_name).as_deref(), Some("refs/heads/main"));
      assert_eq!(request.limit, None);
      cursors.push(text(request.cursor));
    } else if exchange.path.ends_with("/unlock") {
      let request = read_unlock(exchange.request.as_bytes(), room).unwrap();
      assert_eq!(text(request.ref_name).as_deref(), Some("refs/heads/main"));
      forces.push(request.force);
    }
  }
  assert_eq!(cursors, [None, None, Some("lock-2".to_string()), None]);
  assert_eq!(forces, [false, true]);
}

/// A query value's text.
fn query_text(value: Option<super::QueryStr<'_>>) -> Option<String> {
  value.map(|value| String::from_utf8(value.bytes().collect()).unwrap())
}

/// A-610, golden: do read every lock listing's query the stock client sent. Expect the refspec decoded from
/// `refs%2Fheads%2Fmain` in each, the cursor the first page gave on the second, and the path where it filtered by one.
#[test]
fn every_list_query_the_stock_client_sent_is_read() {
  let mut read = Vec::new();
  for exchange in exchanges() {
    if exchange.method != "GET" || exchange.path != format!("{LFS_PREFIX}/locks") {
      continue;
    }
    let query = read_list_query(exchange.query.as_bytes()).unwrap();
    assert_eq!(
      query_text(query.refspec).as_deref(),
      Some("refs/heads/main")
    );
    assert_eq!((query.id, query.limit), (None, None));
    read.push((query_text(query.path), query_text(query.cursor)));
  }
  let one = Some("one.bin".to_string());
  let cursor = Some("lock-2".to_string());
  assert_eq!(
    read,
    [
      (None, None),
      (None, cursor),
      (one.clone(), None),
      (one, None),
      (None, None)
    ]
  );
}

/// A-610, golden: do write, from their fields, every locks answer the stock client accepted: a lock created and
/// released, a conflict, a list's pages with and without a cursor, a verification's pages. Expect each, read back as
/// JSON, equal to the answer the client read; and the client's words for the conflict carry its message.
#[test]
fn every_locks_answer_the_stock_client_accepted_is_written() {
  let mut written = 0;
  for exchange in exchanges() {
    let Some(rest) = exchange.path.strip_prefix(LFS_PREFIX) else {
      continue;
    };
    if !rest.starts_with("/locks") {
      continue;
    }
    let accepted: Value = serde_json::from_str(&exchange.response).unwrap();
    let next_cursor = accepted["next_cursor"].as_str();
    let mut out = Vec::new();
    match (
      exchange.status,
      accepted.get("ours").is_some(),
      accepted.get("locks").is_some(),
    ) {
      (409, _, _) => {
        let error = ErrorBody {
          message: "already created lock",
          request_id: Some("slates-test"),
          documentation_url: None,
        };
        write_conflict(&locks_of(&accepted, "lock")[0], &error, &mut out);
      }
      (_, true, _) => write_verify(
        &locks_of(&accepted, "ours"),
        &locks_of(&accepted, "theirs"),
        next_cursor,
        &mut out,
      ),
      (_, _, true) => write_list(&locks_of(&accepted, "locks"), next_cursor, &mut out),
      _ => write_lock(&locks_of(&accepted, "lock")[0], &mut out),
    }
    assert_eq!(
      serde_json::from_slice::<Value>(&out).unwrap(),
      accepted,
      "{}",
      exchange.response
    );
    written += 1;
  }
  assert_eq!(written, 14);
  assert!(CONFLICT.contains("already created lock"));
}

/// locking.md's examples, in the document's order.
fn locking_examples() -> Vec<Value> {
  json_examples(LOCKING_MD)
    .iter()
    .map(|text| serde_json::from_str(text).unwrap())
    .collect()
}

/// A-610, golden: do read locking.md's three request bodies: a lock's creation, a verification and an unlock. Expect
/// the path, ref, cursor, limit and `force` each gives.
#[test]
fn the_documents_requests_are_read() {
  let examples = json_examples(LOCKING_MD);
  let room = || Admission::new(ROOM);
  let create = read_create(examples[0].as_bytes(), &mut room()).unwrap();
  assert_eq!(
    (text(Some(create.path)), text(create.ref_name)),
    (
      Some("foo/bar.zip".into()),
      Some("refs/heads/my-feature".into())
    )
  );
  let verify = examples
    .iter()
    .find(|text| text.contains("\"cursor\""))
    .unwrap();
  let verify = read_verify(verify.as_bytes(), &mut room()).unwrap();
  assert_eq!(
    (text(verify.cursor), verify.limit),
    (Some("optional cursor".into()), Some(100))
  );
  let unlock = examples
    .iter()
    .find(|text| text.contains("\"force\""))
    .unwrap();
  let unlock = read_unlock(unlock.as_bytes(), &mut room()).unwrap();
  assert_eq!(
    (unlock.force, text(unlock.ref_name)),
    (true, Some("refs/heads/my-feature".into()))
  );
}

/// `value` with each `locked_at` written with `+00:00` written with `Z` instead, as this module writes the same
/// instant.
fn with_utc_z(value: Value) -> Value {
  serde_json::from_str(&value.to_string().replace("+00:00", "Z")).unwrap()
}

/// A-610, golden: do write locking.md's answers with locks in them: a lock created, a list page, a verification page
/// and a lock released. Expect each, read back as JSON, equal to the document's, its `+00:00` read as `Z`.
#[test]
fn the_documents_answers_are_written() {
  let at = Timestamp::from_unix(1_463_500_146).unwrap();
  let lock = |path| Lock {
    id: "some-uuid",
    path,
    locked_at: at,
    owner: Some("Jane Doe"),
  };
  let created = lock("foo/bar.zip");
  let listed = [lock("/path/to/file")];
  let mut checked = 0;
  for example in locking_examples() {
    let mut out = Vec::new();
    if example.get("ours").is_some() {
      write_verify(&listed, &[], Some("optional next ID"), &mut out);
    } else if example.get("locks").is_some() {
      write_list(&listed, Some("optional next ID"), &mut out);
    } else if example["lock"]["path"] == "foo/bar.zip" {
      write_lock(&created, &mut out);
    } else if example["lock"]["path"] == "/path/to/file" {
      write_lock(&listed[0], &mut out);
    } else {
      continue;
    }
    assert_eq!(
      serde_json::from_slice::<Value>(&out).unwrap(),
      with_utc_z(example)
    );
    checked += 1;
  }
  assert_eq!(checked, 4);
}

/// A-610, hostile input: do read requests to create a lock each wrong one way: no path, a path that is a number or
/// given twice, a ref that is an array, a body that is no object. Expect each refused by its kind at its byte.
#[test]
fn every_malformed_locks_request_is_refused_by_kind() {
  let room = || Admission::new(ROOM);
  assert_eq!(
    read_create(b"{}", &mut room()),
    Err(Refusal::Missing {
      field: Field::Path,
      at: 0
    })
  );
  assert_eq!(
    read_create(b"{\"path\":1}", &mut room()),
    Err(Refusal::Type {
      field: Field::Path,
      at: 8
    })
  );
  assert_eq!(
    read_create(b"{\"path\":\"a\",\"path\":\"b\"}", &mut room()),
    Err(Refusal::Repeated {
      field: Field::Path,
      at: 19
    })
  );
  assert_eq!(
    read_create(b"{\"path\":\"a\",\"ref\":[]}", &mut room()),
    Err(Refusal::Type {
      field: Field::Ref,
      at: 18
    })
  );
  assert_eq!(
    read_create(b"[]", &mut room()),
    Err(Refusal::Type {
      field: Field::Body,
      at: 0
    })
  );
}

/// A-610, hostile input: do read verification and unlock bodies each wrong one way: limits negative, fractional and one
/// past Go's `int64`, a cursor that is a number, a `force` that is a string or given twice, and a body cut short.
/// Expect each refused by its kind at its byte.
#[test]
fn every_malformed_verify_or_unlock_request_is_refused_by_kind() {
  let room = || Admission::new(ROOM);
  assert_eq!(
    read_verify(b"{\"limit\":-1}", &mut room()),
    Err(Refusal::NegativeLimit { at: 9 })
  );
  assert_eq!(
    read_verify(b"{\"limit\":2.5}", &mut room()),
    Err(Refusal::Limit {
      at: 9,
      kind: NumberRefusal::NotInteger
    })
  );
  assert_eq!(
    read_verify(b"{\"limit\":9223372036854775808}", &mut room()),
    Err(Refusal::Limit {
      at: 9,
      kind: NumberRefusal::TooLarge
    })
  );
  assert_eq!(
    read_verify(b"{\"cursor\":7}", &mut room()),
    Err(Refusal::Type {
      field: Field::Cursor,
      at: 10
    })
  );
  assert_eq!(
    read_unlock(b"{\"force\":\"yes\"}", &mut room()),
    Err(Refusal::Type {
      field: Field::Force,
      at: 9
    })
  );
  assert_eq!(
    read_unlock(b"{\"force\":true,\"force\":false}", &mut room()),
    Err(Refusal::Repeated {
      field: Field::Force,
      at: 22
    })
  );
  assert_eq!(
    read_unlock(b"{\"force\":tru", &mut room()),
    Err(Refusal::Json(json::Refusal::Truncated { at: 12 }))
  );
}

/// A-610: do read lock request bodies the client may send in other forms: `null` for a cursor, a limit, `force` and a
/// ref, and unknown fields. Expect each read as absent, `force` false.
#[test]
fn nulls_and_unknown_fields_are_read_as_absent() {
  let room = || Admission::new(ROOM);
  let verify = read_verify(
    b"{\"cursor\":null,\"limit\":null,\"ref\":null,\"x\":[{}]}",
    &mut room(),
  )
  .unwrap();
  assert_eq!(
    (verify.cursor, verify.limit, verify.ref_name),
    (None, None, None)
  );
  let unlock = read_unlock(b"{\"force\":null,\"y\":1}", &mut room()).unwrap();
  assert_eq!((unlock.force, unlock.ref_name), (false, None));
  let empty = read_verify(b"{}", &mut room()).unwrap();
  assert_eq!((empty.cursor, empty.limit), (None, None));
}

/// A-610, hostile input: do read list queries each wrong one way: an escape cut short, one with a non-hexadecimal
/// digit, bytes that are not UTF-8, a key given twice, and limits that are not decimal integers within Go's `int64`.
/// Expect each refused by its kind at its byte; and expect a query with a space written `+`, an empty pair, a key
/// without a value and an unknown key read as Go reads it.
#[test]
fn list_queries_are_read_as_go_writes_them_and_refused_by_kind() {
  let cases: [(&[u8], QueryRefusal); 8] = [
    (b"path=a%2", QueryRefusal::Escape { at: 6 }),
    (b"path=a%zz", QueryRefusal::Escape { at: 6 }),
    (b"path=%ff", QueryRefusal::NotUtf8 { at: 5 }),
    (b"path=%c3", QueryRefusal::NotUtf8 { at: 5 }),
    (b"id=1&id=2", QueryRefusal::Repeated { at: 5 }),
    (b"limit=ten", QueryRefusal::Limit { at: 6 }),
    (b"limit=-1", QueryRefusal::Limit { at: 6 }),
    (b"limit=9223372036854775808", QueryRefusal::Limit { at: 6 }),
  ];
  for (query, refusal) in cases {
    assert_eq!(
      read_list_query(query),
      Err(refusal),
      "{}",
      String::from_utf8_lossy(query)
    );
  }
  let query = read_list_query(b"path=dir%2Fa+b%C3%A9&&cursor&limit=25&other=1").unwrap();
  assert_eq!(query_text(query.path).as_deref(), Some("dir/a b\u{e9}"));
  assert_eq!(
    (query_text(query.cursor).as_deref(), query.limit),
    (Some(""), Some(25))
  );
  assert_eq!(read_list_query(b""), Ok(ListQuery::default()));
}

/// A-610: do take a query value as owned text within its exact decoded length and one byte short of it. Expect the
/// first made with its room spent and the second refused with its room untouched.
#[test]
fn a_query_value_is_owned_within_its_decoded_length() {
  let query = read_list_query(b"path=a%2Fb").unwrap();
  let path = query.path.unwrap();
  let mut exact = Admission::new(3);
  assert_eq!(path.owned(&mut exact).unwrap(), "a/b");
  assert_eq!(exact.left(), 0);
  let mut short = Admission::new(2);
  assert!(path.owned(&mut short).is_err());
  assert_eq!(short.left(), 2);
  assert!(path.is("a/b"));
}
