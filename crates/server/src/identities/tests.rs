//! A-664 part 1's server half: a repository's green refuses a submit without a message or without an identity in its
//! tool, before any work is built, and a plain green does not; the identities are set by their owner alone, held to
//! their tools' rules, from the version read; a session reads them once, at its first submit, and every submit of the
//! session carries them over whatever its client claimed.

use std::ops::ControlFlow;
use std::sync::atomic::{AtomicU32, Ordering};

use slates_db::catalog::Principal;
use slates_ipc::protocol::{
  GitIdentity, ObjectFormat, Refusal, ReplyBody, RequestBody, ToolIdentities, VcsTool, VolumeId,
};
use slates_mem::Handle;
use slates_wire::request::{RequestId, Seen};

use super::{READ, Resumed, record, set_on_channel, with_session};
use crate::state::{ClientSlot, ShardState};
use crate::verbs::{Served, dispatch};

/// Shape: the client ids the session tests seat, on audit daemons that seat none of their own.
const FIRST_SESSION: u32 = 41;
/// Shape: see [`FIRST_SESSION`].
const SECOND_SESSION: u32 = 42;

/// The account that owns everything these tests make.
fn owner() -> Principal {
  Principal::Uid {
    host: slates_db::HostId(0),
    uid: 1234,
  }
}

/// The wire's form of the owner, as `SetIdentities` names it.
fn wire_owner() -> slates_ipc::protocol::Principal {
  slates_ipc::protocol::Principal::Uid { host: 0, uid: 1234 }
}

/// An agent of the owner's account, enrolled on partition 0 (its id's high bits name it, `verbs::owner_of_consumer`).
fn agent() -> Principal {
  Principal::Consumer {
    host: slates_db::HostId(0),
    account: 1234,
    consumer: 7,
  }
}

/// `body` asked of `state` as the owner.
fn ask(state: &mut ShardState, body: RequestBody) -> ReplyBody {
  dispatch(state, 1, &owner(), body)
}

/// The green `made` creates.
fn created(state: &mut ShardState, made: RequestBody) -> VolumeId {
  let ReplyBody::GreenCreated { id } = ask(state, made) else {
    panic!("a green");
  };
  id
}

/// A work named after `name` over `green`, holding one edit.
fn edited_work(state: &mut ShardState, green: VolumeId, name: &str) -> VolumeId {
  let ReplyBody::WorkCreated { id: work, .. } = ask(
    state,
    RequestBody::CreateWork {
      green,
      name: format!("{name}-work"),
    },
  ) else {
    panic!("a work");
  };
  let edited = ask(
    state,
    RequestBody::Edit {
      work,
      path: "/f".to_owned(),
      at: 0,
      delete_len: 0,
      bytes: b"hello".to_vec(),
    },
  );
  assert!(matches!(edited, ReplyBody::Edited), "{edited:?}");
  work
}

/// A work holding one edit over the root green of a new `tool` repository named `name`.
fn repository_work(state: &mut ShardState, name: &str, tool: VcsTool) -> VolumeId {
  let default_branch = match tool {
    VcsTool::Subversion => "trunk",
    VcsTool::Git | VcsTool::Mercurial => "main",
  };
  let green = created(
    state,
    RequestBody::CreateRepository {
      name: name.to_owned(),
      tool,
      format: ObjectFormat::Sha1,
      default_branch: default_branch.to_owned(),
      green: None,
    },
  );
  edited_work(state, green, name)
}

/// A work holding one edit over a plain green, no repository's.
fn plain_work(state: &mut ShardState) -> VolumeId {
  let green = created(
    state,
    RequestBody::CreateGreen {
      name: "plain".to_owned(),
      require_evidence: false,
      base: None,
    },
  );
  edited_work(state, green, "plain")
}

/// A submit of `work` carrying `message` and `identities`, as the channel fills them.
fn submit(work: VolumeId, message: Option<&str>, identities: ToolIdentities) -> RequestBody {
  let mut body = RequestBody::submit(work, Vec::new());
  if let Some(message) = message {
    body = body.with_message(message.to_owned());
  }
  if let Some(authorship) = body.authorship_mut() {
    authorship.identities = identities;
  }
  body
}

/// git's identity named `name`, and none in Mercurial.
fn git_only(name: &str) -> ToolIdentities {
  ToolIdentities {
    git: Some(GitIdentity {
      name: name.to_owned(),
      email: "ada@example.com".to_owned(),
    }),
    mercurial: None,
    subversion: None,
  }
}

/// Mercurial's user named `name`, and none in git.
fn mercurial_only(name: &str) -> ToolIdentities {
  ToolIdentities {
    git: None,
    mercurial: Some(format!("{name} <ada@example.com>")),
    subversion: None,
  }
}

/// An identity named `name` in git and in Mercurial.
fn both(name: &str) -> ToolIdentities {
  ToolIdentities {
    git: git_only(name).git,
    mercurial: mercurial_only(name).mercurial,
    subversion: None,
  }
}

/// The refusal `reply` is, if it is one.
fn refusal(reply: ReplyBody) -> Option<Refusal> {
  match reply {
    ReplyBody::Refused { refusal } => Some(refusal),
    _ => None,
  }
}

/// The version `reply` accepted, if it accepted one.
fn accepted_version(reply: &ReplyBody) -> Option<u64> {
  match reply {
    ReplyBody::Submitted { version, .. } => *version,
    _ => None,
  }
}

/// A work over a new `tool` repository's root green named `name`, submitted with identities in both tools and no
/// message, then an empty one, then one: the two refusals, and the version the last accepted.
fn answered_by_message(
  state: &mut ShardState,
  (name, tool): (&str, VcsTool),
) -> (Option<Refusal>, Option<Refusal>, Option<u64>) {
  let work = repository_work(state, name, tool);
  let [none, empty, given] = [None, Some(""), Some("the change")]
    .map(|message| ask(state, submit(work, message, both("Ada"))));
  (refusal(none), refusal(empty), accepted_version(&given))
}

/// A-664 part 1 (§4.17.1 "A push is a work" item 4: "A submit to a green of a repository without a message ... is
/// refused typed ... before any work is built"). Do: in a git, a Mercurial and a Subversion repository, edit a work over
/// the root green and submit it with identities in both tools and no message, then with an empty message, then with
/// one. Expect: `MessageRequired` twice, then accepted at version 1 in each, so neither refusal built or committed
/// anything of the work.
#[test]
fn t_a664_a_repositorys_green_refuses_a_submit_without_a_message_before_any_work_is_built() {
  let answered = crate::daemon::audit_on_shard(|state| {
    [
      ("git", VcsTool::Git),
      ("hg", VcsTool::Mercurial),
      ("svn", VcsTool::Subversion),
    ]
    .map(|repository| answered_by_message(state, repository))
  });
  let refused = Some(Refusal::MessageRequired);
  let each = (refused.clone(), refused, Some(1));
  assert_eq!(answered, [each.clone(), each.clone(), each]);
}

/// A work over a new `tool` repository's root green named `name`, submitted with a message and `first`'s identities,
/// then with `then`'s: the first's refusal, and the version the second accepted.
fn answered_by_identity(
  state: &mut ShardState,
  (name, tool): (&str, VcsTool),
  (first, then): (ToolIdentities, ToolIdentities),
) -> (Option<Refusal>, Option<u64>) {
  let work = repository_work(state, name, tool);
  let [first, then] =
    [first, then].map(|identities| ask(state, submit(work, Some("the change"), identities)));
  (refusal(first), accepted_version(&then))
}

/// A-664 part 1 (§4.17.1 item 4: "... or by a principal with no such identity, is refused typed"). Do: in a git and a
/// Mercurial repository, submit an edited work with a message and an identity in the other tool alone, then with one in
/// its own; in a Subversion repository, with a message and no identity at all. Expect: `IdentityRequired` naming git,
/// and Mercurial, each then accepted at version 1; Subversion's accepted at version 1 at once, a revision's author being
/// the principal's own name.
#[test]
fn t_a664_a_repositorys_green_refuses_a_submit_with_no_identity_in_its_tool() {
  let answered = crate::daemon::audit_on_shard(|state| {
    let git = (mercurial_only("Ada"), git_only("Ada"));
    let mercurial = (git_only("Ada"), mercurial_only("Ada"));
    let svn = repository_work(state, "svn", VcsTool::Subversion);
    let svn = submit(svn, Some("the change"), ToolIdentities::default());
    (
      answered_by_identity(state, ("git", VcsTool::Git), git),
      answered_by_identity(state, ("hg", VcsTool::Mercurial), mercurial),
      accepted_version(&ask(state, svn)),
    )
  });
  let required = |tool| Some(Refusal::IdentityRequired { tool });
  let expected = (
    (required(VcsTool::Git), Some(1)),
    (required(VcsTool::Mercurial), Some(1)),
    Some(1),
  );
  assert_eq!(answered, expected);
}

/// A-664 part 1: a plain green makes no tool's commit. Do: submit a work over a plain green with no message and no
/// identity. Expect: accepted at version 1.
#[test]
fn t_a664_a_plain_green_takes_a_submit_with_neither() {
  let answered = crate::daemon::audit_on_shard(|state| {
    let work = plain_work(state);
    accepted_version(&ask(state, submit(work, None, ToolIdentities::default())))
  });
  assert_eq!(answered, Some(1));
}

/// The launcher's proof over the owner's `identities` from version 0.
fn proven(state: &ShardState, identities: &ToolIdentities) -> [u8; 32] {
  crate::landing::identities_proof(&state.issuer_secret, &wire_owner(), identities, 0)
}

/// What setting the owner's `identities` from version 0 as `caller` is answered at once, proven by `proof` or, without
/// one, by the launcher's: its refusal, or `None` when it went on to record them.
fn set_at_once(
  state: &mut ShardState,
  caller: &Principal,
  identities: ToolIdentities,
  proof: Option<[u8; 32]>,
) -> Option<Refusal> {
  let proof = proof.unwrap_or_else(|| proven(state, &identities));
  match set_on_channel(state, (0, 0), caller, (wire_owner(), identities, 0), proof) {
    Served::Reply(reply) => refusal(reply),
    Served::Forwarded => None,
  }
}

/// A-664 part 1 (§4.13: a principal's identities are its owner's to set, on the control channel; A-285's channel rule).
/// Do: set the owner's identities from an agent's own channel, with the launcher's proof. Expect: `Forbidden`.
#[test]
fn t_a664_an_agent_never_sets_identities_even_with_the_launchers_proof() {
  let answer =
    crate::daemon::audit_on_shard(|state| set_at_once(state, &agent(), both("Ada"), None));
  assert!(
    matches!(answer, Some(Refusal::Forbidden { .. })),
    "{answer:?}"
  );
}

/// A-664 part 1 (A-285's proof). Do: set the owner's identities from the owner's own channel with a proof no launcher's
/// secret made. Expect: `GrantIssuerUnverified`.
#[test]
fn t_a664_identities_set_without_the_launchers_proof_are_refused() {
  let answer =
    crate::daemon::audit_on_shard(|state| set_at_once(state, &owner(), both("Ada"), Some([0; 32])));
  assert_eq!(answer, Some(Refusal::GrantIssuerUnverified));
}

/// Whether `answer` is a `BadRequest` whose reason names `part`.
fn refused_naming(answer: Option<&Refusal>, part: &str) -> bool {
  matches!(answer, Some(Refusal::BadRequest { reason }) if reason.starts_with(part))
}

/// A-664 part 1 (git's own rules, `slates_git::derived::Identity`: no byte git would strip from either end). Do: set a
/// git name with a leading space, with the launcher's proof. Expect: `BadRequest` naming git's identity.
#[test]
fn t_a664_a_git_identity_git_would_change_is_refused() {
  let answer =
    crate::daemon::audit_on_shard(|state| set_at_once(state, &owner(), both(" Ada"), None));
  assert!(
    refused_naming(answer.as_ref(), "git identity"),
    "{answer:?}"
  );
}

/// A-664 part 1 (§4.17.3 decision 7: a changeset's user is the principal's `Name <address>`; Mercurial 7.2's
/// `changelog.add` strips the user, then refuses an empty one or one holding a newline). Do: set a Mercurial user of
/// two lines, an empty one, one beginning with a space and one ending with a vertical tab, each with the launcher's
/// proof. Expect: each `BadRequest` naming Mercurial's user, so no record holds a user Mercurial would refuse or change.
#[test]
fn t_a664_a_mercurial_user_mercurial_would_refuse_or_change_is_refused() {
  let answers = crate::daemon::audit_on_shard(|state| {
    [
      "Ada\n<ada@example.com>",
      "",
      " Ada <ada@example.com>",
      "Ada <ada@example.com>\x0b",
    ]
    .map(|user| {
      let identities = ToolIdentities {
        mercurial: Some(user.to_owned()),
        ..git_only("Ada")
      };
      set_at_once(state, &owner(), identities, None)
    })
  });
  assert!(
    answers
      .iter()
      .all(|answer| refused_naming(answer.as_ref(), "Mercurial user")),
    "{answers:?}"
  );
}

/// A-664 part 1 (A-622's version rule). Do: record the owner's identities from version 0, then others from version 0
/// again. Expect: the first answered `IdentitiesSet { version: 1 }` and kept; the second refused
/// `PolicyChanged { current: 1 }`, the record unchanged.
#[test]
fn t_a664_identities_are_recorded_as_the_version_after_the_one_read() {
  let answered = crate::daemon::audit_on_shard(|state| {
    let first = record(state, owner(), both("Ada"), 0);
    let stale = record(state, owner(), both("Eve"), 0);
    let kept = state
      .db
      .partition()
      .identities(&owner())
      .and_then(|record| record.git.clone())
      .map(|git| git.name);
    (first, stale, kept)
  });
  let stale = ReplyBody::Refused {
    refusal: Refusal::PolicyChanged { current: 1 },
  };
  let expected = (
    ReplyBody::IdentitiesSet { version: 1 },
    stale,
    Some("Ada".to_owned()),
  );
  assert_eq!(answered, expected);
}

/// Seats client `client_id` as `principal` on `state`'s shard as an admission does, over a region made here in the
/// shard's geometry: its seat, and the client's own mapping of the region, which the test reads its replies from.
fn seat(
  state: &mut ShardState,
  client_id: u32,
  principal: Principal,
) -> (Handle<ClientSlot>, slates_ipc::ClientRegion) {
  static REGIONS: AtomicU32 = AtomicU32::new(0);
  let name = format!(
    "slates-cr-identities-{}-{}",
    std::process::id(),
    REGIONS.fetch_add(1, Ordering::Relaxed)
  );
  #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
  let mut region =
    slates_ipc::ClientRegion::create(&name, client_id, state.shard, state.config.region).unwrap();
  #[cfg(target_os = "macos")]
  region.unlink_name().unwrap();
  #[cfg(unix)]
  let client = region.open_again().unwrap();
  #[cfg(windows)]
  let client = {
    let (handoff, len) = region.handoff().unwrap();
    slates_ipc::ClientRegion::open(&handoff, len).unwrap()
  };
  let seat = state
    .clients
    .insert(ClientSlot {
      status_pages: crate::status_pages::StatusPages::default(),
      end: slates_ipc::DaemonEnd::new(region),
      principal,
      client_id,
      pid: 0,
      retiring: false,
      watch: None,
      proven: true,
      control: None,
      revoked: false,
      consumer_policy: None,
      identities: None,
      owner_route: None,
      read_ahead: None,
      read_ahead_pending: 0,
      remote_volumes: Vec::new(),
      maps_sent: std::collections::BTreeSet::new(),
    })
    .unwrap();
  (seat, client)
}

/// The seat at `index` on `state`'s shard.
fn seat_at(state: &ShardState, index: u32) -> Option<Handle<ClientSlot>> {
  let generation = state.clients.generation_at(index)?;
  Some(Handle::from_raw(index, generation))
}

/// Sends `body` as request `sequence` of client `client` seated at `seat`, as `principal`, through the session's
/// identities as `verbs::serve` does once the channel's gates passed: whether it went on behind the session's read,
/// and the body when it is to be routed at once.
fn sent(
  state: &mut ShardState,
  (seat, client): (Handle<ClientSlot>, u32),
  (sequence, principal): (u32, &Principal),
  body: RequestBody,
) -> (bool, Option<RequestBody>) {
  let id = RequestId { client, sequence };
  let key = (state.origin_anchor.0, id, client);
  match with_session(state, seat, (id.word(), key), principal, body) {
    ControlFlow::Continue(body) => (false, Some(body)),
    ControlFlow::Break(served) => (matches!(served, Served::Forwarded), None),
  }
}

/// The next answer `end`'s client reads, waited for within the observation budget: its request's sequence, and the
/// reply.
fn next_answer(end: &mut slates_ipc::ClientEnd) -> Option<(u32, ReplyBody)> {
  let reply = end.wait(Some(crate::daemon::OBSERVE_BUDGET_NS)).ok()?;
  let body: ReplyBody =
    slates_ipc::protocol::unpack(end.region(), reply.kind, &reply.payload).ok()?;
  Some((RequestId::from_word(reply.request).sequence, body))
}

/// A-664 part 1 (main's ruling, 2026-11-19: a session's identities are read once, at its first submit, kept with the
/// session and written over whatever its client sent; a setting made mid-session applies from the next session). Do:
/// with no identities recorded for the owner, a session of the owner submits a work over a git repository's green
/// claiming git's identity "Mallory"; the owner's launcher then sets the owner's identities on that session's channel,
/// and the session submits again, claiming again; a second session submits the work. Expect: the first submit waits for
/// its session's read and is refused `IdentityRequired` (the claim was written over with the session's none); the
/// setting answered `IdentitiesSet` at version 1; the session's second submit carries its kept none at once, neither
/// the claim nor the new setting, with no read; the second session's read finds the setting and its submit is accepted
/// at version 1; two reads counted in all.
#[test]
fn t_a664_a_session_reads_its_identities_once_and_its_clients_claim_never_reaches_the_green() {
  let daemon = crate::daemon::audit_daemon();
  let (work, index, first_behind, first) = crate::daemon::observe_first(&daemon, |state| {
    let work = repository_work(state, "agents", VcsTool::Git);
    let (seat, region) = seat(state, FIRST_SESSION, owner());
    let claimed = submit(work, Some("the change"), git_only("Mallory"));
    let (behind, _) = sent(state, (seat, FIRST_SESSION), (1, &owner()), claimed);
    (work, seat.index(), behind, region)
  });
  let mut first = slates_ipc::ClientEnd::new(first);
  let claim_answered = next_answer(&mut first).map(|(sequence, reply)| (sequence, refusal(reply)));
  let (setting, carried, again_behind) = crate::daemon::observe_first(&daemon, move |state| {
    let Some(seat) = seat_at(state, index) else {
      return (false, None, true);
    };
    let identities = both("Ada");
    let proof = proven(state, &identities);
    let setter = (
      index,
      RequestId {
        client: FIRST_SESSION,
        sequence: 2,
      }
      .word(),
    );
    let set = set_on_channel(
      state,
      setter,
      &owner(),
      (wire_owner(), identities, 0),
      proof,
    );
    let again = submit(work, Some("the change"), git_only("Mallory"));
    let (behind, body) = sent(state, (seat, FIRST_SESSION), (3, &owner()), again);
    let carried = body.and_then(|body| body.authorship().map(|kept| kept.identities.clone()));
    (matches!(set, Served::Forwarded), carried, behind)
  });
  let set_answered = next_answer(&mut first);
  let (second_behind, second) = crate::daemon::observe_first(&daemon, move |state| {
    let (seat, region) = seat(state, SECOND_SESSION, owner());
    let next = submit(work, Some("the change"), ToolIdentities::default());
    let (behind, _) = sent(state, (seat, SECOND_SESSION), (1, &owner()), next);
    (behind, region)
  });
  let mut second = slates_ipc::ClientEnd::new(second);
  let second_answered =
    next_answer(&mut second).map(|(sequence, reply)| (sequence, accepted_version(&reply)));
  let reads = crate::daemon::observe_first(&daemon, |state| state.refusals.get(READ).copied());
  daemon.stop();
  let observed = (
    (first_behind, claim_answered),
    (setting, set_answered, carried, again_behind),
    (second_behind, second_answered, reads),
  );
  let expected = (
    (
      true,
      Some((1, Some(Refusal::IdentityRequired { tool: VcsTool::Git }))),
    ),
    (
      true,
      Some((2, ReplyBody::IdentitiesSet { version: 1 })),
      Some(ToolIdentities {
        subversion: Some(crate::lfs::owner_name(&owner())),
        ..ToolIdentities::default()
      }),
      false,
    ),
    (true, Some((1, Some(1))), Some(2)),
  );
  assert_eq!(observed, expected);
}

/// A-664 part 1 with §4.13's revocation: what `verbs::serve` refuses before it routes holds for a submit that waited for
/// its session's read. Do: a session submits a work over a plain green, and its channel is revoked (as a consumer's
/// revocation marks each of its seats) while the read is away; then the owner submits the work through `dispatch`.
/// Expect: the session's submit refused `ConsumerRevoked`; the owner's accepted at version 1, so the refused one built
/// nothing.
#[test]
fn t_a664_a_submit_waiting_for_its_sessions_read_is_refused_when_its_channel_is_revoked_meanwhile()
{
  let daemon = crate::daemon::audit_daemon();
  let (behind, region, work) = crate::daemon::observe_first(&daemon, |state| {
    let work = plain_work(state);
    let (seat, region) = seat(state, FIRST_SESSION, owner());
    let body = submit(work, None, ToolIdentities::default());
    let (behind, _) = sent(state, (seat, FIRST_SESSION), (1, &owner()), body);
    if let Ok(slot) = state.clients.get_mut(seat) {
      slot.revoked = true;
    }
    (behind, region, work)
  });
  let mut end = slates_ipc::ClientEnd::new(region);
  let answered = next_answer(&mut end).map(|(sequence, reply)| (sequence, refusal(reply)));
  let after = crate::daemon::observe_first(&daemon, move |state| {
    accepted_version(&ask(state, submit(work, None, ToolIdentities::default())))
  });
  daemon.stop();
  let expected = (true, Some((1, Some(Refusal::ConsumerRevoked))), Some(1));
  assert_eq!((behind, answered, after), expected);
}

/// A-664 part 1 (module docs: a session keeps the first read to return, so all its submits carry one set). Do: a
/// session keeps no identity in any tool, its first submit's read; a submit it sent before that read returned comes
/// back from its own read with git's identity, to a git repository's green. Expect: the submit routed with the
/// session's kept none and refused `IdentityRequired`; the session still keeps none.
#[test]
fn t_a664_a_later_read_never_replaces_the_identities_a_session_keeps() {
  let answered = crate::daemon::audit_on_shard(|state| {
    let work = repository_work(state, "agents", VcsTool::Git);
    let (seat, _client) = seat(state, FIRST_SESSION, owner());
    if let Ok(slot) = state.clients.get_mut(seat) {
      slot.identities = Some(ToolIdentities::default());
    }
    let id = RequestId {
      client: FIRST_SESSION,
      sequence: 2,
    };
    let resumed = Resumed {
      client_index: seat.index(),
      request: id.word(),
      key: (state.origin_anchor.0, id, FIRST_SESSION),
      principal: owner(),
    };
    let body = submit(work, Some("the change"), ToolIdentities::default());
    let answer = match resumed.route(state, body, git_only("Ada")) {
      Served::Reply(reply) => refusal(reply),
      Served::Forwarded => None,
    };
    let kept = state
      .clients
      .get(seat)
      .ok()
      .and_then(|slot| slot.identities.clone());
    (answer, kept)
  });
  let refused = Some(Refusal::IdentityRequired { tool: VcsTool::Git });
  assert_eq!(answered, (refused, Some(ToolIdentities::default())));
}

/// A read of git's identity returning for [`FIRST_SESSION`]'s submit of an edited work over a new git repository named
/// `name` to the seat `seat` is now: what the submit is answered and what the seat then keeps; then the version the
/// owner's own submit of the work is accepted at, which says whether the returning submit built anything.
fn returned_to(
  state: &mut ShardState,
  (name, seat): (&str, Handle<ClientSlot>),
) -> (Option<Refusal>, Option<ToolIdentities>, Option<u64>) {
  let work = repository_work(state, name, VcsTool::Git);
  let id = RequestId {
    client: FIRST_SESSION,
    sequence: 1,
  };
  let resumed = Resumed {
    client_index: seat.index(),
    request: id.word(),
    key: (state.origin_anchor.0, id, FIRST_SESSION),
    principal: owner(),
  };
  let body = submit(work, Some("the change"), ToolIdentities::default());
  let answer = match resumed.route(state, body, git_only("Ada")) {
    Served::Reply(reply) => refusal(reply),
    Served::Forwarded => None,
  };
  let kept = state
    .clients
    .get(seat)
    .ok()
    .and_then(|slot| slot.identities.clone());
  let owners = ask(state, submit(work, Some("the change"), git_only("Ada")));
  (answer, kept, accepted_version(&owners))
}

/// A-664 part 1 (module docs: a read is kept with the session that asked, while its seat is still that session's). Do:
/// a read of git's identity returns for a session's submit over a git repository's green, once to the session's seat
/// as another session, an agent's, now holds it, and once to the session's own seat while it retires; then the owner
/// submits each work itself. Expect: each returning submit refused `NotFound`, neither seat keeping an identity, no
/// read counted; each owner's submit accepted at version 1, so neither returning submit built anything.
#[test]
fn t_a664_a_read_returning_to_a_seat_its_session_left_is_kept_by_no_one_and_runs_nothing() {
  let answered = crate::daemon::audit_on_shard(|state| {
    let (taken, _taken_region) = seat(state, SECOND_SESSION, agent());
    let (retiring, _retiring_region) = seat(state, FIRST_SESSION, owner());
    if let Ok(slot) = state.clients.get_mut(retiring) {
      slot.retiring = true;
    }
    let returned = [("taken", taken), ("retiring", retiring)]
      .map(|(name, seat)| returned_to(state, (name, seat)));
    (returned, state.refusals.get(READ).copied())
  });
  let each = (Some(Refusal::NotFound), None, Some(1));
  assert_eq!(answered, ([each.clone(), each], None));
}

/// A-664 part 1 (module docs; A-563: a verb that never ran answers `Overloaded` and is never recorded, so a resend under
/// its id runs it). Do: on a two-shard daemon whose control shard is fenced, so it serves nothing, a session seated on
/// the other shard as an agent whose record the fenced shard keeps submits. Expect: the submit waits for its read, is
/// refused `Overloaded` naming the fenced shard, and no completion is recorded under its id.
#[test]
fn t_a664_a_submit_whose_sessions_read_is_never_answered_is_refused_overloaded_unrecorded() {
  let daemon = crate::daemon::audit_daemon_of(2);
  let fenced = daemon.inject_consensus_failure().is_ok();
  let (control, behind, region) = crate::daemon::observe_at(&daemon, 1, |state| {
    let (seat, region) = seat(state, FIRST_SESSION, agent());
    let unread = VolumeId { bytes: [0; 16] };
    let body = submit(unread, Some("the change"), ToolIdentities::default());
    let (behind, _) = sent(state, (seat, FIRST_SESSION), (1, &agent()), body);
    (state.shards.first().copied(), behind, region)
  });
  let mut end = slates_ipc::ClientEnd::new(region);
  let answered = next_answer(&mut end).map(|(sequence, reply)| (sequence, refusal(reply)));
  let recorded = crate::daemon::observe_at(&daemon, 1, |state| {
    let completion = state
      .db
      .partition()
      .completion(state.origin_anchor.0, FIRST_SESSION, 1);
    !matches!(completion, Seen::New)
  });
  daemon.stop();
  let overloaded = control.map(|shard| Refusal::Overloaded { shard });
  assert_eq!(
    (fenced, behind, answered, recorded),
    (true, true, Some((1, overloaded)), false)
  );
}
