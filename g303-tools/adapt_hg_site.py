"""Adapt the hg site (crates/server/src/vcs_http/hg.rs) to A-625 part 3's edge: the credential proved by the record
(async `prove`, `Unproven`), and a repository another shard holds moving the proven request there (`Located`, `Move`)."""
import sys

PATH = "crates/server/src/vcs_http/hg.rs"
text = open(PATH, encoding="utf-8").read()

def swap(old, new):
  global text
  if text.count(old) != 1:
    sys.exit(f"expected one of:\n{old}\nfound {text.count(old)}")
  text = text.replace(old, new)

swap("""//! 2. The credentials, judged as git's site judges them: none, or none proving a principal, is `401` with the
//!    challenges the connection accepts; a repeated or malformed `Authorization` is `400`.
//! 3. The repository. None of that name, or one of another tool, is `404`.
//! 4. The policy.""",
"""//! 2. The credentials, judged as git's site judges them (its `prove`, over A-625 part 3's credential record): none, or
//!    none proving a principal (unknown, expired, a revoked consumer's, an SSH key's), is `401` with the challenges
//!    the connection accepts; a repeated or malformed `Authorization` is `400`; a record that did not answer is `503`.
//! 3. The repository's shard. A repository another shard holds moves the proven request's connection there whole
//!    (`Answer::Move`, A-625 part 3), where it is decided again.
//! 4. The repository. None of that name, or one of another tool, is `404`.
//! 5. The policy.""")
swap("""//! 5. The service. hgweb's pages (a request naming no command of 7.2's table: the repository's browsing URLs,
//!    §4.17.3 piece 12) are `503`, and so is a repository whose store this shard does not hold (A-662 keeps each
//!    repository's on its owner shard; until it lands no repository has one here, [`NoStores`]).""",
"""//! 6. The service. hgweb's pages (a request naming no command of 7.2's table: the repository's browsing URLs,
//!    §4.17.3 piece 12) are `503`, and so is a repository whose store this shard does not hold (A-662 keeps each
//!    repository's on its owner shard; until the site is handed them no repository has one here, [`NoStores`]).""")
swap("use super::git::{Authenticator, Repositories};",
     "use super::git::{Authenticator, Located, Repositories, Unproven};")
swap("""  /// None proved: `401` with the challenges.
  Unauthorized,
  /// The `Authorization` line itself refused, with this status (`400`).
  Refused(Status),""",
"""  /// None proved: `401` with the challenges.
  Unauthorized,
  /// None decided, with this status: the `Authorization` line itself refused (`400`), or a credential record that did
  /// not answer (`503`).
  Refused(Status),""")
swap("""  pub(crate) fn answer(&mut self, head: &Head<'_>, secure: bool) -> Decided<HgExchange<S>> {""",
     """  pub(crate) async fn answer(&mut self, head: &Head<'_>, secure: bool) -> Decided<HgExchange<S>> {""")
swap("""    let principal = match self.proof(head, secure) {
      Proof::Principal(principal) => principal,
      Proof::Unauthorized => return self.unauthorized(secure),
      Proof::Refused(status) => return refused_with(status, Fields::new(), false),
    };
""",
"""    let principal = match self.proof(head, secure).await {
      Proof::Principal(principal) => principal,
      Proof::Unauthorized => return self.unauthorized(secure),
      Proof::Refused(status) => return refused_with(status, Fields::new(), false),
    };
    if let Located::Elsewhere(shard) = self.repositories.locate(&routed.name) {
      return Decided {
        answer: Answer::Move { shard },
        proven: true,
      };
    }
""")
swap("""  /// What `head`'s credentials prove, as git's site reads them.
  fn proof(&self, head: &Head<'_>, secure: bool) -> Proof {
    match credential::credential(head, secure) {
      Ok(Some(credential)) => self
        .authenticator
        .principal(&credential)
        .map_or(Proof::Unauthorized, Proof::Principal),
      Ok(None) => Proof::Unauthorized,
      Err(refused) if refused.status() == Status::UNAUTHORIZED => Proof::Unauthorized,
      Err(refused) => Proof::Refused(refused.status()),
    }
  }""",
"""  /// What `head`'s credentials prove, as git's site reads them.
  async fn proof(&self, head: &Head<'_>, secure: bool) -> Proof {
    let credential = match credential::credential(head, secure) {
      Ok(Some(credential)) => credential,
      Ok(None) => return Proof::Unauthorized,
      Err(refused) if refused.status() == Status::UNAUTHORIZED => return Proof::Unauthorized,
      Err(refused) => return Proof::Refused(refused.status()),
    };
    match self.authenticator.prove(&credential).await {
      Ok(proven) => Proof::Principal(proven.principal),
      Err(Unproven::Unanswered) => Proof::Refused(Status::SERVICE_UNAVAILABLE),
      Err(Unproven::Unknown | Unproven::Expired | Unproven::Revoked | Unproven::NotAToken) => {
        Proof::Unauthorized
      }
    }
  }""")
swap("""  async fn decide(&mut self, head: &Head<'_>, secure: bool) -> Decided<Self::Exchange> {
    self.answer(head, secure)
  }""",
"""  async fn decide(&mut self, head: &Head<'_>, secure: bool) -> Decided<Self::Exchange> {
    self.answer(head, secure).await
  }""")
open(PATH, "w", encoding="utf-8").write(text)
print("hg.rs adapted")
