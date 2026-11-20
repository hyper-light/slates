//! A-629 part 2b-ii: Git LFS on a shard, its locks in the repository's locks log through the
//! shard's `RepositoryLogs`, judged by the repository's policy; A-629 part 3: its objects in the
//! repository's store, each upload an entry of its logs, made again from them.

use slates_db::HostId;
use slates_db::catalog::{
  ObjectFormat as DbFormat, PolicyEffect, PolicyEntry, PolicyNames, PolicyRight, PolicyRule,
  PolicyWho, Principal, RepositoryId, RepositoryPolicy, RepositoryRecord, VcsTool as DbTool,
};
use slates_git::lfs::objects::LfsNaming;
use slates_git::lfs::serve::{Access, Answer, Need, Routed, route};
use slates_http::Admission as HttpAdmission;
use slates_http::head::{HeadReader, Progress, Target};
use slates_ipc::protocol::{ObjectFormat, ReplyBody, RequestBody, VcsTool};

use super::{LfsAccess, LfsCall, answer, id_of};
use crate::state::ShardState;
use crate::verbs::dispatch;

/// Shape: room enough for every request and answer these tests make.
const ROOM: u64 = 1 << 20;

/// Shape: the lock listing under the test repository's LFS server URL.
const LOCKS: &str = "/repo.git/info/lfs/locks";

/// The repository's owner.
fn owner() -> Principal {
  Principal::Uid {
    host: HostId(0),
    uid: 1_000,
  }
}

/// Another principal.
fn stranger() -> Principal {
  Principal::Uid {
    host: HostId(0),
    uid: 2_000,
  }
}

/// Creates git repository `repo.git`, its default branch `main`, owned by [`owner`]: its id.
fn repository(state: &mut ShardState) -> RepositoryId {
  let create = RequestBody::CreateRepository {
    name: "repo.git".to_owned(),
    tool: VcsTool::Git,
    format: ObjectFormat::Sha1,
    default_branch: "main".to_owned(),
    green: None,
  };
  let reply = dispatch(state, 1, &owner(), create);
  assert!(matches!(reply, ReplyBody::GreenCreated { .. }), "{reply:?}");
  let repositories = state.db.partition().repositories();
  repositories.repository_named("repo.git").unwrap().id
}

/// `method target`, routed as the edge routes it, and its query.
fn routed(method: &str, target: &str) -> (Routed, Vec<u8>) {
  let text = format!(
    "{method} {target} HTTP/1.1\r\nHost: lfs.example\r\n\
     Content-Type: application/vnd.git-lfs+json\r\n\r\n"
  );
  let bytes = text.into_bytes();
  let read = HeadReader::new().read(&bytes, &mut HttpAdmission::new(ROOM));
  let Ok(Progress::Complete { value: head, .. }) = read else {
    panic!("{target}: {read:?}");
  };
  let routed = route(&head, &mut HttpAdmission::new(ROOM)).unwrap();
  let query = match head.target {
    Target::Path { query, .. } => query.unwrap_or_default().to_vec(),
    Target::Asterisk => Vec::new(),
  };
  (routed, query)
}

/// Serves `method target` with `body` as `principal`, as the edge's call does.
fn serve(
  state: &mut ShardState,
  principal: Option<&Principal>,
  (method, target, body): (&str, &str, &[u8]),
) -> Answer {
  let (routed, query) = routed(method, target);
  let call = LfsCall {
    routed: &routed,
    query: &query,
    body,
    principal,
    credential_expiry: None,
    challenge: Some("Basic realm=\"slates\""),
    origin: ("https", "lfs.example"),
    request_id: Some("test"),
    room: ROOM,
  };
  answer(state, &call)
}

/// The text after `"key":"` in an answer's body, to its closing quote.
fn field(answer: &Answer, key: &str) -> String {
  let body = String::from_utf8(answer.body.clone()).unwrap();
  let start = body.find(&format!("\"{key}\":\"")).unwrap() + key.len() + 4;
  let rest = body.get(start..).unwrap();
  rest.get(..rest.find('"').unwrap()).unwrap().to_owned()
}

/// The locks repository `repository`'s locks log holds.
fn logged(state: &ShardState, repository: RepositoryId) -> usize {
  state
    .db
    .partition()
    .repositories()
    .locks(repository)
    .count()
}

/// A-629 part 2b-ii. Do: as the repository's owner, take a lock on `a.bin`, list the locks, and
/// release it by the id the take answered. Expect `201` with an id naming the log's position and
/// the path, the lock listed and held in the repository's locks log, then `200` and the log
/// holding none.
#[test]
fn the_owner_takes_lists_and_releases_a_lock_in_the_repository_s_locks_log() {
  crate::daemon::audit_on_shard(|state| {
    let repository = repository(state);
    let owner = owner();
    let me = Some(&owner);
    let taken = serve(state, me, ("POST", LOCKS, br#"{"path":"a.bin"}"#));
    let (id, held) = (field(&taken, "id"), logged(state, repository));
    let listed = field(&serve(state, me, ("GET", LOCKS, b"")), "id");
    let unlock = format!("{LOCKS}/{id}/unlock");
    let released = serve(state, me, ("POST", &unlock, b"{}"));
    let found = (taken.status.code, held, listed == id, released.status.code);
    assert_eq!(found, (201, 1, true, 200));
    assert_eq!(
      (id.ends_with(".612e62696e"), logged(state, repository)),
      (true, 0)
    );
  });
}

/// A-629 part 2b-ii. Do: take a lock as a principal the repository's policy says nothing of, then
/// without credentials, then ask a repository that is not there. Expect `403`, `401` naming the
/// edge's challenge, and `404`; and nothing in the locks log.
#[test]
fn a_stranger_is_refused_and_a_request_without_credentials_is_asked_for_them() {
  crate::daemon::audit_on_shard(|state| {
    let repository = repository(state);
    let lock: &[u8] = br#"{"path":"a.bin"}"#;
    let refused = serve(state, Some(&stranger()), ("POST", LOCKS, lock));
    let asked = serve(state, None, ("POST", LOCKS, lock));
    let elsewhere = serve(
      state,
      Some(&owner()),
      ("GET", "/other.git/info/lfs/locks", b""),
    );
    let codes = [
      refused.status.code,
      asked.status.code,
      elsewhere.status.code,
    ];
    assert_eq!((codes, asked.challenge.is_some()), ([403, 401, 404], true));
    assert_eq!(logged(state, repository), 0);
  });
}

/// A-629 part 2b-ii. Do: take a lock on `a.bin`, release it, take it again, then release by the
/// first lock's id and by the second's. Expect the two ids to differ, the first to name nothing
/// (`404`) though its path is locked, and the second to release the lock.
#[test]
fn an_id_names_its_own_lock_and_never_one_taken_after_it() {
  crate::daemon::audit_on_shard(|state| {
    repository(state);
    let owner = owner();
    let me = Some(&owner);
    let lock: &[u8] = br#"{"path":"a.bin"}"#;
    let first = field(&serve(state, me, ("POST", LOCKS, lock)), "id");
    serve(
      state,
      me,
      ("POST", &format!("{LOCKS}/{first}/unlock"), b"{}"),
    );
    let second = field(&serve(state, me, ("POST", LOCKS, lock)), "id");
    let stale = serve(
      state,
      me,
      ("POST", &format!("{LOCKS}/{first}/unlock"), b"{}"),
    );
    let current = serve(
      state,
      me,
      ("POST", &format!("{LOCKS}/{second}/unlock"), b"{}"),
    );
    let codes = (first != second, stale.status.code, current.status.code);
    assert_eq!(codes, (true, 404, 200));
  });
}

/// A git repository's record owned by [`owner`], its default branch `main`, with `policy`.
fn record_with(policy: RepositoryPolicy) -> RepositoryRecord {
  RepositoryRecord {
    id: RepositoryId::default(),
    name: "repo.git".to_owned(),
    tool: DbTool::Git,
    format: DbFormat::Sha1,
    default_branch: Some("main".to_owned()),
    owner: owner(),
    policy,
  }
}

/// A policy letting [`stranger`] submit to `refs/heads/main`.
fn stranger_submits_to_main() -> RepositoryPolicy {
  RepositoryPolicy {
    version: 1,
    rules: vec![PolicyRule {
      names: PolicyNames::Refs,
      pattern: "refs/heads/main".to_owned(),
      right: PolicyRight::Submit,
      effect: PolicyEffect::Allow,
      who: PolicyWho::AnyOf {
        entries: vec![PolicyEntry::Is {
          principal: stranger(),
        }],
      },
    }],
    ..RepositoryPolicy::default()
  }
}

/// A-629 part 2b-ii. Do: ask the access of the owner, of a stranger the policy lets submit to
/// `refs/heads/main`, and of no principal, for each need on `main`, on `topic` and on no ref.
/// Expect the owner allowed everything no rule speaks to; the stranger allowed to lock and upload
/// on `main` and on no ref (the default branch), refused on `topic`, refused reading (no rule
/// allows it) and refused another's lock (a rewrite); and no principal refused everything.
#[test]
fn access_is_the_policy_s_verdict_on_the_ref_a_request_names() {
  let record = record_with(stranger_submits_to_main());
  let (me, them) = (owner(), stranger());
  let ask = |principal: Option<&Principal>, need, name| {
    let access = LfsAccess {
      record: &record,
      principal,
    };
    access.allows(need, name)
  };
  let main = Some("refs/heads/main");
  let owner_all = [Need::Download, Need::Lock, Need::UnlockOthers]
    .iter()
    .all(|need| ask(Some(&me), *need, Some("refs/heads/topic")));
  let stranger = [
    ask(Some(&them), Need::Lock, main),
    ask(Some(&them), Need::Upload, None),
    ask(Some(&them), Need::Lock, Some("refs/heads/topic")),
    ask(Some(&them), Need::Download, main),
    ask(Some(&them), Need::UnlockOthers, main),
  ];
  let nobody = ask(None, Need::Download, main);
  assert_eq!(
    (owner_all, stranger, nobody),
    (true, [true, true, false, false, false], false)
  );
  assert!(id_of(7, "a").starts_with("7."));
}

/// The records repository `repository`'s logs hold: what a recovery replays into its store.
fn records(state: &ShardState, repository: RepositoryId) -> usize {
  let repositories = state.db.partition().repositories();
  (0..)
    .take_while(|index| repositories.object_record(repository, *index).is_some())
    .count()
}

/// An object's bytes, longer than a content window so the store cuts them into windows.
fn object_bytes() -> Vec<u8> {
  (0..1_u32 << 17)
    .map(|at| u8::try_from(at % 251).unwrap())
    .collect()
}

/// A-629 part 3 (an acknowledged upload survives a restart). Do: as the repository's owner, upload
/// an object, upload it again, then make the repository's store again from its logs, as a
/// recovery does, and download it and ask the batch API for it. Expect the upload `200` with one
/// record more in the logs, the second `200` with none more, and after the store is made again
/// the download `200` with the object's bytes and the batch naming a download for it.
#[test]
fn an_uploaded_object_is_durable_and_served_after_the_store_is_made_again() {
  crate::daemon::audit_on_shard(|state| {
    let repository = repository(state);
    let owner = owner();
    let me = Some(&owner);
    let bytes = object_bytes();
    let oid = LfsNaming::oid(&bytes);
    let object = format!("/repo.git/info/lfs/objects/{oid}");
    let before = records(state, repository);
    let uploaded = serve(state, me, ("PUT", &object, &bytes)).status.code;
    let logged = records(state, repository) - before;
    let again = serve(state, me, ("PUT", &object, &bytes)).status.code;
    let logged_again = records(state, repository) - before;
    crate::vcs_store::rebuild(state, repository);
    let downloaded = serve(state, me, ("GET", &object, b""));
    let batch = format!(
      r#"{{"operation":"download","objects":[{{"oid":"{oid}","size":{}}}]}}"#,
      bytes.len()
    );
    let listed = serve(
      state,
      me,
      ("POST", "/repo.git/info/lfs/objects/batch", batch.as_bytes()),
    );
    assert_eq!((uploaded, logged, again, logged_again), (200, 1, 200, 1));
    assert_eq!(
      (downloaded.status.code, downloaded.body == bytes),
      (200, true)
    );
    let listed_body = String::from_utf8(listed.body).unwrap();
    assert_eq!(
      (
        listed.status.code,
        listed_body.contains(r#""download":{"href":"#)
      ),
      (200, true),
      "{listed_body}"
    );
  });
}

/// A-629 part 3 (hostile input). Do: as the repository's owner, upload bytes under another
/// object's oid, then download that oid. Expect the upload `422`, no record in the logs, and the
/// download `404`: nothing of the bytes was kept or recorded.
#[test]
fn an_upload_whose_bytes_are_not_its_oid_is_refused_and_nothing_is_recorded() {
  crate::daemon::audit_on_shard(|state| {
    let repository = repository(state);
    let owner = owner();
    let me = Some(&owner);
    let oid = LfsNaming::oid(b"the bytes the client named");
    let object = format!("/repo.git/info/lfs/objects/{oid}");
    let before = records(state, repository);
    let refused = serve(state, me, ("PUT", &object, b"other bytes"))
      .status
      .code;
    let logged = records(state, repository) - before;
    let downloaded = serve(state, me, ("GET", &object, b"")).status.code;
    assert_eq!((refused, logged, downloaded), (422, 0, 404));
  });
}
