//! A-610: the batch API and the basic adapter's actions, held to the requests the stock git-lfs 3.8.1 client sent, the
//! answers it accepted, git-lfs v3.8.1's documents, and hostile bodies.

use proptest::prelude::*;
use serde_json::Value;

use super::{
  Action, Answer, ErrorCode, ExpiresIn, Field, MAX_SIZE, Object, ObjectError, Operation, Outcome,
  Refusal, WriteRefusal, read_request, read_verify, write_response,
};
use crate::Admission;
use crate::lfs::json::{self, JsonStr, NumberRefusal};
use crate::lfs::tests::{
  BASIC_TRANSFERS_MD, BATCH_MD, EXAMPLE_OID, LFS_PREFIX, exchanges, json_examples,
};
use crate::lfs::{Oid, Timestamp};

/// Room enough for any request these tests read.
const ROOM: u64 = 1 << 20;

/// `body` read as a batch request within [`ROOM`].
fn request(body: &str) -> Result<super::Request<'_>, Refusal> {
  read_request(body.as_bytes(), &mut Admission::new(ROOM))
}

/// The strings' texts.
fn texts(strings: &[JsonStr<'_>]) -> Vec<String> {
  strings.iter().map(|text| text.chars().collect()).collect()
}

/// The objects a recorded JSON body names.
fn objects_of(body: &Value) -> Vec<Object> {
  body["objects"]
    .as_array()
    .unwrap()
    .iter()
    .map(|object| Object {
      oid: Oid::from_hex(object["oid"].as_str().unwrap().as_bytes()).unwrap(),
      size: object["size"].as_u64().unwrap(),
    })
    .collect()
}

/// What a recorded batch request says, as the decoder should read it: its operation, objects, transfer adapters and
/// ref.
fn expected_request(sent: &Value) -> (Operation, Vec<Object>, Vec<String>, Option<String>) {
  let operation = match sent["operation"].as_str().unwrap() {
    "upload" => Operation::Upload,
    _ => Operation::Download,
  };
  let transfers = sent["transfers"]
    .as_array()
    .unwrap()
    .iter()
    .map(|name| name.as_str().unwrap().to_string())
    .collect();
  let ref_name = sent["ref"]["name"].as_str().map(String::from);
  (operation, objects_of(sent), transfers, ref_name)
}

/// A-610, golden: do read every batch request the stock client sent. Expect each one's operation, objects, transfer
/// adapters and ref as the recording holds them, and its exact cost charged: the nesting's one word and its two lists.
#[test]
fn every_batch_request_the_stock_client_sent_is_read() {
  let mut read = 0;
  for exchange in exchanges() {
    if exchange.path != format!("{LFS_PREFIX}/objects/batch") {
      continue;
    }
    let sent: Value = serde_json::from_str(&exchange.request).unwrap();
    let mut admission = Admission::new(ROOM);
    let request = read_request(exchange.request.as_bytes(), &mut admission).unwrap();
    let ref_name = request
      .ref_name
      .map(|name| name.chars().collect::<String>());
    let got = (
      request.operation,
      request.objects.clone(),
      texts(&request.transfers),
      ref_name,
    );
    assert_eq!(got, expected_request(&sent));
    let lists = request.objects.len() * size_of::<Object>()
      + request.transfers.len() * size_of::<JsonStr<'_>>();
    assert_eq!(ROOM - admission.left(), 8 + lists as u64);
    read += 1;
  }
  assert_eq!(read, 4);
}

/// A-610, golden: do read every verify callback the stock client sent after an upload. Expect each the object it had
/// just uploaded.
#[test]
fn every_verify_callback_the_stock_client_sent_is_read() {
  let mut read = 0;
  for exchange in exchanges()
    .iter()
    .filter(|exchange| exchange.path == "/verify")
  {
    let sent: Value = serde_json::from_str(&exchange.request).unwrap();
    let object = read_verify(exchange.request.as_bytes(), &mut Admission::new(ROOM)).unwrap();
    assert_eq!(object.oid.to_string(), sent["oid"].as_str().unwrap());
    assert_eq!(object.size, sent["size"].as_u64().unwrap());
    read += 1;
  }
  assert_eq!(read, 2);
}

/// The answers a recorded batch answer gives, borrowing their strings from `owned`.
fn answers_of<'a>(body: &Value, owned: &'a Owned) -> Vec<Answer<'a>> {
  body["objects"]
    .as_array()
    .unwrap()
    .iter()
    .enumerate()
    .map(|(place, object)| {
      let oid = Oid::from_hex(object["oid"].as_str().unwrap().as_bytes()).unwrap();
      let action = |name: &str| {
        let recorded = &object["actions"][name];
        recorded.is_object().then(|| Action {
          href: owned.hrefs[place].get(name).unwrap(),
          header: owned.headers[place].get(name).unwrap(),
          expires_in: recorded["expires_in"]
            .as_i64()
            .map(|seconds| ExpiresIn::from_seconds(seconds).unwrap()),
          expires_at: recorded["expires_at"]
            .as_str()
            .map(|_| Timestamp::from_unix(4_070_908_800).unwrap()),
        })
      };
      let outcome = if object["error"].is_object() {
        Outcome::Error(ObjectError {
          code: ErrorCode::NotFound,
          message: "Object does not exist",
        })
      } else if let Some(download) = action("download") {
        Outcome::Download(download)
      } else if let Some(upload) = action("upload") {
        Outcome::Upload {
          upload,
          verify: action("verify"),
        }
      } else {
        Outcome::Held
      };
      Answer {
        oid,
        size: object["size"].as_u64().unwrap(),
        authenticated: object["authenticated"].as_bool().unwrap_or(false),
        outcome,
      }
    })
    .collect()
}

/// The strings an answer's actions borrow, owned per object and action.
#[derive(Default)]
struct Owned {
  /// Each object's actions' URLs, by action.
  hrefs: Vec<std::collections::BTreeMap<String, String>>,
  /// Each object's actions' headers, by action.
  headers: Vec<std::collections::BTreeMap<String, Vec<(&'static str, &'static str)>>>,
}

/// The strings of a recorded batch answer's actions; the one header the responder sends is leaked into a static.
fn owned_of(body: &Value) -> Owned {
  let mut owned = Owned::default();
  for object in body["objects"].as_array().unwrap() {
    let mut hrefs = std::collections::BTreeMap::new();
    let mut headers = std::collections::BTreeMap::new();
    for (name, action) in object["actions"].as_object().into_iter().flatten() {
      hrefs.insert(name.clone(), action["href"].as_str().unwrap().to_string());
      let pairs: Vec<(&'static str, &'static str)> = action["header"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(key, value)| {
          (
            &*key.clone().leak(),
            &*value.as_str().unwrap().to_string().leak(),
          )
        })
        .collect();
      headers.insert(name.clone(), pairs);
    }
    owned.hrefs.push(hrefs);
    owned.headers.push(headers);
  }
  owned
}

/// A-610, golden: do write, from its fields, every batch answer the stock client accepted: uploads with verify
/// callbacks, downloads with headers and an expiry, and per-object 404s. Expect each, read back as JSON, equal to the
/// answer the client read.
#[test]
fn every_batch_answer_the_stock_client_accepted_is_written() {
  let mut written = 0;
  for exchange in exchanges() {
    if exchange.path != format!("{LFS_PREFIX}/objects/batch") {
      continue;
    }
    let accepted: Value = serde_json::from_str(&exchange.response).unwrap();
    let owned = owned_of(&accepted);
    let answers = answers_of(&accepted, &owned);
    let mut out = Vec::new();
    write_response(&answers, &mut out).unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&out).unwrap(), accepted);
    assert_eq!(json::validate(&out, &mut Admission::new(ROOM)), Ok(()));
    written += 1;
  }
  assert_eq!(written, 4);
}

/// A-610, golden: do read batch.md's four request examples as they stand, then with their illustrative oid
/// `12345678` made a real one. Expect each first refused for that oid, then read: the operation each names, `basic` as
/// its transfer, its ref where it gives one.
#[test]
fn the_documents_requests_are_read_once_their_oid_is_real() {
  let examples: Vec<String> = json_examples(BATCH_MD)
    .into_iter()
    .filter(|text| text.contains("\"operation\""))
    .collect();
  assert_eq!(examples.len(), 4);
  let expected = [
    (Operation::Download, Some("refs/heads/main")),
    (Operation::Download, None),
    (Operation::Upload, None),
    (Operation::Upload, Some("refs/heads/contrib")),
  ];
  for (example, (operation, ref_name)) in examples.iter().zip(expected) {
    assert!(matches!(request(example), Err(Refusal::Oid { .. })));
    let real = example.replace("\"12345678\"", &format!("\"{EXAMPLE_OID}\""));
    let read = request(&real).unwrap();
    assert_eq!(read.operation, operation);
    assert_eq!(texts(&read.transfers), ["basic"]);
    assert_eq!(
      read.ref_name.map(|name| name.chars().collect::<String>()),
      ref_name.map(String::from)
    );
    assert_eq!(
      read.objects,
      [Object {
        oid: Oid::from_hex(EXAMPLE_OID.as_bytes()).unwrap(),
        size: 123
      }]
    );
  }
}

/// A JSON value with every `"oid"` string replaced by [`EXAMPLE_OID`].
fn with_example_oid(mut value: Value) -> Value {
  for object in value["objects"].as_array_mut().unwrap() {
    object["oid"] = Value::String(EXAMPLE_OID.to_string());
  }
  value
}

/// A-610, golden: do write the batch answers of batch.md (a download with a header and `expires_at`, a per-object 404)
/// and basic-transfers.md (a download, an upload, an upload with a verify callback, each with `expires_in`). Expect
/// each, read back as JSON, equal to the document's, its illustrative oid aside.
#[test]
fn the_documents_answers_are_written() {
  let mut documents: Vec<String> = json_examples(BATCH_MD)
    .into_iter()
    .filter(|text| text.contains("\"transfer\""))
    .collect();
  documents.extend(json_examples(BASIC_TRANSFERS_MD));
  assert_eq!(documents.len(), 5);
  let header: &'static [(&str, &str)] = &[("Authorization", "Basic ...")];
  let key: &'static [(&str, &str)] = &[("Key", "value")];
  let oid = Oid::from_hex(EXAMPLE_OID.as_bytes()).unwrap();
  let day = ExpiresIn::from_seconds(86_400);
  let action = |href, header: &'static [(&'static str, &'static str)], expires_in| Action {
    href,
    header,
    expires_in,
    expires_at: None,
  };
  let outcomes = [
    Outcome::Download(Action {
      href: "https://some-download.com",
      header: key,
      expires_in: None,
      expires_at: Timestamp::from_unix(1_478_791_747),
    }),
    Outcome::Error(ObjectError {
      code: ErrorCode::NotFound,
      message: "Object does not exist",
    }),
    Outcome::Download(action("https://some-download.com/1111111", header, day)),
    Outcome::Upload {
      upload: action("https://some-upload.com/1111111", header, day),
      verify: None,
    },
    Outcome::Upload {
      upload: action("https://some-upload.com/1111111", header, day),
      verify: Some(action("https://some-verify-callback.com", header, day)),
    },
  ];
  for (document, outcome) in documents.iter().zip(outcomes) {
    let theirs = with_example_oid(serde_json::from_str(document).unwrap());
    let authenticated = theirs["objects"][0]["authenticated"]
      .as_bool()
      .unwrap_or(false);
    let mut out = Vec::new();
    write_response(
      &[Answer {
        oid,
        size: 123,
        authenticated,
        outcome,
      }],
      &mut out,
    )
    .unwrap();
    let mut ours: Value = serde_json::from_slice(&out).unwrap();
    if theirs.get("hash_algo").is_none() {
      ours.as_object_mut().unwrap().remove("hash_algo");
    }
    assert_eq!(ours, theirs, "{document}");
  }
}

/// Where `needle` first stands in `body`, as a refusal names it.
fn at(body: &str, needle: &str) -> u64 {
  body.find(needle).unwrap() as u64
}

/// A-610, hostile input: do read batch requests each wrong one way: not an object; missing, mistyped and repeated
/// fields; an unknown operation; oids in upper case and one digit short; sizes negative, fractional, in exponent form,
/// one past Go's `int64` and ten thousand digits long; another hash; a mistyped ref, ref name, transfer list and
/// transfer; and a body cut short. Expect each refused by its kind at the byte where its value begins.
#[test]
fn every_malformed_request_is_refused_by_kind() {
  let oid = EXAMPLE_OID;
  let with = |objects: &str| format!("{{\"operation\":\"download\",\"objects\":[{objects}]}}");
  let one = |size: &str| with(&format!("{{\"oid\":\"{oid}\",\"size\":{size}}}"));
  let empty = "{\"operation\":\"download\",\"objects\":[]";
  let cases: Vec<(String, &str, fn(u64) -> Refusal)> = vec![
    ("[]".to_string(), "[", |at| Refusal::Type {
      field: Field::Body,
      at,
    }),
    ("{\"objects\":[]}".to_string(), "{", |at| Refusal::Missing {
      field: Field::Operation,
      at,
    }),
    ("{\"operation\":\"upload\"}".to_string(), "{", |at| {
      Refusal::Missing {
        field: Field::Objects,
        at,
      }
    }),
    (
      "{\"operation\":\"delete\",\"objects\":[]}".to_string(),
      "\"delete",
      |at| Refusal::Operation { at },
    ),
    ("{\"operation\":1,\"objects\":[]}".to_string(), "1", |at| {
      Refusal::Type {
        field: Field::Operation,
        at,
      }
    }),
    (
      "{\"operation\":\"upload\",\"operation\":\"download\",\"objects\":[]}".to_string(),
      "\"download",
      |at| Refusal::Repeated {
        field: Field::Operation,
        at,
      },
    ),
    (
      with(&format!(
        "{{\"oid\":\"{}\",\"size\":1}}",
        oid.to_uppercase()
      )),
      "\"A",
      |at| Refusal::Oid { at },
    ),
    (
      with(&format!("{{\"oid\":\"{}\",\"size\":1}}", &oid[..63])),
      "\"a9",
      |at| Refusal::Oid { at },
    ),
    (with("{\"oid\":12,\"size\":1}"), "12", |at| Refusal::Type {
      field: Field::Oid,
      at,
    }),
    (with("{\"size\":1}"), "{\"size", |at| Refusal::Missing {
      field: Field::Oid,
      at,
    }),
    (with(&format!("{{\"oid\":\"{oid}\"}}")), "{\"oid", |at| {
      Refusal::Missing {
        field: Field::Size,
        at,
      }
    }),
    (one("-1"), "-1", |at| Refusal::NegativeSize { at }),
    (one("1.5"), "1.5", |at| Refusal::Size {
      at,
      kind: NumberRefusal::NotInteger,
    }),
    (one("1e3"), "1e3", |at| Refusal::Size {
      at,
      kind: NumberRefusal::NotInteger,
    }),
    (one("9223372036854775808"), "9223", |at| Refusal::Size {
      at,
      kind: NumberRefusal::TooLarge,
    }),
    (one(&"9".repeat(10_000)), "9999", |at| Refusal::Size {
      at,
      kind: NumberRefusal::TooLarge,
    }),
    (one("\"1\""), "\"1\"", |at| Refusal::Type {
      field: Field::Size,
      at,
    }),
    (with("1"), "1]", |at| Refusal::Type {
      field: Field::Object,
      at,
    }),
    (
      "{\"operation\":\"download\",\"objects\":{}}".to_string(),
      "{}",
      |at| Refusal::Type {
        field: Field::Objects,
        at,
      },
    ),
    (
      format!("{empty},\"hash_algo\":\"sha512\"}}"),
      "\"sha512",
      |at| Refusal::HashAlgo { at },
    ),
    (format!("{empty},\"ref\":\"main\"}}"), "\"main", |at| {
      Refusal::Type {
        field: Field::Ref,
        at,
      }
    }),
    (format!("{empty},\"ref\":{{\"name\":1}}}}"), "1}", |at| {
      Refusal::Type {
        field: Field::RefName,
        at,
      }
    }),
    (
      format!("{empty},\"transfers\":\"basic\"}}"),
      "\"basic",
      |at| Refusal::Type {
        field: Field::Transfers,
        at,
      },
    ),
    (format!("{empty},\"transfers\":[1]}}"), "1]", |at| {
      Refusal::Type {
        field: Field::Transfer,
        at,
      }
    }),
  ];
  for (body, needle, refusal) in cases {
    assert_eq!(request(&body), Err(refusal(at(&body, needle))), "{body}");
  }
  let cut = "{\"operation\":\"download\",\"objects\":[";
  assert_eq!(
    request(cut),
    Err(Refusal::Json(json::Refusal::Truncated {
      at: cut.len() as u64
    }))
  );
}

/// A-610: do read requests the stock client and other clients may send: `"ref": null`, an empty and a `null`
/// `hash_algo`, `"transfers": null`, an oid written with escapes, and unknown fields holding anything. Expect each
/// read, the escaped oid as the plain one.
#[test]
fn what_clients_may_send_is_read() {
  let escaped = EXAMPLE_OID.replace('a', "\\u0061");
  let bodies = [
    format!(
      "{{\"operation\":\"upload\",\"objects\":[{{\"oid\":\"{EXAMPLE_OID}\",\"size\":0}}],\"ref\":null,\"hash_algo\":\"\"}}"
    ),
    format!(
      "{{\"operation\":\"upload\",\"objects\":[{{\"oid\":\"{escaped}\",\"size\":0}}],\"hash_algo\":null,\"transfers\":null}}"
    ),
    format!(
      "{{\"x\":[{{\"y\":[1,2,{{}}]}}],\"operation\":\"upload\",\"objects\":[{{\"oid\":\"{EXAMPLE_OID}\",\"size\":0,\"authenticated\":true,\"z\":{{}}}}],\"ref\":{{\"name\":\"refs/heads/main\",\"other\":[]}}}}"
    ),
  ];
  for body in &bodies {
    let read = request(body).unwrap();
    assert_eq!(
      read.objects,
      [Object {
        oid: Oid::from_hex(EXAMPLE_OID.as_bytes()).unwrap(),
        size: 0
      }],
      "{body}"
    );
  }
}

/// A-610, hostile input: do read every cut of a recorded batch request. Expect each refused as cut short, and the
/// room it was given untouched past the nesting's word.
#[test]
fn every_cut_of_a_request_is_refused() {
  let exchange = exchanges()
    .into_iter()
    .find(|exchange| exchange.path.ends_with("/objects/batch"))
    .unwrap();
  let body = exchange.request.as_bytes();
  for cut in 0..body.len() {
    let mut admission = Admission::new(ROOM);
    assert!(matches!(
      read_request(&body[..cut], &mut admission),
      Err(Refusal::Json(json::Refusal::Truncated { .. }))
    ));
    assert!(ROOM - admission.left() <= 8);
  }
}

/// A-610, hostile input: do read a request of a thousand objects with room for its nesting and all but one of its
/// objects, then a request whose unknown field nests a thousand deep with room for 64 levels. Expect the first refused
/// before its list is built and the second refused by the nesting, each by kind.
#[test]
fn a_request_past_its_admission_is_refused_by_kind() {
  let object = format!("{{\"oid\":\"{EXAMPLE_OID}\",\"size\":1}}");
  let body = format!(
    "{{\"operation\":\"upload\",\"objects\":[{}]}}",
    vec![object; 1_000].join(",")
  );
  let room = 8 + 999 * size_of::<Object>() as u64;
  let mut admission = Admission::new(room);
  assert!(matches!(
    read_request(body.as_bytes(), &mut admission),
    Err(Refusal::PastAdmission(_))
  ));
  let deep = format!(
    "{{\"operation\":\"upload\",\"objects\":[],\"x\":{}{}}}",
    "[".repeat(1_000),
    "]".repeat(1_000)
  );
  assert!(matches!(
    read_request(deep.as_bytes(), &mut Admission::new(8)),
    Err(Refusal::Json(json::Refusal::Nesting { depth: 65, .. }))
  ));
}

/// A-610: do write answers whose sizes pass Go's `int64`, whose header name is not a token, and whose header value
/// carries a line ending; and make expiries at and past ±2147483647 seconds. Expect each answer refused by kind with
/// nothing written, the expiries within the bound made and those past it not.
#[test]
fn answers_a_client_could_not_read_are_refused() {
  let oid = Oid::from_hex(EXAMPLE_OID.as_bytes()).unwrap();
  let answer = |size, header: &'static [(&'static str, &'static str)]| Answer {
    oid,
    size,
    authenticated: false,
    outcome: Outcome::Download(Action {
      href: "https://example.invalid/a",
      header,
      expires_in: None,
      expires_at: None,
    }),
  };
  let mut out = Vec::new();
  assert_eq!(
    write_response(&[answer(MAX_SIZE + 1, &[])], &mut out),
    Err(WriteRefusal::Size { answer: 0 })
  );
  assert_eq!(
    write_response(&[answer(1, &[]), answer(1, &[("Bad Name", "v")])], &mut out),
    Err(WriteRefusal::HeaderName { answer: 1 })
  );
  assert_eq!(
    write_response(&[answer(1, &[("X", "a\r\nInjected: b")])], &mut out),
    Err(WriteRefusal::HeaderValue { answer: 0 })
  );
  assert!(out.is_empty());
  assert!(ExpiresIn::from_seconds(2_147_483_647).is_some());
  assert!(ExpiresIn::from_seconds(-2_147_483_647).is_some());
  assert!(ExpiresIn::from_seconds(2_147_483_648).is_none());
  assert!(ExpiresIn::from_seconds(-2_147_483_648).is_none());
}

/// An answer's outcome from generated parts.
fn outcome_of<'a>(
  kind: u8,
  href: &'a str,
  header: &'a [(&'a str, &'a str)],
  seconds: i64,
) -> Outcome<'a> {
  let action = Action {
    href,
    header,
    expires_in: ExpiresIn::from_seconds(seconds),
    expires_at: Timestamp::from_unix(seconds),
  };
  match kind {
    0 => Outcome::Held,
    1 => Outcome::Download(action),
    2 => Outcome::Upload {
      upload: action,
      verify: Some(action),
    },
    _ => Outcome::Error(ObjectError {
      code: ErrorCode::Validation,
      message: href,
    }),
  }
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

  /// A-610, model: do write the answers of up to 8 generated objects, with any href and header text a client can send.
  /// Expect the body valid JSON whose objects carry each oid, size and outcome as given, in order, under the basic
  /// adapter and SHA-256.
  #[test]
  fn any_answers_are_written_as_given(
    objects in proptest::collection::vec((any::<[u8; 32]>(), 0..=MAX_SIZE, 0u8..4, any::<bool>()), 0..8),
    href in "[ -~\u{e9}]{0,12}",
    value in "[ -~]{0,8}",
    seconds in -2_147_483_647i64..=2_147_483_647,
  ) {
    let header = [("X-Test", value.as_str())];
    let answers: Vec<Answer<'_>> = objects
      .iter()
      .map(|(oid, size, kind, authenticated)| Answer { oid: Oid::from_bytes(*oid), size: *size, authenticated: *authenticated, outcome: outcome_of(*kind, &href, &header, seconds) })
      .collect();
    let mut out = Vec::new();
    write_response(&answers, &mut out).unwrap();
    let body: Value = serde_json::from_slice(&out).unwrap();
    prop_assert_eq!(&body["transfer"], "basic");
    prop_assert_eq!(&body["hash_algo"], "sha256");
    for (written, answer) in body["objects"].as_array().unwrap().iter().zip(&answers) {
      prop_assert_eq!(written["oid"].as_str().unwrap(), answer.oid.to_string());
      prop_assert_eq!(written["size"].as_u64().unwrap(), answer.size);
      let kind = (written["actions"].get("download").is_some(), written["actions"].get("upload").is_some(), written.get("error").is_some());
      let expected = match answer.outcome {
        Outcome::Held => (false, false, false),
        Outcome::Download(_) => (true, false, false),
        Outcome::Upload { .. } => (false, true, false),
        Outcome::Error(_) => (false, false, true),
      };
      prop_assert_eq!(kind, expected);
    }
  }

  /// A-610, model: do read a batch request serde_json writes from up to 8 generated objects, as a client would. Expect
  /// the objects as given, in order.
  #[test]
  fn any_request_a_client_writes_is_read(
    objects in proptest::collection::vec((any::<[u8; 32]>(), 0..=MAX_SIZE), 0..8),
    upload in any::<bool>(),
  ) {
    let model: Vec<Object> = objects.iter().map(|(oid, size)| Object { oid: Oid::from_bytes(*oid), size: *size }).collect();
    let body = serde_json::json!({
      "operation": if upload { "upload" } else { "download" },
      "objects": model.iter().map(|object| serde_json::json!({"oid": object.oid.to_string(), "size": object.size})).collect::<Vec<_>>(),
      "transfers": ["basic"],
      "ref": null,
      "hash_algo": "sha256",
    })
    .to_string();
    let read = request(&body).unwrap();
    prop_assert_eq!(read.objects, model);
    prop_assert_eq!(read.operation, if upload { Operation::Upload } else { Operation::Download });
  }
}
