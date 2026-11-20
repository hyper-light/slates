//! A-703 over the connection: stock `svnsync`'s and `svnrdump`'s recorded sessions, and the scripted replays, answered
//! by a connection serving the reads fixtures' repository, held to what stock `svnserve` 1.15.0 wrote.

use std::borrow::Cow;

use super::super::*;
use super::{LIMITS, PLENTY, SMALLEST};
use crate::index::ContentIdentity;
use crate::mergeinfo::Contents;
use crate::reads::tests::{Fixture, Green as ReadsGreen};
use crate::report::drive::replay::tests::{SESSIONS, normalized, recorded_drives};
use crate::report::recorded::{command, items, read_drive, value_of};

/// The reads fixtures' green as the connection takes a repository: the reads' source, and the commit drive's (which a
/// replay never asks; a text assembled from the reads' pieces, a name its content's hash).
struct Served<'f>(&'f ReadsGreen);

impl Contents for Served<'_> {
  fn entries(&self, directory: &str, revision: Revision) -> Option<Vec<String>> {
    self.0.entries(directory, revision)
  }

  fn mergeinfo_value(&self, properties: &ContentIdentity) -> Option<&[u8]> {
    self.0.mergeinfo_value(properties)
  }
}

impl reads::Repository for Served<'_> {
  fn revision_properties(&self, revision: Revision) -> Option<Vec<(&[u8], &[u8])>> {
    self.0.revision_properties(revision)
  }

  fn properties(&self, identity: &ContentIdentity) -> Option<Vec<(&[u8], &[u8])>> {
    reads::Repository::properties(self.0, identity)
  }

  fn text(&self, identity: &ContentIdentity, offset: u64) -> Option<Cow<'_, [u8]>> {
    reads::Repository::text(self.0, identity, offset)
  }
}

impl commit_drive::Green for Served<'_> {
  fn text(&self, identity: &ContentIdentity) -> Option<Cow<'_, [u8]>> {
    let mut whole = Vec::new();
    while let Some(piece) =
      reads::Repository::text(self.0, identity, u64::try_from(whole.len()).ok()?)
    {
      if piece.is_empty() {
        break;
      }
      whole.extend_from_slice(&piece);
    }
    Some(Cow::Owned(whole))
  }

  fn properties(&self, identity: &ContentIdentity) -> Option<Vec<(&[u8], &[u8])>> {
    reads::Repository::properties(self.0, identity)
  }

  fn name_text(&self, text: &[u8]) -> ContentIdentity {
    *blake3::hash(text).as_bytes()
  }

  fn name_properties(&self, properties: &[(Vec<u8>, Vec<u8>)]) -> ContentIdentity {
    *blake3::hash(&format!("{properties:?}").into_bytes()).as_bytes()
  }
}

/// The repository the recordings' URL named: the reads fixtures' UUID its realm, anonymous reads, mergeinfo.
fn replay_repository() -> Repository<'static> {
  Repository {
    uuid: b"00000000-0000-4000-8000-00000000a613",
    root_url: b"svn://127.0.0.1:36913/repo",
    realm: b"00000000-0000-4000-8000-00000000a613",
    mergeinfo: true,
    anonymous: true,
    tunnel_user: None,
    cram_md5: false,
  }
}

/// What a connection serving the reads fixtures writes for `client`'s bytes, an item at a time, within `budget`.
fn served(fixture: &Fixture, client: &[u8], budget: Budget) -> Vec<u8> {
  let green = Served(&fixture.green);
  let locks = LockTable::new();
  let texts = crate::commit::drive::HeldTexts::new();
  let sources = Sources {
    index: &fixture.index,
    repository: &green,
    locks: &locks,
    now: AprTime(0),
    policy: Policy {
      anonymous: authz::Access::Read,
      authenticated: authz::Access::Write,
      authz: None,
      filesystem: "repo",
    },
    texts: &texts,
  };
  let mut out = Vec::new();
  let mut connection = Connection::new(LIMITS, &mut out).unwrap();
  let mut next = items(client).into_iter();
  let mut held: Vec<u8> = Vec::new();
  let mut established = false;
  loop {
    match connection.receive(established.then_some(&sources), &held, &mut out, budget) {
      Ok(Turn::NeedMore { .. }) => match next.next() {
        Some(item) => held.extend_from_slice(item),
        None => return out,
      },
      Ok(Turn::Took { consumed }) => drop(held.drain(..consumed)),
      Ok(Turn::Wrote) => {}
      Ok(Turn::Opened { consumed, .. }) => {
        held.drain(..consumed);
        connection.open(&replay_repository(), &mut out).unwrap();
      }
      Ok(Turn::Established { consumed, .. }) => {
        held.drain(..consumed);
        established = true;
      }
      other => panic!("{other:?}"),
    }
  }
}

/// The items outside every revision's drive, after the greeting: the trivial authentication request, each
/// revision's properties as a set, each `finish-replay` and the success.
fn skeleton(answer: &[u8]) -> Vec<String> {
  let mut outside = Vec::new();
  let mut inside = false;
  for item in items(answer).into_iter().skip(1) {
    let name = command(item).map(|(name, _)| name);
    match name.as_deref() {
      Some("target-rev") => inside = true,
      Some("finish-replay") => {
        inside = false;
        outside.push(String::from("finish-replay"));
      }
      Some("revprops") => {
        let mut pairs: Vec<String> = match value_of(item) {
          Value::List(parts) => match parts.get(1) {
            Some(Value::List(list)) => list.iter().map(|pair| format!("{pair:?}")).collect(),
            _ => Vec::new(),
          },
          _ => Vec::new(),
        };
        pairs.sort();
        outside.push(format!("revprops {pairs:?}"));
      }
      _ if !inside => outside.push(String::from_utf8_lossy(item).into_owned()),
      _ => {}
    }
  }
  outside
}

/// A-703 (`serve.c` `replay`, `replay_range`, `replay_one_revision`). Do: answer every recorded replay session (stock
/// `svnsync synchronize`, stock `svnrdump dump`, and the scripted replays) through a connection serving the reads
/// fixtures' repository, once with a budget past every answer and once a byte and a step a call. Expect: everything
/// outside the drives as `svnserve` wrote it (the trivial authentication request, each revision's properties as a set,
/// each `finish-replay`, the success), each revision's drive call for call with its property changes as a set, and the
/// same bytes under either budget.
#[test]
fn every_recorded_replay_session_is_answered_as_svnserve_answered_it() {
  let fixture = Fixture::new();
  for (name, client, server) in SESSIONS {
    let ours = served(&fixture, client, PLENTY);
    assert_eq!(
      skeleton(&ours),
      skeleton(server),
      "{name}: around the drives"
    );
    let theirs = recorded_drives(server);
    let drives = recorded_drives(&ours);
    assert_eq!(drives.len(), theirs.len(), "{name}: the drives");
    for (ours, theirs) in drives.iter().zip(&theirs) {
      assert_eq!(
        normalized(read_drive(ours).calls),
        normalized(read_drive(theirs).calls),
        "{name}"
      );
    }
    assert_eq!(
      served(&fixture, client, SMALLEST),
      ours,
      "{name}: a byte and a step a call"
    );
  }
}
