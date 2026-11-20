//! ra_svn's lock verbs (`serve.c` `lock` 2954, `lock_many` 3034, `unlock` 3193, `unlock_many` 3225, Subversion
//! 1.15.0; A-693 part 2): `lock ( path:string ( [comment:string] ) steal:bool ( [rev:number] ) )`,
//! `lock-many ( ( [comment:string] ) steal:bool ( ( path:string ( [rev:number] ) ) ... ) )`,
//! `unlock ( path:string ( [token:string] ) break:bool )` and
//! `unlock-many ( break:bool ( ( path:string ( [token:string] ) ) ... ) )`.
//!
//! **Two steps** ([`Connection::lock_verb`], then [`Connection::locks_landed`]). The connection judges the request
//! against the lock table at the owner's instant (A-602's guard, `LockTable::lock` and `LockTable::unlock`, the head
//! read from the index) and hands its caller the entries it makes; the caller lands them through the owner's step (a
//! `Record::Locks`, durable as a revision is) and says whether they landed; then the connection answers as `svnserve`
//! does. So nothing of the table changes until the step has recorded it.
//!
//! **Access** (`must_have_access` for a write, a user needed unless an unlock breaks): an anonymous session that could
//! authenticate is asked to, for the command, and the verb runs again once it has (with no trivial authentication
//! request, as `svnserve` sends none after one); any other session that may not write is refused "Authorization failed"
//! (170001) with nothing before it; one that may is sent the trivial authentication request first. `lock-many` and `unlock-many` then judge each path (`lookup_access`), a path the session may not write
//! answered "Authorization failed" in its place.
//!
//! **Answers.** `lock`: `( success ( lock ) )`. `unlock`: `( success ( ) )`. `lock-many`: each path in the client's
//! order `( success lock )`, then `done`, then `( success ( ) )`. `unlock-many`: each path `( success ( path ) )`, then
//! `done`, then `( success ( ) )`. A path refused is its failure in FSFS's words in its place; a request the guard
//! refuses whole (no user, a comment XML cannot carry) answers each path "No result for '…'." (160059) and then fails
//! with the refusal after `done`. A lock is written as `write_lock` writes it (`crate::reads::locks`).
//!
//! **Not here.** The pre-lock and post-lock hooks run nowhere yet (GAPS).

use std::collections::BTreeMap;

use super::{Connection, Phase, Refused, Sources, command_of, principal_name, write_failure};
use crate::commit::drive::{self as commit_drive, may_write};
use crate::index::{Index, NodeKind, Revision};
use crate::lock::{
  HeadView, Lock, LockEntry, LockRefusal, LockRequest, LockTarget, UUID_BYTES, UnlockRequest,
};
use crate::ra_svn::{self, Value};
use crate::report::ReportFailure;

/// `lock`'s command word.
const LOCK: &str = "lock";
/// `lock-many`'s command word.
const LOCK_MANY: &str = "lock-many";
/// `unlock`'s command word.
const UNLOCK: &str = "unlock";
/// `unlock-many`'s command word.
const UNLOCK_MANY: &str = "unlock-many";

/// Format: `SVN_ERR_FS_LOCK_OPERATION_FAILED` (`svn_error_codes.h`: the filesystem category's start, 160,000, plus 59).
const LOCK_OPERATION_FAILED: u32 = 160_059;
/// Format: `SVN_ERR_RA_NOT_AUTHORIZED` (`svn_error_codes.h`: the RA category's start, 170,000, plus 1).
const NOT_AUTHORIZED: u32 = 170_001;
/// `SVN_ERR_RA_NOT_AUTHORIZED`'s own words, which `error_create_and_log` gives a path the session may not write.
const NOT_AUTHORIZED_WORDS: &str = "Authorization failed";
/// The word `lock-many` and `unlock-many` end their paths' answers with.
const DONE: &str = "done";
/// The word of a command's success.
const SUCCESS: &str = "success";
/// ra_svn's true.
const TRUE: &str = "true";
/// ra_svn's false.
const FALSE: &str = "false";

/// Whether `name` is one of the lock verbs.
pub(super) fn is_lock_verb(name: &str) -> bool {
  matches!(name, LOCK | LOCK_MANY | UNLOCK | UNLOCK_MANY)
}

/// A refusal's code and words, as a command failure carries them.
type Failure = (u32, String);

/// What a verb asked, read from its item.
struct Asked {
  /// Which verb.
  verb: Verb,
  /// `lock`'s comment.
  comment: Option<Vec<u8>>,
  /// `steal-lock` or `break-lock`.
  force: bool,
  /// Each path as the client wrote it, with its full path and what it brings: a lock's revision or an unlock's token.
  targets: Vec<Target>,
}

/// One path a verb names.
struct Target {
  /// As the client wrote it, which `unlock-many` answers with.
  written: Vec<u8>,
  /// The repository path, the session's base joined with it, canonical.
  full: String,
  /// The working copy's revision a lock names.
  revision: Option<Revision>,
  /// The token an unlock sends.
  token: Option<String>,
}

/// The lock verbs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verb {
  /// `lock`.
  Lock,
  /// `lock-many`.
  LockMany,
  /// `unlock`.
  Unlock,
  /// `unlock-many`.
  UnlockMany,
}

impl Verb {
  /// Whether it locks.
  fn locks(self) -> bool {
    matches!(self, Self::Lock | Self::LockMany)
  }

  /// Whether it names many paths.
  fn many(self) -> bool {
    matches!(self, Self::LockMany | Self::UnlockMany)
  }
}

/// A verb judged, its entries with the caller: each path's answer, and the whole request's refusal if any.
pub(super) struct Pending {
  /// Which verb.
  verb: Verb,
  /// Each path's answer in the client's order: its lock (an unlock's none), or its failure.
  answers: Vec<(Vec<u8>, Result<Option<Lock>, Failure>)>,
  /// The whole request's refusal.
  whole: Option<Failure>,
}

/// The head as the lock guard reads it: the index's youngest revision and its nodes.
struct IndexHead<'i>(&'i Index);

impl HeadView for IndexHead<'_> {
  fn youngest(&self) -> Revision {
    self.0.youngest()
  }

  fn node(&self, path: &str) -> Option<(NodeKind, Revision)> {
    let node = self.0.node(path, self.0.youngest()).ok()??;
    Some((node.kind(), node.created_revision()))
  }
}

impl Connection {
  /// Judges the lock verb the client asked for ([`super::Turn::LockAsked`]) against `sources`' lock table at their
  /// instant (module documentation): the entries it makes, which the caller lands through the owner's step and then
  /// reports with [`Connection::locks_landed`]; or none, the verb answered already (refused for access), the
  /// connection waiting on its next command. New tokens are drawn from `draw`; the table holds at most `room` locks.
  ///
  /// # Errors
  /// [`Refused::OutOfTurn`] unless a lock verb was asked; [`Refused::Malformed`] for parameters `svnserve` cannot read,
  /// which end the connection; [`Refused::Item`] where an answer cannot be written.
  pub fn lock_verb<R: ?Sized>(
    &mut self,
    sources: &Sources<'_, R>,
    draw: &mut impl FnMut() -> [u8; UUID_BYTES],
    room: usize,
    out: &mut Vec<u8>,
  ) -> Result<Option<Vec<LockEntry>>, Refused> {
    let (established, item, authenticated_now) =
      match std::mem::replace(&mut self.phase, Phase::Ended) {
        Phase::LockAsked(established, item, authenticated_now) => {
          (established, item, authenticated_now)
        }
        other => {
          self.phase = other;
          return Err(Refused::OutOfTurn);
        }
      };
    let value = self.whole_item(&item)?;
    let (name, params) = command_of(&value).ok_or(Refused::Malformed)?;
    let asked = read(name, params, &established.base).ok_or(Refused::Malformed)?;
    let user = principal_name(&established.session.principal)
      .ok()
      .flatten()
      .map(str::to_owned);
    let kept = &established.mechanisms;
    let session = commit_drive::Session {
      base_path: &established.base,
      repository_url: &established.repository_url,
      filesystem: sources.policy.filesystem,
      user: user.as_deref(),
      anonymous: sources.policy.anonymous,
      authenticated: sources.policy.authenticated,
      can_authenticate: kept.tunnel_user.is_some() || kept.cram_md5,
      authz: sources.policy.authz,
    };
    let needs_user = asked.verb.locks() || !asked.force;
    let checked = if asked.verb.many() {
      None
    } else {
      asked.targets.first().map(|target| target.full.as_str())
    };
    if !may_write(&session, checked, needs_user) {
      // An anonymous session that could authenticate is asked to, for this command (`must_have_access`), and the verb
      // runs again once it has; any other is refused with nothing before it.
      let asked = if user.is_none() && session.can_authenticate && !authenticated_now {
        self.ask(
          &established,
          &sources.policy,
          crate::authz::Access::Write,
          out,
        )?
      } else {
        None
      };
      match asked {
        Some(server) => {
          let interrupted = Box::new(super::Interrupted::Lock { command: item });
          self.phase = Phase::Authenticating(established, server, interrupted);
        }
        None => {
          ReportFailure::NotAuthorized
            .write(out)
            .map_err(Refused::Report)?;
          self.phase = Phase::Commands(established);
        }
      }
      return Ok(None);
    }
    if !authenticated_now {
      trivial_auth(out)?;
    }
    let allowed =
      |target: &Target| !asked.verb.many() || may_write(&session, Some(&target.full), needs_user);
    let judged = Judged {
      asked: &asked,
      user: user.as_deref(),
      filesystem: sources.policy.filesystem,
    };
    let (entries, pending) = if asked.verb.locks() {
      let head = IndexHead(sources.index);
      judged.lock(&allowed, |request| {
        sources
          .locks
          .lock(request, &head, sources.now, room, &mut *draw)
      })
    } else {
      judged.unlock(&allowed, |request| {
        sources.locks.unlock(request, sources.now)
      })
    };
    self.phase = Phase::Locking(established, Box::new(pending));
    Ok(Some(entries))
  }

  /// Answers the lock verb judged last ([`Connection::lock_verb`]) once its entries `landed` through the owner's step,
  /// or with the step's failure in each judged lock's place where they did not (module documentation).
  ///
  /// # Errors
  /// [`Refused::OutOfTurn`] unless a lock verb's entries are with the caller; [`Refused::Item`] where an answer
  /// cannot be written.
  pub fn locks_landed(
    &mut self,
    landed: Result<(), Failure>,
    out: &mut Vec<u8>,
  ) -> Result<(), Refused> {
    let (established, mut pending) = match std::mem::replace(&mut self.phase, Phase::Ended) {
      Phase::Locking(established, pending) => (established, pending),
      other => {
        self.phase = other;
        return Err(Refused::OutOfTurn);
      }
    };
    if let Err(failure) = landed {
      for (_, answer) in &mut pending.answers {
        if answer.is_ok() {
          *answer = Err(failure.clone());
        }
      }
    }
    answer(&pending, out)?;
    self.phase = Phase::Commands(established);
    Ok(())
  }
}

/// A request being judged: what was asked, by whom, in which filesystem's words.
struct Judged<'a> {
  /// What was asked.
  asked: &'a Asked,
  /// The session's user.
  user: Option<&'a str>,
  /// The filesystem's name, which FSFS's words name.
  filesystem: &'a str,
}

impl Judged<'_> {
  /// A lock request over the allowed paths, judged by `guard`: the entries, and each path's answer.
  fn lock(
    &self,
    allowed: &impl Fn(&Target) -> bool,
    guard: impl FnOnce(
      &LockRequest,
    ) -> Result<crate::lock::Decision<Result<Lock, LockRefusal>>, LockRefusal>,
  ) -> (Vec<LockEntry>, Pending) {
    let targets = self
      .asked
      .targets
      .iter()
      .filter(|target| allowed(target))
      .map(|target| {
        let lock_target = LockTarget {
          current: target.revision,
          token: None,
        };
        (target.full.clone(), lock_target)
      })
      .collect();
    let request = LockRequest {
      user: self.user.map(str::to_owned),
      comment: self.asked.comment.clone(),
      is_dav_comment: false,
      expires: None,
      steal: self.asked.force,
      targets,
    };
    match guard(&request) {
      Ok(decision) => {
        let by_path: BTreeMap<String, Result<Option<Lock>, Failure>> = decision
          .answers
          .into_iter()
          .map(|(path, answer)| {
            (
              path,
              answer.map(Some).map_err(|refusal| self.words(&refusal)),
            )
          })
          .collect();
        (decision.entries, self.pending(allowed, &by_path, None))
      }
      Err(refusal) => (
        Vec::new(),
        self.pending(allowed, &BTreeMap::new(), Some(self.words(&refusal))),
      ),
    }
  }

  /// An unlock request over the allowed paths, judged by `guard`: the entries, and each path's answer.
  fn unlock(
    &self,
    allowed: &impl Fn(&Target) -> bool,
    guard: impl FnOnce(
      &UnlockRequest,
    ) -> Result<crate::lock::Decision<Result<(), LockRefusal>>, LockRefusal>,
  ) -> (Vec<LockEntry>, Pending) {
    let targets = self
      .asked
      .targets
      .iter()
      .filter(|target| allowed(target))
      .map(|target| (target.full.clone(), target.token.clone()))
      .collect();
    let request = UnlockRequest {
      user: self.user.map(str::to_owned),
      break_lock: self.asked.force,
      targets,
    };
    match guard(&request) {
      Ok(decision) => {
        let by_path: BTreeMap<String, Result<Option<Lock>, Failure>> = decision
          .answers
          .into_iter()
          .map(|(path, answer)| {
            (
              path,
              answer
                .map(|()| None)
                .map_err(|refusal| self.words(&refusal)),
            )
          })
          .collect();
        (decision.entries, self.pending(allowed, &by_path, None))
      }
      Err(refusal) => (
        Vec::new(),
        self.pending(allowed, &BTreeMap::new(), Some(self.words(&refusal))),
      ),
    }
  }

  /// Each path's answer in the client's order (module documentation): its own where the guard judged it, "Authorization
  /// failed" where the session may not write it, and "No result" where the whole request was refused.
  fn pending(
    &self,
    allowed: &impl Fn(&Target) -> bool,
    by_path: &BTreeMap<String, Result<Option<Lock>, Failure>>,
    whole: Option<Failure>,
  ) -> Pending {
    let answers = self
      .asked
      .targets
      .iter()
      .map(|target| {
        let answer = if !allowed(target) {
          Err((NOT_AUTHORIZED, NOT_AUTHORIZED_WORDS.to_owned()))
        } else if let Some(answer) = by_path.get(&target.full) {
          answer.clone()
        } else {
          let written = String::from_utf8_lossy(&target.written);
          Err((LOCK_OPERATION_FAILED, format!("No result for '{written}'.")))
        };
        (target.written.clone(), answer)
      })
      .collect();
    Pending {
      verb: self.asked.verb,
      answers,
      whole,
    }
  }

  /// FSFS's words for `refusal` (`lock.c` `lock_body` and `unlock_body`, the messages of `svn_fs_util.h`), the lock
  /// guard's shared words where the DAV site's are the same.
  fn words(&self, refusal: &LockRefusal) -> Failure {
    let filesystem = self.filesystem;
    let message = match refusal {
      LockRefusal::NoUser { .. } if self.asked.verb.locks() => {
        "Cannot lock path, no authenticated username available.".to_owned()
      }
      LockRefusal::NoUser { .. } => {
        "Cannot unlock path, no authenticated username available".to_owned()
      }
      LockRefusal::OwnerMismatch { owner, .. } => format!(
        "User '{}' is trying to use a lock owned by '{owner}' in filesystem '{filesystem}'",
        self.user.unwrap_or_default()
      ),
      other => crate::dav::locking::lock_error_message(other, filesystem)
        .unwrap_or_else(|| format!("{other:?}")),
    };
    (refusal.subversion_code(), message)
  }
}

/// The verb `name`'s request read from `params` (module documentation), each path joined with `base`; none where a
/// parameter is not what the verb's prototype names, which `svnserve` reads as malformed.
fn read(name: &str, params: &[Value<'_>], base: &str) -> Option<Asked> {
  match (name, params) {
    (
      LOCK,
      [
        Value::String(path),
        Value::List(comment),
        Value::Word(steal),
        Value::List(revision),
        ..,
      ],
    ) => Some(Asked {
      verb: Verb::Lock,
      comment: optional_string(comment)?,
      force: boolean(steal)?,
      targets: vec![target(path, base, optional_revision(revision)?, None)],
    }),
    (
      LOCK_MANY,
      [
        Value::List(comment),
        Value::Word(steal),
        Value::List(paths),
        ..,
      ],
    ) => Some(Asked {
      verb: Verb::LockMany,
      comment: optional_string(comment)?,
      force: boolean(steal)?,
      targets: paths
        .iter()
        .map(|item| match item {
          Value::List(parts) => match parts.as_slice() {
            [Value::String(path), Value::List(revision), ..] => {
              Some(target(path, base, optional_revision(revision)?, None))
            }
            _ => None,
          },
          _ => None,
        })
        .collect::<Option<_>>()?,
    }),
    (
      UNLOCK,
      [
        Value::String(path),
        Value::List(token),
        Value::Word(force),
        ..,
      ],
    ) => Some(Asked {
      verb: Verb::Unlock,
      comment: None,
      force: boolean(force)?,
      targets: vec![target(path, base, None, token_of(token)?)],
    }),
    (UNLOCK_MANY, [Value::Word(force), Value::List(paths), ..]) => Some(Asked {
      verb: Verb::UnlockMany,
      comment: None,
      force: boolean(force)?,
      targets: paths
        .iter()
        .map(|item| match item {
          Value::List(parts) => match parts.as_slice() {
            [Value::String(path), Value::List(token), ..] => {
              Some(target(path, base, None, token_of(token)?))
            }
            _ => None,
          },
          _ => None,
        })
        .collect::<Option<_>>()?,
    }),
    _ => None,
  }
}

/// A path the verb names (module documentation).
fn target(path: &[u8], base: &str, revision: Option<Revision>, token: Option<String>) -> Target {
  Target {
    written: path.to_vec(),
    full: crate::reads::full_path(base, path),
    revision,
    token,
  }
}

/// An optional string, `( [string] )`: none for an empty list, nothing for anything else.
fn optional_string(items: &[Value<'_>]) -> Option<Option<Vec<u8>>> {
  match items {
    [] => Some(None),
    [Value::String(bytes)] => Some(Some(bytes.to_vec())),
    _ => None,
  }
}

/// An optional token, a string that must be UTF-8.
fn token_of(items: &[Value<'_>]) -> Option<Option<String>> {
  match optional_string(items)? {
    None => Some(None),
    Some(bytes) => String::from_utf8(bytes).ok().map(Some),
  }
}

/// An optional revision, `( [number] )`.
fn optional_revision(items: &[Value<'_>]) -> Option<Option<Revision>> {
  match items {
    [] => Some(None),
    [Value::Number(number)] => Some(Some(*number)),
    _ => None,
  }
}

/// ra_svn's boolean, a word.
fn boolean(word: &str) -> Option<bool> {
  match word {
    TRUE => Some(true),
    FALSE => Some(false),
    _ => None,
  }
}

/// `trivial_auth_request`: `( success ( ( ) 0: ) )`.
fn trivial_auth(out: &mut Vec<u8>) -> Result<(), Refused> {
  let request = Value::List(vec![
    Value::Word(SUCCESS),
    Value::List(vec![Value::List(Vec::new()), Value::String(b"")]),
  ]);
  ra_svn::write(&request, out).map_err(Refused::Item)
}

/// `( success params )`.
fn success(params: Value<'_>, out: &mut Vec<u8>) -> Result<(), Refused> {
  ra_svn::write(&Value::List(vec![Value::Word(SUCCESS), params]), out).map_err(Refused::Item)
}

/// The verb's answer (module documentation).
fn answer(pending: &Pending, out: &mut Vec<u8>) -> Result<(), Refused> {
  if !pending.verb.many() {
    let Some((_, answer)) = pending.answers.first() else {
      return write_failure(
        &[pending
          .whole
          .clone()
          .unwrap_or((LOCK_OPERATION_FAILED, String::new()))],
        out,
      );
    };
    let answer = match (&pending.whole, answer) {
      (Some(whole), _) => Err(whole.clone()),
      (None, answer) => answer.clone(),
    };
    return match answer {
      Ok(Some(lock)) => {
        crate::reads::locks::with_lock_value(&lock, |value| success(Value::List(vec![value]), out))
      }
      Ok(None) => success(Value::List(Vec::new()), out),
      Err(failure) => write_failure(&[failure], out),
    };
  }
  for (written, answer) in &pending.answers {
    match answer {
      Ok(Some(lock)) => crate::reads::locks::with_lock_value(lock, |value| success(value, out))?,
      Ok(None) => success(Value::List(vec![Value::String(written)]), out)?,
      Err(failure) => write_failure(std::slice::from_ref(failure), out)?,
    }
  }
  ra_svn::write(&Value::Word(DONE), out).map_err(Refused::Item)?;
  match &pending.whole {
    Some(whole) => write_failure(std::slice::from_ref(whole), out),
    None => success(Value::List(Vec::new()), out),
  }
}
