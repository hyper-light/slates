//! A simulated S3 store, the S3 base's oracle (A-524; §4.15 "An S3 base": `SimS3`, an in-RAM bucket with an
//! outsider's put and delete and a clock the test controls, as `SimHost` is a directory's). It answers the host's
//! fetches ([`Fetch`]) as S3's API reference says S3 answers them, so the host is tested against what a store does:
//! - **Listing** (`ListObjectsV2`, `delimiter=/`): the objects directly under the prefix and the common prefixes
//!   beneath it, merged in UTF-8 binary order, at most a page's keys (a common prefix counted as one key, as S3 counts
//!   it), a truncated page naming an opaque continuation token, a token the store did not give refused `400
//!   InvalidArgument`. A listing shows each key's current object alone.
//! - **Reading** (ranged `GetObject`): `If-Match` an ETag other than the object's answers `412 Precondition Failed`; a
//!   key with no current object `404 NoSuchKey`; a version id the store does not hold `404 NoSuchVersion`; a range
//!   starting at or past the end `416`; otherwise `206` with the bytes, the ETag, and the version id on a versioned
//!   bucket. A version named is read whatever is current, so a replaced object's old version still reads.
//! - **Versioning**: a versioned bucket keeps every version and a delete adds a delete marker; an unversioned one keeps
//!   the current object alone and a delete removes it.
//! - **Credentials**: a refused or expired credential answers `403 AccessDenied` or `400 ExpiredToken` to everything.
//! - **Consistency**: every answer sees every put and delete made before it (S3's strong read-after-write consistency).
//!
//! An object's ETag is a digest of its bytes ("The ETag reflects changes only to the contents of an object"), so the
//! same bytes put again keep their ETag, as S3's do; its `LastModified` is the put's time to the second, as S3 states
//! it.

use std::collections::BTreeMap;
use std::ops::Bound;

use aws_lc_rs::digest;
use slates_transport::pacer::NANOS_PER_SECOND;

use super::client::{RangeRead, S3Error};
use super::fetch::{Fetch, Fetched};
use super::http;
use super::keys::SEPARATOR;
use super::list::{ListPage, Listed, MAX_KEYS};
use super::time::iso8601_of;

/// Format: the bytes of a content digest an ETag keeps: sixteen, an MD5's length, as S3's single-part ETags are.
const ETAG_BYTES: usize = 16;

/// The credential the store is asked with, as the store judges it now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Credential {
  /// Accepted.
  Valid,
  /// Refused (`403 AccessDenied`).
  Denied,
  /// Expired (`400 ExpiredToken`).
  Expired,
}

/// One object's body.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Body {
  bytes: Vec<u8>,
  etag: String,
  last_modified_ns: i64,
}

/// One version of a key: a body, or a delete marker (none).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Version {
  id: Option<String>,
  body: Option<Body>,
}

/// What the store has answered, for a test's non-vacuity checks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SimCounts {
  /// Listing pages answered.
  pub pages: u64,
  /// Ranges answered.
  pub ranges: u64,
  /// Requests refused.
  pub refused: u64,
}

/// An entry of a listing's merged order: an object, or a common prefix.
enum Item {
  Object(Listed),
  Prefix,
}

/// The simulated store: one bucket.
#[derive(Debug)]
pub struct SimS3 {
  versioned: bool,
  objects: BTreeMap<String, Vec<Version>>,
  page_keys: usize,
  credential: Credential,
  tokens: BTreeMap<String, (String, String)>,
  issued: u64,
  versions_made: u64,
  wall_ns: fn() -> i64,
  counts: SimCounts,
}

impl SimS3 {
  /// An empty bucket, versioned when `versioned`, its times read from `wall_ns`, its pages S3's thousand keys.
  pub fn new(versioned: bool, wall_ns: fn() -> i64) -> SimS3 {
    SimS3 {
      versioned,
      objects: BTreeMap::new(),
      page_keys: usize::try_from(MAX_KEYS).unwrap_or(usize::MAX),
      credential: Credential::Valid,
      tokens: BTreeMap::new(),
      issued: 0,
      versions_made: 0,
      wall_ns,
      counts: SimCounts::default(),
    }
  }

  /// Answers listings `keys` keys a page (at least one), so a test meets truncated pages with few keys.
  pub fn set_page_keys(&mut self, keys: usize) {
    self.page_keys = keys.max(1);
  }

  /// Judges the credential every later request is asked with as `credential`.
  pub fn set_credential(&mut self, credential: Credential) {
    self.credential = credential;
  }

  /// What the store has answered so far.
  pub fn counts(&self) -> SimCounts {
    self.counts
  }

  /// An outsider's put of `bytes` at `key`, replacing its current object: its ETag.
  pub fn put(&mut self, key: &str, bytes: &[u8]) -> String {
    let now = (self.wall_ns)();
    let body = Body {
      bytes: bytes.to_vec(),
      etag: etag_of(bytes),
      last_modified_ns: now.saturating_sub(now.checked_rem_euclid(seconds_ns()).unwrap_or(0)),
    };
    let etag = body.etag.clone();
    let id = self.next_version();
    let version = Version {
      id,
      body: Some(body),
    };
    if self.versioned {
      self
        .objects
        .entry(key.to_owned())
        .or_default()
        .push(version);
    } else {
      self.objects.insert(key.to_owned(), vec![version]);
    }
    etag
  }

  /// An outsider's delete of `key`: a delete marker on a versioned bucket, the object gone on an unversioned one.
  pub fn delete(&mut self, key: &str) {
    if self.versioned {
      let id = self.next_version();
      self
        .objects
        .entry(key.to_owned())
        .or_default()
        .push(Version { id, body: None });
    } else {
      self.objects.remove(key);
    }
  }

  /// The current bytes of `key`, if it has a current object.
  pub fn current(&self, key: &str) -> Option<&[u8]> {
    self.current_body(key).map(|body| body.bytes.as_slice())
  }

  /// Every key with a current object, in UTF-8 binary order, as a listing without a delimiter shows them.
  pub fn keys(&self) -> Vec<Listed> {
    self
      .objects
      .keys()
      .filter_map(|key| self.listed(key))
      .collect()
  }

  /// The store's answer to `fetch`, as S3 gives it.
  pub fn answer(&mut self, fetch: &Fetch) -> Result<Fetched, S3Error> {
    let answer = match self.credential {
      Credential::Denied => Err(S3Error::Denied {
        code: Some("AccessDenied".to_owned()),
      }),
      Credential::Expired => Err(S3Error::CredentialExpired),
      Credential::Valid => match fetch {
        Fetch::List { prefix, token } => self.page(prefix, token.as_deref()).map(Fetched::Page),
        Fetch::Range {
          key,
          first,
          len,
          etag,
          version,
        } => self
          .range(key, (*first, *len), etag, version.as_deref())
          .map(Fetched::Range),
      },
    };
    match &answer {
      Ok(Fetched::Page(_)) => self.counts.pages = self.counts.pages.saturating_add(1),
      Ok(Fetched::Range(_)) => self.counts.ranges = self.counts.ranges.saturating_add(1),
      Err(_) => self.counts.refused = self.counts.refused.saturating_add(1),
    }
    answer
  }

  /// A new version id on a versioned bucket; none on an unversioned one (S3's `null` version).
  fn next_version(&mut self) -> Option<String> {
    if !self.versioned {
      return None;
    }
    self.versions_made = self.versions_made.saturating_add(1);
    Some(format!("v{}", self.versions_made))
  }

  /// The current body of `key`: its latest version, unless that is a delete marker.
  fn current_body(&self, key: &str) -> Option<&Body> {
    self.objects.get(key)?.last()?.body.as_ref()
  }

  /// `key` as a listing shows it, when it has a current object.
  fn listed(&self, key: &str) -> Option<Listed> {
    let body = self.current_body(key)?;
    Some(Listed {
      key: key.to_owned(),
      size: u64::try_from(body.bytes.len()).unwrap_or(u64::MAX),
      etag: body.etag.clone(),
      last_modified: iso8601_of(body.last_modified_ns),
    })
  }

  /// One delimited listing page of `prefix`, after the item `token` names.
  fn page(&mut self, prefix: &str, token: Option<&str>) -> Result<ListPage, S3Error> {
    let after = self.resumed_after(prefix, token)?;
    let items = self.items_under(prefix);
    Ok(self.fill_page(prefix, &items, after))
  }

  /// Where a listing of `prefix` resumes: after the item `token` names, from its start with no token. A token this
  /// store did not give for `prefix` is refused `400 InvalidArgument`, as S3 refuses one.
  fn resumed_after(&self, prefix: &str, token: Option<&str>) -> Result<Option<String>, S3Error> {
    let Some(token) = token else {
      return Ok(None);
    };
    match self.tokens.get(token) {
      Some((listed, after)) if listed == prefix => Ok(Some(after.clone())),
      _ => Err(S3Error::Unexpected {
        status: http::BAD_REQUEST,
        code: Some("InvalidArgument".to_owned()),
      }),
    }
  }

  /// The items directly under `prefix`, by name in UTF-8 binary order: each key with a current object, and each common
  /// prefix that has one beneath it.
  fn items_under(&self, prefix: &str) -> BTreeMap<String, Item> {
    let mut items: BTreeMap<String, Item> = BTreeMap::new();
    let under = self
      .objects
      .range::<str, _>((Bound::Included(prefix), Bound::Unbounded))
      .take_while(|(key, _)| key.starts_with(prefix));
    for (key, _) in under {
      let Some(rest) = key.get(prefix.len()..) else {
        continue;
      };
      match rest.find(SEPARATOR) {
        Some(at) => {
          let common = key
            .get(..prefix.len().saturating_add(at).saturating_add(1))
            .unwrap_or(key);
          if self.has_current_under(common) {
            items.insert(common.to_owned(), Item::Prefix);
          }
        }
        None => {
          if let Some(listed) = self.listed(key) {
            items.insert(key.clone(), Item::Object(listed));
          }
        }
      }
    }
    items
  }

  /// The page of `items` after `after`, at most the store's keys a page, a common prefix counted as one key, with a
  /// continuation token issued for `prefix` when more follow.
  fn fill_page(
    &mut self,
    prefix: &str,
    items: &BTreeMap<String, Item>,
    after: Option<String>,
  ) -> ListPage {
    let start = match &after {
      Some(after) => Bound::Excluded(after.as_str()),
      None => Bound::Unbounded,
    };
    let mut page = ListPage::default();
    let mut last = None;
    let mut remaining = items.range::<str, _>((start, Bound::Unbounded)).peekable();
    while page.objects.len().saturating_add(page.prefixes.len()) < self.page_keys {
      let Some((name, item)) = remaining.next() else {
        break;
      };
      match item {
        Item::Object(listed) => page.objects.push(listed.clone()),
        Item::Prefix => page.prefixes.push(name.clone()),
      }
      last = Some(name.clone());
    }
    if remaining.peek().is_some()
      && let Some(last) = last
    {
      self.issued = self.issued.saturating_add(1);
      let token = format!("token-{}", self.issued);
      self.tokens.insert(token.clone(), (prefix.to_owned(), last));
      page.truncated = true;
      page.next_token = Some(token);
    }
    page
  }

  /// Whether a key under `common` has a current object, so its common prefix lists.
  fn has_current_under(&self, common: &str) -> bool {
    self
      .objects
      .range::<str, _>((Bound::Included(common), Bound::Unbounded))
      .take_while(|(key, _)| key.starts_with(common))
      .any(|(key, _)| self.current_body(key).is_some())
  }

  /// `len` bytes of `key` from `first`, bound by `If-Match` to `etag`, of the version `version` names or the current.
  fn range(
    &self,
    key: &str,
    (first, len): (u64, u64),
    etag: &str,
    version: Option<&str>,
  ) -> Result<RangeRead, S3Error> {
    let (body, id) = match version {
      Some(wanted) => {
        let found = self
          .objects
          .get(key)
          .and_then(|versions| versions.iter().find(|v| v.id.as_deref() == Some(wanted)))
          .and_then(|v| v.body.as_ref().map(|body| (body, v.id.clone())));
        found.ok_or(S3Error::Missing {
          code: Some("NoSuchVersion".to_owned()),
        })?
      }
      None => {
        let latest = self.objects.get(key).and_then(|versions| versions.last());
        let body = latest
          .and_then(|v| v.body.as_ref())
          .ok_or(S3Error::Missing {
            code: Some("NoSuchKey".to_owned()),
          })?;
        (body, latest.and_then(|v| v.id.clone()))
      }
    };
    if body.etag != etag {
      return Err(S3Error::Changed);
    }
    let total = u64::try_from(body.bytes.len()).unwrap_or(u64::MAX);
    if first >= total {
      return Err(S3Error::RangeNotSatisfiable);
    }
    let from = usize::try_from(first).unwrap_or(usize::MAX);
    let to = usize::try_from(first.saturating_add(len).min(total)).unwrap_or(usize::MAX);
    Ok(RangeRead {
      bytes: body.bytes.get(from..to).unwrap_or_default().to_vec(),
      total,
      etag: body.etag.clone(),
      version: id,
    })
  }
}

/// A second in nanoseconds, as a signed time's unit.
fn seconds_ns() -> i64 {
  i64::try_from(NANOS_PER_SECOND).unwrap_or(i64::MAX)
}

/// The ETag of `bytes`: a digest of the content, quoted as S3 quotes an ETag.
fn etag_of(bytes: &[u8]) -> String {
  let digest = digest::digest(&digest::SHA256, bytes);
  let hex: String = digest
    .as_ref()
    .iter()
    .take(ETAG_BYTES)
    .map(|byte| format!("{byte:02x}"))
    .collect();
  format!("\"{hex}\"")
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Shape: the simulated clock's reading for a test: a fixed instant, 2023-11-14T22:13:20Z.
  fn clock() -> i64 {
    1_700_000_000_000_000_000
  }

  /// A-524 (S3's `ListObjectsV2` with a delimiter): do list a prefix whose keys are files, a nested directory, a
  /// directory marker, and a key past the prefix, two keys a page. Expect the objects and common prefixes in S3's
  /// order across three pages, each truncated page naming a token the next asks with, the last naming none; and a token
  /// the store did not give refused.
  #[test]
  fn a_delimited_listing_pages_in_s3s_order_with_its_tokens() {
    let mut store = SimS3::new(false, clock);
    for key in ["p/a", "p/b/c", "p/b/d/e", "p/f", "p/g/", "q/z"] {
      store.put(key, key.as_bytes());
    }
    store.set_page_keys(2);
    let mut token = None;
    let mut seen = Vec::new();
    loop {
      let fetch = Fetch::List {
        prefix: "p/".to_owned(),
        token: token.clone(),
      };
      let Ok(Fetched::Page(page)) = store.answer(&fetch) else {
        panic!("a page");
      };
      seen.extend(page.objects.iter().map(|o| o.key.clone()));
      seen.extend(page.prefixes.iter().cloned());
      assert_eq!(page.truncated, page.next_token.is_some());
      match page.next_token {
        Some(next) => token = Some(next),
        None => break,
      }
    }
    seen.sort();
    assert_eq!(seen, ["p/a", "p/b/", "p/f", "p/g/"]);
    assert_eq!(store.counts().pages, 2);
    let forged = Fetch::List {
      prefix: "p/".to_owned(),
      token: Some("token-forged".to_owned()),
    };
    assert!(matches!(
      store.answer(&forged),
      Err(S3Error::Unexpected { status: 400, .. })
    ));
  }

  /// A-524 (S3's conditional and versioned reads): do read a range bound to an object's ETag, replace the object, and
  /// read again bound to the old ETag, then by the old version on a versioned bucket; delete it; and read past its end.
  /// Expect the bytes, then `412`, the old version's bytes whatever is current, `404 NoSuchKey` after the delete, and
  /// `416` past the end.
  #[test]
  fn reads_answer_412_for_a_replaced_object_and_a_version_reads_whatever_is_current() {
    let mut store = SimS3::new(true, clock);
    let old = store.put("k", b"old bytes");
    let read = |store: &mut SimS3, etag: &str, version: Option<&str>| {
      store.answer(&Fetch::Range {
        key: "k".to_owned(),
        first: 4,
        len: 5,
        etag: etag.to_owned(),
        version: version.map(str::to_owned),
      })
    };
    let Ok(Fetched::Range(first)) = read(&mut store, &old, None) else {
      panic!("a range");
    };
    assert_eq!((first.bytes.as_slice(), first.total), (&b"bytes"[..], 9));
    let version = first.version.clone();
    assert!(version.is_some());
    store.put("k", b"new content");
    assert_eq!(read(&mut store, &old, None), Err(S3Error::Changed));
    let Ok(Fetched::Range(kept)) = read(&mut store, &old, version.as_deref()) else {
      panic!("the old version");
    };
    assert_eq!(kept.bytes, b"bytes");
    store.delete("k");
    assert!(matches!(
      read(&mut store, &old, None),
      Err(S3Error::Missing { .. })
    ));
    let replaced = store.put("k", b"x");
    assert_eq!(
      read(&mut store, &replaced, None),
      Err(S3Error::RangeNotSatisfiable)
    );
    store.set_credential(Credential::Expired);
    assert_eq!(
      read(&mut store, &replaced, None),
      Err(S3Error::CredentialExpired)
    );
  }
}
