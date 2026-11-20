//! ra_svn's revision property changes (`serve.c` `change_rev_prop` 1266, `change_rev_prop2` 1226 and
//! `do_change_rev_prop` 1198, `fs-wrap.c` `svn_repos_fs_change_rev_prop4`, Subversion 1.15.0; A-702 part 1):
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
//! **Then the hook.** `svnserve` changes a revision property only where a pre-revprop-change hook allows it, and a
//! repository with none refuses every change "Repository has not been enabled to accept revision propchanges;\nask
//! the administrator to create a pre-revprop-change hook" (165006). No ra_svn path runs a repository's hooks yet
//! (`crate::serve`'s commit refuses where one is configured), so every change is answered so: the change itself, its
//! previous value's check (`SVN_ERR_FS_PROP_BASEVALUE_MISMATCH`), its record through the owner's step
//! (`Record::RevisionProperties`) and post-revprop-change wait on the hooks running (GAPS, A-702).

use super::{
  Connection, Established, Interrupted, Refused, Sources, principal_name, write_failure,
};
use crate::commit::drive::{self as commit_drive, may_write, revision_property_errors};
use crate::index::Revision;
use crate::ra_svn::{self, Value};
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
/// Format: `SVN_ERR_REPOS_DISABLED_FEATURE` (`svn_error_codes.h`: the repository category's start, 165,000, plus 6).
const DISABLED_FEATURE: u32 = 165_006;
/// `svn_repos__hooks_pre_revprop_change`'s words for a repository with no pre-revprop-change hook (`hooks.c`).
const NO_HOOK_WORDS: &str = "Repository has not been enabled to accept revision propchanges;\nask the administrator to \
                             create a pre-revprop-change hook";
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
  /// Whether the client named the value it expects the property to hold (`change-rev-prop2` without `dont-care`).
  expects: bool,
  /// Whether it named both `dont-care` and a previous value.
  both: bool,
}

/// What the change's answer leads to.
pub(super) enum Changed {
  /// Answered here.
  Answered,
  /// An authentication for the command was asked: it runs, then the change again.
  Authenticating(Box<crate::handshake::Server>, Box<Interrupted>),
}

impl Connection {
  /// Answers the revision property change `name` with `params`, whose item is `item` (module documentation), on
  /// `established`; `authenticated_now` once the client has just authenticated for it.
  pub(super) fn change_revision_property<R: ?Sized>(
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
    let refusal = judged(sources.index, rules, &change);
    write_failure(&refusal, out)?;
    Ok(Changed::Answered)
  }
}

/// What the change is refused with (module documentation): the first check it fails, else the missing hook's.
fn judged(
  index: &crate::index::Index,
  rules: Option<&crate::authz::PrincipalRules>,
  change: &Change,
) -> Vec<(u32, String)> {
  let revision = change.revision;
  match crate::reads::revision_wholly_readable(index, rules, revision) {
    Err(failure) => return vec![failure],
    Ok(false) => {
      return vec![(
        AUTHZ_UNREADABLE,
        format!("Write denied:  not authorized to read all of revision {revision}"),
      )];
    }
    Ok(true) => {}
  }
  if let Some(errors) = revision_property_errors(&change.name, change.value.as_deref()) {
    return errors;
  }
  if !change.expects && revision > index.youngest() {
    return vec![(NO_SUCH_REVISION, format!("No such revision {revision}"))];
  }
  vec![(DISABLED_FEATURE, NO_HOOK_WORDS.to_owned())]
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
        expects: false,
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
        [Value::Word(word)] => (boolean(word)?, false),
        [Value::Word(word), Value::String(_)] => (boolean(word)?, true),
        _ => return None,
      };
      Some(Change {
        revision: *revision,
        name: property.to_vec(),
        value,
        expects: !dont_care,
        both: dont_care && previous,
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
