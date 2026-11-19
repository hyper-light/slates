//! A protected branch's questions about a move (A-644; §4.17.2 "Protected branches, in git's terms", "A push" step 6
//! and "Pieces (git)" piece 11): how a move changes its ref, and which commits it brings to the ref, read from the
//! store's rows and the push it holds in quarantine. The answers are facts; the policy that turns them into a verdict
//! is the server's (§4.17.1 "Policy", A-622), and git's words for its refusals are `receive::protected`'s.
//!
//! **Each ref against its own position (A-642).** A move is judged against its own green's current head and nothing
//! else: no other ref's position enters an answer, so a push to one branch never reads, waits for or races another's.
//!
//! **How a move changes its ref** ([`Repository::change`]): a create where the green has no head (never made, or
//! deleted by its last version), a delete where the move names none, a fast-forward where the current head is the new
//! one or one of its ancestors, and a rewrite otherwise, or where either head is no commit (a tag moved is a rewrite).
//! The fast-forward is the store's own, the question its admission asks, so a protected branch and the store's reached
//! sets never disagree. It is decided with no walk where generation numbers decide it: a commit's number is one more
//! than its parents' greatest \[B: gitformat-commit-graph, "generation number"\], so a new head whose number is no
//! greater than the current head's descends from it only by being it. Only a greater number needs the walk back from
//! the new head, which stops below the current head's number. The store counts both ([`super::StoreCounters`]).
//!
//! **What a move brings** ([`Repository::merge_commit`], [`Repository::unsigned_commit`]): the commits its new head
//! reaches that its current head does not, the commits of the version's D(k) (the store's module docs), found by a walk
//! back from the new head that stops at every commit the green's reached set holds. A create brings every commit its
//! head reaches: its ref has no position of its own to stop at, and stopping at other refs' commits instead would read
//! their positions (A-642) and let a protected name be made at a commit no protection judged (pushed first to a branch
//! nothing protects). The walk takes the newest generation first, ties by the greater id, and judges each commit once,
//! so the commit a refusal names is the same on every run.
//!
//! **Linear history.** A commit with more than one parent among those a move brings is a merge commit; the first found
//! is named.
//!
//! **Signed commits.** Each commit a move brings is read, and its signature is what git's verifier reads (commit.c's
//! `parse_buffer_signed_by_header`, A-590's [`Commit::write_signature`]): every header of the repository's own
//! signature name (`gpgsig` in a SHA-1 repository, `gpgsig-sha256` in a SHA-256 one), told apart by the armour's first
//! line as gpg-interface.c's `get_format_by_sig` tells it \[C: git v2.50.1\]. An SSH signature goes with what it
//! covers, the commit without its signature headers ([`Commit::write_signed_payload`]), to the caller's verifier
//! ([`CommitSigners`]; on the server A-321's, under git's namespace, `ssh-keygen -Y sign -n git`). OpenPGP and X.509
//! signatures are refused, typed, since verifying them is a later piece (§4.17.2); a commit with no signature, or one
//! in no format git signs with, is refused. The first commit refused is named, with why.
//!
//! **Bounds.** The walks read rows, and the bytes of the commits they judge, as the store's own walks do; slicing them
//! under the shard's budget and charging their sets is the store's owed item (module docs), not done here.

use slates_vcs::ContentStore;

use super::{Repository, StoreRefused};
use crate::object_name::ObjectId;
use crate::store::Push;

/// How a move changes its ref (module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
  /// The ref has no head: never made, or deleted by its last version.
  Create,
  /// The new head is the current one or descends from it.
  FastForward,
  /// The new head does not descend from the current one, or either is no commit.
  Rewrite,
  /// The move deletes the ref.
  Delete,
}

/// What verifies a commit's SSH signature (module docs): on the server, A-321's verifier under git's namespace, over
/// the keys the repository's policy trusts.
pub trait CommitSigners {
  /// Why a signature did not verify.
  type Refused;

  /// Verifies `signature`, an SSH signature's armour as the commit holds it (its lines unfolded, each ending in a line
  /// feed), over `payload`, the commit without its signature headers: what git signed.
  fn verify(&self, signature: &[u8], payload: &[u8]) -> Result<(), Self::Refused>;

  /// Why a signature was refused, as a refusal says it after naming the commit (`its signer is not trusted`).
  fn words(refused: &Self::Refused) -> &'static [u8];
}

/// Why a commit a move brings is not one a trusted key signed (module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unsigned<R> {
  /// It carries no signature in the repository's own header.
  Missing,
  /// Its signature is OpenPGP's, which a later piece verifies (§4.17.2).
  OpenPgp,
  /// Its signature is X.509's, which a later piece verifies.
  X509,
  /// Its signature is in no format git signs with.
  Unknown,
  /// Its SSH signature, which the verifier refused.
  Ssh(R),
}

impl Repository {
  /// How moving `green` to `to` changes it: the stub answers a fast-forward (A-644, tests first).
  pub fn change(
    &self,
    _store: &ContentStore,
    _push: &Push,
    _green: u64,
    _to: Option<ObjectId>,
  ) -> Result<Change, StoreRefused> {
    Ok(Change::FastForward)
  }

  /// The first merge commit a move brings: the stub finds none (A-644, tests first).
  pub fn merge_commit(
    &self,
    _store: &ContentStore,
    _push: &Push,
    _green: u64,
    _to: ObjectId,
  ) -> Result<Option<ObjectId>, StoreRefused> {
    Ok(None)
  }

  /// The first commit a move brings that no trusted key signed: the stub finds none (A-644, tests first).
  pub fn unsigned_commit<S: CommitSigners>(
    &self,
    _store: &ContentStore,
    _push: &Push,
    _green: u64,
    _to: ObjectId,
    _signers: &S,
  ) -> Result<Option<(ObjectId, Unsigned<S::Refused>)>, StoreRefused> {
    Ok(None)
  }
}
