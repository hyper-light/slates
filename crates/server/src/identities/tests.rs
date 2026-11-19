//! A-664 part 1's server half: a repository's green refuses a submit without a message or without an identity in its
//! tool, before any work is built, and a plain green does not; the identities are set by their owner alone, held to
//! their tools' rules, from the version read.

use slates_db::catalog::Principal;
use slates_ipc::protocol::{
  GitIdentity, ObjectFormat, Refusal, ReplyBody, RequestBody, ToolIdentities, VcsTool, VolumeId,
};

use super::{record, set_on_channel};
use crate::state::ShardState;
use crate::verbs::{Served, dispatch};

/// The account that owns everything these tests make.
fn owner() -> Principal {
  Principal::Uid {
    host: slates_db::HostId(0),
    uid: 1234,
  }
}

/// `body` asked of `state` as the owner.
fn ask(state: &mut ShardState, body: RequestBody) -> ReplyBody {
  dispatch(state, 1, &owner(), body)
}

/// A work over a new green of a git repository (`repository`) or, with `None`, over a plain green, holding one edit.
fn edited_work(state: &mut ShardState, repository: Option<&str>) -> VolumeId {
  let made = match repository {
    Some(name) => RequestBody::CreateRepository {
      name: name.to_owned(),
      tool: VcsTool::Git,
      format: ObjectFormat::Sha1,
      default_branch: "main".to_owned(),
      green: None,
    },
    None => RequestBody::CreateGreen {
      name: "plain".to_owned(),
      require_evidence: false,
      base: None,
    },
  };
  let ReplyBody::GreenCreated { id: green } = ask(state, made) else {
    panic!("a green");
  };
  let ReplyBody::WorkCreated { id: work, .. } = ask(
    state,
    RequestBody::CreateWork {
      green,
      name: "agent-work".to_owned(),
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

/// A submit of `work` carrying `message`, and git's identity when `identified`, as the channel fills it.
fn submit(work: VolumeId, message: Option<&str>, identified: bool) -> RequestBody {
  let mut body = RequestBody::submit(work, Vec::new());
  if let Some(message) = message {
    body = body.with_message(message.to_owned());
  }
  if identified && let Some(authorship) = body.authorship_mut() {
    authorship.identities.git = Some(GitIdentity {
      name: "A U Thor".to_owned(),
      email: "author@example.com".to_owned(),
    });
  }
  body
}

/// `reply` is the version `version` accepted.
fn accepted_at(reply: &ReplyBody, version: u64) {
  assert!(
    matches!(reply, ReplyBody::Submitted { version: Some(v), .. } if *v == version),
    "{reply:?}"
  );
}

/// A-664 part 1 (§4.17.1 "A push is a work" item 4: "A submit to a green of a repository without a message, or by a
/// principal with no such identity, is refused typed ... before any work is built"). Do: edit a work over a git
/// repository's root green and submit it with no message, then with a message and no identity, then with both. Expect:
/// `MessageRequired`, then `IdentityRequired { tool: Git }`, then accepted at version 1, so neither refusal built or
/// committed anything of the work.
#[test]
fn t_a664_a_repositorys_green_refuses_a_submit_without_a_message_or_an_identity_before_any_work_is_built()
 {
  crate::daemon::audit_on_shard(|state| {
    let work = edited_work(state, Some("agents"));
    let refused = |reply: ReplyBody| match reply {
      ReplyBody::Refused { refusal } => refusal,
      other => panic!("not refused: {other:?}"),
    };
    assert_eq!(
      refused(ask(state, submit(work, None, true))),
      Refusal::MessageRequired
    );
    assert_eq!(
      refused(ask(state, submit(work, Some("the change"), false))),
      Refusal::IdentityRequired { tool: VcsTool::Git }
    );
    accepted_at(&ask(state, submit(work, Some("the change"), true)), 1);
  });
}

/// A-664 part 1: a plain green makes no tool's commit. Do: submit a work over a plain green with no message and no
/// identity. Expect: accepted at version 1.
#[test]
fn t_a664_a_plain_green_takes_a_submit_with_neither() {
  crate::daemon::audit_on_shard(|state| {
    let work = edited_work(state, None);
    accepted_at(&ask(state, submit(work, None, false)), 1);
  });
}

/// The wire's form of the owner, as `SetIdentities` names it.
fn wire_owner() -> slates_ipc::protocol::Principal {
  slates_ipc::protocol::Principal::Uid { host: 0, uid: 1234 }
}

/// git's identity of `name`.
fn git(name: &str) -> ToolIdentities {
  ToolIdentities {
    git: Some(GitIdentity {
      name: name.to_owned(),
      email: "ada@example.com".to_owned(),
    }),
    mercurial: Some("Ada <ada@example.com>".to_owned()),
  }
}

/// What `set_on_channel` answered at once: a refusal, or `None` when it went on as a task.
fn answered_at_once(served: Served) -> Option<Refusal> {
  match served {
    Served::Reply(ReplyBody::Refused { refusal }) => Some(refusal),
    Served::Reply(other) => panic!("not a refusal: {other:?}"),
    Served::Forwarded => None,
  }
}

/// A-664 part 1 (§4.13: set by the principal's owner on the control channel; A-285's channel rule). Do: set an
/// identity from an agent's own channel with the launcher's proof; from the owner's channel with a wrong proof; with the
/// proof over a git name git would change, and over a Mercurial user of two lines. Expect: `Forbidden`, then
/// `GrantIssuerUnverified`, then `BadRequest` twice, each answered at once with no record written.
#[test]
fn t_a664_identities_are_set_by_their_owner_alone_and_held_to_their_tools_rules() {
  crate::daemon::audit_on_shard(|state| {
    let agent = Principal::Consumer {
      host: slates_db::HostId(0),
      account: 1234,
      consumer: 7,
    };
    let proven = |state: &ShardState, identities: &ToolIdentities| {
      crate::landing::identities_proof(&state.issuer_secret, &wire_owner(), identities, 0)
    };
    let set = |state: &mut ShardState, caller: &Principal, identities: ToolIdentities, proof| {
      answered_at_once(set_on_channel(
        state,
        (0, 0),
        caller,
        (wire_owner(), identities, 0),
        proof,
      ))
    };
    let proof = proven(state, &git("Ada"));
    assert!(matches!(
      set(state, &agent, git("Ada"), proof),
      Some(Refusal::Forbidden { .. })
    ));
    let wrong = set(state, &owner(), git("Ada"), [0; 32]);
    assert_eq!(wrong, Some(Refusal::GrantIssuerUnverified));
    let crud = git(" Ada");
    let proof = proven(state, &crud);
    assert!(matches!(
      set(state, &owner(), crud, proof),
      Some(Refusal::BadRequest { .. })
    ));
    let mut two_lines = git("Ada");
    two_lines.mercurial = Some("Ada\n<ada@example.com>".to_owned());
    let proof = proven(state, &two_lines);
    assert!(matches!(
      set(state, &owner(), two_lines, proof),
      Some(Refusal::BadRequest { .. })
    ));
    assert!(state.db.partition().identities(&owner()).is_none());
  });
}

/// A-664 part 1 (A-622's version rule). Do: record the owner's identities from version 0, then again from version 0.
/// Expect: the first answered `IdentitiesSet { version: 1 }` and held by the partition; the second refused
/// `PolicyChanged { current: 1 }`, the record unchanged.
#[test]
fn t_a664_identities_are_recorded_as_the_version_after_the_one_read() {
  crate::daemon::audit_on_shard(|state| {
    let first = record(state, owner(), git("Ada"), 0);
    assert!(
      matches!(first, ReplyBody::IdentitiesSet { version: 1 }),
      "{first:?}"
    );
    let stale = record(state, owner(), git("Eve"), 0);
    let current = Refusal::PolicyChanged { current: 1 };
    assert!(
      matches!(stale, ReplyBody::Refused { refusal } if refusal == current),
      "{stale:?}"
    );
    let held = state
      .db
      .partition()
      .identities(&owner())
      .map(|record| record.git.clone());
    let name = held.flatten().map(|git| git.name);
    assert_eq!(name.as_deref(), Some("Ada"));
  });
}
