use slates_db::HostId;
use slates_db::catalog::{
  CredentialKind, CredentialRecord, ObjectFormat, PolicyEffect, PolicyEntry, PolicyNames,
  PolicyRight, PolicyRule, PolicyWho, Principal, RepositoryId, RepositoryPolicy, RepositoryRecord,
  VcsTool,
};
use slates_ipc::protocol::{
  CredentialMaterial, Principal as WirePrincipal, ReplyBody, RequestBody,
};
use slates_ssh::command::Command as Asked;
use slates_ssh::userauth::{Holding, KeyHolders, UserKey};

use super::{
  Dispatched, GIT_DIE_STATUS, HG_ABORT_STATUS, KEY_EXPIRED, KEY_NOT_THIS, KEY_REVOKED, KEY_UNKNOWN,
  KeyUnproven, NotYet, RecordKeys, Refused, SVNSERVE_FAILURE_STATUS, Services, SshSite, Start,
  Stepped, ToolSite, Unserved, key_holder,
};
use crate::vcs_http::git::{Located, Repositories};

/// Shape: the uid of the tests' repositories' owner.
const OWNER: u32 = 1;
/// Shape: the uid of a principal the owner's rules may name.
const STRANGER: u32 = 2;
/// Shape: the instant the record's decisions are judged at, in nanoseconds since the Unix epoch (2026-11-19).
const NOW_NS: u64 = 1_795_000_000_000_000_000;
/// Shape: the shards of the daemon the record's lookup runs on, so a key's record may live on a shard other than the
/// lookup's.
const SHARDS: u16 = 2;
/// Shape: the cross-shard calls the record's lookup test waits for at most: a find for each of its five keys, and a
/// consumer's enrollment.
const CALLS: u64 = 6;
/// Shape: an agent's consumer id that no enrollment holds.
const UNENROLLED_CONSUMER: u64 = 77;
/// Shape: the runtime shard the locating test's repositories elsewhere live on.
const AWAY_SHARD: u16 = 1;

/// The principal with uid `uid` on host 0.
fn principal(uid: u32) -> Principal {
  Principal::Uid {
    host: HostId(0),
    uid,
  }
}

/// Holds no keys: these tests decide `exec`s for principals already proven.
#[derive(Clone)]
struct NoKeys;

impl KeyHolders for NoKeys {
  type Principal = Principal;

  fn holding(&mut self, _user: &[u8], _key: &UserKey, _blob: &[u8]) -> Holding<Principal> {
    Holding::Nobody
  }
}

/// Holds the git repository `r.git` and the Mercurial repository `hg/r`, the owner's, each under its rules.
#[derive(Default)]
struct Held {
  /// The git repository's rules.
  git: Vec<PolicyRule>,
  /// The Mercurial repository's rules.
  hg: Vec<PolicyRule>,
}

impl Repositories for Held {
  fn locate(&self, _name: &str) -> Located {
    Located::Here
  }

  fn find(&self, name: &str) -> Option<RepositoryRecord> {
    let (tool, rules) = match name {
      "r.git" => (VcsTool::Git, self.git.clone()),
      "hg/r" => (VcsTool::Mercurial, self.hg.clone()),
      _ => return None,
    };
    Some(RepositoryRecord {
      id: RepositoryId { bytes: [0; 16] },
      name: name.to_owned(),
      tool,
      format: ObjectFormat::Sha1,
      default_branch: None,
      owner: principal(OWNER),
      policy: RepositoryPolicy {
        version: 1,
        rules,
        hooks: Vec::new(),
        hooks_env: Vec::new(),
        signers: Vec::new(),
      },
    })
  }
}

/// What one start was: the service, the repository's name, who asked, and the `GIT_PROTOCOL` value.
type Started = (super::Service, Option<String>, Principal, Option<Vec<u8>>);

/// Starts every service, recording what started it.
#[derive(Default)]
struct Recorded {
  /// Each start.
  started: Vec<Started>,
}

/// A command that never runs in these tests.
struct Idle;

impl super::Command for Idle {
  fn input(&mut self, _run: &[u8]) -> Result<(), Refused> {
    Ok(())
  }

  fn input_end(&mut self) {}

  async fn step(&mut self, _gone: super::ClientGone<'_>) -> Result<Stepped, Refused> {
    Ok(Stepped {
      consumed: 0,
      next: super::Next::Exited(0),
    })
  }

  fn output(&self) -> (&[u8], &[u8]) {
    (&[], &[])
  }

  fn sent(&mut self, _stderr: usize, _stdout: usize) {}
}

impl Services for Recorded {
  type Command = Idle;

  fn start(&mut self, start: &Start<'_>) -> Result<Idle, Unserved> {
    self.started.push((
      start.service,
      start.repository.map(|repository| repository.name.clone()),
      start.principal.clone(),
      start.git_protocol.map(<[u8]>::to_vec),
    ));
    Ok(Idle)
  }
}

/// A rule letting the stranger read every branch.
fn stranger_reads() -> Vec<PolicyRule> {
  vec![PolicyRule {
    names: PolicyNames::Refs,
    pattern: "refs/heads/".to_owned(),
    right: PolicyRight::Read,
    effect: PolicyEffect::Allow,
    who: PolicyWho::AnyOf {
      entries: vec![PolicyEntry::Is {
        principal: principal(STRANGER),
      }],
    },
  }]
}

/// What `site` answers `command` for `uid`: the refusal, or none when it ran.
fn refusal<S: Services>(
  site: &mut ToolSite<NoKeys, Held, S>,
  uid: u32,
  command: Asked,
) -> Option<Refused> {
  match site.dispatch(&principal(uid), &command, None) {
    Dispatched::Run(_) => None,
    Dispatched::Refuse(refused) => Some(refused),
  }
}

/// The refusal `stderr` with `exit_status`.
fn refused(stderr: &str, exit_status: u32) -> Option<Refused> {
  Some(Refused {
    stderr: stderr.as_bytes().to_vec(),
    exit_status,
  })
}

/// A-651 (§4.17.2 "Transports" item 2; the module doc's step 3). Do: as the owner, ask git's two services, Mercurial's and
/// Subversion's on the services the server has today. Expect: each refused as not served, in its tool's own words and
/// exit status (git's `fatal:` and 128, Mercurial's `abort:` and 255, svnserve's `svnserve:` and 1).
#[test]
fn every_service_is_refused_as_not_served_in_its_tools_words() {
  let mut site = ToolSite::new(NoKeys, Held::default(), NotYet);
  let answers = [
    Asked::GitUploadPack {
      path: b"/r.git".to_vec(),
    },
    Asked::GitReceivePack {
      path: b"r.git".to_vec(),
    },
    Asked::Mercurial {
      repository: b"hg/r".to_vec(),
      hidden: false,
    },
    Asked::Subversion,
  ]
  .map(|command| refusal(&mut site, OWNER, command));
  assert_eq!(
    answers,
    [
      refused("fatal: git-upload-pack is not served yet\n", GIT_DIE_STATUS),
      refused(
        "fatal: git-receive-pack is not served yet\n",
        GIT_DIE_STATUS
      ),
      refused(
        "abort: hg serve --stdio is not served yet\n",
        HG_ABORT_STATUS
      ),
      refused(
        "svnserve: svnserve -t is not served yet\n",
        SVNSERVE_FAILURE_STATUS
      ),
    ]
  );
}

/// A-651 (the module doc's step 1). Do: ask git's upload-pack and Mercurial's serve for names no repository has, for a
/// repository of the other tool, and for a path that is not UTF-8. Expect: each refused as its tool refuses a missing
/// repository, naming the path as the client sent it.
#[test]
fn a_missing_repository_is_refused_as_its_tool_refuses_one() {
  let mut site = ToolSite::new(NoKeys, Held::default(), NotYet);
  let answers = [
    Asked::GitUploadPack {
      path: b"/none.git".to_vec(),
    },
    Asked::GitUploadPack {
      path: b"hg/r".to_vec(),
    },
    Asked::GitUploadPack {
      path: b"r.git\xff".to_vec(),
    },
    Asked::Mercurial {
      repository: b"r.git".to_vec(),
      hidden: true,
    },
  ]
  .map(|command| refusal(&mut site, OWNER, command));
  assert_eq!(
    answers,
    [
      refused(
        "fatal: '/none.git' does not appear to be a git repository\n",
        GIT_DIE_STATUS
      ),
      refused(
        "fatal: 'hg/r' does not appear to be a git repository\n",
        GIT_DIE_STATUS
      ),
      refused(
        "fatal: 'r.git\u{fffd}' does not appear to be a git repository\n",
        GIT_DIE_STATUS
      ),
      refused("abort: repository r.git not found\n", HG_ABORT_STATUS),
    ]
  );
}

/// A-651 (the module doc's step 2; `vcs_policy::reaches`). Do: with a rule letting the stranger read branches, ask as the
/// stranger to fetch, to push, and to serve Mercurial; and as the owner to push. Expect: the fetch and the owner's push
/// started, the stranger's push refused by the policy in git's words, and the stranger's Mercurial session refused (no
/// rule speaks to it there).
#[test]
fn the_repositorys_policy_decides_who_reaches_it() {
  let held = Held {
    git: stranger_reads(),
    hg: Vec::new(),
  };
  let mut site = ToolSite::new(NoKeys, held, Recorded::default());
  let fetch = refusal(
    &mut site,
    STRANGER,
    Asked::GitUploadPack {
      path: b"/r.git".to_vec(),
    },
  );
  let push = refusal(
    &mut site,
    STRANGER,
    Asked::GitReceivePack {
      path: b"/r.git".to_vec(),
    },
  );
  let hg = refusal(
    &mut site,
    STRANGER,
    Asked::Mercurial {
      repository: b"hg/r".to_vec(),
      hidden: false,
    },
  );
  let owner_push = refusal(
    &mut site,
    OWNER,
    Asked::GitReceivePack {
      path: b"r.git".to_vec(),
    },
  );
  assert_eq!(
    (fetch, push, owner_push),
    (
      None,
      refused(
        "fatal: /r.git: git-receive-pack is not allowed by the repository's policy\n",
        GIT_DIE_STATUS
      ),
      None
    )
  );
  assert_eq!(
    hg,
    refused(
      "abort: hg/r: hg serve --stdio is not allowed by the repository's policy\n",
      HG_ABORT_STATUS
    )
  );
}

/// A-651 (the module doc's step 2: "never judged without it"). Do: give the Mercurial repository a rule of git's kind,
/// which its tool's matcher cannot read, and ask to serve it as its owner. Expect: refused as an unreadable policy, the
/// owner included, in Mercurial's words.
#[test]
fn a_policy_its_tool_cannot_read_is_refused_and_never_judged() {
  let held = Held {
    git: Vec::new(),
    hg: stranger_reads(),
  };
  let mut site = ToolSite::new(NoKeys, held, Recorded::default());
  let hg = refusal(
    &mut site,
    OWNER,
    Asked::Mercurial {
      repository: b"hg/r".to_vec(),
      hidden: false,
    },
  );
  assert_eq!(
    (hg, site.services.started.len()),
    (
      refused(
        "abort: hg/r: the repository's policy cannot be read
",
        HG_ABORT_STATUS
      ),
      0
    )
  );
}

/// A-651. Do: dispatch a fetch with a `GIT_PROTOCOL` value, a push, Mercurial's serve with `--hidden`, and Subversion's
/// tunnel, all allowed. Expect: each service started for the principal who asked, with its repository by name (none for
/// the tunnel, whose client names one later) and the fetch with its protocol value.
#[test]
fn a_dispatched_service_starts_with_its_repository_and_protocol() {
  let mut site = ToolSite::new(NoKeys, Held::default(), Recorded::default());
  let owner = principal(OWNER);
  let commands = [
    (
      Asked::GitUploadPack {
        path: b"/r.git".to_vec(),
      },
      Some(&b"version=2"[..]),
    ),
    (
      Asked::GitReceivePack {
        path: b"r.git".to_vec(),
      },
      None,
    ),
    (
      Asked::Mercurial {
        repository: b"/hg/r".to_vec(),
        hidden: true,
      },
      None,
    ),
    (Asked::Subversion, None),
  ];
  for (command, protocol) in commands {
    assert!(matches!(
      site.dispatch(&owner, &command, protocol),
      Dispatched::Run(_)
    ));
  }
  assert_eq!(
    site.services.started,
    [
      (
        super::Service::GitUploadPack,
        Some("r.git".to_owned()),
        owner.clone(),
        Some(b"version=2".to_vec())
      ),
      (
        super::Service::GitReceivePack,
        Some("r.git".to_owned()),
        owner.clone(),
        None
      ),
      (
        super::Service::Mercurial { hidden: true },
        Some("hg/r".to_owned()),
        owner.clone(),
        None
      ),
      (super::Service::Subversion, None, owner, None),
    ]
  );
}

/// Locates the repositories `away.git` and `hg/away` on [`AWAY_SHARD`], every other name here; finds none, since these
/// tests only locate.
struct Away;

impl Repositories for Away {
  fn locate(&self, name: &str) -> Located {
    if matches!(name, "away.git" | "hg/away") {
      Located::Elsewhere(AWAY_SHARD)
    } else {
      Located::Here
    }
  }

  fn find(&self, _name: &str) -> Option<RepositoryRecord> {
    None
  }
}

/// A-651 part 4 (main's ruling for the HTTP edge, A-625 part 3: a connection moves to its repository's owner shard;
/// D-14, a name routes to its partition). Do: locate git's two services and Mercurial's on repositories another shard
/// holds, git's path with its leading slash and without; git's on a repository here; and Subversion's tunnel, which
/// names none. Expect: the first three on that shard, each by its name without the slash, and the last two here.
#[test]
fn an_exec_is_located_on_the_shard_its_repository_lives_on() {
  let site = ToolSite::new(NoKeys, Away, NotYet);
  let commands = [
    Asked::GitUploadPack {
      path: b"/away.git".to_vec(),
    },
    Asked::GitReceivePack {
      path: b"away.git".to_vec(),
    },
    Asked::Mercurial {
      repository: b"hg/away".to_vec(),
      hidden: false,
    },
    Asked::GitUploadPack {
      path: b"/r.git".to_vec(),
    },
    Asked::Subversion,
  ];
  let away = Located::Elsewhere(AWAY_SHARD);
  assert_eq!(
    commands.map(|command| site.locate(&command)),
    [away, away, away, Located::Here, Located::Here]
  );
}

/// An Ed25519 key of public bytes `byte`, and its blob (RFC 8709 §4: the type's name, then the 32 bytes, each an SSH
/// string).
fn ed25519(byte: u8) -> (UserKey, Vec<u8>) {
  let public = [byte; 32];
  let mut blob = Vec::new();
  for field in [&b"ssh-ed25519"[..], &public[..]] {
    blob.extend_from_slice(&u32::try_from(field.len()).unwrap_or(u32::MAX).to_be_bytes());
    blob.extend_from_slice(field);
  }
  (UserKey::Ed25519 { public }, blob)
}

/// `material` enrolled to prove `proves` until `expires_ns` by the owner at the terminal, on the shard of the partition
/// its id routes to, as the human surface enrolls it (§4.13): the daemon's reply.
fn enrolled(
  daemon: &crate::daemon::Daemon,
  proves: WirePrincipal,
  material: CredentialMaterial,
  expires_ns: Option<u64>,
) -> ReplyBody {
  let id = crate::credentials::id_of(&material);
  let partition = usize::from(crate::credentials::owner_of(&id, usize::from(SHARDS)));
  crate::daemon::observe_at(daemon, partition, move |state| {
    let proof =
      crate::credentials::enroll_proof(&state.issuer_secret, &proves, &material, expires_ns);
    let asked = RequestBody::EnrollCredential {
      principal: proves,
      material,
      expires_ns,
      proof,
    };
    crate::verbs::dispatch(state, 0, &principal(OWNER), asked)
  })
}

/// What this shard counted of the record's refusals, by reason: expired, not this key, revoked, unknown.
fn counted_reasons() -> Option<[u64; 4]> {
  crate::state::with_state(|state| {
    [KEY_EXPIRED, KEY_NOT_THIS, KEY_REVOKED, KEY_UNKNOWN]
      .map(|kind| state.refusals.get(kind).copied().unwrap_or(0))
  })
}

/// What the production site's lookup answered about each key, and what it counted by reason ([`counted_reasons`]).
type LookedUp = (Vec<Holding<Principal>>, Option<[u64; 4]>);

/// [`LookedUp`] when the production site's lookup is asked, on `daemon`'s first shard, about each of `keys` for the user
/// `git`; none when its task did not answer within the calls' budget.
fn looked_up(daemon: &crate::daemon::Daemon, keys: Vec<(UserKey, Vec<u8>)>) -> Option<LookedUp> {
  let (sent, answered) = std::sync::mpsc::sync_channel(1);
  crate::daemon::observe_first(daemon, move |_state| {
    let task = slates_rt::futures::spawn(async move {
      let mut site = ToolSite::new(RecordKeys, Held::default(), NotYet);
      let mut answers = Vec::new();
      for (key, blob) in &keys {
        answers.push(site.lookup(b"git", key, blob).await);
      }
      let _ = sent.send((answers, counted_reasons()));
    });
    if let Ok(task) = task {
      let _ = slates_rt::futures::detach(task);
    }
  });
  let wait_ns = crate::daemon::LIVENESS_BUDGET_NS.saturating_mul(CALLS);
  answered
    .recv_timeout(std::time::Duration::from_nanos(wait_ns))
    .ok()
}

/// A-651 part 3 (a key's record lives on the partition its id routes to, which a session's synchronous ask cannot wait
/// for). Do: ask the production key holders about an Ed25519 key, for any user. Expect: each answered "not yet", so the
/// session waits for the site's lookup; no file or default key stands in.
#[test]
fn the_production_key_holders_leave_every_key_to_the_sites_lookup() {
  let (key, blob) = ed25519(9);
  let answers = [&b"git"[..], b"root", b""].map(|user| RecordKeys.holding(user, &key, &blob));
  assert_eq!(
    answers,
    [Holding::Unknown, Holding::Unknown, Holding::Unknown]
  );
}

/// A-651 part 3 (main's recipe: a record of an SSH key of the same bytes, live on the wall clock). Do: decide a record of
/// the owner's key for that key with no expiry, with one a nanosecond after now, and at its expiry; for a key of other
/// bytes; and a token's record at the key's id. Expect: the owner twice, then expired, then not this key twice.
#[test]
fn a_record_proves_its_principal_for_its_own_key_while_live() {
  let (_, blob) = ed25519(1);
  let (_, other) = ed25519(2);
  let key = || CredentialKind::SshKey {
    algorithm: "ssh-ed25519".to_owned(),
    blob: blob.clone(),
  };
  let record = |kind, expires_ns| CredentialRecord {
    id: crate::credentials::ssh_key_id(&blob),
    principal: principal(OWNER),
    kind,
    expires_ns,
  };
  let later = Some(NOW_NS.saturating_add(1));
  assert_eq!(
    [
      key_holder(&record(key(), None), &blob, NOW_NS),
      key_holder(&record(key(), later), &blob, NOW_NS),
      key_holder(&record(key(), Some(NOW_NS)), &blob, NOW_NS),
      key_holder(&record(key(), None), &other, NOW_NS),
      key_holder(
        &record(
          CredentialKind::HttpToken {
            cram_md5_contexts: [0; 32],
          },
          None,
        ),
        &blob,
        NOW_NS,
      ),
    ],
    [
      Ok(principal(OWNER)),
      Ok(principal(OWNER)),
      Err(KeyUnproven::Expired),
      Err(KeyUnproven::NotThisKey),
      Err(KeyUnproven::NotThisKey),
    ]
  );
}

/// A-651 part 3 (main's recipe over g299's credential record; D-14: a key's record lives on the partition its id routes
/// to). Do: on a two-shard daemon, enroll for the owner an Ed25519 key with no expiry and one long expired, a token at a
/// third key's id, and a fourth key for a consumer never enrolled; then, from the first shard, ask the production site's
/// lookup about each and about a fifth key nobody enrolled. Expect: the owner holds the first key with one key to sign,
/// nobody holds the rest, and each refusal is counted once by its reason.
#[test]
fn the_production_lookup_proves_a_key_by_the_credential_record_from_any_shard() {
  let daemon = crate::daemon::audit_daemon_of(SHARDS);
  let keys: Vec<(UserKey, Vec<u8>)> = (1..=5).map(ed25519).collect();
  let blob = |at: usize| {
    keys
      .get(at)
      .map(|(_, blob)| blob.clone())
      .unwrap_or_default()
  };
  let owner = WirePrincipal::Uid {
    host: 0,
    uid: OWNER,
  };
  let agent = WirePrincipal::Consumer {
    host: 0,
    account: OWNER,
    consumer: UNENROLLED_CONSUMER,
  };
  let token = CredentialMaterial::HttpToken {
    id: crate::credentials::ssh_key_id(&blob(2)),
    cram_md5_contexts: [0; 32],
  };
  let replies = [
    enrolled(
      &daemon,
      owner.clone(),
      CredentialMaterial::SshKey { blob: blob(0) },
      None,
    ),
    enrolled(
      &daemon,
      owner.clone(),
      CredentialMaterial::SshKey { blob: blob(1) },
      Some(1),
    ),
    enrolled(&daemon, owner, token, None),
    enrolled(
      &daemon,
      agent,
      CredentialMaterial::SshKey { blob: blob(3) },
      None,
    ),
  ];
  let looked = looked_up(&daemon, keys);
  daemon.stop();
  assert!(
    replies
      .iter()
      .all(|reply| matches!(reply, ReplyBody::CredentialEnrolled { .. })),
    "{replies:?}"
  );
  let holds = Holding::Holds {
    principal: principal(OWNER),
    keys_required: 1,
  };
  let nobody = Holding::Nobody;
  assert_eq!(
    looked,
    Some((
      vec![
        holds,
        nobody.clone(),
        nobody.clone(),
        nobody.clone(),
        nobody
      ],
      Some([1, 1, 1, 1])
    ))
  );
}
