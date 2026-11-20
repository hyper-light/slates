//! Subversion's site on the HTTP edge (A-681 part 1; §4.17.4 item 5 "HTTP", item 10 and piece 13): each request's head
//! routed as `mod_dav_svn` routes a repository under `SVNParentPath` (the URL's first segment names the repository),
//! its credentials proved to a principal of §4.13 (git's [`Authenticator`]), its repository found ([`Repositories`]),
//! the principal's reach of it decided by the repository's policy (`crate::vcs_policy::reaches`, A-597, A-622), and
//! the request answered by `slates_svn::dav::site` from the repository's index, green and lock table, which its owner
//! shard lends a step at a time ([`SvnStores`]) with the principal's compiled read rules (A-606), each path the answer
//! names judged by them (`mod_authz_svn`'s questions).
//!
//! **The order**, each refusal with its status:
//! 1. The route. A path whose escapes are malformed or name a slash or a NUL is `400`; a path past the request's room is
//!    `414`; a path naming no repository (the server's root) is `404`.
//! 2. The credentials, judged as git's and Mercurial's sites judge them: none, or none proving a principal, is `401`
//!    with the edge's challenges, Basic's first and then Bearer's, on every connection (stock svn sends Basic after one
//!    challenge and on every request after, as recorded; A-686: Basic with the token as the password is taken on the
//!    loopback cleartext edge as over TLS); a repeated or malformed `Authorization` is `400`; a credential record that
//!    did not answer is `503`.
//! 3. The repository's shard. A repository another shard holds moves the connection there whole (`Answer::Move`).
//! 4. The repository. None of that name, or one of another tool, is `404`.
//! 5. The policy. A principal the policy lets read none of the repository is `403`, for every request.
//! 6. The store. A repository whose store this shard does not hold is `503` (the daemon's lender:
//!    `crate::vcs_store::svn_view::ServedRecords`, the records of the repositories this shard's map holds).
//!
//! A served request's body is read whole within the request's room (`413` past it), then answered at its end from the
//! store as `slates_svn::dav::site` decides: its status, which the reply names as it begins (`207` for a PROPFIND; the
//! route decides its status after its body, `StatusMode::AfterBody`, A-629 part 2c's engine rule), its headers and its
//! body; a file's text a piece a reply run, lent from the green each run, never held whole; a REPORT's answer a slice
//! of the room a reply run, the store lent again for each, its first slices stepped at the body's end (the task
//! yielding between them) until it has written its first bytes or ended, since they decide its status: a walk refused
//! before its first byte is answered with its error, as `mod_dav` answers one, and one refused after it ends the
//! connection (a refusal within those first slices after the first byte is `500`, nothing sent).
//!
//! **Commits** (A-681 part C, `slates_svn::dav::commit`). A POST to `!svn/me` begins a transaction over the
//! repository's records, named `REV-SEQ` and kept on the owner shard between its requests (`VcsStores`, one a
//! repository and principal, a new POST replacing the older); PUT, MKCOL, DELETE and PROPPATCH under `!svn/txr/NAME/`,
//! a COPY whose `Destination` is there, and a DELETE of `!svn/txn/NAME` each drive it a step; a MERGE naming it closes
//! it, the accepted work landed through the owner's commit step (`repository::svn_commit::commit_work`) and answered
//! with the new revision. A hook the repository configures at start-commit or pre-commit refuses the commit, as the
//! svn+ssh tunnel refuses it, until hooks run. A commit presents its lock tokens as a stock client sends them: each
//! request's `If` header, then the MERGE's `S:lock-token-list` with `X-SVN-Options: keep-locks` deciding whether the
//! verdict releases them. A transaction idle past the edge's learned idle bound (`super::idle_bound_ns`) is released
//! at the table's next use, as are any its principal left when it begins another.
//!
//! **Locks** (A-681, over A-627's `slates_svn::dav::locking` and A-602's lock table): a LOCK or an UNLOCK of a public
//! path is read, judged by the repository's lock table at the owner's instant with a token drawn from the system's
//! generator, its entries landed through the owner's step as A-693 part 2's `Record::Locks` (durable as a revision
//! is), and answered as `mod_dav_svn` answers it; a landing the step refuses is answered as a commit's refusal.
//!
//! **Names.** A repository is the one named by the URL's first path segment, decoded; the rest of the path is below
//! its root (`/repo/!svn/rvr/5/trunk` is `repo`'s `/!svn/rvr/5/trunk`). A repository whose name holds a slash cannot be
//! reached over HTTP: `SVNParentPath`'s rule, which a name routed to its partition by its hash cannot widen to a
//! longest-prefix search without asking every partition (GAPS). A URL's query is not read: `mod_dav_svn`'s `?p=` and
//! `?r=` for a browser's GET are not served yet.
//!
//! **Bounds.** The exchange holds the request's path, its `Depth` and its body, each charged to the request's room
//! before it is held. The XML reader's bounds ([`xml_bounds`]) are derived from the same room: a body of the room's
//! bytes, and as many elements, nesting levels and attributes as parsed elements fit in the room, so the parsed body
//! holds at most the room again. An answer is written within the room; one past it is `500`.

use crate::repository::svn_commit::{Committed, Uncommitted};
use crate::vcs_hooks::{self, CommittedView, HookPoint, LockToken, PendingView};
use crate::vcs_svn::{HookSite, Hooked, MERGEINFO, run_hook};
use slates_db::catalog::{Principal, RepositoryId, RepositoryRecord, VcsTool};
use slates_http::Admission;
use slates_http::head::{Head, Target};
use slates_http::percent::{self, Plus};
use slates_http::reply::Status;
use slates_http_edge::connection::{Fields, StatusMode};
use slates_http_edge::credential;
use slates_rt::futures::yield_now;
use slates_svn::authz::PrincipalRules;
use slates_svn::commit::drive::Due;
use slates_svn::commit::drive::{Setup, Work};
use slates_svn::dav::Bounds as XmlBounds;
use slates_svn::dav::commit::{
  self as commit, DavCommit, Hooks, MergeStep, Merged, PutStart, World as CommitWorld,
};
use slates_svn::dav::locking::{self, HttpAnswer, LockHttp, UnlockHttp};
use slates_svn::dav::response::MergeResponse;
use slates_svn::dav::site::{
  self, Answer as SiteAnswer, Budget, Headers, Quantum, Report, ReportProgress, Request, Served,
};
use slates_svn::dav::xml::Element;
use slates_svn::index::NodeKind;
use slates_svn::index::{Index, Text};
use slates_svn::lock::CommitLocks;
use slates_svn::lock::{AprTime, LockTable};
use slates_svn::lock::{HeadView, LockEntry, UUID_BYTES};
use slates_svn::migration::Repository as SvnRecords;
use slates_svn::reads::Repository;
use std::collections::BTreeMap;

use super::git::{Authenticator, Located, Repositories, Unproven};
use super::{Answer, ClientGone, Decided, Exchange, Failed, Site};
use crate::vcs_policy::{self, Right};

/// Format: the bytes an escape may not name in a repository's path: a slash, which would move a segment's boundary,
/// and NUL.
const PATH_REFUSED: &[u8] = b"/\0";

/// Format: `HEAD`, whose reply is the head alone.
const HEAD: &[u8] = b"HEAD";

/// Format: the `Depth` header's name (RFC 4918 §10.2).
const DEPTH: &[u8] = b"depth";

/// A count as `u64`, saturating where it could not fit (no target slates ships has a wider `usize`).
fn to_u64(count: usize) -> u64 {
  u64::try_from(count).unwrap_or(u64::MAX)
}

/// A count as `usize`, saturating where it could not fit.
fn to_usize(count: u64) -> usize {
  usize::try_from(count).unwrap_or(usize::MAX)
}

/// Derived: the XML reader's bounds for a request of `room` bytes (the module doc): the body the room; a name the room;
/// elements, nesting levels and attributes as many as parsed elements (`size_of::<Element>()` each) fit in the room.
pub(crate) fn xml_bounds(room: u64) -> XmlBounds {
  let bytes = to_usize(room);
  let elements = bytes.checked_div(size_of::<Element>()).unwrap_or(bytes);
  XmlBounds {
    body_bytes: bytes,
    depth: elements,
    name_bytes: bytes,
    attributes: elements,
    elements,
  }
}

/// What a repository's store lends for one step: its index, its green, its UUID, its locks, the principal's compiled
/// read rules (none where the repository names no authorization file) and the instant locks are judged at.
pub(crate) struct Lent<'a, R: ?Sized> {
  /// The index.
  pub(crate) index: &'a Index,
  /// The green.
  pub(crate) repository: &'a R,
  /// The repository's UUID.
  pub(crate) uuid: &'a str,
  /// Its locks.
  pub(crate) locks: &'a LockTable,
  /// The principal's compiled read rules.
  pub(crate) rules: Option<&'a PrincipalRules>,
  /// The instant a lock's expiry is judged at, on the owner's clock.
  pub(crate) now: AprTime,
}

/// Where Subversion repositories' stores are lent on this shard (A-662: each repository's tool store, kept on its owner
/// shard with its record), afresh for each step, as git's and Mercurial's sites lend theirs, so a shard's state is never
/// held across a wait.
pub(crate) trait SvnStores {
  /// The green a repository's requests are answered from.
  type Repository: Repository + ?Sized;
  /// Whether the store of repository `id` is held here.
  fn holds(&self, id: RepositoryId) -> bool;
  /// A fetch's hold on the generation of a repository's records it began on (A-745, §4.17.1 "A fetch's answer" rule 4),
  /// given back as it is dropped.
  type Hold;
  /// Holds repository `id`'s records as they are now: taken in the step a report's answer began, so its later slices
  /// read what that one read; none when they are not held here.
  fn hold(&mut self, id: RepositoryId) -> Option<Self::Hold>;
  /// Runs `step` over the records `hold` holds as `principal` reads them; none when they are gone.
  fn lend_held<T>(
    &mut self,
    hold: &Self::Hold,
    principal: &Principal,
    step: impl FnOnce(&Lent<'_, Self::Repository>) -> T,
  ) -> Option<T>;
  /// The time each slice of a fetch's answer may run (A-745, §4.17.1 "A fetch's answer" rule 3): the shard's step
  /// quantum; `None` where the lender names none, and a slice is as many steps as its room.
  fn fetch_quantum(&self) -> Option<u64> {
    None
  }
  /// Runs `step` over repository `id`'s store as `principal` reads it; none when it is not held here.
  fn lend<T>(
    &mut self,
    id: RepositoryId,
    principal: &Principal,
    step: impl FnOnce(&Lent<'_, Self::Repository>) -> T,
  ) -> Option<T>;

  /// Begins an HTTP commit in repository `id` for `principal` (A-681 part C): the transaction named, the setup
  /// `setup_of` makes from its name and the owner's instant, the commit begun over the records as far as start-commit
  /// (A-733, `DavCommit::begin_hooked`), and kept; its name and start-commit's due where the commit stopped for it, which
  /// the caller runs and answers on the kept commit (`DavCommit::resume_begin`), or the POST's refusal. None where this
  /// lender serves no commits or does not hold `id`'s records.
  fn begin_commit(
    &mut self,
    id: RepositoryId,
    principal: &Principal,
    setup_of: impl FnOnce(String, AprTime) -> Setup,
  ) -> Option<Result<(String, Option<Due>), HttpAnswer>> {
    let _unserved = (id, principal, setup_of);
    None
  }

  /// Runs `step` on repository `id`'s transaction `name`, which `principal` made, over the records, keeping it unless
  /// it ended: none where this lender serves no commits or does not hold `id`'s records, `Some(None)` where no such
  /// transaction is open for `principal`.
  fn on_commit<T>(
    &mut self,
    id: RepositoryId,
    principal: &Principal,
    name: &str,
    step: impl FnOnce(&mut DavCommit, &CommitWorld<'_, SvnRecords>) -> T,
  ) -> Option<Option<T>> {
    let _unserved = (id, principal, name, step);
    None
  }

  /// Judges a lock verb over repository `id`'s lock table and lands the entries it makes through the owner's step
  /// (A-681; `Record::Locks`, durable as a revision is, A-693 part 2): `judge` has the records lent with a fresh token's
  /// draw, and gives the entries and the answer; a landing the step refuses is its refusal. None where this lender
  /// serves no locks.
  fn lock_verb(
    &mut self,
    id: RepositoryId,
    principal: &Principal,
    judge: impl FnOnce(
      &Lent<'_, SvnRecords>,
      &mut dyn FnMut() -> [u8; UUID_BYTES],
    ) -> (Vec<LockEntry>, HttpAnswer),
  ) -> Option<Result<HttpAnswer, Uncommitted>> {
    let _unserved = (id, principal, judge);
    None
  }

  /// Lands an accepted `work` on repository `id`'s logs by `principal` through the owner's commit step
  /// (`crate::repository::svn_commit::commit_work`): none where this lender serves no commits.
  fn land(
    &mut self,
    id: RepositoryId,
    principal: &Principal,
    work: Work,
  ) -> Option<Result<Committed, Uncommitted>> {
    let _unserved = (id, principal, work);
    None
  }
}

/// What a routed head names: its repository, and the path below its root.
struct Routed {
  /// The repository's name.
  name: String,
  /// The decoded path below the repository's root.
  path: String,
}

/// What a request's credentials came to.
enum Proof {
  /// The principal they prove.
  Principal(Principal),
  /// None proved: `401` with the challenges.
  Unauthorized,
  /// None decided, with this status: the `Authorization` line itself refused (`400`), or a credential record that did
  /// not answer (`503`).
  Refused(Status),
}

/// Subversion's site: the realm its challenges name, the room a request is charged to, and the authenticator,
/// repositories and stores it decides and answers with (the module doc).
pub(crate) struct SvnSite<A, R, S> {
  /// The realm a `401`'s challenges name.
  realm: String,
  /// The room a request's route and exchange are each charged to: the edge's request room.
  room: u64,
  /// What proves credentials.
  authenticator: A,
  /// Where repositories are found.
  repositories: R,
  /// Where their stores are read.
  stores: S,
}

impl<A: Authenticator, R: Repositories, S: SvnStores + Clone> SvnSite<A, R, S> {
  /// A site naming `realm` in its challenges, charging each request to `room`.
  pub(crate) fn new(
    realm: String,
    room: u64,
    authenticator: A,
    repositories: R,
    stores: S,
  ) -> Self {
    Self {
      realm,
      room,
      authenticator,
      repositories,
      stores,
    }
  }

  /// The answer to `head`, on any connection, in the module doc's order.
  pub(crate) async fn answer(&mut self, head: &Head<'_>) -> Decided<SvnExchange<S>> {
    let routed = match route(head, self.room) {
      Ok(routed) => routed,
      Err(status) => return refused_with(status, Fields::new(), false),
    };
    let principal = match self.proof(head).await {
      Proof::Principal(principal) => principal,
      Proof::Unauthorized => return self.unauthorized(),
      Proof::Refused(status) => return refused_with(status, Fields::new(), false),
    };
    if let Located::Elsewhere(shard) = self.repositories.locate(&routed.name) {
      return Decided {
        answer: Answer::Move { shard },
        proven: true,
      };
    }
    let Some(repository) = self
      .repositories
      .find(&routed.name)
      .filter(|repository| repository.tool == VcsTool::Subversion)
    else {
      return refused_with(Status::NOT_FOUND, Fields::new(), true);
    };
    match vcs_policy::reaches(&repository, &principal, &[Right::Read]) {
      Ok(true) => {}
      Ok(false) => return refused_with(Status::FORBIDDEN, Fields::new(), true),
      // A policy its tool cannot read is a damaged record (A-622's guard keeps such rules out): never judged without it.
      Err(_) => return refused_with(Status::INTERNAL_SERVER_ERROR, Fields::new(), true),
    }
    if !self.stores.holds(repository.id) {
      return refused_with(Status::SERVICE_UNAVAILABLE, Fields::new(), true);
    }
    let asked = Asked {
      head,
      routed,
      principal,
    };
    match SvnExchange::new(self.stores.clone(), &repository, asked, self.room) {
      Some(exchange) => Decided {
        answer: Answer::Serve {
          fields: Fields::new(),
          head_only: head.method == HEAD,
          status: StatusMode::AfterBody,
          exchange,
        },
        proven: true,
      },
      None => refused_with(Status::FIELDS_TOO_LARGE, Fields::new(), true),
    }
  }

  /// What `head`'s credentials prove, as git's and Mercurial's sites read them.
  async fn proof(&self, head: &Head<'_>) -> Proof {
    let credential = match credential::credential(head) {
      Ok(Some(credential)) => credential,
      Ok(None) => return Proof::Unauthorized,
      Err(refused) if refused.status() == Status::UNAUTHORIZED => return Proof::Unauthorized,
      Err(refused) => return Proof::Refused(refused.status()),
    };
    match self.authenticator.prove(&credential).await {
      Ok(proven) => Proof::Principal(proven.principal),
      Err(Unproven::Unanswered) => Proof::Refused(Status::SERVICE_UNAVAILABLE),
      Err(Unproven::Unknown | Unproven::Expired | Unproven::Revoked | Unproven::NotAToken) => {
        Proof::Unauthorized
      }
    }
  }

  /// `401` with the challenges, Basic's first and then Bearer's.
  fn unauthorized(&self) -> Decided<SvnExchange<S>> {
    let Ok(challenges) = credential::challenges(&self.realm) else {
      // A realm a challenge cannot carry is the operator's misconfiguration, never the client's.
      return refused_with(Status::INTERNAL_SERVER_ERROR, Fields::new(), false);
    };
    let fields: Fields = challenges
      .into_iter()
      .map(|challenge| (credential::WWW_AUTHENTICATE.to_vec(), challenge))
      .collect();
    refused_with(Status::UNAUTHORIZED, fields, false)
  }
}

impl<A: Authenticator, R: Repositories, S: SvnStores + Clone> Site for SvnSite<A, R, S> {
  type Exchange = SvnExchange<S>;

  /// The site's answer, whatever the connection: no step of it reads whether the connection is TLS (A-686).
  async fn decide(&mut self, head: &Head<'_>, _secure: bool) -> Decided<Self::Exchange> {
    self.answer(head).await
  }
}

/// Format: the methods git's and Mercurial's clients send (git's smart HTTP and Git LFS: GET, HEAD and POST; hg's wire
/// protocol: GET and POST), which a Subversion request needs its special segment to be told by.
const SHARED_METHODS: [&[u8]; 3] = [b"GET", b"HEAD", b"POST"];

/// Whether `head` is Subversion's (A-681; the daemon's `ToolSites` routes it here): a method no other tool's client sends
/// (OPTIONS, PROPFIND, REPORT, MERGE, PUT, ...), or a path with the special URI's segment ([`site::SPECIAL`]), as stock
/// svn's GETs and HTTPv2 POSTs name `!svn/...`. The segment is compared as sent: svn writes it unescaped.
pub(crate) fn dav_request(head: &Head<'_>) -> bool {
  if !SHARED_METHODS.contains(&head.method) {
    return true;
  }
  let Target::Path { path, .. } = head.target else {
    return false;
  };
  path
    .split(|byte| *byte == b'/')
    .any(|segment| segment == site::SPECIAL.as_bytes())
}

/// Routes `head` (the module doc's names): its repository's name and the path below its root, read within `room`;
/// refused `400` where the path's escapes cannot be read, `414` where it is past `room`, and `404` where no repository
/// is named.
fn route(head: &Head<'_>, room: u64) -> Result<Routed, Status> {
  let Target::Path { path, .. } = head.target else {
    return Err(Status::NOT_FOUND);
  };
  if percent::names_any(path, PATH_REFUSED) {
    return Err(Status::BAD_REQUEST);
  }
  let length = percent::decoded_len(path).map_err(|_| Status::BAD_REQUEST)?;
  Admission::new(room)
    .charge(to_u64(length))
    .map_err(|_| Status::URI_TOO_LONG)?;
  let mut decoded = Vec::with_capacity(length);
  percent::decode_into(path, Plus::Plus, &mut decoded).map_err(|_| Status::BAD_REQUEST)?;
  let decoded = String::from_utf8(decoded).map_err(|_| Status::NOT_FOUND)?;
  let trimmed = decoded.trim_start_matches('/');
  let (name, rest) = trimmed.split_once('/').unwrap_or((trimmed, ""));
  if name.is_empty() {
    return Err(Status::NOT_FOUND);
  }
  Ok(Routed {
    name: name.to_owned(),
    path: format!("/{rest}"),
  })
}

/// A refusal with `status` and `fields`; `proven` when the request's credentials proved a principal.
fn refused_with<E>(status: Status, fields: Fields, proven: bool) -> Decided<E> {
  Decided {
    answer: Answer::Refuse { status, fields },
    proven,
  }
}

/// What a decided head asks: the head itself, its route and the principal its credentials proved.
struct Asked<'h, 'b> {
  /// The head.
  head: &'h Head<'b>,
  /// Its route.
  routed: Routed,
  /// The principal.
  principal: Principal,
}

/// What a served request's answer is, once its body ended.
enum Answered {
  /// A whole answer; its body taken by the first reply run.
  Whole {
    /// The status.
    status: Status,
    /// The headers.
    headers: Headers,
    /// The body, until the reply takes it.
    body: Vec<u8>,
  },
  /// A file's text, read from the green a piece a reply run, from `offset`.
  Text {
    /// The headers.
    headers: Headers,
    /// The text, none for an empty file.
    text: Option<Text>,
    /// How much of it the reply has given.
    offset: u64,
  },
  /// A REPORT's answer, a slice a reply run.
  Report {
    /// The headers.
    headers: Headers,
    /// The report.
    report: Report,
    /// What its first slices wrote, until the first reply run takes it.
    pending: Vec<u8>,
    /// Whether the answer is whole.
    done: bool,
  },
}

/// One slice's budget of a report's answer (A-745, §4.17.1 "A fetch's answer"): `room` bytes, taken a step of the
/// drive at a time until `quantum` has run where the lender names one, else as many steps as the room.
fn slice_budget(room: usize, quantum: Option<u64>) -> Budget {
  match quantum {
    Some(ns) => Budget {
      room,
      steps: 1,
      quantum: Some(Quantum {
        now_ns: slates_rt::futures::now_ns,
        ns,
      }),
    },
    None => Budget {
      room,
      steps: room,
      quantum: None,
    },
  }
}

/// The repository as `site` reads it for one step: what `lent` lends, under the repository root's URL path `root`.
fn served_of<'a, R: ?Sized>(lent: &'a Lent<'a, R>, root: &'a str) -> Served<'a, R> {
  Served {
    index: lent.index,
    repository: lent.repository,
    root,
    uuid: lent.uuid,
    locks: lent.locks,
    rules: lent.rules,
    now: lent.now,
  }
}

/// Steps `report` until its first bytes or its end, the records `hold` holds lent for each slice of `room` and the task yielding
/// between them (§4.3): what they come to decides the status the reply begins with, as `dav_method_report` answers a
/// report's error only while it has sent nothing. A refusal after its first bytes, within these slices, fails the
/// request `500` before any of it is sent, where `mod_dav` would have sent the bytes and ended the connection.
async fn first_slices<S: SvnStores>(
  stores: &mut S,
  asker: (&S::Hold, &Principal, &str),
  mut report: Report,
  room: usize,
) -> Result<Answered, Failed> {
  let (hold, principal, root) = asker;
  let mut pending = Vec::new();
  loop {
    let budget = slice_budget(room, stores.fetch_quantum());
    let progress = stores
      .lend_held(hold, principal, |lent| {
        report.resume(&served_of(lent, root), &mut pending, budget)
      })
      .ok_or(Failed(Status::SERVICE_UNAVAILABLE))?;
    let done = match progress {
      ReportProgress::More if pending.is_empty() => {
        yield_now().await;
        continue;
      }
      ReportProgress::More => false,
      ReportProgress::Done => true,
      ReportProgress::Refused(answer) => {
        return Ok(Answered::Whole {
          status: status_of(answer.status),
          headers: answer.headers,
          body: answer.body,
        });
      }
      ReportProgress::Broken => return Err(Failed(Status::INTERNAL_SERVER_ERROR)),
    };
    return Ok(Answered::Report {
      headers: report.headers(),
      report,
      pending,
      done,
    });
  }
}

/// A served request's exchange (the module doc's bounds): its route, method and depth copied from the head, its body,
/// and its answer once the body has ended.
pub(crate) struct SvnExchange<S: SvnStores> {
  /// Where the repository's store is read.
  stores: S,
  /// A report's hold on the generation of the records it began on (A-745 rule 4), from the step that made it to the
  /// exchange's end.
  generation: Option<S::Hold>,
  /// The repository.
  repository: RepositoryId,
  /// The repository root's URL path, decoded (`/repo`).
  root: String,
  /// Who asks.
  principal: Principal,
  /// The method.
  method: String,
  /// The decoded path below the repository's root.
  path: String,
  /// The `Depth` header's value, if one was sent.
  depth: Option<String>,
  /// The request's body.
  body: Vec<u8>,
  /// The room the exchange may still hold.
  room: Admission,
  /// The room the request was given, from which the XML reader's bounds and an answer's room are derived.
  request_room: u64,
  /// The answer, once made.
  answered: Option<Answered>,
  /// The repository's name, the green's name a commit's messages name.
  name: String,
  /// The HTTPv2 commit headers the request sent ([`CommitHeaders`]).
  commit_headers: CommitHeaders,
  /// The repository's policy: the hooks its commits and locks run (A-733).
  policy: slates_db::catalog::RepositoryPolicy,
  /// A PUT on a transaction's root, its body streamed into the transaction's drive run by run and never held
  /// (§4.17.1 "A push's intake"); none for every other request.
  streamed: Option<Streamed>,
}

/// A PUT whose body is streamed into its transaction (A-681): the transaction, the path below its root, whether its file
/// is begun, and the refusal its drive met, after which the body's runs are read and dropped.
struct Streamed {
  /// The transaction's name.
  transaction: String,
  /// The path below the transaction's root.
  path: String,
  /// Whether the file's delta is open.
  begun: bool,
  /// The refusal met, which the PUT is answered with.
  refused: Option<HttpAnswer>,
}

/// The streamed PUT `method` on `path` names, when it is a PUT under a transaction's root (`!svn/txr/NAME/...`).
fn streamed_put(method: &str, path: &str) -> Option<Streamed> {
  if method != "PUT" {
    return None;
  }
  let segments: Vec<&str> = path.split('/').filter(|name| !name.is_empty()).collect();
  let [first, "txr", name, rest @ ..] = segments.as_slice() else {
    return None;
  };
  (*first == site::SPECIAL).then(|| Streamed {
    transaction: (*name).to_owned(),
    path: format!("/{}", rest.join("/")),
    begun: false,
    refused: None,
  })
}

/// The headers an HTTPv2 commit's requests carry (`libsvn_ra_serf/commit.c`), copied from the head.
#[derive(Clone, Debug, Default)]
struct CommitHeaders {
  /// `X-SVN-Version-Name`: an opened node's base revision.
  version_name: Option<u64>,
  /// `X-SVN-Base-Fulltext-MD5`.
  base_md5: Option<String>,
  /// `X-SVN-Result-Fulltext-MD5`.
  result_md5: Option<String>,
  /// `Destination`: a COPY's target URL.
  destination: Option<String>,
  /// `X-SVN-Version-Name` as sent, which a LOCK reads itself.
  version_text: Option<String>,
  /// `If`: the lock tokens a request presents.
  presented: Option<String>,
  /// `X-SVN-Options`: `lock-steal`, `lock-break`, `keep-locks`.
  options: Option<String>,
  /// `Timeout`: a LOCK's.
  timeout: Option<String>,
  /// `Lock-Token`: an UNLOCK's.
  lock_token: Option<String>,
}

impl<S: SvnStores> SvnExchange<S> {
  /// The exchange for `asked` on `repository`, its route, method and depth copied and charged to `room`; none past it
  /// or for a `Depth` that is no text.
  fn new(
    stores: S,
    repository: &RepositoryRecord,
    asked: Asked<'_, '_>,
    room: u64,
  ) -> Option<Self> {
    let mut admission = Admission::new(room);
    let method = String::from_utf8(asked.head.method.to_vec()).ok()?;
    let depth = match asked.head.field(DEPTH) {
      Ok(Some(value)) => Some(String::from_utf8(value.to_vec()).ok()?),
      Ok(None) => None,
      Err(_) => return None,
    };
    let held = to_u64(asked.routed.path.len())
      .saturating_add(to_u64(asked.routed.name.len()))
      .saturating_add(to_u64(method.len()))
      .saturating_add(to_u64(depth.as_ref().map_or(0, String::len)));
    let text = |name: &[u8]| match asked.head.field(name) {
      Ok(Some(value)) => String::from_utf8(value.to_vec()).ok(),
      _ => None,
    };
    let commit_headers = CommitHeaders {
      version_name: text(b"X-SVN-Version-Name").and_then(|value| value.trim().parse().ok()),
      base_md5: text(b"X-SVN-Base-Fulltext-MD5"),
      result_md5: text(b"X-SVN-Result-Fulltext-MD5"),
      destination: text(b"Destination"),
      version_text: text(b"X-SVN-Version-Name"),
      presented: text(b"If"),
      options: text(b"X-SVN-Options"),
      timeout: text(b"Timeout"),
      lock_token: text(b"Lock-Token"),
    };
    let held = [
      &commit_headers.destination,
      &commit_headers.version_text,
      &commit_headers.presented,
      &commit_headers.options,
      &commit_headers.timeout,
      &commit_headers.lock_token,
    ]
    .into_iter()
    .flatten()
    .fold(held, |held, value| held.saturating_add(to_u64(value.len())));
    admission.charge(held).ok()?;
    Some(Self {
      streamed: streamed_put(&method, &asked.routed.path),
      name: asked.routed.name.clone(),
      commit_headers,
      policy: repository.policy.clone(),
      stores,
      repository: repository.id,
      root: format!("/{}", asked.routed.name),
      principal: asked.principal,
      method,
      path: asked.routed.path,
      depth,
      body: Vec::new(),
      room: admission,
      request_room: room,
      answered: None,
      generation: None,
    })
  }
}

/// Format: `X-SVN-Options`' word asking a commit to keep the locks it presented (`SVN_DAV_OPTION_KEEP_LOCKS`).
const KEEP_LOCKS: &str = "keep-locks";
/// Format: `SVN_ERR_FS_GENERAL`, the error a lock answer that cannot be written is refused with.
const LOCK_UNWRITABLE: u32 = 160_000;
/// Format: `SVN_ERR_UNSUPPORTED_FEATURE`, the error a commit meeting a configured hook is refused with.
const UNSUPPORTED_FEATURE: u32 = 200_007;
/// The words a commit is refused with at a point where the repository configures a hook, which this site does not run
/// yet, as the svn+ssh tunnel words it.
const HOOK_NOT_RUN_WORDS: &str = "This server does not run this repository's hooks yet";
/// The words a change is refused with where its client hung up before its hook ended (A-733): the change does not go
/// on, and no one reads them.
const CLIENT_LEFT_WORDS: &str = "The client left before the repository's hook ended";
/// The words a LOCK is refused with where its pre-lock hook wrote more before any NUL than the site keeps of its output:
/// the token is not all here, so no lock is made with it.
const TOKEN_PAST_CAP_WORDS: &str =
  "The pre-lock hook's token is longer than the server keeps of its output";
/// The first status that is no success: a lock answered below it was made (RFC 9110 §15.4).
/// Format: RFC 9110 §15.4, the 3xx class.
const HTTP_REDIRECTION: u16 = 300;
/// The status a refused pre-hook is answered with where `dav_svn__convert_err` maps its error to none.
/// Format: `lock.c`, `HTTP_INTERNAL_SERVER_ERROR`.
const HTTP_INTERNAL_SERVER_ERROR: u16 = 500;
/// Counter: a post-lock hook that failed, its lock standing; logged as `mod_dav_svn` logs it, never told.
const POST_LOCK_FAILED: &str = "vcs_http.svn_post_lock_failed";
/// Counter: a post-unlock hook that failed, the lock gone; logged as `mod_dav_svn` logs it, never told.
const POST_UNLOCK_FAILED: &str = "vcs_http.svn_post_unlock_failed";

impl<S: SvnStores> SvnExchange<S> {
  /// The answer to an HTTPv2 commit's request (A-681 part C; `slates_svn::dav::commit`): a POST to `!svn/me`, a
  /// request on a transaction's root (`!svn/txr/NAME/...`) or on the transaction itself (`!svn/txn/NAME`), a COPY
  /// whose destination is in a transaction, or a MERGE; none for every other request, which the read side answers.
  async fn commit_side(&mut self, gone: ClientGone<'_>) -> Option<HttpAnswer> {
    if let Some(streamed) = self.streamed.take() {
      return Some(self.end_stream(streamed));
    }
    let path = self.path.clone();
    let segments: Vec<&str> = path.split('/').filter(|name| !name.is_empty()).collect();
    let method = self.method.clone();
    if let Some(answer) = self.on_special_path(method.as_str(), &segments) {
      return Some(answer);
    }
    Some(match (method.as_str(), segments.as_slice()) {
      ("POST", [first, "me"]) if *first == site::SPECIAL => self.post(gone).await,
      ("MERGE", _) => self.merge(gone).await,
      ("LOCK", _) => self.lock(gone).await,
      ("UNLOCK", _) => self.unlock(gone).await,
      _ => return None,
    })
  }

  /// The answer to a request on a transaction (`!svn/txn/NAME`: a DELETE aborts it, anything else is unserved), a COPY
  /// from a revision (`!svn/rvr` or `!svn/bc`), or any request on a transaction's root (`!svn/txr/NAME/...`); none for
  /// every other path. Asked before the commit's own POST, MERGE, LOCK and UNLOCK, as `commit_side` matched them.
  fn on_special_path(&mut self, method: &str, segments: &[&str]) -> Option<HttpAnswer> {
    let special = |segment: &str| segment == site::SPECIAL;
    Some(match (method, segments) {
      ("DELETE", [first, "txn", name]) if special(first) => {
        self.on_transaction(name, |commit, world, hooks| commit.abort(world, hooks))
      }
      (_, [first, "txn", _]) if special(first) => commit::unserved("This request on a transaction"),
      ("COPY", [first, "rvr" | "bc", revision, source @ ..]) if special(first) => {
        self.copy(revision, &format!("/{}", source.join("/")))
      }
      (method, [first, "txr", name, rest @ ..]) if special(first) => {
        self.on_transaction_root(method, name, &format!("/{}", rest.join("/")))
      }
      _ => return None,
    })
  }

  /// The hook answer of a request that meets no hook: the transaction's own requests between its POST and its MERGE
  /// (a PUT, a MKCOL, a DELETE, a COPY, a PROPPATCH), where the drive pauses for none; the POST and the MERGE run theirs
  /// (A-733). Refused, never passed, were one ever due.
  fn hooks(&self) -> impl Fn(&Due) -> Result<(), Vec<(u32, String)>> + use<S> {
    |_due: &Due| Err(vec![(UNSUPPORTED_FEATURE, HOOK_NOT_RUN_WORDS.to_owned())])
  }

  /// What this repository's hooks need of it (A-733): its id, name and REPOS.
  fn hooked(&self) -> Hooked {
    Hooked::new(self.repository, &self.name)
  }

  /// A POST making a transaction: its revision properties from the body's skel, the principal its author.
  async fn post(&mut self, gone: ClientGone<'_>) -> HttpAnswer {
    let user = crate::lfs::owner_name(&self.principal);
    let hook_user = user.clone();
    let properties = match commit::transaction_properties(&self.body, Some(&user)) {
      Ok(properties) => properties,
      Err(_) => return commit::unserved("A POST whose body is no create-txn skel"),
    };
    let (root, name, room) = (
      self.root.clone(),
      self.name.clone(),
      to_usize(self.request_room),
    );
    let setup_of = move |transaction: String, now: AprTime| Setup {
      base_path: String::from("/"),
      repository_url: root,
      filesystem: name,
      transaction,
      locks: CommitLocks {
        user: Some(user),
        presented: BTreeMap::new(),
        keep: false,
      },
      now,
      room,
      rules: None,
      revision_properties: properties,
    };
    let (transaction, due) =
      match self
        .stores
        .begin_commit(self.repository, &self.principal, setup_of)
      {
        Some(Ok(begun)) => begun,
        Some(Err(refused)) => return refused,
        None => return commit::unserved("Commits"),
      };
    let Some(due) = due else {
      return commit::created(&transaction);
    };
    // start-commit, run where `mod_dav_svn` runs it: once the transaction is made (A-733). Its capabilities are
    // `mergeinfo`, which every stock client announces in its requests' `DAV` header.
    let base = self
      .stores
      .on_commit(
        self.repository,
        &self.principal,
        &transaction,
        |commit, _| commit.pending_base(),
      )
      .flatten()
      .unwrap_or_default();
    let named = match &due {
      Due::StartCommit { transaction } | Due::PreCommit { transaction } => transaction.clone(),
    };
    let ran = run_hook(
      HookSite::Http(gone, &self.policy),
      &self.hooked(),
      to_usize(self.request_room),
      HookPoint::SvnStartCommit,
      |env, repos| {
        vcs_hooks::svn_start_commit(
          PendingView(base),
          env,
          repos,
          &hook_user,
          &[MERGEINFO],
          &named,
        )
      },
    )
    .await;
    let answer = ran
      .verdict
      .unwrap_or_else(|| Err(vec![(UNSUPPORTED_FEATURE, CLIENT_LEFT_WORDS.to_owned())]));
    match self.stores.on_commit(
      self.repository,
      &self.principal,
      &transaction,
      |commit, world| commit.resume_begin(world, answer),
    ) {
      Some(Some(Ok(()))) => commit::created(&transaction),
      Some(Some(Err(refused))) => refused,
      Some(None) => commit::no_such_transaction(&transaction),
      None => commit::unserved("Commits"),
    }
  }

  /// `step` on transaction `name`: `404` where no such transaction is open for the principal.
  fn on_transaction(
    &mut self,
    name: &str,
    step: impl FnOnce(&mut DavCommit, &CommitWorld<'_, SvnRecords>, Hooks<'_>) -> HttpAnswer,
  ) -> HttpAnswer {
    let hooks = self.hooks();
    match self
      .stores
      .on_commit(self.repository, &self.principal, name, |commit, world| {
        step(commit, world, &hooks)
      }) {
      Some(Some(answer)) => answer,
      Some(None) => commit::no_such_transaction(name),
      None => commit::unserved("Commits"),
    }
  }

  /// A request on transaction `name`'s root at `path`: PUT, MKCOL, DELETE or PROPPATCH.
  fn on_transaction_root(&mut self, method: &str, name: &str, path: &str) -> HttpAnswer {
    let headers = self.commit_headers.clone();
    let body = std::mem::take(&mut self.body);
    let bounds = xml_bounds(self.request_room);
    let method = method.to_owned();
    self.on_transaction(name, |commit, world, hooks| {
      for token in DavCommit::if_tokens(headers.presented.as_deref().unwrap_or_default()) {
        commit.present(path, token);
      }
      transaction_root_step(
        commit,
        (world, hooks),
        (&method, path),
        (&headers, &body, &bounds),
      )
    })
  }

  /// A COPY of the repository path `source` in `revision` to its `Destination` in a transaction.
  fn copy(&mut self, revision: &str, source: &str) -> HttpAnswer {
    let Ok(revision) = revision.parse::<u64>() else {
      return commit::unserved("A COPY from a revision that is no number");
    };
    let destination = self
      .commit_headers
      .destination
      .as_deref()
      .and_then(|destination| below_root(destination, &self.root));
    let Some(destination) = destination else {
      return commit::unserved("A COPY whose Destination is outside this repository");
    };
    let segments: Vec<&str> = destination
      .split('/')
      .filter(|name| !name.is_empty())
      .collect();
    let [first, "txr", name, rest @ ..] = segments.as_slice() else {
      return commit::unserved("A COPY whose Destination is no transaction's");
    };
    if *first != site::SPECIAL {
      return commit::unserved("A COPY whose Destination is no transaction's");
    }
    let target = format!("/{}", rest.join("/"));
    self.on_transaction(name, |commit, world, hooks| {
      commit.copy(world, hooks, (source, revision), &target)
    })
  }

  /// A MERGE of the transaction its body names: closed, judged, landed through the owner's step, answered with the new
  /// revision (`merge.c` `dav_svn__merge_response`).
  async fn merge(&mut self, gone: ClientGone<'_>) -> HttpAnswer {
    let bounds = xml_bounds(self.request_room);
    let Ok(request) = slates_svn::dav::request::merge(&self.body, &bounds) else {
      return commit::unserved("A MERGE whose body cannot be read");
    };
    let Some(source) = below_root(&request.source, &self.root) else {
      return commit::unserved("A MERGE of something outside this repository");
    };
    let segments: Vec<&str> = source.split('/').filter(|name| !name.is_empty()).collect();
    let [first, "txn", name] = segments.as_slice() else {
      return commit::unserved("A MERGE of an activity");
    };
    if *first != site::SPECIAL {
      return commit::unserved("A MERGE of an activity");
    }
    let name = (*name).to_owned();
    let target = self.path.clone();
    let keep = self
      .commit_headers
      .options
      .as_deref()
      .is_some_and(|options| options.contains(KEEP_LOCKS));
    let tokens = request.lock_tokens;
    let stepped =
      self
        .stores
        .on_commit(self.repository, &self.principal, &name, |commit, world| {
          for (path, token) in &tokens {
            commit.present(&format!("{}/{path}", target.trim_end_matches('/')), token);
          }
          commit.keep_locks(keep);
          let step = commit.merge_hooked(world);
          (step, commit.pending_base(), commit.pending_tokens())
        });
    let (step, base, presented) = match stepped {
      Some(Some(stepped)) => stepped,
      Some(None) => return commit::no_such_transaction(&name),
      None => return commit::unserved("Commits"),
    };
    let merged = match step {
      MergeStep::Done(merged) => merged,
      MergeStep::Hook(due) => {
        // pre-commit, run where `mod_dav_svn` runs it: before the verdict (A-733).
        let named = match &due {
          Due::StartCommit { transaction } | Due::PreCommit { transaction } => transaction.clone(),
        };
        let tokens: Vec<LockToken> = presented
          .into_iter()
          .map(|(path, token)| LockToken { path, token })
          .collect();
        let ran = run_hook(
          HookSite::Http(gone, &self.policy),
          &self.hooked(),
          to_usize(self.request_room),
          HookPoint::SvnPreCommit,
          |env, repos| vcs_hooks::svn_pre_commit(PendingView(base), env, repos, &named, &tokens),
        )
        .await;
        let answer = ran
          .verdict
          .unwrap_or_else(|| Err(vec![(UNSUPPORTED_FEATURE, CLIENT_LEFT_WORDS.to_owned())]));
        match self
          .stores
          .on_commit(self.repository, &self.principal, &name, |commit, world| {
            commit.resume_merge(world, answer)
          }) {
          Some(Some(merged)) => merged,
          Some(None) => return commit::no_such_transaction(&name),
          None => return commit::unserved("Commits"),
        }
      }
    };
    let work = match merged {
      Merged::Accepted(work) => work,
      Merged::Refused(refused) => return refused,
    };
    let transaction = work.record.transaction.clone();
    match self.stores.land(self.repository, &self.principal, *work) {
      Some(Ok(committed)) => {
        // post-commit, once the revision stands: its failure is the MERGE response's `S:post-commit-err` (A-733).
        let revision = committed.revision;
        let post = run_hook(
          HookSite::Http(gone, &self.policy),
          &self.hooked(),
          to_usize(self.request_room),
          HookPoint::SvnPostCommit,
          |env, repos| {
            vcs_hooks::svn_post_commit(CommittedView(revision), env, repos, revision, &transaction)
          },
        )
        .await;
        merged_answer(
          &self.root,
          &committed,
          to_usize(self.request_room),
          post.post_commit_error,
        )
      }
      Some(Err(uncommitted)) => not_committed(&uncommitted),
      None => commit::unserved("Commits"),
    }
  }
}

impl<S: SvnStores> SvnExchange<S> {
  /// One run of a streamed PUT's body handed to its transaction's drive (the file begun at the first run, its `If`
  /// tokens presented then), and held no longer; a run after a refusal is dropped.
  fn stream(&mut self, run: &[u8]) {
    let Some(mut streamed) = self.streamed.take() else {
      return;
    };
    if streamed.refused.is_none() {
      let outcome = self.put_step(&streamed, Some(run));
      streamed.begun = true;
      streamed.refused = outcome.err();
    }
    self.streamed = Some(streamed);
  }

  /// A streamed PUT's end: its refusal, or its file begun (a body with no run) and ended.
  fn end_stream(&mut self, streamed: Streamed) -> HttpAnswer {
    if let Some(refused) = streamed.refused {
      return refused;
    }
    let started = if streamed.begun {
      Ok(())
    } else {
      self.put_step(&streamed, None)
    };
    if let Err(refused) = started {
      return refused;
    }
    let hooks = self.hooks();
    self.on_transaction(&streamed.transaction, |commit, world, _| {
      commit.end_put(world, &hooks)
    })
  }

  /// One step of a streamed PUT on its transaction: the file begun where it is not yet, then `run` as the delta's next
  /// chunk where one is given.
  fn put_step(&mut self, streamed: &Streamed, run: Option<&[u8]>) -> Result<(), HttpAnswer> {
    let headers = self.commit_headers.clone();
    let hooks = self.hooks();
    let (begun, path) = (streamed.begun, streamed.path.as_str());
    let stepped = self.stores.on_commit(
      self.repository,
      &self.principal,
      &streamed.transaction,
      |commit, world| {
        if !begun {
          for token in DavCommit::if_tokens(headers.presented.as_deref().unwrap_or_default()) {
            commit.present(path, token);
          }
          let start = PutStart {
            base_md5: headers.base_md5.as_deref(),
            result_md5: headers.result_md5.as_deref(),
            version_name: headers.version_name,
          };
          commit.begin_put(world, &hooks, path, &start)?;
        }
        run.map_or(Ok(()), |run| commit.put_chunk(world, &hooks, run))
      },
    );
    match stepped {
      Some(Some(stepped)) => stepped,
      Some(None) => Err(commit::no_such_transaction(&streamed.transaction)),
      None => Err(commit::unserved("Commits")),
    }
  }

  /// A LOCK of the public path the request names (`slates_svn::dav::locking`, A-627): read, judged by the lock table
  /// at the owner's instant with a fresh token, its entries landed through the owner's step, and answered as
  /// `mod_dav_svn` answers it.
  async fn lock(&mut self, gone: ClientGone<'_>) -> HttpAnswer {
    let user = crate::lfs::owner_name(&self.principal);
    let path = public_path(&self.path);
    let headers = self.commit_headers.clone();
    let body = std::mem::take(&mut self.body);
    let bounds = xml_bounds(self.request_room);
    let (filesystem, room, depth) = (
      self.name.clone(),
      to_usize(self.request_room),
      self.depth.clone(),
    );
    // pre-lock, run where `svn_repos_fs_lock` runs it: once the request reads as a lock, before the table judges it
    // (A-733). A request refused before then is answered by the judge below, no hook run.
    let planned = self
      .stores
      .lend(self.repository, &self.principal, |lent| {
        let asked = LockHttp {
          path: &path,
          user: Some(&user),
          body: &body,
          depth: depth.as_deref(),
          timeout: headers.timeout.as_deref(),
          options: headers.options.as_deref(),
          version_name: headers.version_text.as_deref(),
        };
        let head = IndexHead(lent.index);
        locking::lock_request(&asked, &head, lent.now, &bounds)
          .ok()
          .map(|plan| (plan.request.comment, plan.request.steal))
      })
      .flatten();
    let mut token = None;
    let lock_planned = planned.is_some();
    if let Some((comment, steal)) = planned {
      let comment = comment
        .map(|comment| String::from_utf8_lossy(&comment).into_owned())
        .unwrap_or_default();
      let ran = run_hook(
        HookSite::Http(gone, &self.policy),
        &self.hooked(),
        room,
        HookPoint::SvnPreLock,
        |env, repos| {
          vcs_hooks::svn_pre_lock(CommittedView(0), env, repos, &path, &user, &comment, steal)
        },
      )
      .await;
      match (ran.verdict, ran.token) {
        (Some(Err(chain)), _) => return hook_refused(&chain),
        (None, _) => return hook_refused(&[(UNSUPPORTED_FEATURE, CLIENT_LEFT_WORDS.to_owned())]),
        (Some(Ok(())), Some(vcs_hooks::SvnLockToken::Given(given))) => {
          token = Some(String::from_utf8_lossy(&given).into_owned());
        }
        (Some(Ok(())), Some(vcs_hooks::SvnLockToken::PastCap)) => {
          return hook_refused(&[(LOCK_UNWRITABLE, TOKEN_PAST_CAP_WORDS.to_owned())]);
        }
        (Some(Ok(())), Some(vcs_hooks::SvnLockToken::Made) | None) => {}
      }
    }
    let judged = self
      .stores
      .lock_verb(self.repository, &self.principal, |lent, draw| {
        let asked = LockHttp {
          path: &path,
          user: Some(&user),
          body: &body,
          depth: depth.as_deref(),
          timeout: headers.timeout.as_deref(),
          options: headers.options.as_deref(),
          version_name: headers.version_text.as_deref(),
        };
        let head = IndexHead(lent.index);
        let mut plan = match locking::lock_request(&asked, &head, lent.now, &bounds) {
          Ok(plan) => plan,
          Err(refused) => return (Vec::new(), refused.answer()),
        };
        // pre-lock's token, where it gave one, is the lock's, judged as a client's is (`hooks.c`; A-733).
        if let Some(token) = &token
          && let Some(target) = plan.request.targets.get_mut(&path)
        {
          target.token = Some(token.clone());
        }
        let decision = match lent
          .locks
          .lock(&plan.request, &head, lent.now, room, &mut || draw())
        {
          Ok(decision) => decision,
          Err(refused) => {
            return (
              Vec::new(),
              answer_or_unwritable(locking::lock_answer(
                &plan,
                Err(&refused),
                lent.now,
                &filesystem,
              )),
            );
          }
        };
        let decided = decision
          .answers
          .iter()
          .find(|(named, _)| *named == path)
          .map(|(_, answer)| answer.as_ref());
        match decided {
          Some(decided) => {
            let answer =
              answer_or_unwritable(locking::lock_answer(&plan, decided, lent.now, &filesystem));
            (
              if decided.is_ok() {
                decision.entries
              } else {
                Vec::new()
              },
              answer,
            )
          }
          None => (
            Vec::new(),
            commit::unserved("A LOCK the table did not answer"),
          ),
        }
      });
    let answered = landed(judged);
    if lock_planned && answered.status < HTTP_REDIRECTION {
      // post-lock once the lock stands; its failure is logged, never told: the lock is made and its answer stands
      // (`lock.c` `append_locks`, SVN_ERR_REPOS_POST_LOCK_HOOK_FAILED).
      let ran = run_hook(
        HookSite::Http(gone, &self.policy),
        &self.hooked(),
        room,
        HookPoint::SvnPostLock,
        |env, repos| {
          vcs_hooks::svn_post_lock(CommittedView(0), env, repos, &user, &[path.as_str()])
        },
      )
      .await;
      if let Some(Err(_)) = ran.verdict {
        crate::fleet::count_refusal(POST_LOCK_FAILED);
      }
    }
    answered
  }

  /// An UNLOCK of the public path the request names, judged and landed as [`Self::lock`] is.
  async fn unlock(&mut self, gone: ClientGone<'_>) -> HttpAnswer {
    let user = crate::lfs::owner_name(&self.principal);
    let path = public_path(&self.path);
    let headers = self.commit_headers.clone();
    let filesystem = self.name.clone();
    let room = to_usize(self.request_room);
    // pre-unlock, run where `svn_repos_fs_unlock` runs it: once the request reads as an unlock, before the table judges
    // it (A-733).
    let planned = self
      .stores
      .lend(self.repository, &self.principal, |lent| {
        let asked = UnlockHttp {
          path: &path,
          user: Some(&user),
          lock_token: headers.lock_token.as_deref(),
          options: headers.options.as_deref(),
        };
        locking::unlock_request(&asked, lent.locks, lent.now)
          .ok()
          .map(|request| {
            let token = request.targets.get(&path).cloned().flatten();
            (token.unwrap_or_default(), request.break_lock)
          })
      })
      .flatten();
    if let Some((token, break_lock)) = &planned {
      let ran = run_hook(
        HookSite::Http(gone, &self.policy),
        &self.hooked(),
        room,
        HookPoint::SvnPreUnlock,
        |env, repos| {
          vcs_hooks::svn_pre_unlock(
            CommittedView(0),
            env,
            repos,
            &path,
            &user,
            token,
            *break_lock,
          )
        },
      )
      .await;
      match ran.verdict {
        Some(Err(chain)) => return hook_refused(&chain),
        None => return hook_refused(&[(UNSUPPORTED_FEATURE, CLIENT_LEFT_WORDS.to_owned())]),
        Some(Ok(())) => {}
      }
    }
    let judged = self
      .stores
      .lock_verb(self.repository, &self.principal, |lent, _| {
        let asked = UnlockHttp {
          path: &path,
          user: Some(&user),
          lock_token: headers.lock_token.as_deref(),
          options: headers.options.as_deref(),
        };
        let request = match locking::unlock_request(&asked, lent.locks, lent.now) {
          Ok(request) => request,
          Err(refused) => return (Vec::new(), refused.answer()),
        };
        match lent.locks.unlock(&request, lent.now) {
          Err(refused) => (
            Vec::new(),
            locking::unlock_answer(&request, Err(&refused), &filesystem),
          ),
          Ok(decision) => {
            let decided = decision
              .answers
              .iter()
              .find(|(named, _)| *named == path)
              .map(|(_, answer)| answer.as_ref().map(|_unit| ()));
            match decided {
              Some(decided) => {
                let answer = locking::unlock_answer(&request, decided, &filesystem);
                (
                  if decided.is_ok() {
                    decision.entries
                  } else {
                    Vec::new()
                  },
                  answer,
                )
              }
              None => (
                Vec::new(),
                commit::unserved("An UNLOCK the table did not answer"),
              ),
            }
          }
        }
      });
    let answered = landed(judged);
    if planned.is_some() && answered.status < HTTP_REDIRECTION {
      // post-unlock once the lock is gone; its failure is logged, never told (`lock.c` `remove_lock`,
      // SVN_ERR_REPOS_POST_UNLOCK_HOOK_FAILED).
      let ran = run_hook(
        HookSite::Http(gone, &self.policy),
        &self.hooked(),
        room,
        HookPoint::SvnPostUnlock,
        |env, repos| {
          vcs_hooks::svn_post_unlock(CommittedView(0), env, repos, &user, &[path.as_str()])
        },
      )
      .await;
      if let Some(Err(_)) = ran.verdict {
        crate::fleet::count_refusal(POST_UNLOCK_FAILED);
      }
    }
    answered
  }
}

/// A lock verb refused by its pre-hook, `chain` its errors, outermost first: answered as `dav_svn__convert_err` answers
/// the hook's error, its status from the error, `500` where it names none ("Failed to create new lock.", A-733).
fn hook_refused(chain: &[(u32, String)]) -> HttpAnswer {
  let (code, message) = chain
    .first()
    .cloned()
    .unwrap_or((UNSUPPORTED_FEATURE, String::new()));
  locking::error_answer(
    locking::convert_status(code, HTTP_INTERNAL_SERVER_ERROR),
    true,
    code,
    &message,
  )
}

/// A lock verb's answer once its entries landed: the step's refusal answered as a commit's is.
fn landed(judged: Option<Result<HttpAnswer, Uncommitted>>) -> HttpAnswer {
  match judged {
    Some(Ok(answer)) => answer,
    Some(Err(uncommitted)) => not_committed(&uncommitted),
    None => commit::unserved("Locks"),
  }
}

/// An answer the lock writers could write; one holding a value no header can carry is `500`, as httpd refuses it.
fn answer_or_unwritable(written: Result<HttpAnswer, slates_svn::dav::DavRefusal>) -> HttpAnswer {
  written
    .unwrap_or_else(|_| commit::refusal(LOCK_UNWRITABLE, "The lock's answer could not be written"))
}

/// A public URL's repository path, canonical: `/` for the root, no slash at the end.
fn public_path(path: &str) -> String {
  let names: Vec<&str> = path.split('/').filter(|name| !name.is_empty()).collect();
  format!("/{}", names.join("/"))
}

/// The head as the lock guard reads it: the index's youngest revision and its nodes (as `slates_svn::serve`'s lock
/// verbs read it).
struct IndexHead<'i>(&'i Index);

impl HeadView for IndexHead<'_> {
  fn youngest(&self) -> u64 {
    self.0.youngest()
  }

  fn node(&self, path: &str) -> Option<(NodeKind, u64)> {
    let node = self.0.node(path, self.0.youngest()).ok()??;
    Some((node.kind(), node.created_revision()))
  }
}

/// One request on a transaction's root (a MKCOL, DELETE or PROPPATCH at `path`; a PUT is streamed), its headers' tokens
/// presented.
fn transaction_root_step(
  commit: &mut DavCommit,
  (world, hooks): (&CommitWorld<'_, SvnRecords>, Hooks<'_>),
  (method, path): (&str, &str),
  (headers, body, bounds): (&CommitHeaders, &[u8], &XmlBounds),
) -> HttpAnswer {
  match method {
    "MKCOL" => commit.mkcol(world, hooks, path),
    "DELETE" => commit.delete(world, hooks, path, headers.version_name),
    "PROPPATCH" => commit.proppatch(world, hooks, (path, headers.version_name), (body, bounds)),
    _ => commit::unserved("This method on a transaction"),
  }
}

/// The path of `location`, a URL or a path, below the repository root `root`, decoded; none outside it.
fn below_root(location: &str, root: &str) -> Option<String> {
  let path = match location.split_once("://") {
    Some((_, rest)) => rest.find('/').and_then(|at| rest.get(at..)).unwrap_or("/"),
    None => location,
  };
  let mut decoded = Vec::with_capacity(path.len());
  percent::decode_into(path.as_bytes(), Plus::Plus, &mut decoded).ok()?;
  let decoded = String::from_utf8(decoded).ok()?;
  let rest = decoded.strip_prefix(root)?;
  (rest.is_empty() || rest.starts_with('/')).then(|| rest.to_owned())
}

/// The MERGE's answer for `committed` (`dav_svn__merge_response`), `200` with the new baseline's facts.
fn merged_answer(
  root: &str,
  committed: &Committed,
  room: usize,
  post_commit_error: Option<String>,
) -> HttpAnswer {
  let response = MergeResponse {
    vcc: format!("{root}/{}/vcc/default", site::SPECIAL),
    revision: committed.revision,
    date: committed.date.clone(),
    author: committed.author.clone(),
    post_commit_error,
  };
  match slates_svn::dav::response::merge(&response, room) {
    Ok(body) => HttpAnswer {
      status: Status::OK.code,
      headers: vec![("Content-Type", String::from("text/xml; charset=\"utf-8\""))],
      body,
    },
    Err(_) => commit::unserved("A MERGE answer past the room"),
  }
}

/// The MERGE's answer where the owner's step did not commit the work: its Subversion error, `409` for one out of
/// date, else as `dav_svn__convert_err` answers it.
fn not_committed(uncommitted: &Uncommitted) -> HttpAnswer {
  let (code, message) = match uncommitted.answered() {
    slates_svn::serve::Uncommitted::Step(refusal) => (refusal.subversion_code(), refusal.message()),
    slates_svn::serve::Uncommitted::NotServed => (
      UNSUPPORTED_FEATURE,
      String::from("This server does not make commits yet"),
    ),
    slates_svn::serve::Uncommitted::Refused { code, message } => (code, message),
  };
  commit::refusal(code, &message)
}

/// The status a site's answer names, by its code (RFC 9110 §15, RFC 4918 §11.1); a code this site never answers is
/// `500`.
fn status_of(code: u16) -> Status {
  [
    Status::OK,
    Status::CREATED,
    Status::NO_CONTENT,
    Status::MULTI_STATUS,
    Status::BAD_REQUEST,
    Status::FORBIDDEN,
    Status::NOT_FOUND,
    Status::CONFLICT,
    Status::PRECONDITION_FAILED,
    Status::LOCKED,
    Status::CONTENT_TOO_LARGE,
    Status::NOT_IMPLEMENTED,
  ]
  .into_iter()
  .find(|status| status.code == code)
  .unwrap_or(Status::INTERNAL_SERVER_ERROR)
}

impl<S: SvnStores> Exchange for SvnExchange<S> {
  async fn body(&mut self, run: &[u8]) -> Result<(), Failed> {
    if self.streamed.is_some() {
      self.stream(run);
      return Ok(());
    }
    self
      .room
      .charge(to_u64(run.len()))
      .map_err(|_| Failed(Status::CONTENT_TOO_LARGE))?;
    self.body.extend_from_slice(run);
    Ok(())
  }

  async fn body_end(&mut self, gone: ClientGone<'_>) -> Result<(), Failed> {
    if let Some(answer) = self.commit_side(gone).await {
      self.answered = Some(Answered::Whole {
        status: status_of(answer.status),
        headers: answer.headers,
        body: answer.body,
      });
      return Ok(());
    }
    let request = Request {
      method: &self.method,
      path: &self.path,
      depth: self.depth.as_deref(),
      body: &self.body,
    };
    let bounds = xml_bounds(self.request_room);
    let room = to_usize(self.request_room);
    let root = self.root.as_str();
    let answer = self
      .stores
      .lend(self.repository, &self.principal, |lent| {
        site::answer(&served_of(lent, root), &request, &bounds, room)
      })
      .ok_or(Failed(Status::SERVICE_UNAVAILABLE))?;
    let answered = match answer {
      SiteAnswer::Whole(whole) => Answered::Whole {
        status: status_of(whole.status),
        headers: whole.headers,
        body: whole.body,
      },
      SiteAnswer::Text { headers, text } => Answered::Text {
        headers,
        text,
        offset: 0,
      },
      SiteAnswer::Report(report) => {
        let hold = self
          .stores
          .hold(self.repository)
          .ok_or(Failed(Status::SERVICE_UNAVAILABLE))?;
        let held = self.generation.insert(hold);
        first_slices(
          &mut self.stores,
          (held, &self.principal, root),
          report,
          room,
        )
        .await?
      }
    };
    self.answered = Some(answered);
    Ok(())
  }

  fn reply_status(&self) -> Status {
    match &self.answered {
      Some(Answered::Whole { status, .. }) => *status,
      Some(Answered::Text { .. } | Answered::Report { .. }) => Status::OK,
      None => Status::INTERNAL_SERVER_ERROR,
    }
  }

  async fn reply_fields(&mut self) -> Result<Fields, Failed> {
    let headers = match &self.answered {
      Some(
        Answered::Whole { headers, .. }
        | Answered::Text { headers, .. }
        | Answered::Report { headers, .. },
      ) => headers,
      None => return Err(Failed(Status::INTERNAL_SERVER_ERROR)),
    };
    Ok(
      headers
        .iter()
        .map(|(name, value)| (name.as_bytes().to_vec(), value.as_bytes().to_vec()))
        .collect(),
    )
  }

  async fn reply(&mut self, out: &mut Vec<u8>) -> Result<bool, Failed> {
    match &mut self.answered {
      Some(Answered::Whole { body, .. }) => {
        out.append(body);
        Ok(false)
      }
      Some(Answered::Text { text, offset, .. }) => {
        let Some(text) = *text else {
          return Ok(false);
        };
        let at = *offset;
        let piece = self
          .stores
          .lend(self.repository, &self.principal, |lent| {
            lent
              .repository
              .text(&text.identity, at)
              .map(std::borrow::Cow::into_owned)
          })
          .flatten()
          .ok_or(Failed(Status::SERVICE_UNAVAILABLE))?;
        *offset = at.saturating_add(to_u64(piece.len()));
        out.extend_from_slice(&piece);
        Ok(!piece.is_empty() && *offset < text.size)
      }
      Some(Answered::Report {
        report,
        pending,
        done,
        ..
      }) => {
        if !pending.is_empty() || *done {
          out.append(pending);
          return Ok(!*done);
        }
        let root = self.root.as_str();
        let room = to_usize(self.request_room);
        let budget = slice_budget(room, self.stores.fetch_quantum());
        let hold = self
          .generation
          .as_ref()
          .ok_or(Failed(Status::INTERNAL_SERVER_ERROR))?;
        let diag_before = out.len();
        let diag_began = std::time::Instant::now();
        let progress = self
          .stores
          .lend_held(hold, &self.principal, |lent| {
            report.resume(&served_of(lent, root), out, budget)
          })
          .ok_or(Failed(Status::SERVICE_UNAVAILABLE))?;
        // DIAG (w-bugs, never for main): each reply run's bytes against the room, and the shard's long polls so far.
        let counters = slates_rt::registry::with_current(|ctx| ctx.counters()).unwrap_or_default();
        let wrote = out.len().saturating_sub(diag_before);
        let slice_ns = u64::try_from(diag_began.elapsed().as_nanos()).unwrap_or(u64::MAX);
        DIAG_NS.set(DIAG_NS.get().max(slice_ns));
        DIAG_PAST.set(
          DIAG_PAST.get() + u64::from(slice_ns > slates_rt::futures::step_budget_ns().unwrap_or(0)),
        );
        if wrote > room || matches!(progress, ReportProgress::Done) {
          slates_machine::log_line!(
            "diag-svnfetch dav report run: wrote {wrote}, most a run {}, slice_ns {slice_ns}, longest slice_ns so far {}, runs past the quantum so far {}, room {room}, done {}, quantum_ns {}, shard long_steps {}, longest_step_ns {}",
            DIAG_MOST.get().max(wrote),
            DIAG_NS.get(),
            DIAG_PAST.get(),
            matches!(progress, ReportProgress::Done),
            slates_rt::futures::step_budget_ns().unwrap_or(0),
            counters.long_steps,
            counters.longest_step_ns
          );
        }
        DIAG_MOST.set(DIAG_MOST.get().max(wrote));
        match progress {
          ReportProgress::More => Ok(true),
          ReportProgress::Done => {
            *done = true;
            Ok(false)
          }
          // Its first slices wrote its header, so a refusal now can only end the connection.
          ReportProgress::Refused(_) | ReportProgress::Broken => {
            Err(Failed(Status::INTERNAL_SERVER_ERROR))
          }
        }
      }
      None => Err(Failed(Status::INTERNAL_SERVER_ERROR)),
    }
  }
}

thread_local! {
  /// DIAG (w-bugs): the most one report run wrote on this thread.
  static DIAG_MOST: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
  /// DIAG (w-bugs): the longest report run, and the runs past the quantum, on this thread.
  static DIAG_NS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
  /// DIAG (w-bugs): see DIAG_NS.
  static DIAG_PAST: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
#[path = "tests/svn.rs"]
mod tests;
