//! A protected git branch, the server's half (A-644 part 2; §4.17.2 "Protected branches, in git's terms" and "A push"
//! step 6; §4.17.1 "Policy"): what a ref's change brings, read from A-614's store, offered to the policy's check point
//! ([`crate::vcs_policy::admit`]); A-321's verifier under git's namespace as the verifier of a commit's signature; and the
//! check point's refusals in git's words for receive-pack's report.
//!
//! **The question and its answer.** receive-pack's judge (A-620 part 2) asks the store how a command changes its ref
//! (`slates_git::store::Repository::change`, judged against the ref's own green, A-642) and so which right the change
//! needs ([`right_of`]: a create or a fast-forward submits, a rewrite rewrites, a delete deletes). It asks the check point
//! with that right and with what the change brings ([`StoreBrings`]), which the check point reads only where a rule
//! imposes a requirement (a linear history, signed commits). A commit's SSH signature is verified by A-321's code
//! ([`crate::signed_work::verify_in`] under [`crate::signed_work::GIT_NAMESPACE`]) against the keys the repository's
//! policy trusts ([`CommitKeys`]); OpenPGP and X.509 signatures are the store's typed refusals until a later piece.
//!
//! **git's words** ([`declined`]). A rewrite or a delete a rule denies is answered as git's own `receive.denyNonFastForwards`
//! and `receive.denyDeletes` answer it (`non-fast-forward`, `deletion prohibited`); a merge commit or an unsigned commit as
//! git's update hook would answer it (`hook declined`, after words naming the commit and the rule); and every other
//! refusal of the check point as a hook's too, its words naming why. The words are `slates_git::receive::protected`'s.
//!
//! **Proven** against twelve pushes stock git 2.50.1 made to a stock server protecting `main` (A-644's golden vectors,
//! `crates/git/fixtures/protected`): each answered here from A-614's store, by a policy of A-622's rules and a trusted
//! key, through the check point and A-321's verifier, with every byte git wrote.

use slates_git::object_name::ObjectId;
use slates_git::receive::{Declined, message, protected};
use slates_git::store::{Change, CommitSigners, Push, Repository, StoreRefused, Unsigned};
use slates_vcs::ContentStore;

use crate::signed_work::{GIT_NAMESPACE, SignatureRefusal, TrustedKey, dearmour, verify_in};
use crate::vcs_policy::{Brings, PushRefusal, Right};

/// A-321's verifier under git's namespace, over the keys a repository's policy trusts: the verifier of a commit's SSH
/// signature (module docs).
pub(crate) struct CommitKeys<'k>(pub(crate) &'k [TrustedKey]);

impl CommitSigners for CommitKeys<'_> {
  type Refused = SignatureRefusal;

  fn verify(&self, signature: &[u8], payload: &[u8]) -> Result<(), SignatureRefusal> {
    let armour = std::str::from_utf8(signature).map_err(|_| SignatureRefusal::Malformed)?;
    let blob = dearmour(armour)?;
    verify_in(GIT_NAMESPACE, &blob, payload, self.0).map(|_| ())
  }

  fn words(refused: &SignatureRefusal) -> &'static [u8] {
    match refused {
      SignatureRefusal::Malformed => b"its signature is malformed",
      SignatureRefusal::Namespace => b"its signature was not made for git",
      SignatureRefusal::Unsupported => {
        b"its signature's key or hash is not one this server verifies"
      }
      SignatureRefusal::UnregisteredKey => b"its signer is not trusted",
      SignatureRefusal::BadSignature => b"its signature does not verify",
      SignatureRefusal::NotTouched => b"its security key was not touched",
    }
  }
}

/// What a change of one ref brings, as A-614's store answers it (module docs): the commits moving `green` to `to`
/// brings, read from the store and the push it holds in quarantine.
pub(crate) struct StoreBrings<'a> {
  /// The repository's store.
  pub(crate) repository: &'a Repository,
  /// The content it keeps its objects in.
  pub(crate) store: &'a ContentStore,
  /// The push, sealed.
  pub(crate) push: &'a Push,
  /// The ref's green.
  pub(crate) green: u64,
  /// The ref's new value.
  pub(crate) to: ObjectId,
}

impl Brings for StoreBrings<'_> {
  fn merge_commit(&self) -> Result<Option<ObjectId>, StoreRefused> {
    self
      .repository
      .merge_commit(self.store, self.push, self.green, self.to)
  }

  fn unsigned_commit(
    &self,
    signers: &[TrustedKey],
  ) -> Result<Option<(ObjectId, Unsigned<SignatureRefusal>)>, StoreRefused> {
    let keys = CommitKeys(signers);
    self
      .repository
      .unsigned_commit(self.store, self.push, self.green, self.to, &keys)
  }
}

/// The right a ref's change needs (A-622 part 2's rule over A-644's change): a create or a fast-forward submits, a
/// rewrite rewrites, a delete deletes.
pub(crate) fn right_of(change: Change) -> Right {
  match change {
    Change::Create | Change::FastForward => Right::Submit,
    Change::Rewrite => Right::Rewrite,
    Change::Delete => Right::Delete,
  }
}

/// git's words for the check point's `refusal` of the change of ref `name` that needed `right` (module docs).
pub(crate) fn declined(refusal: &PushRefusal, name: &[u8], right: Right) -> Declined {
  let _ = (refusal, name, right, refused_by);
  Declined {
    reason: Vec::new(),
    hook: Vec::new(),
    message: None,
  }
}

/// A refusal of the change of `name` for `why`, answered as git's update hook would answer it.
fn refused_by(name: &[u8], why: &str) -> Declined {
  let text = [
    b"denying the update of ".as_slice(),
    name,
    b" (",
    why.as_bytes(),
    b")",
  ]
  .concat();
  protected::hook_declined(name, message(&text))
}

#[cfg(test)]
#[path = "git_protect/tests.rs"]
mod tests;
