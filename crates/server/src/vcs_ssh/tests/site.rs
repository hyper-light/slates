use slates_db::HostId;
use slates_db::catalog::{
  ObjectFormat, PolicyEffect, PolicyEntry, PolicyNames, PolicyRight, PolicyRule, PolicyWho,
  Principal, RepositoryId, RepositoryPolicy, RepositoryRecord, VcsTool,
};
use slates_ssh::command::Command as Asked;
use slates_ssh::userauth::{Holding, KeyHolders, UserKey};

use super::{
  Dispatched, GIT_DIE_STATUS, HG_ABORT_STATUS, NotYet, Refused, SVNSERVE_FAILURE_STATUS, Services,
  SshSite, Start, ToolSite, Unserved,
};
use crate::vcs_http::git::Repositories;

/// Shape: the uid of the tests' repositories' owner.
const OWNER: u32 = 1;
/// Shape: the uid of a principal the owner's rules may name.
const STRANGER: u32 = 2;

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
      },
    })
  }
}

/// Starts every service, recording what started it: the service, the repository's name, and the `GIT_PROTOCOL` value.
#[derive(Default)]
struct Recorded {
  /// What each start was.
  started: Vec<(super::Service, Option<String>, Option<Vec<u8>>)>,
}

/// A command that never runs in these tests.
struct Idle;

impl super::Command for Idle {
  async fn input(&mut self, _run: &[u8]) -> Result<(), Refused> {
    Ok(())
  }

  async fn input_end(&mut self) -> Result<(), Refused> {
    Ok(())
  }

  async fn output(
    &mut self,
    _out: &mut Vec<u8>,
    _err: &mut Vec<u8>,
  ) -> Result<super::Next, Refused> {
    Ok(super::Next::Exited(0))
  }
}

impl Services for Recorded {
  type Command = Idle;

  fn start(&mut self, start: &Start<'_>) -> Result<Idle, Unserved> {
    self.started.push((
      start.service,
      start.repository.map(|repository| repository.name.clone()),
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
/// tunnel, all allowed. Expect: each service started with its repository by name (none for the tunnel, whose client names
/// one later) and the fetch with its protocol value.
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
        Some(b"version=2".to_vec())
      ),
      (
        super::Service::GitReceivePack,
        Some("r.git".to_owned()),
        None
      ),
      (
        super::Service::Mercurial { hidden: true },
        Some("hg/r".to_owned()),
        None
      ),
      (super::Service::Subversion, None, None),
    ]
  );
}
