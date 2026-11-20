//! ra_svn's revision property changes (`serve.c` `change_rev_prop` 1266, `change_rev_prop2` 1226 and
//! `do_change_rev_prop` 1198, `fs-wrap.c` `svn_repos_fs_change_rev_prop4`, Subversion 1.15.0; A-702 parts 1 and 2):
//! `change-rev-prop ( rev:number name:string [ value:string ] )` and
//! `change-rev-prop2 ( rev:number name:string ( [ value:string ] ) ( dont-care:bool [ previous-value:string ] ) )`.
//!
//! **In `svnserve`'s order.** `change-rev-prop2` naming both `dont-care` and a previous value is refused first,
//! "'previous-value' and 'dont-care' cannot both be set in 'change-rev-prop2' request" (200004), with nothing before it.
//! Then write access anywhere in the repository, no user needed (`must_have_access(..., svn_authz_write, NULL,
//! FALSE)`): an anonymous session that could authenticate is asked to, for the command, and it runs again once it has
//! (with no trivial authentication request after one); any other session that may not write is refused "Authorization
//! failed" (170001); one that may is sent the trivial authentication request. Then the revision must be readable
//! whole, "Write denied:  not authorized to read all of revision N" (220001), a revision the index does not hold "No
//! such revision N" (160006); then `svn_repos__validate_prop` (the commit drive's words); then, where no previous
//! value was named, the revision must exist to read it from (160006).
//!
//! **Then the hooks** (A-702 part 2), handed to the caller as the lock verbs hand theirs (A-730): the change's
//! pre-revprop-change call ([`RevisionPropertyCall`]: the revision, the user, the name, the action and the new value,
//! the action read against the value the client expects, else the one held), on whose refusal its chain is the answer;
//! a repository with none refuses every change 165006 in the hooks' runner (`svn_hooks`). Allowed, the change is judged
//! at the owner's instant ([`Connection::revision_property_hooked`]): a revision not held "No such revision N"
//! (160006), a held value other than the one expected "revprop 'NAME' has unexpected value in filesystem"
//! (`SVN_ERR_FS_PROP_BASEVALUE_MISMATCH`, FSFS's `change_rev_prop`); else the revision's properties whole, which the
//! caller lands through the owner's step (`Record::RevisionProperties`). Landed, post-revprop-change is handed out with
//! the old value ([`Connection::revision_property_landed`]); its verdict ends the command, `( success ( ) )` or its
//! chain ([`Connection::revision_property_finished`]).

use super::Phase;
use super::{
  Connection, Established, Interrupted, Refused, Sources, principal_name, write_failure,
};
use crate::commit::drive::{self as commit_drive, may_write, revision_property_errors};
use crate::commit::record::Record;
use crate::index::Revision;
use crate::migration::Properties;
use crate::ra_svn::{self, Value};
use crate::reads;
use crate::report::ReportFailure;

/// `change-rev-prop`'s command word.
const CHANGE_REV_PROP: &str = "change-rev-prop";
/// `change-rev-prop2`'s command word.
const CHANGE_REV_PROP2: &str = "change-rev-prop2";

/// Format: `SVN_ERR_INCORRECT_PARAMS` (`svn_error_codes.h`: the miscellaneous category's start, 200,000, plus 4).
const INCORRECT_PARAMS: u32 = 200_004;
/// Format: `SVN_ERR_AUTHZ_UNREADABLE` (`svn_error_codes.h`: the authorization category's start, 220,000, plus 1).
const AUTHZ_UNREADABLE: u32 = 220_001;
/// Format: `SVN_ERR_FS_NO_SUCH_REVISION` (`svn_error_codes.h`: the filesystem category's start, 160,000, plus 6).
const NO_SUCH_REVISION: u32 = 160_006;
/// Format: `SVN_ERR_FS_PROP_BASEVALUE_MISMATCH` (`svn_error_codes.h`: the filesystem category's start, 160,000, plus
/// 49).
const BASEVALUE_MISMATCH: u32 = 160_049;
/// `change_rev_prop2`'s words for a request naming both `dont-care` and a previous value.
const BOTH_WORDS: &str =
  "'previous-value' and 'dont-care' cannot both be set in 'change-rev-prop2' request";
/// The word of a command's success.
const SUCCESS: &str = "success";
/// ra_svn's true.
const TRUE: &str = "true";
/// ra_svn's false.
const FALSE: &str = "false";

/// Whether `name` is one of the revision property changes.
pub(super) fn is_revision_property_change(name: &str) -> bool {
  matches!(name, CHANGE_REV_PROP | CHANGE_REV_PROP2)
}

/// A change read from its item.
struct Change {
  /// The revision.
  revision: Revision,
  /// The property's name.
  name: Vec<u8>,
  /// The new value; none deletes it.
  value: Option<Vec<u8>>,
  /// The value the client expects the property to hold, none for absent (`change-rev-prop2` without `dont-care`).
  expected: Option<Option<Vec<u8>>>,
  /// Whether it named both `dont-care` and a previous value.
  both: bool,
}

/// What the change's answer leads to.
pub(super) enum Changed {
  /// Answered here.
  Answered,
  /// Past its checks: its pre-revprop-change call is the caller's to run (module documentation).
  Hooking(Box<Pending>),
  /// An authentication for the command was asked: it runs, then the change again.
  Authenticating(Box<crate::handshake::Server>, Box<Interrupted>),
}

impl Connection {
  /// Answers the revision property change `name` with `params`, whose item is `item` (module documentation), on
  /// `established`; `authenticated_now` once the client has just authenticated for it.
  pub(super) fn change_revision_property<R: reads::Repository + ?Sized>(
    &self,
    established: &mut Established,
    sources: &Sources<'_, R>,
    (name, params, item): (&str, &[Value<'_>], &[u8]),
    authenticated_now: bool,
    out: &mut Vec<u8>,
  ) -> Result<Changed, Refused> {
    let change = read(name, params).ok_or(Refused::Malformed)?;
    if change.both {
      write_failure(&[(INCORRECT_PARAMS, BOTH_WORDS.to_owned())], out)?;
      return Ok(Changed::Answered);
    }
    let user = principal_name(&established.session.principal)
      .ok()
      .flatten();
    let author = user.unwrap_or_default().to_owned();
    let kept = &established.mechanisms;
    let session = commit_drive::Session {
      base_path: &established.base,
      repository_url: &established.repository_url,
      filesystem: sources.policy.filesystem,
      user,
      anonymous: sources.policy.anonymous,
      authenticated: sources.policy.authenticated,
      can_authenticate: kept.tunnel_user.is_some() || kept.cram_md5,
      authz: sources.policy.authz,
    };
    if !may_write(&session, None, false) {
      let asked = if user.is_none() && session.can_authenticate && !authenticated_now {
        self.ask(
          established,
          &sources.policy,
          crate::authz::Access::Write,
          out,
        )?
      } else {
        None
      };
      return match asked {
        Some(server) => Ok(Changed::Authenticating(
          server,
          Box::new(Interrupted::RevisionProperty {
            command: item.to_vec(),
          }),
        )),
        None => {
          ReportFailure::NotAuthorized
            .write(out)
            .map_err(Refused::Report)?;
          Ok(Changed::Answered)
        }
      };
    }
    if !authenticated_now {
      trivial_auth(out)?;
    }
    let rules = established.compiled_read_rules(&sources.policy);
    if let Some(refusal) = judged(sources.index, rules, &change) {
      write_failure(&refusal, out)?;
      return Ok(Changed::Answered);
    }
    let old = match &change.expected {
      Some(expected) => expected.clone(),
      None => held_value(sources.repository, change.revision, &change.name),
    };
    let action = match (&change.value, &old) {
      (None, _) => RevisionPropertyAction::Deleted,
      (Some(_), None) => RevisionPropertyAction::Added,
      (Some(_), Some(_)) => RevisionPropertyAction::Modified,
    };
    let call = RevisionPropertyCall {
      revision: change.revision,
      user: author,
      name: String::from_utf8_lossy(&change.name).into_owned(),
      action,
      value: change.value.clone().unwrap_or_default(),
    };
    Ok(Changed::Hooking(Box::new(Pending {
      change,
      old,
      call,
      stage: Stage::Hooking,
    })))
  }

  /// Goes on with the revision property change whose pre-revprop-change call was handed out
  /// ([`super::Turn::RevisionPropertyHook`]) once the hook's `verdict` is in (module documentation): the record of the
  /// revision's properties whole, which the caller lands through the owner's step with no wait between and reports
  /// ([`Connection::revision_property_landed`]); or none, the change answered already, the connection waiting on its
  /// next command.
  ///
  /// # Errors
  /// [`Refused::OutOfTurn`] unless a change's pre-hook was handed out; [`Refused::Item`] where an answer cannot be
  /// written.
  pub fn revision_property_hooked<R: reads::Repository + ?Sized>(
    &mut self,
    sources: &Sources<'_, R>,
    verdict: Result<(), Vec<(u32, String)>>,
    out: &mut Vec<u8>,
  ) -> Result<Option<Record>, Refused> {
    let (established, mut pending) = self.revision_property_at(Stage::Hooking)?;
    let judged = verdict.and_then(|()| changed_properties(sources.repository, &pending.change));
    match judged {
      Ok(properties) => {
        pending.stage = Stage::Landing;
        let revision = pending.change.revision;
        self.phase = Phase::RevisionProperty(established, pending);
        Ok(Some(Record::RevisionProperties {
          revision,
          properties,
        }))
      }
      Err(chain) => {
        self.phase = Phase::Commands(established);
        write_failure(&chain, out)?;
        Ok(None)
      }
    }
  }

  /// Goes on with the revision property change whose record the caller `landed`, or failed to with the step's
  /// failure, which is then the answer: post-revprop-change's call, the old value its input, for the caller to run and
  /// report ([`Connection::revision_property_finished`]); or none, the change answered already.
  ///
  /// # Errors
  /// [`Refused::OutOfTurn`] unless a change's record is with the caller; [`Refused::Item`] where an answer cannot be
  /// written.
  pub fn revision_property_landed(
    &mut self,
    landed: Result<(), (u32, String)>,
    out: &mut Vec<u8>,
  ) -> Result<Option<RevisionPropertyCall>, Refused> {
    let (established, mut pending) = self.revision_property_at(Stage::Landing)?;
    if let Err(failure) = landed {
      self.phase = Phase::Commands(established);
      write_failure(&[failure], out)?;
      return Ok(None);
    }
    let mut call = pending.call.clone();
    call.value = pending.old.clone().unwrap_or_default();
    pending.stage = Stage::Finishing;
    self.phase = Phase::RevisionProperty(established, pending);
    Ok(Some(call))
  }

  /// Ends the landed revision property change with post-revprop-change's `verdict`: `( success ( ) )`, or the hook's
  /// chain in its place, the change standing (`svn_repos_fs_change_rev_prop4` returns it after the change).
  ///
  /// # Errors
  /// [`Refused::OutOfTurn`] unless a change's post-hook was handed out; [`Refused::Item`] where an answer cannot be
  /// written.
  pub fn revision_property_finished(
    &mut self,
    verdict: Result<(), Vec<(u32, String)>>,
    out: &mut Vec<u8>,
  ) -> Result<(), Refused> {
    let (established, _) = self.revision_property_at(Stage::Finishing)?;
    self.phase = Phase::Commands(established);
    match verdict {
      Ok(()) => {
        let success = Value::List(vec![Value::Word(SUCCESS), Value::List(Vec::new())]);
        ra_svn::write(&success, out).map_err(Refused::Item)
      }
      Err(chain) => write_failure(&chain, out),
    }
  }

  /// The change waiting at `stage`, taken out of the phase; the phase left as it stood where none waits there.
  fn revision_property_at(
    &mut self,
    stage: Stage,
  ) -> Result<(Box<Established>, Box<Pending>), Refused> {
    match std::mem::replace(&mut self.phase, Phase::Ended) {
      Phase::RevisionProperty(established, pending) if pending.stage == stage => {
        Ok((established, pending))
      }
      other => {
        self.phase = other;
        Err(Refused::OutOfTurn)
      }
    }
  }
}

/// What a revision property change's hooks are run with (A-702 part 2; `hooks.c`
/// `svn_repos__hooks_pre_revprop_change`, `_post_revprop_change`), handed to the caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevisionPropertyCall {
  /// The revision.
  pub revision: Revision,
  /// The session's user, empty for none.
  pub user: String,
  /// The property's name.
  pub name: String,
  /// What happens to the property.
  pub action: RevisionPropertyAction,
  /// The hook's standard input: pre-revprop-change's the new value (empty for a deletion), post-revprop-change's the
  /// old (empty for an addition).
  pub value: Vec<u8>,
}

/// How a revision property changes (`svn_repos_fs_change_rev_prop4`'s action: `D` with no new value, else `A` with no
/// old one, else `M`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RevisionPropertyAction {
  /// `A`.
  Added,
  /// `M`.
  Modified,
  /// `D`.
  Deleted,
}

/// Where a change past its checks stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
  /// Its pre-hook is with the caller.
  Hooking,
  /// Its record is with the caller.
  Landing,
  /// Its post-hook is with the caller.
  Finishing,
}

/// A change past its checks (module documentation).
pub(super) struct Pending {
  /// The change.
  change: Change,
  /// The value its hooks are told the property held: the one expected, else the one held when it was asked.
  old: Option<Vec<u8>>,
  /// Its pre-revprop-change call.
  call: RevisionPropertyCall,
  /// Where it stands.
  stage: Stage,
}

impl std::fmt::Debug for Pending {
  /// The stage alone: the values are the client's.
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(formatter, "Pending({:?})", self.stage)
  }
}

impl Pending {
  /// Its pre-revprop-change call.
  pub(super) fn call(&self) -> RevisionPropertyCall {
    self.call.clone()
  }
}

/// `name`'s value in `revision`'s properties `repository` holds, none where it holds no such property or revision.
fn held_value<R: reads::Repository + ?Sized>(
  repository: &R,
  revision: Revision,
  name: &[u8],
) -> Option<Vec<u8>> {
  repository
    .revision_properties(revision)?
    .into_iter()
    .find(|(held, _)| *held == name)
    .map(|(_, value)| value.to_vec())
}

/// `change`'s revision's properties once it is made (FSFS's `change_rev_prop`), or its refusal: a revision `repository`
/// does not hold, or a held value other than the one expected.
fn changed_properties<R: reads::Repository + ?Sized>(
  repository: &R,
  change: &Change,
) -> Result<Properties, Vec<(u32, String)>> {
  let revision = change.revision;
  let held = repository
    .revision_properties(revision)
    .ok_or_else(|| vec![(NO_SUCH_REVISION, format!("No such revision {revision}"))])?;
  let mut properties: Properties = held
    .into_iter()
    .map(|(name, value)| (name.to_vec(), value.to_vec()))
    .collect();
  if let Some(expected) = &change.expected
    && properties.get(&change.name) != expected.as_ref()
  {
    let name = String::from_utf8_lossy(&change.name);
    return Err(vec![(
      BASEVALUE_MISMATCH,
      format!("revprop '{name}' has unexpected value in filesystem"),
    )]);
  }
  match &change.value {
    Some(value) => properties.insert(change.name.clone(), value.clone()),
    None => properties.remove(&change.name),
  };
  Ok(properties)
}

/// What the change is refused with before its hooks (module documentation): the first check it fails, none where it
/// passes them all.
fn judged(
  index: &crate::index::Index,
  rules: Option<&crate::authz::PrincipalRules>,
  change: &Change,
) -> Option<Vec<(u32, String)>> {
  let revision = change.revision;
  match crate::reads::revision_wholly_readable(index, rules, revision) {
    Err(failure) => return Some(vec![failure]),
    Ok(false) => {
      return Some(vec![(
        AUTHZ_UNREADABLE,
        format!("Write denied:  not authorized to read all of revision {revision}"),
      )]);
    }
    Ok(true) => {}
  }
  if let Some(errors) = revision_property_errors(&change.name, change.value.as_deref()) {
    return Some(errors);
  }
  if change.expected.is_none() && revision > index.youngest() {
    return Some(vec![(
      NO_SUCH_REVISION,
      format!("No such revision {revision}"),
    )]);
  }
  None
}

/// The change `name` asks with `params` (module documentation); none where a parameter is not what its prototype
/// names (`rc?s`, `rc(?s)(b?s)`), which `svnserve` reads as malformed.
fn read(name: &str, params: &[Value<'_>]) -> Option<Change> {
  match (name, params) {
    (CHANGE_REV_PROP, [Value::Number(revision), Value::String(property), rest @ ..]) => {
      let value = match rest {
        [] => None,
        [Value::String(value), ..] => Some(value.to_vec()),
        _ => return None,
      };
      Some(Change {
        revision: *revision,
        name: property.to_vec(),
        value,
        expected: None,
        both: false,
      })
    }
    (
      CHANGE_REV_PROP2,
      [
        Value::Number(revision),
        Value::String(property),
        Value::List(value),
        Value::List(previous),
        ..,
      ],
    ) => {
      let value = match value.as_slice() {
        [] => None,
        [Value::String(value)] => Some(value.to_vec()),
        _ => return None,
      };
      let (dont_care, previous) = match previous.as_slice() {
        [Value::Word(word)] => (boolean(word)?, None),
        [Value::Word(word), Value::String(previous)] => (boolean(word)?, Some(previous.to_vec())),
        _ => return None,
      };
      Some(Change {
        revision: *revision,
        name: property.to_vec(),
        value,
        both: dont_care && previous.is_some(),
        expected: (!dont_care).then_some(previous),
      })
    }
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
