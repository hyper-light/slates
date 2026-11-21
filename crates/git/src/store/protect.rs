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

use std::collections::{BTreeSet, BinaryHeap};

use slates_vcs::ContentStore;

use super::{Chain, Repository, StoreRefused};
use crate::Admission;
use crate::object_name::ObjectId;
use crate::objects::commit::Commit;
use crate::store::Push;

/// Format: the first line of an SSH signature's armour (OpenSSH `PROTOCOL.sshsig`; gpg-interface.c's `ssh_sigs`).
const SSH_ARMOUR: &[u8] = b"-----BEGIN SSH SIGNATURE-----";
/// Format: the first lines of an OpenPGP signature's armour (gpg-interface.c's `openpgp_sigs`).
const OPENPGP_ARMOURS: [&[u8]; 2] = [
  b"-----BEGIN PGP SIGNATURE-----",
  b"-----BEGIN PGP MESSAGE-----",
];
/// Format: the first line of an X.509 signature's armour (gpg-interface.c's `x509_sigs`).
const X509_ARMOUR: &[u8] = b"-----BEGIN SIGNED MESSAGE-----";

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

/// The first commit a move brings that no trusted key signed, and why.
pub type UnsignedCommit<R> = (ObjectId, Unsigned<R>);

impl Repository {
  /// Refuses a push of another format, or one not sealed: its commits' generation numbers are not known yet, and a
  /// zero would read as no descendant of anything.
  fn ready(&self, push: &Push) -> Result<(), StoreRefused> {
    self.same_format(push)?;
    if push.is_sealed() {
      Ok(())
    } else {
      Err(StoreRefused::Unsealed)
    }
  }

  /// How moving `green` to `to` (`None` deletes it) changes it, judged against the green's own current head (module
  /// docs).
  pub fn change(
    &self,
    store: &ContentStore,
    push: &Push,
    green: u64,
    to: Option<ObjectId>,
  ) -> Result<Change, StoreRefused> {
    self.ready(push)?;
    let Some(to) = to else {
      return Ok(Change::Delete);
    };
    let Some(position) = self.chains.get(&green).and_then(Chain::head) else {
      return Ok(Change::Create);
    };
    let head = self
      .id_at(store, position)
      .ok_or(StoreRefused::Unreadable { id: to })?;
    Ok(if self.fast_forward(store, push, to, head)? {
      Change::FastForward
    } else {
      Change::Rewrite
    })
  }

  /// The first merge commit moving `green` to `to` brings to it (module docs): a commit with more than one parent.
  pub fn merge_commit(
    &self,
    store: &ContentStore,
    push: &Push,
    green: u64,
    to: ObjectId,
  ) -> Result<Option<ObjectId>, StoreRefused> {
    self.judge_brought(store, push, (green, to), |id, parents| {
      Ok((parents.len() > 1).then_some(id))
    })
  }

  /// The first commit moving `green` to `to` brings to it that no key `signers` trusts signed, and why (module docs).
  pub fn unsigned_commit<S: CommitSigners>(
    &self,
    store: &ContentStore,
    push: &Push,
    green: u64,
    to: ObjectId,
    signers: &S,
  ) -> Result<Option<UnsignedCommit<S::Refused>>, StoreRefused> {
    self.judge_brought(store, push, (green, to), |id, _| {
      let why = self.signature_refused(store, push, &id, signers)?;
      Ok(why.map(|why| (id, why)))
    })
  }

  /// The commits moving `green` to `to` brings to it, each handed to `judge` with its parents until one answers (module
  /// docs): newest generation first, ties by the greater id, each once, never one the green's current head reaches.
  /// Decided as git's `rev-list <to> ^<head>` decides it, with no reached set (A-614 amended: a green's sets may be
  /// derived only on first read): the head's commits are painted back by generation number as the new head's are
  /// walked, an equal generation's paint first, so a commit the head reaches is painted before it could be judged; the
  /// walk ends once no commit of the new head's is left, so it costs the commits between the two heads.
  fn judge_brought<T>(
    &self,
    store: &ContentStore,
    push: &Push,
    (green, to): (u64, ObjectId),
    mut judge: impl FnMut(ObjectId, &[ObjectId]) -> Result<Option<T>, StoreRefused>,
  ) -> Result<Option<T>, StoreRefused> {
    self.ready(push)?;
    let Some(start) = self.peel(store, push, to)? else {
      return Ok(None);
    };
    let old = match self
      .current_head(green)
      .and_then(|head| self.id_at(store, head))
    {
      Some(head) => self.peel(store, push, head)?,
      None => None,
    };
    // Each entry a commit, its generation number and whether it is the old head's paint; an equal generation's paint
    // pops first, since `true` orders after `false`.
    let mut heap: BinaryHeap<(u64, ObjectId, bool)> = BinaryHeap::new();
    heap.push((self.generation(store, push, &start)?, start, false));
    let mut brought_left = 1usize;
    if let Some(old) = old {
      heap.push((self.generation(store, push, &old)?, old, true));
    }
    let (mut judged, mut painted): (BTreeSet<ObjectId>, BTreeSet<ObjectId>) = Default::default();
    while brought_left > 0 {
      let Some((_, id, paint)) = heap.pop() else {
        break;
      };
      if paint {
        if painted.insert(id) {
          for parent in self.parents(store, push, &id)? {
            heap.push((self.generation(store, push, &parent)?, parent, true));
          }
        }
        continue;
      }
      brought_left = brought_left.saturating_sub(1);
      if painted.contains(&id) || !judged.insert(id) {
        continue;
      }
      let parents = self.parents(store, push, &id)?;
      self.count(|counters| counters.brought_judged = counters.brought_judged.saturating_add(1));
      if let Some(found) = judge(id, &parents)? {
        return Ok(Some(found));
      }
      for parent in parents {
        heap.push((self.generation(store, push, &parent)?, parent, false));
        brought_left = brought_left.saturating_add(1);
      }
    }
    Ok(None)
  }

  /// Why the commit `id` is not one a key `signers` trusts signed, or none where one did (module docs).
  fn signature_refused<S: CommitSigners>(
    &self,
    store: &ContentStore,
    push: &Push,
    id: &ObjectId,
    signers: &S,
  ) -> Result<Option<Unsigned<S::Refused>>, StoreRefused> {
    let place = self
      .locate(store, push, id)
      .ok_or(StoreRefused::Missing { id: *id })?;
    let bytes = self.bytes_of(store, id, &place)?;
    let commit = Commit::parse(&bytes, &mut Admission::new(store.room()))
      .map_err(|_| StoreRefused::Malformed { id: *id })?;
    let mut signature = Vec::new();
    if !commit.write_signature(self.format, &mut signature) {
      return Ok(Some(Unsigned::Missing));
    }
    if signature.starts_with(SSH_ARMOUR) {
      let mut payload = Vec::new();
      commit.write_signed_payload(&mut payload);
      return Ok(
        signers
          .verify(&signature, &payload)
          .err()
          .map(Unsigned::Ssh),
      );
    }
    let openpgp = OPENPGP_ARMOURS
      .iter()
      .any(|first| signature.starts_with(first));
    Ok(Some(if openpgp {
      Unsigned::OpenPgp
    } else if signature.starts_with(X509_ARMOUR) {
      Unsigned::X509
    } else {
      Unsigned::Unknown
    }))
  }
}
