//! The handshake's tests (A-599; §4.17.4 "5. The wire" and "10. Who may do what").
//!
//! The oracle is stock Subversion: 1.15.0's own sessions, `svn` against `svnserve` recorded by the relay of
//! `crates/svn/fixtures/handshake/make-fixtures.py` (each file's SHA-256 in `SHA256SUMS`), replayed through the state
//! machine, which must write what `svnserve` wrote, its greeting's `svndiff1` aside, end where it ended and
//! authenticate whom it authenticated, whole, in two pieces at every cut and cut short at every byte. Beside them:
//! RFC 2195's worked example and RFC 2202's HMAC-MD5 vectors; `cram.c`'s digest and its reading of a response
//! transcribed as oracles; `vparse_tuple`'s reading of the answer and of the auth-response as tables; and hostile input:
//! unlisted mechanisms, wrong, malformed and mistyped responses, items past their bounds, and calls out of turn.

use std::ffi::CString;

use md5::{Digest, Md5};
use proptest::prelude::*;

use super::*;

/// One session as the relay recorded it: what the client sent and what `svnserve` sent.
#[derive(Clone, Copy)]
struct Session {
  /// The session's name, its files' stem.
  name: &'static str,
  /// The client's bytes.
  client: &'static [u8],
  /// The server's bytes.
  server: &'static [u8],
}

/// The recorded session `$name`, its two files included at build time.
macro_rules! session {
  ($name:literal) => {
    Session {
      name: $name,
      client: include_bytes!(concat!("../../fixtures/handshake/", $name, ".client.bin")),
      server: include_bytes!(concat!("../../fixtures/handshake/", $name, ".server.bin")),
    }
  };
}

/// The anonymous repository's identity, which is also its realm (`make-fixtures.py` `UUIDS`, `CONFIGS`).
const UUID_ANON: &[u8] = b"00000000-0000-4000-8000-00000000a599";

/// The CRAM-MD5 repository's identity.
const UUID_CRAM: &[u8] = b"00000000-0000-4000-8000-00000001a599";

/// The identity of the repository that allows both, which is also its realm.
const UUID_BOTH: &[u8] = b"00000000-0000-4000-8000-00000002a599";

/// The CRAM-MD5 repository's realm (`CONFIGS["cram"]`).
const REALM_CRAM: &[u8] = b"slates fixture realm";

/// The tunnel's user (`TUNNEL_USER`).
const TUNNEL_USER: &[u8] = b"alice";

/// alice's secret, shorter than HMAC-MD5's 64-byte block (`PASSWD`).
const SECRET_SHORT: &[u8] = b"cram-secret";

/// carol's secret, exactly as long as HMAC-MD5's block, used as it is (`SECRET_64`).
const SECRET_BLOCK: [u8; 64] = [b's'; 64];

/// dave's secret, longer than HMAC-MD5's block, hashed first (`SECRET_70`).
const SECRET_LONG: [u8; 70] = [b't'; 70];

/// The first command every stock session sends once the handshake is done (`svn info`).
const FIRST_COMMAND: &[u8] = b"( get-latest-rev ( ) ) ";

/// The capability `svnserve` announces and slates does not.
const SVNDIFF1: &[u8] = b"svndiff1 ";

/// The anonymous repository of the recording, as its `svnserve.conf` says: anonymous users read; no password database.
fn anon_repository() -> Repository<'static> {
  Repository {
    uuid: UUID_ANON,
    root_url: b"svn://127.0.0.1/anon",
    realm: UUID_ANON,
    mergeinfo: true,
    anonymous: true,
    tunnel_user: None,
    cram_md5: false,
  }
}

/// The CRAM-MD5 repository: authenticated users alone, in a realm of its own.
fn cram_repository() -> Repository<'static> {
  Repository {
    uuid: UUID_CRAM,
    root_url: b"svn://127.0.0.1/cram",
    realm: REALM_CRAM,
    mergeinfo: true,
    anonymous: false,
    tunnel_user: None,
    cram_md5: true,
  }
}

/// The repository that allows both, reached at `root_url`, through the tunnel or not.
fn both_repository(root_url: &'static [u8], tunnel: bool) -> Repository<'static> {
  Repository {
    uuid: UUID_BOTH,
    root_url,
    realm: UUID_BOTH,
    mergeinfo: true,
    anonymous: true,
    tunnel_user: tunnel.then_some(TUNNEL_USER),
    cram_md5: true,
  }
}

/// The repository that allows no one: no anonymous access and no way to authenticate.
fn none_repository() -> Repository<'static> {
  Repository {
    uuid: b"00000000-0000-4000-8000-00000003a599",
    root_url: b"svn://127.0.0.1/none",
    realm: b"00000000-0000-4000-8000-00000003a599",
    mergeinfo: true,
    anonymous: false,
    tunnel_user: None,
    cram_md5: false,
  }
}

/// The parts of one CRAM-MD5 challenge, `<nonce.time@host>`.
#[derive(Clone, Debug)]
struct ChallengeParts {
  nonce: u64,
  time_micros: i64,
  host: CString,
}

/// What the test's caller answers the client's URL with.
enum UrlAnswer {
  /// The repository the URL names.
  Open(Repository<'static>),
  /// A command failure.
  Refuse(Unserved<'static>),
}

/// How the test's caller answers what the handshake asks of it.
struct Caller {
  /// What the client's URL is answered with.
  url: UrlAnswer,
  /// Each challenge's parts in the order the client asks for them; `None` where the caller can make none.
  challenges: Vec<Option<ChallengeParts>>,
  /// The users the repository holds keys for.
  keys: Vec<(Vec<u8>, CramKey)>,
}

impl Caller {
  /// A caller that names `repository` for any URL.
  fn opening(repository: Repository<'static>) -> Self {
    Self {
      url: UrlAnswer::Open(repository),
      challenges: Vec::new(),
      keys: Vec::new(),
    }
  }

  /// A caller that refuses any URL with `unserved`.
  fn refusing(unserved: Unserved<'static>) -> Self {
    Self {
      url: UrlAnswer::Refuse(unserved),
      challenges: Vec::new(),
      keys: Vec::new(),
    }
  }

  /// The caller, giving `challenges` in turn.
  fn with_challenges(mut self, challenges: Vec<Option<ChallengeParts>>) -> Self {
    self.challenges = challenges;
    self
  }

  /// The caller, holding `user`'s key made from `secret`.
  fn with_key(mut self, user: &[u8], secret: &[u8]) -> Self {
    self
      .keys
      .push((user.to_vec(), CramKey::from_secret(secret).unwrap()));
    self
  }
}

/// How a replay ended.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Ending {
  /// The handshake is done.
  Established(Principal),
  /// The handshake refused the client.
  Refused(Refused),
  /// The caller's answer to the URL ended the handshake.
  Unserved,
  /// The client's bytes ran out with the handshake waiting on them.
  Waiting,
}

/// What a replay came to.
struct Replay {
  /// Everything the server wrote, the greeting first.
  written: Vec<u8>,
  /// How it ended.
  ending: Ending,
  /// How many of the client's bytes the handshake took.
  taken: usize,
  /// Each failed attempt, in order.
  failures: Vec<AuthFailure>,
}

/// What one turn of a replay did.
enum Step {
  /// The handshake needs more of the client's bytes.
  More,
  /// The handshake took a message; the replay goes on.
  Took {
    consumed: usize,
    failure: Option<AuthFailure>,
  },
  /// The handshake ended, after taking `consumed` more bytes.
  End { consumed: usize, ending: Ending },
}

/// `bytes` in the pieces `cuts` makes, each cut an offset where one piece ends and the next begins.
fn pieces<'a>(bytes: &'a [u8], cuts: &[usize]) -> Vec<&'a [u8]> {
  let mut pieces = Vec::new();
  let mut start = 0;
  for cut in cuts {
    let end = (*cut).clamp(start, bytes.len());
    pieces.push(&bytes[start..end]);
    start = end;
  }
  pieces.push(&bytes[start..]);
  pieces
}

/// Replays `client` through a handshake reading items within `item_bytes`, its caller answering as `caller` says, the
/// client's bytes arriving in the pieces `cuts` makes.
fn replay(client: &[u8], caller: &Caller, item_bytes: usize, cuts: &[usize]) -> Replay {
  let mut written = Vec::new();
  let mut server = Server::new(item_bytes, &mut written).unwrap();
  let mut pieces = pieces(client, cuts).into_iter();
  let mut challenges = caller.challenges.iter();
  let mut held = Vec::new();
  let mut taken = 0;
  let mut failures = Vec::new();
  loop {
    match step(&mut server, &held, caller, &mut challenges, &mut written) {
      Step::More => {
        let Some(piece) = pieces.next() else {
          return Replay {
            written,
            ending: Ending::Waiting,
            taken,
            failures,
          };
        };
        held.extend_from_slice(piece);
      }
      Step::Took { consumed, failure } => {
        held.drain(..consumed);
        taken += consumed;
        failures.extend(failure);
      }
      Step::End { consumed, ending } => {
        return Replay {
          written,
          ending,
          taken: taken + consumed,
          failures,
        };
      }
    }
  }
}

/// One turn: the handshake reads `held`, and the caller answers whatever it asks.
fn step(
  server: &mut Server,
  held: &[u8],
  caller: &Caller,
  challenges: &mut std::slice::Iter<'_, Option<ChallengeParts>>,
  written: &mut Vec<u8>,
) -> Step {
  match server.receive(held, written) {
    Err(refused) => Step::End {
      consumed: 0,
      ending: Ending::Refused(refused),
    },
    Ok(Progress::NeedMore { .. }) => Step::More,
    Ok(Progress::Opened { consumed, .. }) => answer_url(server, caller, consumed, written),
    Ok(Progress::ChallengeNeeded { consumed }) => {
      send_challenge(server, challenges.next(), consumed, written)
    }
    Ok(Progress::KeyNeeded { user, consumed }) => give_key(server, caller, user, consumed, written),
    Ok(Progress::Failed { failure, consumed }) => Step::Took {
      consumed,
      failure: Some(failure),
    },
    Ok(Progress::Established {
      principal,
      consumed,
    }) => Step::End {
      consumed,
      ending: Ending::Established(principal),
    },
  }
}

/// The caller answers the client's URL.
fn answer_url(server: &mut Server, caller: &Caller, consumed: usize, written: &mut Vec<u8>) -> Step {
  let ended = match &caller.url {
    UrlAnswer::Open(repository) => server.open(repository, written).unwrap() == Access::Denied,
    UrlAnswer::Refuse(unserved) => {
      server.refuse(*unserved, written).unwrap();
      true
    }
  };
  if ended {
    Step::End {
      consumed,
      ending: Ending::Unserved,
    }
  } else {
    Step::Took {
      consumed,
      failure: None,
    }
  }
}

/// The caller gives the next challenge, or has none.
fn send_challenge(
  server: &mut Server,
  parts: Option<&Option<ChallengeParts>>,
  consumed: usize,
  written: &mut Vec<u8>,
) -> Step {
  match parts.expect("the caller holds a challenge for every one the client asks for") {
    Some(parts) => {
      server
        .challenge(parts.nonce, parts.time_micros, &parts.host, written)
        .unwrap();
      Step::Took {
        consumed,
        failure: None,
      }
    }
    None => {
      server.challenge_failed(written).unwrap();
      Step::Took {
        consumed,
        failure: Some(AuthFailure::Internal),
      }
    }
  }
}

/// The caller gives the named user's key, where it holds one.
fn give_key(
  server: &mut Server,
  caller: &Caller,
  user: &[u8],
  consumed: usize,
  written: &mut Vec<u8>,
) -> Step {
  let key = caller
    .keys
    .iter()
    .find(|(name, _)| name.as_slice() == user)
    .map(|(_, key)| key);
  match server.key(key, written).unwrap() {
    Verdict::Established(principal) => Step::End {
      consumed,
      ending: Ending::Established(principal),
    },
    Verdict::Failed(failure) => Step::Took {
      consumed,
      failure: Some(failure),
    },
  }
}

/// An item bound no message of `client` reaches: as many bytes, and as many items, as the whole session holds.
fn roomy(client: &[u8]) -> usize {
  client.len().max(1) * PARSED_ITEM_BYTES
}

/// Bounds that admit any item of `bytes`.
fn admitting(bytes: &[u8]) -> Bounds {
  Bounds {
    item_bytes: bytes.len() + 1,
    word_bytes: bytes.len(),
    depth: bytes.len(),
    nodes: bytes.len(),
  }
}

/// `bytes` split into its items, each with its whitespace, read by the item codec.
fn items(bytes: &[u8]) -> Vec<&[u8]> {
  let mut items = Vec::new();
  let mut rest = bytes;
  while !rest.is_empty() {
    let Parsed::Item { consumed, .. } = Parser::new(admitting(rest)).parse(rest).unwrap() else {
      panic!("the bytes end inside an item: {:?}", String::from_utf8_lossy(rest));
    };
    items.push(&rest[..consumed]);
    rest = &rest[consumed..];
  }
  items
}

/// The value of one whole item.
fn value_of(item: &[u8]) -> Value<'_> {
  match Parser::new(admitting(item)).parse(item).unwrap() {
    Parsed::Item { value, consumed } => {
      assert_eq!(consumed, item.len(), "one whole item");
      value
    }
    Parsed::NeedMore { .. } => panic!("not a whole item"),
  }
}

/// `svnserve`'s greeting with its `svndiff1` taken out, which it holds exactly once.
fn without_svndiff1(greeting: &[u8]) -> Vec<u8> {
  let at: Vec<usize> = greeting
    .windows(SVNDIFF1.len())
    .enumerate()
    .filter(|(_, window)| *window == SVNDIFF1)
    .map(|(at, _)| at)
    .collect();
  assert_eq!(at.len(), 1, "svnserve's greeting announces svndiff1 once");
  cat(&[&greeting[..at[0]], &greeting[at[0] + SVNDIFF1.len()..]])
}

/// The first `count` items `svnserve` wrote in `session`, the `svndiff1` of its greeting taken out: what slates writes.
fn svnserve_wrote(session: Session, count: usize) -> Vec<u8> {
  let items = items(session.server);
  assert!(
    items.len() >= count,
    "{}: svnserve wrote {} items",
    session.name,
    items.len()
  );
  let mut expected = without_svndiff1(items[0]);
  for item in &items[1..count] {
    expected.extend_from_slice(item);
  }
  expected
}

/// The parts of a challenge item, `( step ( <nonce.time@host> ) )`, or `None` for any other item.
fn challenge_parts(item: &[u8]) -> Option<ChallengeParts> {
  let Value::List(parts) = value_of(item) else {
    return None;
  };
  let [Value::Word(STEP), Value::List(inner)] = parts.as_slice() else {
    return None;
  };
  let [Value::String(challenge)] = inner.as_slice() else {
    panic!("a step holds one string");
  };
  let text = std::str::from_utf8(challenge).unwrap();
  let (numbers, host) = text.strip_prefix('<')?.strip_suffix('>')?.split_once('@')?;
  let (nonce, time_micros) = numbers.split_once('.')?;
  Some(ChallengeParts {
    nonce: nonce.parse().unwrap(),
    time_micros: time_micros.parse().unwrap(),
    host: CString::new(host).unwrap(),
  })
}

/// The parts of every challenge `svnserve` sent in `session`, in order.
fn challenges_in(session: Session) -> Vec<Option<ChallengeParts>> {
  items(session.server)
    .into_iter()
    .filter_map(challenge_parts)
    .map(Some)
    .collect()
}

/// Asserts two byte strings equal, shown as text first so a difference reads.
fn assert_bytes(actual: &[u8], expected: &[u8], what: &str) {
  assert_eq!(
    String::from_utf8_lossy(actual),
    String::from_utf8_lossy(expected),
    "{what}"
  );
  assert_eq!(actual, expected, "{what}");
}

/// What a replay of a recorded session must come to.
struct Expected {
  /// How many of `svnserve`'s items the handshake writes.
  count: usize,
  /// How it ends.
  ending: Ending,
  /// The attempts that fail, in order.
  failures: Vec<AuthFailure>,
}

/// Replays `session` whole, in two pieces at every cut, and cut short at every byte, and expects: `svnserve`'s first
/// `count` items written (its greeting's `svndiff1` aside), with the ending and the failures `expected` names; the same
/// from every two pieces; and from every shortened session a prefix of those bytes, ending as the whole or waiting.
fn assert_answered_as_svnserve(session: Session, caller: &Caller, expected: &Expected) {
  let whole = replay(session.client, caller, roomy(session.client), &[]);
  assert_bytes(
    &whole.written,
    &svnserve_wrote(session, expected.count),
    session.name,
  );
  assert_eq!(whole.ending, expected.ending, "{}", session.name);
  assert_eq!(whole.failures, expected.failures, "{}", session.name);
  for cut in 1..session.client.len() {
    assert_cut(session, caller, &whole, cut);
  }
}

/// Replays `session` in two pieces cut at `cut`, and cut short there, against its whole replay.
fn assert_cut(session: Session, caller: &Caller, whole: &Replay, cut: usize) {
  let split = replay(session.client, caller, roomy(session.client), &[cut]);
  assert_eq!(split.written, whole.written, "{} cut at {cut}", session.name);
  assert_eq!(split.ending, whole.ending, "{} cut at {cut}", session.name);
  let short = replay(&session.client[..cut], caller, roomy(session.client), &[]);
  assert!(
    whole.written.starts_with(&short.written),
    "{} cut short at {cut}",
    session.name
  );
  assert!(
    short.ending == whole.ending || short.ending == Ending::Waiting,
    "{} cut short at {cut}: {:?}",
    session.name,
    short.ending
  );
}

/// Asserts the handshake took the client's handshake alone: its first command follows, unread.
fn assert_commands_follow(session: Session, caller: &Caller) {
  let whole = replay(session.client, caller, roomy(session.client), &[]);
  assert!(
    session.client[whole.taken..].starts_with(FIRST_COMMAND),
    "{}: the client's first command follows the handshake",
    session.name
  );
}

/// Each stock session with the caller its recording's configuration makes.
fn stock_sessions() -> Vec<(Session, Caller)> {
  let cram = |session: Session, user: &[u8], secret: &[u8]| {
    let caller = Caller::opening(cram_repository())
      .with_challenges(challenges_in(session))
      .with_key(user, secret);
    (session, caller)
  };
  vec![
    (session!("anonymous"), Caller::opening(anon_repository())),
    cram(session!("cram-md5"), b"alice", SECRET_SHORT),
    cram(session!("cram-md5-secret-64"), b"carol", &SECRET_BLOCK),
    cram(session!("cram-md5-secret-70"), b"dave", &SECRET_LONG),
    cram(session!("cram-md5-wrong-password"), b"alice", SECRET_SHORT),
    (
      session!("anonymous-or-cram-md5"),
      Caller::opening(both_repository(b"svn://127.0.0.1/both", false)),
    ),
    (
      session!("external"),
      Caller::opening(both_repository(b"svn+fixture://127.0.0.1/both", true)),
    ),
    (
      session!("no-repository"),
      Caller::refusing(Unserved::NoRepository {
        url: b"svn://127.0.0.1/missing",
      }),
    ),
    (session!("no-access"), Caller::opening(none_repository())),
  ]
}

/// A-599 (stock session `anonymous`: `svn info` against `svnserve` 1.15.0, anonymous read, a SASL client's trace token
/// in its `ANONYMOUS`): replay it whole, in two pieces at every cut and cut short at every byte; expect the greeting,
/// the auth request listing `ANONYMOUS` in the default realm, the success and the repository's information, byte for
/// byte as `svnserve` wrote them (its greeting's `svndiff1` aside), no one authenticated, and the client's first command
/// left unread.
#[test]
fn a_stock_anonymous_session_is_answered_as_svnserve_answered_it() {
  let session = session!("anonymous");
  let caller = Caller::opening(anon_repository());
  let expected = Expected {
    count: 4,
    ending: Ending::Established(Principal::Anonymous),
    failures: Vec::new(),
  };
  assert_answered_as_svnserve(session, &caller, &expected);
  assert_commands_follow(session, &caller);
}

/// A-599 (stock sessions `cram-md5`, `cram-md5-secret-64` and `cram-md5-secret-70`: CRAM-MD5 with a secret shorter
/// than HMAC-MD5's 64-byte block, exactly as long, and longer, which RFC 2104 and `cram.c` hash first): replay each with
/// the challenge `svnserve` sent, cut anywhere; expect its bytes (the auth request in the repository's realm, the
/// challenge, the success, the repository's information), the client's digest proved by the user's key, the user
/// authenticated and the first command left unread; and with any other secret, "Password incorrect".
#[test]
fn stock_cram_md5_sessions_prove_the_clients_digest_whatever_the_secrets_length() {
  let sessions: [(Session, &[u8], &[u8]); 3] = [
    (session!("cram-md5"), b"alice", SECRET_SHORT),
    (session!("cram-md5-secret-64"), b"carol", &SECRET_BLOCK),
    (session!("cram-md5-secret-70"), b"dave", &SECRET_LONG),
  ];
  for (session, user, secret) in sessions {
    let caller = Caller::opening(cram_repository())
      .with_challenges(challenges_in(session))
      .with_key(user, secret);
    let expected = Expected {
      count: 5,
      ending: Ending::Established(Principal::User(user.to_vec())),
      failures: Vec::new(),
    };
    assert_answered_as_svnserve(session, &caller, &expected);
    assert_commands_follow(session, &caller);
    let other = Caller::opening(cram_repository())
      .with_challenges(challenges_in(session))
      .with_key(user, b"not the secret");
    let refused = replay(session.client, &other, roomy(session.client), &[]);
    assert_eq!(
      refused.failures.first(),
      Some(&AuthFailure::WrongPassword),
      "{}",
      session.name
    );
  }
}

/// A-599 (stock session `cram-md5-wrong-password`: the client's digest made from another secret; a SASL client then
/// asks again, `( CRAM-MD5 ( ) )`, before it learns it has no further credentials, and ends): replay it, cut anywhere;
/// expect "Password incorrect", a second challenge for the second attempt, byte for byte as `svnserve` wrote them, and
/// the handshake waiting on the response the client never sent.
#[test]
fn a_wrong_password_is_answered_password_incorrect_and_the_client_may_try_again() {
  let session = session!("cram-md5-wrong-password");
  let caller = Caller::opening(cram_repository())
    .with_challenges(challenges_in(session))
    .with_key(b"alice", SECRET_SHORT);
  assert_eq!(caller.challenges.len(), 2, "svnserve sent two challenges");
  let expected = Expected {
    count: 5,
    ending: Ending::Waiting,
    failures: vec![AuthFailure::WrongPassword],
  };
  assert_answered_as_svnserve(session, &caller, &expected);
}

/// A-599 (stock sessions `anonymous-or-cram-md5`, `svn://` to a repository allowing both, and `external`, the tunnel's
/// `svnserve -t` listing `ANONYMOUS EXTERNAL CRAM-MD5`): replay each, cut anywhere; expect `svnserve`'s bytes, the client
/// taking `ANONYMOUS` over `svn://` and `EXTERNAL` through the tunnel, as `internal_auth.c` and `cyrus_auth.c` choose,
/// and the tunnel's user authenticated through it.
#[test]
fn a_stock_client_takes_the_mechanism_svnserve_lists_first_for_its_transport() {
  let daemon = Caller::opening(both_repository(b"svn://127.0.0.1/both", false));
  let anonymous = Expected {
    count: 4,
    ending: Ending::Established(Principal::Anonymous),
    failures: Vec::new(),
  };
  assert_answered_as_svnserve(session!("anonymous-or-cram-md5"), &daemon, &anonymous);
  let tunnel = Caller::opening(both_repository(b"svn+fixture://127.0.0.1/both", true));
  let external = Expected {
    count: 4,
    ending: Ending::Established(Principal::User(TUNNEL_USER.to_vec())),
    failures: Vec::new(),
  };
  assert_answered_as_svnserve(session!("external"), &tunnel, &external);
  assert_commands_follow(session!("external"), &tunnel);
}

/// A-599 (stock sessions `no-repository` and `no-access`: `find_repos` finds nothing at the URL; the repository allows
/// no one): replay each, cut anywhere; expect `svnserve`'s command failure, 210005 "No repository found in
/// 'svn://127.0.0.1/missing'" and 170001 "No access allowed to this repository", as a release build writes it (no
/// source file, line 0), after the greeting alone, and the handshake ended.
#[test]
fn a_url_naming_no_repository_or_one_allowing_no_one_is_refused_as_svnserve_refuses_it() {
  let missing = Caller::refusing(Unserved::NoRepository {
    url: b"svn://127.0.0.1/missing",
  });
  let refused = Expected {
    count: 2,
    ending: Ending::Unserved,
    failures: Vec::new(),
  };
  assert_answered_as_svnserve(session!("no-repository"), &missing, &refused);
  assert_answered_as_svnserve(
    session!("no-access"),
    &Caller::opening(none_repository()),
    &refused,
  );
}

/// A-599 (hand-made sessions `hostile-version-3`, `hostile-no-edit-pipeline`, `hostile-capability-not-a-word`,
/// `hostile-ra-client-without-client-list` and `hostile-answer-only-version`, each answered by `svnserve` with its
/// greeting alone: `serve.c` 4280-4286 and 4295-4297 refuse the version and a missing `edit-pipeline` with
/// `SVN_ERR_RA_SVN_BAD_VERSION`, `marshal.c` `svn_ra_svn__set_capabilities` a capability that is no word and
/// `vparse_tuple` the rest with `SVN_ERR_RA_SVN_MALFORMED_DATA`): replay each, cut anywhere; expect the greeting alone,
/// the refusal typed, and its number the one `svnserve` refuses with.
#[test]
fn every_answer_svnserve_refuses_is_refused_after_the_greeting_alone_with_its_number() {
  for (session, refused, code) in [
    (
      session!("hostile-version-3"),
      Refused::UnsupportedVersion(3),
      SVN_ERR_RA_SVN_BAD_VERSION,
    ),
    (
      session!("hostile-no-edit-pipeline"),
      Refused::NoEditPipeline,
      SVN_ERR_RA_SVN_BAD_VERSION,
    ),
    (
      session!("hostile-capability-not-a-word"),
      Refused::CapabilityNotWord,
      SVN_ERR_RA_SVN_MALFORMED_DATA,
    ),
    (
      session!("hostile-ra-client-without-client-list"),
      Refused::NotAnswer,
      SVN_ERR_RA_SVN_MALFORMED_DATA,
    ),
    (
      session!("hostile-answer-only-version"),
      Refused::NotAnswer,
      SVN_ERR_RA_SVN_MALFORMED_DATA,
    ),
  ] {
    let expected = Expected {
      count: 1,
      ending: Ending::Refused(refused),
      failures: Vec::new(),
    };
    assert_answered_as_svnserve(session, &Caller::opening(anon_repository()), &expected);
    assert_eq!(refused.subversion_code(), Some(code), "{}", session.name);
  }
}

/// A-599 (hand-made session `hostile-unlisted-mechanism-then-anonymous`: `PLAIN`, which `svnserve` does not implement,
/// then `CRAM-MD5` where no password database lists it, then `ANONYMOUS`): replay it, cut anywhere; expect "Must
/// authenticate with listed mechanism" twice, then the success and the repository's information, as `svnserve` wrote
/// them.
#[test]
fn an_unlisted_mechanism_fails_its_attempt_and_the_client_may_answer_again() {
  let expected = Expected {
    count: 6,
    ending: Ending::Established(Principal::Anonymous),
    failures: vec![AuthFailure::NotListed, AuthFailure::NotListed],
  };
  assert_answered_as_svnserve(
    session!("hostile-unlisted-mechanism-then-anonymous"),
    &Caller::opening(anon_repository()),
    &expected,
  );
}

/// A-599 (hand-made session `hostile-external-other-user-then-own`, through `svnserve -t`: `EXTERNAL` naming `bob`, then
/// the tunnel's own user): replay it, cut anywhere; expect "Requested username does not match", then the tunnel's user
/// authenticated, as `svnserve` wrote them.
#[test]
fn an_external_token_naming_another_user_fails_and_the_tunnels_own_succeeds() {
  let expected = Expected {
    count: 5,
    ending: Ending::Established(Principal::User(TUNNEL_USER.to_vec())),
    failures: vec![AuthFailure::UsernameMismatch],
  };
  assert_answered_as_svnserve(
    session!("hostile-external-other-user-then-own"),
    &Caller::opening(both_repository(b"svn+fixture://127.0.0.1/both", true)),
    &expected,
  );
}

/// `parts` joined.
fn cat(parts: &[&[u8]]) -> Vec<u8> {
  parts.concat()
}

/// A string item, `length:bytes ` (`protocol` §1).
fn string_item(bytes: &[u8]) -> Vec<u8> {
  cat(&[format!("{}:", bytes.len()).as_bytes(), bytes, b" "])
}

/// The answer a stock 1.15 client built without SASL's client string writes for `url` (`client.c` `open_session`).
fn stock_answer(url: &[u8]) -> Vec<u8> {
  cat(&[
    b"( 2 ( edit-pipeline svndiff1 accepts-svndiff2 absent-entries depth mergeinfo log-revprops ) ",
    &string_item(url),
    b"10:SVN/1.15.0 ( ) ) ",
  ])
}

/// A command failure of one error as a release build of `svnserve` writes it (`svn_ra_svn__write_cmd_failure`): the
/// form the recorded `no-repository` and `no-access` sessions show.
fn command_failure(code: u32, message: &[u8]) -> Vec<u8> {
  cat(&[
    format!("( failure ( ( {code} ").as_bytes(),
    &string_item(message),
    b"0: 0 ) ) ) ",
  ])
}

/// A-599 (`find_repos` 3902-3904 and 3918-3920: a URL that is no `svn://` URL, and a path climbing out of the root):
/// refuse the client's URL with each; expect `svnserve`'s words, 125002 "Non-svn URL passed to svn server: '`url`'" and
/// 125001 "Couldn't determine repository path", in the form the recorded refusals show, and the handshake ended.
#[test]
fn a_url_svnserve_cannot_use_is_refused_in_its_words() {
  let url: &'static [u8] = b"http://example.org/repo";
  for (unserved, code, message) in [
    (
      Unserved::BadUrl { url },
      SVN_ERR_BAD_URL,
      cat(&[b"Non-svn URL passed to svn server: '", url, b"'"]),
    ),
    (
      Unserved::BadPath,
      SVN_ERR_BAD_FILENAME,
      b"Couldn't determine repository path".to_vec(),
    ),
  ] {
    let client = stock_answer(b"svn://127.0.0.1/repo");
    let refused = replay(&client, &Caller::refusing(unserved), roomy(&client), &[]);
    let mut expected = without_svndiff1(items(session!("anonymous").server)[0]);
    expected.extend(command_failure(code, &message));
    assert_bytes(&refused.written, &expected, "the refusal");
    assert_eq!(refused.ending, Ending::Unserved);
  }
}

/// The values of a hexadecimal string, lowercase or uppercase.
fn hex_bytes(hex: &str) -> Vec<u8> {
  (0..hex.len())
    .step_by(2)
    .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).unwrap())
    .collect()
}

/// A-599 (RFC 2195 §2's worked example: secret `tanstaaftanstaaf`, challenge
/// `<1896.697170952@postoffice.reston.mci.net>`, digest `b913a602c7eda7a495b4e6e7334d3890`): take the digest of the
/// key; expect RFC 2195's. Then drive the handshake with the challenge from those parts, which `cram.c`'s form gives
/// byte for byte, and tim's response; expect the challenge written so and tim authenticated.
#[test]
fn rfc_2195s_worked_example_authenticates_tim() {
  let challenge = b"<1896.697170952@postoffice.reston.mci.net>";
  let digest = hex_bytes("b913a602c7eda7a495b4e6e7334d3890");
  let key = CramKey::from_secret(b"tanstaaftanstaaf").unwrap();
  assert_eq!(key.digest(challenge).as_slice(), digest.as_slice());
  let client = [
    stock_answer(b"svn://127.0.0.1/cram"),
    b"( CRAM-MD5 ( ) ) ".to_vec(),
    string_item(b"tim b913a602c7eda7a495b4e6e7334d3890"),
  ]
  .concat();
  let parts = ChallengeParts {
    nonce: 1896,
    time_micros: 697_170_952,
    host: CString::new("postoffice.reston.mci.net").unwrap(),
  };
  let caller = Caller::opening(cram_repository())
    .with_challenges(vec![Some(parts)])
    .with_key(b"tim", b"tanstaaftanstaaf");
  let done = replay(&client, &caller, roomy(&client), &[]);
  let step = cat(&[b"( step ( ", &string_item(challenge), b") ) "]);
  assert!(
    done.written.windows(step.len()).any(|window| window == step),
    "the challenge in cram.c's form"
  );
  assert_eq!(done.ending, Ending::Established(Principal::User(b"tim".to_vec())));
}

/// A-599 (RFC 2202 §2's seven HMAC-MD5 test cases, keys of 16, 4, 16, 25, 16 and 80 bytes, two longer than the block):
/// take each case's digest of its data under its key; expect RFC 2202's.
#[test]
fn rfc_2202s_hmac_md5_vectors() {
  let cases: [(Vec<u8>, Vec<u8>, &str); 7] = [
    (vec![0x0b; 16], b"Hi There".to_vec(), "9294727a3638bb1c13f48ef8158bfc9d"),
    (
      b"Jefe".to_vec(),
      b"what do ya want for nothing?".to_vec(),
      "750c783e6ab0b503eaa86e310a5db738",
    ),
    (vec![0xaa; 16], vec![0xdd; 50], "56be34521d144c88dbb8c733f0e8b3f6"),
    ((1..=25).collect(), vec![0xcd; 50], "697eaf0aca3a3aea3a75164746ffaa79"),
    (
      vec![0x0c; 16],
      b"Test With Truncation".to_vec(),
      "56461ef2342edc00f9bab995690efd4c",
    ),
    (
      vec![0xaa; 80],
      b"Test Using Larger Than Block-Size Key - Hash Key First".to_vec(),
      "6b1ab7fe4bd7bf8f0b62e6ce61b9d0cd",
    ),
    (
      vec![0xaa; 80],
      b"Test Using Larger Than Block-Size Key and Larger Than One Block-Size Data".to_vec(),
      "6f630fad67cda0ee1fb1f562db3aa53e",
    ),
  ];
  for (key, data, digest) in cases {
    let made = CramKey::from_secret(&key).unwrap().digest(&data);
    assert_eq!(made.as_slice(), hex_bytes(digest).as_slice(), "key {key:02x?}");
    assert!(CramKey::from_secret(&key).unwrap().verifies(&data, &made));
  }
}

/// The bytes `cram.c` munges a password into (`compute_digest` 85): MD5's block.
const CRAM_C_SECRET_BYTES: usize = 64;

/// `cram.c`'s inner pad byte, XORed into the secret (`compute_digest` 97; RFC 2104 §2's `ipad`).
const CRAM_C_INNER_PAD: u8 = 0x36;

/// `cram.c`'s outer pad byte (`compute_digest` 102; RFC 2104 §2's `opad`).
const CRAM_C_OUTER_PAD: u8 = 0x5c;

/// `cram.c`'s `compute_digest` (82-111), transcribed: the password munged into a 64-byte secret, copied when it fits and
/// its MD5 otherwise, then MD5(secret XOR opad, MD5(secret XOR ipad, challenge)).
fn cram_c_digest(password: &[u8], challenge: &[u8]) -> [u8; DIGEST_BYTES] {
  let mut secret = [0u8; CRAM_C_SECRET_BYTES];
  if password.len() <= CRAM_C_SECRET_BYTES {
    secret[..password.len()].copy_from_slice(password);
  } else {
    secret[..DIGEST_BYTES].copy_from_slice(&Md5::digest(password));
  }
  for byte in &mut secret {
    *byte ^= CRAM_C_INNER_PAD;
  }
  let inner = Md5::new().chain_update(secret).chain_update(challenge).finalize();
  for byte in &mut secret {
    *byte ^= CRAM_C_INNER_PAD ^ CRAM_C_OUTER_PAD;
  }
  Md5::new()
    .chain_update(secret)
    .chain_update(inner)
    .finalize()
    .into()
}

/// `cram.c`'s `hex_to_int` (44-49): lowercase digits alone.
fn cram_c_hex(digit: u8) -> Option<u8> {
  match digit {
    b'0'..=b'9' => Some(digit - b'0'),
    b'a'..=b'f' => Some(digit - b'a' + 10),
    _ => None,
  }
}

/// `cram.c`'s reading of a response (`svn_ra_svn_cram_server` 164-168, `hex_decode` 56-69), transcribed over C's view of
/// the bytes: `strrchr` finds the last space before the first NUL; the digest's length is counted from there to the
/// response's true end; `hex_decode` reads 32 bytes there, failing on any but a lowercase digit, a NUL among them.
fn cram_c_split(response: &[u8]) -> Option<(Vec<u8>, [u8; DIGEST_BYTES])> {
  let c_string = response.split(|byte| *byte == 0).next().unwrap();
  let separator = c_string.iter().rposition(|byte| *byte == b' ')?;
  if response.len() - (separator + 1) != 2 * DIGEST_BYTES {
    return None;
  }
  let mut digest = [0u8; DIGEST_BYTES];
  for (at, slot) in digest.iter_mut().enumerate() {
    let high = cram_c_hex(response[separator + 1 + 2 * at])?;
    let low = cram_c_hex(response[separator + 2 + 2 * at])?;
    *slot = (high << 4) | low;
  }
  Some((response[..separator].to_vec(), digest))
}

/// What `split_response` makes of `response`, owned.
fn split(response: &[u8]) -> Option<(Vec<u8>, [u8; DIGEST_BYTES])> {
  split_response(response).map(|(user, digest)| (user.to_vec(), digest))
}

/// A-599 (`cram.c`'s reading of a response as the oracle, NULs where its C string ends): split well-formed responses,
/// responses with an uppercase digit, one digit short or long, no space, spaces in the user, and a NUL in the user, in
/// the digest and after it; expect what `cram.c` reads, a NUL anywhere refusing the response.
#[test]
fn a_response_splits_as_cram_c_reads_it_a_nul_anywhere_refusing_it() {
  let digest = "988c9f284ea35500a38f1cb139a5341a";
  for response in [
    format!("alice {digest}").into_bytes(),
    format!("a b c {digest}").into_bytes(),
    format!(" {digest}").into_bytes(),
    format!("alice {}", digest.to_uppercase()).into_bytes(),
    format!("alice {}", &digest[1..]).into_bytes(),
    format!("alice {digest}0").into_bytes(),
    format!("alice{digest}").into_bytes(),
    cat(&[b"al\0ice ", digest.as_bytes()]),
    cat(&[b"alice\0 ", digest.as_bytes()]),
    cat(&[b"alice ", &digest.as_bytes()[..31], b"\0"]),
    cat(&[b"alice ", digest.as_bytes(), b"\0"]),
  ] {
    assert_eq!(split(&response), cram_c_split(&response), "{response:?}");
  }
  assert!(split(format!("alice {digest}").as_bytes()).is_some());
  assert!(split(&cat(&[b"al\0ice ", digest.as_bytes()])).is_none());
}

/// A handshake that asked for authentication on `repository`, the client's answer read and its URL named; what it
/// wrote so far is in `written`.
fn asked(repository: Repository<'static>, written: &mut Vec<u8>) -> Server {
  let answer = stock_answer(repository.root_url);
  let mut server = Server::new(roomy(&answer), written).unwrap();
  let Ok(Progress::Opened { consumed, .. }) = server.receive(&answer, written) else {
    panic!("a stock answer opens");
  };
  assert_eq!(consumed, answer.len());
  assert_eq!(server.open(&repository, written), Ok(Access::Asked));
  server
}

/// What a handshake asked for authentication on `repository` makes of the auth-response `response`.
fn read_auth_response<'a>(
  repository: Repository<'static>,
  response: &'a [u8],
) -> Result<Progress<'a>, Refused> {
  let mut written = Vec::new();
  let mut server = asked(repository, &mut written);
  server.receive(response, &mut written)
}

/// A-599 (`serve.c` 711 reads the auth-response as `w(?c)` through `vparse_tuple`: a word, then a list holding a string
/// or nothing, items past those ignored; anything else "Malformed network data", which ends the connection): read each
/// form on a repository allowing `ANONYMOUS` and `CRAM-MD5`; expect each taken or refused as `vparse_tuple` takes it.
#[test]
fn the_auth_response_is_read_as_vparse_tuple_reads_it() {
  let established = |response: &'static [u8]| {
    let progress = read_auth_response(both_repository(b"svn://127.0.0.1/both", false), response);
    assert!(
      matches!(progress, Ok(Progress::Established { principal: Principal::Anonymous, .. })),
      "{:?}: {progress:?}",
      String::from_utf8_lossy(response)
    );
  };
  established(b"( ANONYMOUS ( ) ) ");
  established(b"( ANONYMOUS ( 0: ) ) ");
  established(b"( ANONYMOUS ( 3:abc 7 ) ) ");
  established(b"( ANONYMOUS ( ) 7 trailing ) ");
  let table: [(&[u8], Result<&str, Refused>); 10] = [
    (b"( CRAM-MD5 ( ) ) ", Ok("challenge")),
    (b"( FOO ( ) ) ", Ok("not listed")),
    (b"( EXTERNAL ( 0: ) ) ", Ok("not listed")),
    (b"( ANONYMOUS ) ", Err(Refused::NotAuthResponse)),
    (b"( ANONYMOUS 3:abc ) ", Err(Refused::NotAuthResponse)),
    (b"( ANONYMOUS ( 7 ) ) ", Err(Refused::NotAuthResponse)),
    (b"( 7 ( ) ) ", Err(Refused::NotAuthResponse)),
    (b"( ) ", Err(Refused::NotAuthResponse)),
    (b"ANONYMOUS ", Err(Refused::NotAuthResponse)),
    (b"3:abc ", Err(Refused::NotAuthResponse)),
  ];
  for (response, expected) in table {
    let progress = read_auth_response(both_repository(b"svn://127.0.0.1/both", false), response);
    let read = progress.map(|progress| match progress {
      Progress::ChallengeNeeded { .. } => "challenge",
      Progress::Failed {
        failure: AuthFailure::NotListed,
        ..
      } => "not listed",
      other => panic!("{other:?}"),
    });
    assert_eq!(read, expected, "{:?}", String::from_utf8_lossy(response));
  }
  assert_eq!(
    Refused::NotAuthResponse.subversion_code(),
    Some(SVN_ERR_RA_SVN_MALFORMED_DATA)
  );
}

/// What a fresh handshake makes of the answer `answer`: its URL, `ra-client` and client, or its refusal.
type AnswerRead = Result<(Vec<u8>, Option<Vec<u8>>, Option<Vec<u8>>), Refused>;

/// Reads `answer` on a fresh handshake.
fn read_answer(answer: &[u8]) -> AnswerRead {
  let mut written = Vec::new();
  let mut server = Server::new(roomy(answer), &mut written).unwrap();
  match server.receive(answer, &mut written)? {
    Progress::Opened { open, consumed } => {
      assert_eq!(consumed, answer.len());
      Ok((
        open.url.to_vec(),
        open.ra_client.map(<[u8]>::to_vec),
        open.client.map(<[u8]>::to_vec),
      ))
    }
    other => panic!("an answer read as {other:?}"),
  }
}

/// A-599 (`serve.c` 4278 reads the answer as `nlc?c(?c)` through `vparse_tuple`, then 4280-4297 judge the version, the
/// capabilities' kinds and `edit-pipeline`, in that order): read each form; expect it taken with what it names, or
/// refused as `svnserve` refuses it, the form judged before the version and the version before the capabilities.
#[test]
fn the_answer_is_read_as_vparse_tuple_reads_it_and_judged_in_serve_cs_order() {
  let url = || b"url".to_vec();
  let named = |ra: &str, client: Option<&str>| -> AnswerRead {
    Ok((url(), Some(ra.as_bytes().to_vec()), client.map(|client| client.as_bytes().to_vec())))
  };
  let table: [(&[u8], AnswerRead); 18] = [
    (b"( 2 ( edit-pipeline ) 3:url ) ", Ok((url(), None, None))),
    (b"( 2 ( edit-pipeline ) 3:url 3:abc ( ) ) ", named("abc", None)),
    (b"( 2 ( edit-pipeline ) 3:url 3:abc ( 3:xyz ) ) ", named("abc", Some("xyz"))),
    (b"( 2 ( edit-pipeline ) 3:url 3:abc ( 3:xyz 7 ) ) ", named("abc", Some("xyz"))),
    (b"( 2 ( edit-pipeline ) 3:url 3:abc ( ) 7 more ) ", named("abc", None)),
    (b"( 2 ( edit-pipeline ) 3:url 3:abc ) ", Err(Refused::NotAnswer)),
    (b"( 2 ( edit-pipeline ) 3:url 3:abc 7 ) ", Err(Refused::NotAnswer)),
    (b"( 2 ( edit-pipeline ) 3:url 3:abc ( 7 ) ) ", Err(Refused::NotAnswer)),
    (b"( 2 ( edit-pipeline ) 3:url 7 ( ) ) ", Err(Refused::NotAnswer)),
    (b"( 2 ( edit-pipeline ) url ) ", Err(Refused::NotAnswer)),
    (b"( 2 edit-pipeline 3:url ) ", Err(Refused::NotAnswer)),
    (b"( two ( edit-pipeline ) 3:url ) ", Err(Refused::NotAnswer)),
    (b"2 ", Err(Refused::NotAnswer)),
    (b"( 2 ( 7 ) 3:url 3:abc ) ", Err(Refused::NotAnswer)),
    (b"( 1 ( edit-pipeline ) 3:url ) ", Err(Refused::UnsupportedVersion(1))),
    (b"( 3 ( 7 ) 3:url ) ", Err(Refused::UnsupportedVersion(3))),
    (b"( 2 ( 7 edit-pipeline ) 3:url ) ", Err(Refused::CapabilityNotWord)),
    (b"( 2 ( ) 3:url ) ", Err(Refused::NoEditPipeline)),
  ];
  for (answer, expected) in table {
    assert_eq!(read_answer(answer), expected, "{:?}", String::from_utf8_lossy(answer));
  }
}

/// A-599 (`EXTERNAL`, `serve.c` 657-668: the tunnel's user, failing a token that names another; here a token is
/// compared whole, and an absent one is an empty one): answer `EXTERNAL` on a tunnel to a repository allowing it with no
/// token, an empty one, the user's own name, another name, the user's name with a NUL after it, and with a byte after
/// it; expect the tunnel's user for the first three and "Requested username does not match" for the rest; and through
/// no tunnel, `EXTERNAL` not listed.
#[test]
fn an_external_token_must_name_the_tunnels_user_whole_or_nothing() {
  let tunnel = || both_repository(b"svn+fixture://127.0.0.1/both", true);
  let accepted: [&[u8]; 3] = [b"( ) ", b"( 0: ) ", b"( 5:alice ) "];
  for token in accepted {
    let response = cat(&[b"( EXTERNAL ", token, b") "]);
    let progress = read_auth_response(tunnel(), &response);
    assert!(
      matches!(
        progress,
        Ok(Progress::Established { principal: Principal::User(ref user), consumed })
          if user.as_slice() == TUNNEL_USER && consumed == response.len()
      ),
      "{:?}: {progress:?}",
      String::from_utf8_lossy(&response)
    );
  }
  let refused: [&[u8]; 3] = [b"( 3:bob ) ", b"( 6:alice\0 ) ", b"( 6:alicex ) "];
  for token in refused {
    let response = cat(&[b"( EXTERNAL ", token, b") "]);
    let progress = read_auth_response(tunnel(), &response);
    assert!(
      matches!(progress, Ok(Progress::Failed { failure: AuthFailure::UsernameMismatch, .. })),
      "{:?}",
      String::from_utf8_lossy(&response)
    );
  }
  let progress = read_auth_response(both_repository(b"svn://127.0.0.1/both", false), b"( EXTERNAL ( ) ) ");
  assert!(matches!(progress, Ok(Progress::Failed { failure: AuthFailure::NotListed, .. })));
}

/// The client's bytes for a CRAM-MD5 attempt on the repository allowing both, its response `response`, followed by
/// `then`.
fn cram_attempt(response: &[u8], then: &[u8]) -> Vec<u8> {
  cat(&[
    &stock_answer(b"svn://127.0.0.1/both"),
    b"( CRAM-MD5 ( ) ) ",
    response,
    then,
  ])
}

/// The challenge the CRAM-MD5 attempts of these tests are given.
fn fixed_challenge() -> ChallengeParts {
  ChallengeParts {
    nonce: 5_688_694_775_020_434_939,
    time_micros: 1_795_059_982_203_537,
    host: CString::new("slates.test").unwrap(),
  }
}

/// alice's correct response to [`fixed_challenge`].
fn alices_response() -> Vec<u8> {
  let parts = fixed_challenge();
  let challenge = format!(
    "<{}.{}@{}>",
    parts.nonce,
    parts.time_micros,
    parts.host.to_str().unwrap()
  );
  let digest = CramKey::from_secret(SECRET_SHORT)
    .unwrap()
    .digest(challenge.as_bytes());
  let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
  string_item(format!("alice {hex}").as_bytes())
}

/// A caller for the repository allowing both, holding alice's key and giving [`fixed_challenge`] to as many attempts
/// as `attempts`.
fn caller_with_alice(attempts: usize) -> Caller {
  Caller::opening(both_repository(b"svn://127.0.0.1/both", false))
    .with_challenges(vec![Some(fixed_challenge()); attempts])
    .with_key(b"alice", SECRET_SHORT)
}

/// A-599 (hostile CRAM-MD5 responses, `cram.c` 159-176): answer the challenge with a number and with a list, which
/// `cram.c` passes over without a word; with no space, an uppercase digit, a digit short, a NUL in the user before a
/// good digest, an unknown user and a wrong digest; then `ANONYMOUS`; expect each failure as `cram.c` words it, none
/// written for the first two, and the client authenticated anonymously after it, never as a user a failed attempt
/// named.
#[test]
fn a_failed_cram_md5_attempt_is_worded_as_cram_c_words_it_and_names_no_one() {
  let anonymous = b"( ANONYMOUS ( 0: ) ) ".as_slice();
  let good = alices_response();
  for (response, failure) in [
    (b"5 ".to_vec(), AuthFailure::NotAString),
    (b"( 3:abc ) ".to_vec(), AuthFailure::NotAString),
    (string_item(b"alice"), AuthFailure::MalformedResponse),
    (
      string_item(b"alice 988C9F284EA35500A38F1CB139A5341A"),
      AuthFailure::MalformedResponse,
    ),
    (
      string_item(b"alice 988c9f284ea35500a38f1cb139a5341"),
      AuthFailure::MalformedResponse,
    ),
    (
      string_item(&cat(&[b"al\0ice ", &good[9..good.len() - 1]])),
      AuthFailure::MalformedResponse,
    ),
    (
      string_item(&cat(&[b"mallory ", &good[9..good.len() - 1]])),
      AuthFailure::UnknownUser,
    ),
    (
      string_item(b"alice 00000000000000000000000000000000"),
      AuthFailure::WrongPassword,
    ),
  ] {
    let client = cram_attempt(&response, anonymous);
    let done = replay(&client, &caller_with_alice(1), roomy(&client), &[]);
    assert_eq!(done.failures, vec![failure], "{:?}", String::from_utf8_lossy(&response));
    assert_eq!(done.ending, Ending::Established(Principal::Anonymous));
    let worded = failure
      .message()
      .map(|message| cat(&[b"( failure ( ", &string_item(message.as_bytes()), b") ) "]))
      .unwrap_or_default();
    let step_end = done.written.windows(4).position(|window| window == b"> ) ").unwrap() + 6;
    let after_step = cat(&[&worded, b"( success ( ) ) "]);
    assert!(done.written[step_end..].starts_with(&after_step), "{failure:?}");
  }
  let client = cram_attempt(&good, b"");
  let done = replay(&client, &caller_with_alice(1), roomy(&client), &[]);
  assert_eq!(done.ending, Ending::Established(Principal::User(b"alice".to_vec())));
}

/// A-599 (`cram.c` 150-153: a challenge that cannot be made fails the attempt, "Internal server error in
/// authentication", and `internal_auth_request`'s loop reads the client's next answer): the client chooses CRAM-MD5 and
/// the caller has no challenge, then the client tries again and is given one; expect the failure written, then the
/// challenge, and alice authenticated.
#[test]
fn a_challenge_the_caller_cannot_make_fails_the_attempt_and_the_client_may_try_again() {
  let client = cat(&[
    &stock_answer(b"svn://127.0.0.1/both"),
    b"( CRAM-MD5 ( ) ) ",
    b"( CRAM-MD5 ( ) ) ",
    &alices_response(),
  ]);
  let caller = Caller::opening(both_repository(b"svn://127.0.0.1/both", false))
    .with_challenges(vec![None, Some(fixed_challenge())])
    .with_key(b"alice", SECRET_SHORT);
  let done = replay(&client, &caller, roomy(&client), &[]);
  assert_eq!(done.failures, vec![AuthFailure::Internal]);
  assert_eq!(done.ending, Ending::Established(Principal::User(b"alice".to_vec())));
  let internal = cat(&[
    b"( failure ( ",
    &string_item(b"Internal server error in authentication"),
    b") ) ( step ( ",
  ]);
  assert!(done.written.windows(internal.len()).any(|window| window == internal));
}

/// The tightest item bound `item` needs: its bytes, or what its items cost parsed, whichever is more.
fn needed_for(item: &[u8]) -> usize {
  let bounds = Bounds::of(&value_of(item)).unwrap();
  item.len().max(bounds.nodes * PARSED_ITEM_BYTES)
}

/// A-599 (the bounds: the caller's bytes; the longest word `marshal.c` takes, 24; the prototypes' depth, 2; the item count
/// the bytes over a parsed item's cost): make handshakes for item bounds from none to the most; expect those bounds.
#[test]
fn the_bounds_are_the_callers_bytes_and_what_the_handshake_derives_from_them() {
  for item_bytes in [
    0,
    1,
    PARSED_ITEM_BYTES - 1,
    PARSED_ITEM_BYTES,
    PARSED_ITEM_BYTES * 3 + 1,
    usize::MAX,
  ] {
    let bounds = Server::new(item_bytes, &mut Vec::new()).unwrap().bounds();
    let expected = Bounds {
      item_bytes,
      word_bytes: 24,
      depth: 2,
      nodes: item_bytes / PARSED_ITEM_BYTES,
    };
    assert_eq!(bounds, expected);
  }
}

/// A-599 (each bound exactly enough, the item count included: no message's values cost more memory than its bytes may):
/// for every stock session, find the tightest item bound its handshake needs, each message's bytes or what its items
/// cost parsed, whichever is more; expect the session to end as it does with room to spare at that bound, and refused
/// one byte below it, as an item too large or too many items.
#[test]
fn the_tightest_item_bound_a_stock_session_needs_is_exactly_enough() {
  for (session, caller) in stock_sessions() {
    let whole = replay(session.client, &caller, roomy(session.client), &[]);
    let needed = items(&session.client[..whole.taken])
      .into_iter()
      .map(needed_for)
      .max()
      .unwrap();
    let tight = replay(session.client, &caller, needed, &[]);
    assert_eq!(tight.ending, whole.ending, "{} at {needed}", session.name);
    let short = replay(session.client, &caller, needed - 1, &[]);
    assert!(
      matches!(
        short.ending,
        Ending::Refused(Refused::Item(
          ItemError::ItemTooLarge { .. } | ItemError::TooManyItems { .. }
        ))
      ),
      "{} at {}: {:?}",
      session.name,
      needed - 1,
      short.ending
    );
  }
}

/// A-599 (hostile items, refused typed with their numbers, nothing written after them): a capability of 25 characters,
/// one past `marshal.c`'s 24; a list nested three deep in the answer; a URL declaring 4,294,967,295 bytes, refused at its
/// `:`; a version past 2^64 - 1; a CRAM-MD5 response declaring 2^64 - 1 bytes; a list of more one-character words than
/// the bound pays for parsed; and a stock answer whose URL is longer than the bound.
#[test]
fn items_past_the_handshakes_bounds_are_refused_typed_after_what_was_due() {
  let long_url = stock_answer(&[b'u'; 1000]);
  let many_words = cat(&[b"( 2 ( ", &b"e ".repeat(PARSED_ITEM_BYTES), b") 3:url ) "]);
  let table: [(Vec<u8>, usize, bool); 7] = [
    (cat(&[b"( 2 ( edit-pipeline ", &[b'a'; 25], b" ) 3:url ) "]), 4096, true),
    (b"( 2 ( edit-pipeline ( x ) ) 3:url ) ".to_vec(), 4096, true),
    (b"( 2 ( edit-pipeline ) 4294967295:".to_vec(), 4096, false),
    (b"( 18446744073709551616 ".to_vec(), 4096, true),
    (cram_attempt(b"18446744073709551615:", b""), 4096, false),
    (many_words.clone(), many_words.len(), false),
    (long_url, 999, false),
  ];
  for (client, item_bytes, malformed) in table {
    let done = replay(&client, &caller_with_alice(1), item_bytes, &[]);
    let Ending::Refused(Refused::Item(error)) = done.ending else {
      panic!("{:?}: {:?}", String::from_utf8_lossy(&client), done.ending);
    };
    let code = if malformed {
      SVN_ERR_RA_SVN_MALFORMED_DATA
    } else {
      SVN_ERR_RA_SVN_REQUEST_SIZE
    };
    assert_eq!(Refused::Item(error).subversion_code(), Some(code), "{error:?}");
    assert!(
      done.written.ends_with(b"list ) ) ) ") || done.written.ends_with(b"> ) ) "),
      "nothing after the greeting, or after the challenge: {:?}",
      String::from_utf8_lossy(&done.written)
    );
  }
}

/// `SVN_ERR_RA_SVN_REQUEST_SIZE`, an item past its bound (`svn_error_codes.h`: the ra_svn category's start plus 9).
const SVN_ERR_RA_SVN_REQUEST_SIZE: u32 = 210_009;

/// A-599 (calls out of turn are the caller's error: typed, with no Subversion number, writing nothing and changing
/// nothing): on a fresh handshake, open, refuse, challenge, fail a challenge and give a key; expect each refused and the
/// greeting alone written; read the answer, then read again and give a key before naming the repository; expect those
/// refused; then name it, authenticate, and expect every call after the handshake refused.
#[test]
fn a_call_out_of_turn_is_refused_typed_and_changes_nothing() {
  let mut written = Vec::new();
  let answer = stock_answer(b"svn://127.0.0.1/anon");
  let mut server = Server::new(roomy(&answer), &mut written).unwrap();
  let greeting = written.clone();
  assert_eq!(server.open(&anon_repository(), &mut written), Err(Refused::OutOfTurn));
  assert_eq!(server.refuse(Unserved::NoAccess, &mut written), Err(Refused::OutOfTurn));
  assert_eq!(server.challenge(1, 2, c"host", &mut written), Err(Refused::OutOfTurn));
  assert_eq!(server.challenge_failed(&mut written), Err(Refused::OutOfTurn));
  assert_eq!(server.key(None, &mut written), Err(Refused::OutOfTurn));
  assert_eq!(written, greeting);
  assert_eq!(Refused::OutOfTurn.subversion_code(), None);
  assert!(matches!(server.receive(&answer, &mut written), Ok(Progress::Opened { .. })));
  assert_eq!(server.receive(&answer, &mut written), Err(Refused::OutOfTurn));
  assert_eq!(server.key(None, &mut written), Err(Refused::OutOfTurn));
  assert_eq!(written, greeting);
  assert_eq!(server.open(&anon_repository(), &mut written), Ok(Access::Asked));
  assert_eq!(server.open(&anon_repository(), &mut written), Err(Refused::OutOfTurn));
  let response = b"( ANONYMOUS ( 0: ) ) ";
  assert!(matches!(
    server.receive(response, &mut written),
    Ok(Progress::Established { .. })
  ));
  assert!(server.established());
  let done = written.clone();
  assert_eq!(server.receive(response, &mut written), Err(Refused::OutOfTurn));
  assert_eq!(server.refuse(Unserved::NoAccess, &mut written), Err(Refused::OutOfTurn));
  assert_eq!(written, done);
}

/// A-599 (`serve.c` 4345-4351: a repository that allows no one is refused before any request): name a repository with
/// no anonymous access and no mechanism; expect `Access::Denied`, "No access allowed to this repository" written, and
/// the handshake ended.
#[test]
fn a_repository_allowing_no_one_is_denied_before_any_request() {
  let mut written = Vec::new();
  let answer = stock_answer(b"svn://127.0.0.1/none");
  let mut server = Server::new(roomy(&answer), &mut written).unwrap();
  let greeting = written.clone();
  assert!(matches!(server.receive(&answer, &mut written), Ok(Progress::Opened { .. })));
  assert_eq!(server.open(&none_repository(), &mut written), Ok(Access::Denied));
  assert_eq!(
    &written[greeting.len()..],
    command_failure(SVN_ERR_RA_NOT_AUTHORIZED, b"No access allowed to this repository").as_slice()
  );
  assert!(!server.established());
  assert_eq!(server.receive(b"( ANONYMOUS ( ) ) ", &mut written), Err(Refused::OutOfTurn));
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::default()))]

  /// A-599 (resumable at any split, the codec's promise carried through the handshake): deliver a stock session's bytes
  /// in pieces at arbitrary cuts; expect what the whole delivered at once comes to.
  #[test]
  fn a_session_in_any_pieces_is_answered_as_the_whole(
    choice in any::<prop::sample::Index>(),
    cuts in proptest::collection::vec(0usize..400, 0..12),
  ) {
    let sessions = stock_sessions();
    let (session, caller) = &sessions[choice.index(sessions.len())];
    let mut cuts = cuts;
    cuts.sort_unstable();
    let whole = replay(session.client, caller, roomy(session.client), &[]);
    let pieces = replay(session.client, caller, roomy(session.client), &cuts);
    prop_assert_eq!(pieces.written, whole.written);
    prop_assert_eq!(pieces.ending, whole.ending);
    prop_assert_eq!(pieces.taken, whole.taken);
  }

  /// A-599 (`cram.c`'s `compute_digest` as the oracle): take the key's digest of any challenge under any password
  /// `cram.c` can read, none to 150 bytes, either side of the 64-byte block; expect `cram.c`'s.
  #[test]
  fn the_key_digests_as_cram_c_computes(
    password in proptest::collection::vec(1u8..=255, 0..150),
    challenge in proptest::collection::vec(1u8..=255, 0..120),
  ) {
    let key = CramKey::from_secret(&password).unwrap();
    prop_assert_eq!(key.digest(&challenge), cram_c_digest(&password, &challenge));
    prop_assert!(key.verifies(&challenge, &cram_c_digest(&password, &challenge)));
  }

  /// A-599 (`cram.c`'s reading of a response as the oracle): split responses made of the bytes that decide it (digits,
  /// letters either case, spaces, NULs), and well-formed ones with one byte changed; expect what `cram.c` reads.
  #[test]
  fn any_response_splits_as_cram_c_reads_it(
    noise in proptest::collection::vec(prop::sample::select(b"0af9AFgx \0".to_vec()), 0..48),
    user in proptest::collection::vec(prop::sample::select(b"al ce\0".to_vec()), 0..8),
    digest in any::<[u8; DIGEST_BYTES]>(),
    at in any::<prop::sample::Index>(),
    byte in prop::sample::select(b"0aA \0g".to_vec()),
  ) {
    prop_assert_eq!(split(&noise), cram_c_split(&noise));
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    let mut response = cat(&[&user, b" ", hex.as_bytes()]);
    prop_assert_eq!(split(&response), cram_c_split(&response));
    let changed = at.index(response.len());
    response[changed] = byte;
    prop_assert_eq!(split(&response), cram_c_split(&response));
  }
}
