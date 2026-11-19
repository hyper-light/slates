//! The reporter's recorded sessions, shared by its tests (A-616): stock Subversion 1.15.0's `svn`
//! against `svnserve` over `history.txt`'s repository, every byte each way, recorded by
//! `crates/svn/fixtures/report/make-fixtures.py` (each file's SHA-256 in `SHA256SUMS`), and the
//! helpers that read their items.
//!
//! Every session is included here, in one table, so each file's path is read from this file's
//! place whichever test asks for it.

use crate::handshake::{ClientCapabilities, Principal, Session};
use crate::ra_svn::{Bounds, Parsed, Parser, Value};

/// One recorded connection: what the client sent and what `svnserve` sent.
#[derive(Clone, Copy)]
pub(super) struct Recorded {
  /// The session's name and connection, its files' stem.
  pub(super) name: &'static str,
  /// The client's bytes.
  pub(super) client: &'static [u8],
  /// The server's bytes.
  pub(super) server: &'static [u8],
}

/// The recorded connection `$name`, its two files included at build time.
macro_rules! recorded {
  ($name:literal) => {
    Recorded {
      name: $name,
      client: include_bytes!(concat!("../../fixtures/report/", $name, ".client.bin")),
      server: include_bytes!(concat!("../../fixtures/report/", $name, ".server.bin")),
    }
  };
}

/// Every recorded connection, in `make-fixtures.py`'s order.
pub(super) const SESSIONS: &[Recorded] = &[
  recorded!("checkout-r1.1"),
  recorded!("update-to-head.1"),
  recorded!("switch-to-branch.1"),
  recorded!("checkout-depth-empty.1"),
  recorded!("update-set-depth-infinity.1"),
  recorded!("checkout-depth-immediates.1"),
  recorded!("update-depth-files.1"),
  recorded!("checkout-head.1"),
  recorded!("update-file-to-r2.1"),
  recorded!("update-mixed-revisions.1"),
  recorded!("checkout-r2.1"),
  recorded!("status-u.1"),
  recorded!("status-u.2"),
  recorded!("diff-r1-r6.1"),
  recorded!("diff-r1-r6.2"),
  recorded!("checkout-for-switch.1"),
  recorded!("switch-subtree.1"),
  recorded!("update-with-link.1"),
  recorded!("checkout-for-not-present.1"),
  recorded!("update-subtree-to-r1.1"),
  recorded!("update-with-not-present.1"),
  recorded!("checkout-for-lock.1"),
  recorded!("lock-a.1"),
  recorded!("update-with-lock.1"),
  recorded!("update-with-broken-lock.1"),
  recorded!("hostile-report-unknown-command.1"),
  recorded!("hostile-report-first-not-root.1"),
  recorded!("hostile-report-missing-path.1"),
  recorded!("hostile-report-bad-depth.1"),
  recorded!("hostile-report-link-elsewhere.1"),
  recorded!("hostile-report-abort.1"),
  recorded!("hostile-update-depth-exclude.1"),
  recorded!("hostile-report-delete-root.1"),
  recorded!("hostile-report-revision-past-head.1"),
  recorded!("hostile-update-target-absent.1"),
  recorded!("hostile-report-malformed-set-path.1"),
  recorded!("hostile-report-early-edit-response.1"),
  recorded!("hostile-report-client-refuses-the-edit.1"),
];

/// The recorded connection named `name`.
pub(super) fn recorded(name: &str) -> Recorded {
  SESSIONS
    .iter()
    .find(|session| session.name == name)
    .copied()
    .unwrap_or_else(|| panic!("no recorded session {name}"))
}

/// The repository's URL every session reached it at.
pub(super) const ROOT_URL: &[u8] = b"svn://127.0.0.1/repo";

/// The repository's identity (`make-fixtures.py` `UUID`).
pub(super) const UUID: &[u8] = b"00000000-0000-4000-8000-0000000a0616";

/// The capabilities a stock 1.15 client announces.
pub(super) fn stock_capabilities() -> ClientCapabilities {
  ClientCapabilities {
    svndiff1: true,
    accepts_svndiff2: true,
    absent_entries: true,
    depth: true,
    mergeinfo: true,
    log_revprops: true,
  }
}

/// An anonymous session of a stock 1.15 client on the fixtures' repository.
pub(super) fn session() -> Session {
  Session {
    principal: Principal::Anonymous,
    capabilities: stock_capabilities(),
    uuid: UUID.to_vec(),
    root_url: ROOT_URL.to_vec(),
  }
}

/// Bounds that admit any item of `bytes`.
pub(super) fn admitting(bytes: &[u8]) -> Bounds {
  Bounds {
    item_bytes: bytes.len() + 1,
    word_bytes: bytes.len(),
    depth: bytes.len(),
    nodes: bytes.len(),
  }
}

/// `bytes` split into its items, each with its whitespace.
pub(super) fn items(bytes: &[u8]) -> Vec<&[u8]> {
  let mut items = Vec::new();
  let mut rest = bytes;
  while !rest.is_empty() {
    let Parsed::Item { consumed, .. } = Parser::new(admitting(rest)).parse(rest).unwrap() else {
      panic!("the bytes end inside an item");
    };
    items.push(&rest[..consumed]);
    rest = &rest[consumed..];
  }
  items
}

/// The value of one whole item.
pub(super) fn value_of(item: &[u8]) -> Value<'_> {
  match Parser::new(admitting(item)).parse(item).unwrap() {
    Parsed::Item { value, .. } => value,
    Parsed::NeedMore { .. } => panic!("not a whole item"),
  }
}

/// A command item's word and parameters, or none for an item that is no command.
pub(super) fn command(item: &[u8]) -> Option<(String, Vec<Value<'_>>)> {
  match value_of(item) {
    Value::List(parts) => match parts.as_slice() {
      [Value::Word(name), Value::List(params)] => Some(((*name).to_owned(), params.clone())),
      _ => None,
    },
    _ => None,
  }
}
