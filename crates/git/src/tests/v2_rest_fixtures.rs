//! A-624's golden vectors: what stock git 2.50.1 (Apple Git-155) answered on its own wire for `object-info`, `bundle-uri`
//! and `packfile-uris`, and the requests it answered, made by `fixtures/v2-rest/make-v2-rest-fixtures.sh` (its header
//! names the commands), compiled in, never read at run time, and each held to the SHA-256 the script recorded.

use crate::object_name::{ObjectFormat, ObjectId};

/// The vectors' SHA-256 sums as the script wrote them, a line a file.
pub(crate) const SUMS: &str = include_str!("../../fixtures/v2-rest/SHA256SUMS");

/// The capability advertisement with `object-info`, `bundle-uri`, `sideband-all` and `packfile-uris` on.
pub(crate) const ADVERTISEMENT: &[u8] = include_bytes!("../../fixtures/v2-rest/advertisement.bin");

/// The refs the bundle clone ended with, `refs/bundles/heads/base` the bundle's.
pub(crate) const BUNDLE_CLONE_REFS_TXT: &[u8] =
  include_bytes!("../../fixtures/v2-rest/bundle-clone.refs.txt");

/// Everything stock git's client sent in a clone with `transfer.bundleURI` on: `ls-refs`, `bundle-uri`, `fetch`.
pub(crate) const BUNDLE_CLONE_REQUEST: &[u8] =
  include_bytes!("../../fixtures/v2-rest/bundle-clone.request.bin");

/// Everything the server answered in that clone: the advertisement, then each command's answer.
pub(crate) const BUNDLE_CLONE_RESPONSE: &[u8] =
  include_bytes!("../../fixtures/v2-rest/bundle-clone.response.bin");

/// A `bundle-uri` request as git's client writes one: its capabilities, the command, a delimiter, the flush.
pub(crate) const BUNDLE_URI_REQUEST: &[u8] =
  include_bytes!("../../fixtures/v2-rest/bundle-uri.request.bin");

/// Its answer: the server's `bundle.*` configuration, a line a key.
pub(crate) const BUNDLE_URI_RESPONSE: &[u8] =
  include_bytes!("../../fixtures/v2-rest/bundle-uri.response.bin");

/// The git that made these vectors.
pub(crate) const GIT_VERSION_TXT: &[u8] = include_bytes!("../../fixtures/v2-rest/git-version.txt");

/// The SHA-1 repository's commits, with their subjects.
pub(crate) const LOG_TXT: &[u8] = include_bytes!("../../fixtures/v2-rest/log.txt");

/// An `object-info` request asking for sizes and naming no object.
pub(crate) const OBJECT_INFO_NO_OID_REQUEST: &[u8] =
  include_bytes!("../../fixtures/v2-rest/object-info-no-oid.request.bin");

/// Its answer: the flush alone.
pub(crate) const OBJECT_INFO_NO_OID_RESPONSE: &[u8] =
  include_bytes!("../../fixtures/v2-rest/object-info-no-oid.response.bin");

/// An `object-info` request naming objects without asking for sizes.
pub(crate) const OBJECT_INFO_NO_SIZE_REQUEST: &[u8] =
  include_bytes!("../../fixtures/v2-rest/object-info-no-size.request.bin");

/// Its answer: each id alone.
pub(crate) const OBJECT_INFO_NO_SIZE_RESPONSE: &[u8] =
  include_bytes!("../../fixtures/v2-rest/object-info-no-size.response.bin");

/// An `object-info` request in a SHA-256 repository.
pub(crate) const OBJECT_INFO_SHA256_REQUEST: &[u8] =
  include_bytes!("../../fixtures/v2-rest/object-info-sha256.request.bin");

/// Its answer.
pub(crate) const OBJECT_INFO_SHA256_RESPONSE: &[u8] =
  include_bytes!("../../fixtures/v2-rest/object-info-sha256.response.bin");

/// An `object-info` request for the sizes of a blob, a tree, a commit, an object the server lacks and a larger blob.
pub(crate) const OBJECT_INFO_SIZES_REQUEST: &[u8] =
  include_bytes!("../../fixtures/v2-rest/object-info-sizes.request.bin");

/// Its answer: `size`, then each object's line.
pub(crate) const OBJECT_INFO_SIZES_RESPONSE: &[u8] =
  include_bytes!("../../fixtures/v2-rest/object-info-sizes.response.bin");

/// An `object-info` request naming a blob in upper case.
pub(crate) const OBJECT_INFO_UPPER_REQUEST: &[u8] =
  include_bytes!("../../fixtures/v2-rest/object-info-upper.request.bin");

/// Its answer, git echoing the id as the client wrote it.
pub(crate) const OBJECT_INFO_UPPER_RESPONSE: &[u8] =
  include_bytes!("../../fixtures/v2-rest/object-info-upper.response.bin");

/// Every object of the SHA-1 repository, each a loose object's bytes: header and content, uncompressed.
pub(crate) const OBJECTS: &[u8] = include_bytes!("../../fixtures/v2-rest/objects.bin");

/// Every object of the SHA-1 repository: its id, type and size.
pub(crate) const OBJECTS_TXT: &[u8] = include_bytes!("../../fixtures/v2-rest/objects.txt");

/// Every object of the SHA-256 repository: its id, type and size.
pub(crate) const SHA256_OBJECTS_TXT: &[u8] =
  include_bytes!("../../fixtures/v2-rest/sha256-objects.txt");

/// The pack the server names by URI: its hash, and the URI.
pub(crate) const URI_PACK_TXT: &[u8] = include_bytes!("../../fixtures/v2-rest/uri-pack.txt");

/// A fetch taking URIs of `ftp` or `https`, under `sideband-all`.
pub(crate) const URIS_FTP_HTTPS_REQUEST: &[u8] =
  include_bytes!("../../fixtures/v2-rest/uris-ftp-https.request.bin");

/// Its answer: the blob's pack by URI.
pub(crate) const URIS_FTP_HTTPS_RESPONSE: &[u8] =
  include_bytes!("../../fixtures/v2-rest/uris-ftp-https.response.bin");

/// A fetch taking URIs of `http` only, under `sideband-all`.
pub(crate) const URIS_HTTP_REQUEST: &[u8] =
  include_bytes!("../../fixtures/v2-rest/uris-http.request.bin");

/// Its answer: everything inline, the URI's protocol not taken.
pub(crate) const URIS_HTTP_RESPONSE: &[u8] =
  include_bytes!("../../fixtures/v2-rest/uris-http.response.bin");

/// A fetch taking URIs of `https`, under `sideband-all`.
pub(crate) const URIS_HTTPS_REQUEST: &[u8] =
  include_bytes!("../../fixtures/v2-rest/uris-https.request.bin");

/// Its answer: the blob's pack by URI, the rest inline.
pub(crate) const URIS_HTTPS_RESPONSE: &[u8] =
  include_bytes!("../../fixtures/v2-rest/uris-https.response.bin");

/// A fetch taking URIs of `https` without `sideband-all`.
pub(crate) const URIS_NO_SIDEBAND_ALL_REQUEST: &[u8] =
  include_bytes!("../../fixtures/v2-rest/uris-no-sideband-all.request.bin");

/// Its answer: everything inline, as git clears the protocols without `sideband-all`.
pub(crate) const URIS_NO_SIDEBAND_ALL_RESPONSE: &[u8] =
  include_bytes!("../../fixtures/v2-rest/uris-no-sideband-all.response.bin");

/// Every vector the script wrote, by its name in `SHA256SUMS`.
pub(crate) const FILES: [(&str, &[u8]); 30] = [
  ("advertisement.bin", ADVERTISEMENT),
  ("bundle-clone.refs.txt", BUNDLE_CLONE_REFS_TXT),
  ("bundle-clone.request.bin", BUNDLE_CLONE_REQUEST),
  ("bundle-clone.response.bin", BUNDLE_CLONE_RESPONSE),
  ("bundle-uri.request.bin", BUNDLE_URI_REQUEST),
  ("bundle-uri.response.bin", BUNDLE_URI_RESPONSE),
  ("git-version.txt", GIT_VERSION_TXT),
  ("log.txt", LOG_TXT),
  ("object-info-no-oid.request.bin", OBJECT_INFO_NO_OID_REQUEST),
  (
    "object-info-no-oid.response.bin",
    OBJECT_INFO_NO_OID_RESPONSE,
  ),
  (
    "object-info-no-size.request.bin",
    OBJECT_INFO_NO_SIZE_REQUEST,
  ),
  (
    "object-info-no-size.response.bin",
    OBJECT_INFO_NO_SIZE_RESPONSE,
  ),
  ("object-info-sha256.request.bin", OBJECT_INFO_SHA256_REQUEST),
  (
    "object-info-sha256.response.bin",
    OBJECT_INFO_SHA256_RESPONSE,
  ),
  ("object-info-sizes.request.bin", OBJECT_INFO_SIZES_REQUEST),
  ("object-info-sizes.response.bin", OBJECT_INFO_SIZES_RESPONSE),
  ("object-info-upper.request.bin", OBJECT_INFO_UPPER_REQUEST),
  ("object-info-upper.response.bin", OBJECT_INFO_UPPER_RESPONSE),
  ("objects.bin", OBJECTS),
  ("objects.txt", OBJECTS_TXT),
  ("sha256-objects.txt", SHA256_OBJECTS_TXT),
  ("uri-pack.txt", URI_PACK_TXT),
  ("uris-ftp-https.request.bin", URIS_FTP_HTTPS_REQUEST),
  ("uris-ftp-https.response.bin", URIS_FTP_HTTPS_RESPONSE),
  ("uris-http.request.bin", URIS_HTTP_REQUEST),
  ("uris-http.response.bin", URIS_HTTP_RESPONSE),
  ("uris-https.request.bin", URIS_HTTPS_REQUEST),
  ("uris-https.response.bin", URIS_HTTPS_RESPONSE),
  (
    "uris-no-sideband-all.request.bin",
    URIS_NO_SIDEBAND_ALL_REQUEST,
  ),
  (
    "uris-no-sideband-all.response.bin",
    URIS_NO_SIDEBAND_ALL_RESPONSE,
  ),
];

/// The id `hex` writes in `format`.
pub(crate) fn id(format: ObjectFormat, hex: &str) -> ObjectId {
  ObjectId::from_hex(format, hex.as_bytes()).unwrap()
}

/// The objects a `<id> <type> <size>` listing names, with their sizes.
pub(crate) fn sizes(format: ObjectFormat, listing: &[u8]) -> Vec<(ObjectId, u64)> {
  std::str::from_utf8(listing)
    .unwrap()
    .lines()
    .map(|line| {
      let mut words = line.split(' ');
      let hex = words.next().unwrap();
      let size = words.nth(1).unwrap().parse().unwrap();
      (id(format, hex), size)
    })
    .collect()
}

/// A-624, golden vectors: do hash every file the script wrote with SHA-256 (aws-lc-rs) and compare each with the sum the
/// script recorded. Expect: every sum equal and every file listed, so the bytes the tests read are the ones stock git
/// 2.50.1 wrote.
#[test]
fn every_golden_vector_is_the_file_the_script_recorded() {
  let mut listed = Vec::new();
  for line in SUMS.lines() {
    let (sum, name) = line.split_once("  ").unwrap();
    let bytes = FILES
      .iter()
      .find(|(known, _)| *known == name)
      .map(|(_, bytes)| *bytes)
      .unwrap_or_else(|| panic!("no vector {name}"));
    let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, bytes);
    let hex = ObjectId::from_bytes(ObjectFormat::Sha256, digest.as_ref())
      .unwrap()
      .to_string();
    assert_eq!(hex, sum, "{name}");
    listed.push(name);
  }
  let names: Vec<&str> = FILES.iter().map(|(name, _)| *name).collect();
  assert_eq!(
    (listed, GIT_VERSION_TXT),
    (names, &b"git version 2.50.1 (Apple Git-155)\n"[..])
  );
}
