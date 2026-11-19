//! Subversion's hooks as the server runs them (§4.17.4 item 9; A-661): the glue between a Subversion server's call
//! points and the hook driver. A caller builds the point's input (`crate::vcs_hooks`' Subversion builders, with the
//! environment the repository's record gives, `SvnHooksEnv::of`), and [`run`] finds the hook the repository's policy
//! configures at that point, runs it over the caller's view (`crate::hook_driver`), and answers what `svnserve` tells
//! its client (`crate::vcs_hooks::svn_told`), what the run teaches the hook's times, and pre-lock's token.
//!
//! **Who calls it** (main's agreement of 2026-11-19, relayed to each owner). The commit drive pauses where `svnserve`
//! runs start-commit (`open-root`, after its revision check, the transaction named) and pre-commit (`close-edit`,
//! before the verdict), and resumes with [`SvnHookRan::verdict`] (g268, A-655). The `svn://` connection task runs
//! those, and post-commit after its append, whose words are `commit-info`'s `post-commit-err` (g234, A-656). The lock
//! hooks run around A-602's lock check, on `lock-many` and `unlock-many` and on DAV's LOCK and UNLOCK: pre-lock per
//! path, its token the lock's ([`SvnHookRan::token`]), and post-lock once, with the paths locked.
//!
//! **A point with no hook** answers as `svnserve` does: nothing, but at pre-revprop-change, which refuses every change
//! (`E165006`). No process runs and nothing is learned.
//!
//! **A client that left** is answered no verdict: its caller has nothing to resume, and the change does not go on.
//!
//! **The hook's times** are its caller's (the table that keeps them is owed, A-622's list): the caller hands [`run`] the
//! bound they learned and teaches them the run's lesson.

use std::future::Future;
use std::path::Path;

use slates_db::catalog::{HookConfig, RepositoryPolicy};

use crate::hook_driver::{Ended, Launch};
use crate::vcs_hooks::{
  HookInput, HookPoint, Lesson, Outcome, SvnLockToken, SvnTold, svn_lock_token, svn_told,
  svn_told_unconfigured,
};

/// What a Subversion hook's run needs from its caller beside the hook's input.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SvnHookSite<'a> {
  /// The repository's policy: the hooks configured at its points.
  pub(crate) policy: &'a RepositoryPolicy,
  /// The root of the read-only view the hook runs over (`hook_driver::Launch::view_root`).
  pub(crate) view_root: &'a Path,
  /// How many bytes of a hook's words, and of pre-lock's standard output, the caller keeps: its connection's share.
  pub(crate) words_cap: usize,
}

/// What a Subversion hook's run answers its caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SvnHookRan {
  /// What `svnserve` tells its client: nothing, a failure, or post-commit's words.
  pub(crate) told: SvnTold,
  /// What the run teaches its hook's times; `None` where no hook is configured.
  pub(crate) lesson: Option<Lesson>,
  /// pre-lock's token (`svn_lock_token`); `None` at every other point, and where no hook is configured.
  pub(crate) token: Option<SvnLockToken>,
  /// Whether the run's client left before it ended.
  pub(crate) client_left: bool,
}

impl SvnHookRan {
  /// What an admission point's caller resumes with (the drive's `resume`): `Ok` to go on, or the chain its command is
  /// refused with, each error its number and message, outermost first. `None` when the client left, since there is
  /// nothing to resume and the change does not go on. An observing point's failure is no refusal: `Ok`.
  pub(crate) fn verdict(&self) -> Option<Result<(), Vec<(u32, String)>>> {
    if self.client_left {
      return None;
    }
    Some(match &self.told {
      SvnTold::Failure(chain) => Err(
        chain
          .iter()
          .map(|error| (error.code, error.message.clone()))
          .collect(),
      ),
      SvnTold::Nothing | SvnTold::PostCommitErr(_) => Ok(()),
    })
  }
}

/// The hook `policy` configures at `point`, if any: Subversion keeps one hook a point, named for it (A-622's guard).
pub(crate) fn configured(policy: &RepositoryPolicy, point: HookPoint) -> Option<&HookConfig> {
  policy
    .hooks
    .iter()
    .find(|hook| HookPoint::of(hook.point) == point)
}

/// Runs the hook `site`'s policy configures at `input`'s point, held to `bound_ns` (its hook's learned bound, none
/// before its first finished run) and ended when `client_gone` resolves (the module doc). A point with no hook answers
/// at once, running nothing.
pub(crate) async fn run(
  site: &SvnHookSite<'_>,
  input: &HookInput,
  bound_ns: Option<u64>,
  client_gone: impl Future<Output = ()>,
) -> SvnHookRan {
  let Some(hook) = configured(site.policy, input.point) else {
    return unconfigured(input.point);
  };
  let launch = Launch {
    program: Path::new(&hook.program),
    input,
    view_root: site.view_root,
    words_cap: site.words_cap,
  };
  let ended = crate::hook_driver::run(launch, bound_ns, client_gone).await;
  answer(input.point, &hook.program, ended)
}

/// What a point with no hook answers (the module doc).
fn unconfigured(point: HookPoint) -> SvnHookRan {
  SvnHookRan {
    told: svn_told_unconfigured(point).unwrap_or(SvnTold::Nothing),
    lesson: None,
    token: None,
    client_left: false,
  }
}

/// What a hook at `point`, the program `program`, answers having run to `ended`.
fn answer(point: HookPoint, program: &str, ended: Ended) -> SvnHookRan {
  SvnHookRan {
    told: svn_told(point, program, &ended.outcome).unwrap_or(SvnTold::Nothing),
    lesson: Some(ended.lesson),
    token: ended.standard_output.as_ref().map(svn_lock_token),
    client_left: matches!(ended.outcome, Outcome::Abandoned),
  }
}

#[cfg(test)]
mod tests;
