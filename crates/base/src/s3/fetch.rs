//! What an S3 base's host asks of its store, and what the store answers (A-524; §4.15 "An S3 base"). The host
//! ([`super::host`]) is sans-io: a miss names a [`Fetch`], the shard's driver runs it against the store, and the answer
//! is handed back to the host, which answers the parked request when it is asked again. The store is the network client
//! ([`run`] over [`Client`], A-471) or the simulated one ([`super::sim::SimS3::answer`]), so the host's every rule is
//! tested against what a store does, with no socket.

use super::client::{Client, RangeRead, S3Error};
use super::list::ListPage;

/// One request of the host to its store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fetch {
  /// One page of the delimited listing (`delimiter=/`) of the directory whose keys begin with `prefix`, continuing from
  /// `token` when it is not the first.
  List {
    /// The directory's prefix: the base's prefix and the directory's path, ending in `/`, or the base's prefix itself
    /// for its root.
    prefix: String,
    /// The continuation token of the page before.
    token: Option<String>,
  },
  /// `len` bytes of the object `key` from `first`, bound to the object the host opened: `If-Match` its ETag, and its
  /// version where the store named one, so the bytes are the object's and never a replacement's.
  Range {
    /// The object's key.
    key: String,
    /// The first byte.
    first: u64,
    /// How many bytes, fewer at the object's end.
    len: u64,
    /// The ETag the read is bound to.
    etag: String,
    /// The version the read is bound to, where the store named one.
    version: Option<String>,
  },
}

/// A store's answer to a [`Fetch`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fetched {
  /// A listing's page.
  Page(ListPage),
  /// A range's bytes.
  Range(RangeRead),
}

/// Runs `fetch` against the store `client` reaches: a delimited `ListObjectsV2` page, or a ranged `GetObject` bound by
/// `If-Match` (and the version, where one is named).
pub async fn run(client: &mut Client, fetch: &Fetch) -> Result<Fetched, S3Error> {
  match fetch {
    Fetch::List { prefix, token } => client
      .list_page(prefix, true, token.as_deref())
      .await
      .map(Fetched::Page),
    Fetch::Range {
      key,
      first,
      len,
      etag,
      version,
    } => client
      .get_range(key, (*first, *len), Some(etag), version.as_deref())
      .await
      .map(Fetched::Range),
  }
}
