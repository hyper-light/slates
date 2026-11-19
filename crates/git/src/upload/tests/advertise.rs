//! The advertisement and a request held to it (A-640 part 2; §4.17.2 "The wire").

use super::{MAIN_REF, answer_of, answer_v2, pkt};
use crate::Admission;
use crate::fixtures::ADVERTISEMENT;
use crate::http::Version;
use crate::object_name::ObjectFormat;
use crate::served::{self, REFS};
use crate::upload::{Asked, Refusal};
use crate::v2::advertisement::{self, Advertisement};
use crate::v2::request::Command;

/// Each key an advertisement names, in order.
fn keys<'a>(advertisement: &Advertisement<'a>) -> Vec<&'a [u8]> {
  advertisement
    .capabilities
    .iter()
    .map(|line| line.value.key)
    .collect()
}

/// A-640 part 2 (§4.17.2 "The wire"): do ask for the advertisement. Expect: `version 2` and §4.17.2's capabilities in
/// `serve.c`'s order, nothing refused.
#[test]
fn the_advertisement_names_what_the_design_names_in_gits_order() {
  let fixture = served::served();
  let (bytes, answer) = answer_of(&fixture, &REFS, MAIN_REF, Version::V2, Asked::Advertise);
  assert_eq!(answer.refusal(), None);
  let ours = advertisement::decode(&bytes, &mut Admission::new(u64::MAX)).unwrap();
  assert_eq!(
    keys(&ours),
    [
      b"agent".as_slice(),
      b"ls-refs",
      b"fetch",
      b"server-option",
      b"object-format",
      b"session-id",
      b"object-info"
    ]
  );
  assert_eq!(ours.get(b"ls-refs"), Some(Some(b"unborn".as_slice())));
  for feature in [
    b"shallow".as_slice(),
    b"wait-for-done",
    b"filter",
    b"ref-in-want",
    b"sideband-all",
    b"packfile-uris",
  ] {
    assert!(ours.supports(b"fetch", feature));
  }
  assert_eq!(ours.object_format(), Some(ObjectFormat::Sha1));
  assert!(ours.get(b"agent").flatten().unwrap().starts_with(b"slates/"));
}

/// A-640 part 2 (git's advertisement of the same repository, `fixtures/advertisement.bin`): do read stock git 2.50.1's
/// advertisement beside ours. Expect: every key git advertised in ours, in git's order, its `ls-refs` and
/// `object-format` values git's, and each `fetch` feature git listed one ours lists.
#[test]
fn the_advertisement_holds_everything_stock_git_advertised() {
  let fixture = served::served();
  let (bytes, _) = answer_of(&fixture, &REFS, MAIN_REF, Version::V2, Asked::Advertise);
  let ours = advertisement::decode(&bytes, &mut Admission::new(u64::MAX)).unwrap();
  let gits = advertisement::decode(ADVERTISEMENT, &mut Admission::new(u64::MAX)).unwrap();
  let git_keys = keys(&gits);
  let shared: Vec<&[u8]> = keys(&ours)
    .into_iter()
    .filter(|key| git_keys.contains(key))
    .collect();
  assert_eq!(shared, git_keys);
  assert_eq!(ours.get(b"ls-refs"), gits.get(b"ls-refs"));
  assert_eq!(ours.get(b"object-format"), gits.get(b"object-format"));
  let git_features = gits.get(b"fetch").flatten().unwrap();
  for feature in git_features.split(|byte| *byte == b' ') {
    assert!(ours.supports(b"fetch", feature));
  }
}

/// A-640 part 2 (`serve.c` `parse_command`, `receive_client_capability`, `process_request`): do send `bundle-uri`,
/// which is not advertised, a request with `promisor-remote`, which is not either, and one in SHA-256. Expect: each
/// refused with git's words in an `ERR` line.
#[test]
fn a_request_past_the_advertisement_is_refused_in_gits_words() {
  let fixture = served::served();
  let bundle = [
    pkt(b"command=bundle-uri\n"),
    pkt(b"object-format=sha1\n"),
    b"0000".to_vec(),
  ]
  .concat();
  let (bytes, answer) = answer_v2(&fixture, &bundle);
  assert_eq!(answer.refusal(), Some(&Refusal::Command(Command::BundleUri)));
  assert_eq!(bytes, pkt(b"ERR invalid command 'bundle-uri'"));

  let promisor = [
    pkt(b"command=ls-refs\n"),
    pkt(b"promisor-remote=x\n"),
    b"0000".to_vec(),
  ]
  .concat();
  let (_, answer) = answer_v2(&fixture, &promisor);
  assert_eq!(answer.refusal(), Some(&Refusal::Capability));

  let request = [
    pkt(b"command=ls-refs\n"),
    pkt(b"object-format=sha256\n"),
    b"0000".to_vec(),
  ]
  .concat();
  let (bytes, answer) = answer_v2(&fixture, &request);
  assert_eq!(
    answer.refusal(),
    Some(&Refusal::ObjectFormat {
      server: ObjectFormat::Sha1,
      client: Some(ObjectFormat::Sha256)
    })
  );
  assert_eq!(
    bytes,
    pkt(b"ERR mismatched object format: server sha1; client sha256")
  );
}
