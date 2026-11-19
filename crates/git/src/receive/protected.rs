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
//! [`unsigned_commit`]. A hosted server answers its own protections through a hook's report too (Gitea v1.24.0's
//! pre-receive hook, which git reports `pre-receive hook declined`), and the stock client shows the words whatever the
//! reason says.
//!
//! **Golden vectors.** `fixtures/protected/make-protected-fixtures.sh` captured ten pushes stock git 2.50.1 made to a
//! stock server protecting `main` this way (and two to a SHA-256 one), its update hook speaking these words; the tests
//! answer each push from A-614's store and compare every byte.

use super::{Declined, message};
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

/// What git answers a non-fast-forward of `name` it refuses.
pub fn non_fast_forward(name: &[u8]) -> Declined {
  declined_by_git(
    NON_FAST_FORWARD,
    &[
      b"denying non-fast-forward ",
      name,
      b" (you should pull first)",
    ],
  )
}

/// What git answers a delete of `name` it refuses.
pub fn deletion_prohibited(name: &[u8]) -> Declined {
  declined_by_git(DELETION_PROHIBITED, &[b"denying ref deletion for ", name])
}

/// What git answers when an update hook refuses `name` with `words` (the hook's output, as it wrote it).
pub fn hook_declined(name: &[u8], words: Vec<u8>) -> Declined {
  Declined {
    reason: HOOK_DECLINED.to_vec(),
    hook: words,
    message: Some(message(&[b"hook declined to update ", name].concat())),
  }
}

/// The refusal of a merge commit `commit` a move of `name` brings, where the branch keeps a linear history.
pub fn merge_commit(name: &[u8], commit: &ObjectId) -> Declined {
  let hex = commit.to_string();
  let text = [
    b"denying merge commit ",
    hex.as_bytes(),
    b" on ",
    name,
    b" (linear history required)",
  ]
  .concat();
  hook_declined(name, message(&text))
}

/// The refusal of a commit `commit` a move of `name` brings that no trusted key signed, and why, where the branch keeps
/// signed commits.
pub fn unsigned_commit<S: CommitSigners>(
  name: &[u8],
  commit: &ObjectId,
  why: &Unsigned<S::Refused>,
) -> Declined {
  let hex = commit.to_string();
  let text = match why {
    Unsigned::Missing => [
      b"denying unsigned commit ",
      hex.as_bytes(),
      b" on ",
      name,
      b" (signed commits required)",
    ]
    .concat(),
    other => [
      b"denying commit ",
      hex.as_bytes(),
      b" on ",
      name,
      b": ",
      unsigned_words::<S>(other),
      b" (signed commits required)",
    ]
    .concat(),
  };
  hook_declined(name, message(&text))
}

/// Why a signature was refused, after naming its commit.
fn unsigned_words<S: CommitSigners>(why: &Unsigned<S::Refused>) -> &'static [u8] {
  match why {
    Unsigned::Missing => b"it is not signed",
    Unsigned::OpenPgp => b"its signature is OpenPGP, which this server does not verify",
    Unsigned::X509 => b"its signature is X.509, which this server does not verify",
    Unsigned::Unknown => b"its signature is in no format git signs with",
    Unsigned::Ssh(refused) => S::words(refused),
  }
}

/// A refusal git makes itself: `reason`, after its `rp_error` line of `parts`.
fn declined_by_git(reason: &[u8], parts: &[&[u8]]) -> Declined {
  Declined {
    reason: reason.to_vec(),
    hook: Vec::new(),
    message: Some(message(&parts.concat())),
  }
}
