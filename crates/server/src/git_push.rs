//! git's push on its repository's owner shard (A-654 part 2): the driver both edges call. Red stub: every call refused,
//! so its tests fail first.

use slates_db::catalog::{Principal, RepositoryId};
use slates_git::out::Out;
use slates_git::receive::{self, Received};

use crate::state::ShardState;
use crate::vcs_policy::PushRefusal;

#[cfg(test)]
mod tests;

/// Format: the agent receive-pack advertises (`agent=`): this server's name and its crate's version.
const AGENT: &[u8] = concat!("slates/", env!("CARGO_PKG_VERSION")).as_bytes();

/// Slates' words: the reason every command of a push is given while a git hook is configured and its view is not served.
pub(crate) const HOOK_NOT_RUN: &[u8] = b"hook not run";

/// Why a push cannot go on, typed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PushFailed {
  /// No repository of this id on this shard's partition.
  NotFound,
  /// The repository serves another tool.
  NotGit,
  /// Its git store is not held here.
  NoStore,
  /// Its policy holds a rule git's matcher cannot read.
  Policy(PushRefusal),
  /// receive-pack refused the exchange.
  Exchange(receive::Refusal),
  /// `answer` asked of a push not yet read whole and checked, or already answered.
  Unchecked,
}

/// One git push over one connection.
pub(crate) struct ReceivePack;

impl ReceivePack {
  /// Starts a push (stub).
  pub(crate) fn start(
    _state: &mut ShardState,
    _who: (&Principal, RepositoryId),
    _how: (u64, bool),
    _out: &mut impl Out,
  ) -> Result<ReceivePack, PushFailed> {
    Err(PushFailed::NotFound)
  }

  /// Takes the client's next bytes (stub).
  pub(crate) fn input(&mut self, _state: &mut ShardState, _bytes: &[u8]) -> Result<bool, PushFailed> {
    Err(PushFailed::NotFound)
  }

  /// The client's bytes have ended (stub).
  pub(crate) fn end(&mut self, _state: &mut ShardState) -> Result<bool, PushFailed> {
    Err(PushFailed::NotFound)
  }

  /// Decides the push and writes git's report (stub).
  pub(crate) fn answer(
    &mut self,
    _state: &mut ShardState,
    _out: &mut impl Out,
  ) -> Result<Received, PushFailed> {
    Err(PushFailed::NotFound)
  }
}
