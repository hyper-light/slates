//! git's words for a protected branch's refusals (A-644; §4.17.2 "Protected branches, in git's terms"): what stock git
//! 2.50.1's receive-pack sends of a command a protected branch refuses, so a stock client prints what it prints for
//! git's own server \[C: builtin/receive-pack.c `update()`, `run_update_hook()`, `copy_to_sideband()`, v2.50.1\].
//!
//! **What git refuses itself.** A non-fast-forward (`receive.denyNonFastForwards`) is reported `non-fast-forward`,
//! after the band-2 line `error: denying non-fast-forward <ref> (you should pull first)`; a delete
//! (`receive.denyDeletes`) `deletion prohibited`, after `error: denying ref deletion for <ref>`. A branch's policy that
//! refuses a rewrite or a delete answers in these words ([`non_fast_forward`], [`deletion_prohibited`]).
//!
//! **What git leaves to a hook.** A linear history and signed commits are checks git makes only through an `update`
//! hook, and git answers a hook's refusal with the hook's own words on band 2, read 128 bytes at a time, then its own
//! line `error: hook declined to update <ref>`, and reports `hook declined`. slates' checks answer as that hook would
//! ([`hook_declined`]), in words of its own that name the commit and the rule: [`merge_commit`] and
//! [`unsigned_commit`]. Every hosted git server answers its own protections through this report (GitLab and Gitea say
//! `pre-receive hook declined`, GitHub `protected branch hook declined`), and the stock client shows the words whatever
//! the reason says.
//!
//! **Golden vectors.** `fixtures/protected/make-protected-fixtures.sh` captured ten pushes stock git 2.50.1 made to a
//! stock server protecting `main` this way (and two to a SHA-256 one), its update hook speaking these words; the tests
//! answer each push from A-614's store and compare every byte.

use super::Declined;
use crate::object_name::ObjectId;
use crate::store::{CommitSigners, Unsigned};

/// Format: the report's reason for a non-fast-forward a branch refuses (`update()`, `receive.denyNonFastForwards`).
pub const NON_FAST_FORWARD: &[u8] = b"non-fast-forward";

/// Format: the report's reason for a delete a branch refuses (`update()`, `receive.denyDeletes`).
pub const DELETION_PROHIBITED: &[u8] = b"deletion prohibited";

/// Format: the report's reason for a command a hook refused (`update()`, after `run_update_hook()`).
pub const HOOK_DECLINED: &[u8] = b"hook declined";

/// Format: the most bytes of a hook's words one band-2 packet carries: `copy_to_sideband`'s buffer (`char data[128]`,
/// one `xread` of it a packet).
pub const HOOK_PIECE: usize = 128;

/// The refusal the stub gives every protected branch's question (A-644, tests first): no words.
fn unworded() -> Declined {
  Declined {
    reason: Vec::new(),
    hook: Vec::new(),
    message: None,
  }
}

/// What git answers a non-fast-forward of `name` it refuses: the stub's none.
pub fn non_fast_forward(_name: &[u8]) -> Declined {
  unworded()
}

/// What git answers a delete of `name` it refuses: the stub's none.
pub fn deletion_prohibited(_name: &[u8]) -> Declined {
  unworded()
}

/// What git answers when an update hook refuses `name` with `words`: the stub's none.
pub fn hook_declined(_name: &[u8], _words: Vec<u8>) -> Declined {
  unworded()
}

/// The refusal of a merge commit: the stub's none.
pub fn merge_commit(_name: &[u8], _commit: &ObjectId) -> Declined {
  unworded()
}

/// The refusal of a commit no trusted key signed: the stub's none.
pub fn unsigned_commit<S: CommitSigners>(
  _name: &[u8],
  _commit: &ObjectId,
  _why: &Unsigned<S::Refused>,
) -> Declined {
  unworded()
}
