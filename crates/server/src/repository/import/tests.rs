//! A-678's tests: the import verb's refusals by kind, its held reply and a resend taking the route over, the shard's task
//! answering the request at the import's end, and each import failure's refusal on the wire.

use std::time::{Duration, Instant};

use slates_db::catalog::{Principal, RepositoryId};
use slates_hg::PastAdmission;
use slates_hg::import::ImportRefused;
use slates_ipc::protocol::{
  ImportFault, ObjectFormat as WireFormat, Refusal, ReplyBody, RequestBody, VcsTool,
};
use slates_vfs::host::{FetchId, HostError};
use slates_wire::request::RequestId;

use super::{ANSWERED, HELD, Opened, REJOINED, import_with, refusal_of, reply_of};
use crate::hg_base::BaseRefused;
use crate::hg_base::tests::{STATES, laid_out};
use crate::merge_service::ReplyRoute;
use crate::state::ShardState;
use crate::vcs_store::hg_import::{Finished, ImportFailed};
use crate::verbs::dispatch;

/// Shape: the origin the tests' requests come from, any number.
const ORIGIN: u64 = 7;

/// Shape: a client slot no client holds, so a reply sent to it is counted undeliverable rather than written.
const NO_CLIENT: u32 = 4096;

/// Shape: the room the mapping test's imports may hold, any number.
const ROOM: u64 = 1 << 20;

/// Shape: the directory the tests name, which only the simulated host opens.
const PATH: &str = "/repository";

/// Shape: how long a test waits for the shard's task to answer: far past a debug build's import of the fixture
/// (milliseconds), so only a task that never answers reaches it.
const TASK_WAIT: Duration = Duration::from_secs(60);

/// The principal that creates the tests' repositories.
fn owner() -> Principal {
  Principal::Uid {
    host: slates_db::HostId(0),
    uid: 1234,
  }
}

/// Another principal.
fn stranger() -> Principal {
  Principal::Uid {
    host: slates_db::HostId(0),
    uid: 4321,
  }
}

/// Creates repository `name` of `tool` on the shard: its id.
fn created(state: &mut ShardState, name: &str, tool: VcsTool) -> RepositoryId {
  let reply = dispatch(
    state,
    1,
    &owner(),
    RequestBody::CreateRepository {
      name: name.to_owned(),
      tool,
      format: WireFormat::Sha1,
      default_branch: "default".to_owned(),
      green: None,
    },
  );
  assert!(
    matches!(reply, ReplyBody::GreenCreated { .. }),
    "no repository: {reply:?}"
  );
  let repositories = state.db.partition().repositories();
  repositories.repository_named(name).expect("the record").id
}

/// The request `request` as the verb runner leaves it before a verb: its origin and id, and the route its reply goes by
/// to client slot `client_index` of this shard.
fn routed(state: &mut ShardState, request: u64, client_index: u32) {
  state.current_request = Some((ORIGIN, RequestId::from_word(request)));
  state.reply_route = Some(ReplyRoute {
    shard: state.shard,
    client_index,
    request,
    through_work: false,
  });
}

/// The `states` repository 7.2 kept, on a simulated host.
fn the_states_repository(_: &str) -> Result<Opened, HostError> {
  let (host, directory) = laid_out(STATES.iter().copied());
  Ok((Box::new(host), directory))
}

/// An opener a refusal must never reach.
fn never_opened(path: &str) -> Result<Opened, HostError> {
  panic!("{path} opened")
}

/// Asks the shard to import `PATH` into `name` as `principal`, the directory opened by `open`: the reply, and whether
/// the verb deferred it (taken, as the verb runner takes it).
fn ask(
  state: &mut ShardState,
  (principal, name): (&Principal, &str),
  open: fn(&str) -> Result<Opened, HostError>,
) -> (ReplyBody, bool) {
  let reply = import_with(state, principal, (name, PATH), open);
  (reply, std::mem::take(&mut state.acceptance_deferred))
}

/// `ImportRefused` naming `fault`, as a reply.
fn import_refused(fault: ImportFault) -> ReplyBody {
  ReplyBody::Refused {
    refusal: Refusal::ImportRefused { fault },
  }
}

/// Counter `name` on the shard.
fn count(state: &ShardState, name: &str) -> u64 {
  state.refusals.get(name).copied().unwrap_or(0)
}

/// A-678 (refused by kind, before anything is opened): do ask for an import into a repository nobody made, into the
/// owner's repository as another principal, into a git repository, into the owner's Mercurial repository with no
/// request running (directly and through the shard's dispatch) and with no route, and from a directory the host does
/// not find. Expect `NotFound`, `Forbidden`, `ImportRefused { NotMercurial }`, `ImportRefused { Unawaited }` three
/// times, and the host's refusal naming the directory; none deferred, and the directory opened by none but the last.
#[test]
fn t_a678_the_verb_refuses_by_kind_before_anything_is_opened() {
  crate::daemon::audit_on_shard(|state| {
    created(state, "hg", VcsTool::Mercurial);
    created(state, "git", VcsTool::Git);
    let own = owner();
    let refusals = [
      (ask(state, (&own, "none"), never_opened), "NotFound"),
      (ask(state, (&stranger(), "hg"), never_opened), "Forbidden"),
      (ask(state, (&own, "git"), never_opened), "NotMercurial"),
      (
        ask(state, (&own, "hg"), never_opened),
        "Unawaited (no request)",
      ),
    ];
    let expected = [
      ReplyBody::Refused {
        refusal: Refusal::NotFound,
      },
      crate::verbs::forbidden("import_repository"),
      import_refused(ImportFault::NotMercurial),
      import_refused(ImportFault::Unawaited),
    ];
    for (((reply, deferred), case), wanted) in refusals.into_iter().zip(expected) {
      assert_eq!((reply, deferred), (wanted, false), "{case}");
    }
    let through_dispatch = RequestBody::ImportRepository {
      repository: "hg".to_owned(),
      path: PATH.to_owned(),
    };
    let dispatched = dispatch(state, 1, &own, through_dispatch);
    assert_eq!(
      dispatched,
      import_refused(ImportFault::Unawaited),
      "through dispatch"
    );
    state.current_request = Some((ORIGIN, RequestId::from_word(1)));
    assert_eq!(
      ask(state, (&own, "hg"), never_opened),
      (import_refused(ImportFault::Unawaited), false)
    );
    routed(state, 2, NO_CLIENT);
    let (unfound, deferred) = ask(state, (&own, "hg"), |_| Err(HostError::NotFound));
    let named = |refusal: &Refusal| match refusal {
      Refusal::BaseUnavailable { path, .. } | Refusal::BaseUnavailableWin32 { path, .. } => {
        path == PATH
      }
      _ => false,
    };
    assert!(
      matches!(&unfound, ReplyBody::Refused { refusal } if named(refusal)) && !deferred,
      "{unfound:?}"
    );
  });
}

/// A-678 (the reply held, a resend taking it over): do start an import of the `states` repository under request 1; send
/// request 1 again from another client slot; then send request 2. Expect the first held (deferred, counted, its route
/// the request's), the resend held in its place (deferred, its route now the new slot's, counted rejoined) without the
/// directory being opened again, and request 2 refused `UnderWay`, not deferred.
#[test]
fn t_a678_a_started_import_holds_its_reply_and_a_resend_takes_the_route_over() {
  crate::daemon::audit_on_shard(|state| {
    let id = created(state, "hg", VcsTool::Mercurial);
    let own = owner();
    routed(state, 1, NO_CLIENT);
    assert_eq!(
      ask(state, (&own, "hg"), the_states_repository),
      (ReplyBody::Acknowledged, true)
    );
    let held = |state: &ShardState| {
      state
        .vcs_stores
        .answer(id)
        .map(|held| (held.route.request, held.route.client_index))
    };
    assert_eq!((held(state), count(state, HELD)), (Some((1, NO_CLIENT)), 1));
    let other_slot = NO_CLIENT.saturating_add(1);
    routed(state, 1, other_slot);
    assert_eq!(
      ask(state, (&own, "hg"), never_opened),
      (ReplyBody::Acknowledged, true)
    );
    assert_eq!(
      (held(state), count(state, REJOINED)),
      (Some((1, other_slot)), 1)
    );
    routed(state, 2, NO_CLIENT);
    assert_eq!(
      ask(state, (&own, "hg"), never_opened),
      (import_refused(ImportFault::UnderWay), false)
    );
  });
}

/// A-678 (answered when it ends): do start an import of the `states` repository on a running shard under a request
/// whose client slot holds no client, then wait for the shard's task to end it. Expect the import ended and its request
/// answered within the wait: counted answered once, its reply sent and counted undeliverable (no client is there), the
/// held request let go, and the store holding the imported history.
#[test]
fn t_a678_the_held_request_is_answered_when_the_import_ends() {
  let daemon = crate::daemon::audit_daemon();
  let id = crate::daemon::observe_first(&daemon, |state| {
    let id = created(state, "hg", VcsTool::Mercurial);
    routed(state, 1, NO_CLIENT);
    assert_eq!(
      ask(state, (&owner(), "hg"), the_states_repository),
      (ReplyBody::Acknowledged, true)
    );
    id
  });
  let began = Instant::now();
  let seen = loop {
    let seen = crate::daemon::observe_first(&daemon, move |state| {
      let held = state.vcs_stores.answer(id).is_some() || state.vcs_stores.importing(id);
      let counted = [
        count(state, ANSWERED),
        count(state, crate::state::REPLY_UNDELIVERABLE),
      ];
      let rows = state
        .vcs_stores
        .mercurial(id)
        .map_or(0, |(_, store)| store.index.rows().rows());
      (held, counted, rows)
    });
    if !seen.0 || began.elapsed() > TASK_WAIT {
      break seen;
    }
  };
  daemon.stop();
  let (held, counted, rows) = seen;
  assert!(!held, "still held after {TASK_WAIT:?}");
  assert_eq!(counted, [1, 1]);
  assert!(rows > 0);
}

/// A-678 (each failure's refusal on the wire): do map every way an import fails or ends. Expect `ContentUnavailable` for
/// a store not held, `NotEmpty`, `UnderWay`, the budget's room, `PastRoom` with the import's room, `Pending` for a fetch
/// still pending, `Damaged` naming the path a refusal names (a name not UTF-8 read lossily), `Format` for a censored
/// revision, `Abandoned` for an import that ended or was given back, and what a finished import took.
#[test]
fn t_a678_each_import_failure_maps_to_its_refusal() {
  let refused = |refusal| ImportFailed::Refused(refusal);
  let cases: [(ImportFailed, Refusal); 9] = [
    (ImportFailed::NoStore, Refusal::ContentUnavailable),
    (
      ImportFailed::NotEmpty,
      Refusal::ImportRefused {
        fault: ImportFault::NotEmpty,
      },
    ),
    (
      ImportFailed::Busy,
      Refusal::ImportRefused {
        fault: ImportFault::UnderWay,
      },
    ),
    (
      ImportFailed::Budget { available: 7 },
      Refusal::BudgetExceeded { available: 7 },
    ),
    (
      refused(ImportRefused::PastAdmission(PastAdmission {
        asked: 9,
        left: 1,
      })),
      Refusal::ImportRefused {
        fault: ImportFault::PastRoom { room: ROOM },
      },
    ),
    (
      refused(ImportRefused::Host(BaseRefused::Host(HostError::Pending(
        FetchId(3),
      )))),
      Refusal::ImportRefused {
        fault: ImportFault::Pending,
      },
    ),
    (
      refused(ImportRefused::Host(BaseRefused::NotUtf8 {
        name: b"a\xff".to_vec().into(),
      })),
      Refusal::ImportRefused {
        fault: ImportFault::Damaged {
          path: "a\u{fffd}".to_owned(),
        },
      },
    ),
    (
      refused(ImportRefused::Missing {
        path: b"store/data/a.i".to_vec().into(),
      }),
      Refusal::ImportRefused {
        fault: ImportFault::Damaged {
          path: "store/data/a.i".to_owned(),
        },
      },
    ),
    (
      refused(ImportRefused::Censored {
        revlog: b"data/a".to_vec().into(),
        rev: 0,
      }),
      Refusal::ImportRefused {
        fault: ImportFault::Format,
      },
    ),
  ];
  for (failed, expected) in cases {
    assert_eq!(
      refusal_of(PATH, ROOM, failed.clone()),
      expected,
      "{failed:?}"
    );
  }
  let ended = reply_of(PATH, ROOM, Some(Err(refused(ImportRefused::Ended))));
  let given_back = reply_of(PATH, ROOM, None);
  assert_eq!(
    (&ended, &given_back),
    (
      &import_refused(ImportFault::Abandoned),
      &import_refused(ImportFault::Abandoned)
    )
  );
  check_imported();
}

/// Holds a finished import's reply to what it took.
fn check_imported() {
  let finished = Finished {
    changesets: 3,
    manifests: 3,
    file_logs: 2,
    file_revisions: 4,
    markers: 1,
    phase_roots: Vec::new(),
    dropped_phase_roots: 0,
    bookmarks: Vec::new(),
    dropped_bookmarks: 0,
    malformed_bookmarks: 0,
    turns: 5,
  };
  let reply = reply_of(PATH, ROOM, Some(Ok(Box::new(finished))));
  let wanted = ReplyBody::RepositoryImported {
    changesets: 3,
    manifests: 3,
    file_logs: 2,
    file_revisions: 4,
    markers: 1,
  };
  assert_eq!(reply, wanted);
}
