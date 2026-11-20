"""Adapt the hg site's tests (crates/server/src/vcs_http/tests/hg.rs) to A-625 part 3's edge, and test what it adds."""
import sys

PATH = "crates/server/src/vcs_http/tests/hg.rs"
text = open(PATH, encoding="utf-8").read()

def swap(old, new):
  global text
  if text.count(old) != 1:
    sys.exit(f"expected one of:\n{old}\nfound {text.count(old)}")
  text = text.replace(old, new)

swap("use super::super::git::{Authenticator, Repositories};",
     "use super::super::git::{Authenticator, Located, Proven, Repositories, Unproven};")
swap("""/// Shape: the id of the git repository `r.git`.
const GIT: RepositoryId = RepositoryId { bytes: [3; 16] };
""",
"""/// Shape: the id of the git repository `r.git`.
const GIT: RepositoryId = RepositoryId { bytes: [3; 16] };
/// Shape: credentials whose record did not answer (its partition quiet).
const UNANSWERED_BEARER: &str = "Authorization: Bearer unanswered-token";
/// Shape: the token they carry.
const UNANSWERED_TOKEN: &[u8] = b"unanswered-token";
/// Shape: a Mercurial repository another runtime shard holds.
const AWAY: &str = "away";
/// Shape: the runtime shard that holds it.
const AWAY_SHARD: u16 = 3;
""")
swap("""/// Proves [`OWNER_TOKEN`] the owner and [`STRANGER_TOKEN`] the stranger, from a bearer token.
struct Tokens;

impl Authenticator for Tokens {
  fn principal(&self, credential: &Credential) -> Option<Principal> {
    let secret = match credential {
      Credential::Basic { password, .. } => password.as_slice(),
      Credential::Bearer(token) => token.as_slice(),
    };
    [(OWNER_TOKEN, OWNER), (STRANGER_TOKEN, STRANGER)]
      .into_iter()
      .find(|(token, _)| *token == secret)
      .map(|(_, uid)| principal(uid))
  }
}""",
"""/// Proves [`OWNER_TOKEN`] the owner and [`STRANGER_TOKEN`] the stranger, from a bearer token; its record does not
/// answer for [`UNANSWERED_TOKEN`].
struct Tokens;

impl Authenticator for Tokens {
  async fn prove(&self, credential: &Credential) -> Result<Proven, Unproven> {
    let secret = match credential {
      Credential::Basic { password, .. } => password.as_slice(),
      Credential::Bearer(token) => token.as_slice(),
    };
    if secret == UNANSWERED_TOKEN {
      return Err(Unproven::Unanswered);
    }
    [(OWNER_TOKEN, OWNER), (STRANGER_TOKEN, STRANGER)]
      .into_iter()
      .find(|(token, _)| *token == secret)
      .map(|(_, uid)| Proven {
        principal: principal(uid),
        bearer_expires_ns: None,
      })
      .ok_or(Unproven::Unknown)
  }
}""")
swap("""/// Holds the Mercurial repositories `hg` (the owner's, its policy `rules`) and `bare` (no store on this shard), and the
/// git repository `r.git`.
struct Held {""",
"""/// Holds the Mercurial repositories `hg` (the owner's, its policy `rules`) and `bare` (no store on this shard), and the
/// git repository `r.git`; [`AWAY`] lives on runtime shard [`AWAY_SHARD`].
struct Held {""")
swap("""impl Repositories for Held {
  fn find(&self, name: &str) -> Option<RepositoryRecord> {""",
"""impl Repositories for Held {
  fn locate(&self, name: &str) -> Located {
    if name == AWAY {
      Located::Elsewhere(AWAY_SHARD)
    } else {
      Located::Here
    }
  }

  fn find(&self, name: &str) -> Option<RepositoryRecord> {""")
swap("""/// A refusal's status and fields, or `None` for a served request.
fn refusal<E>(decided: &Decided<E>) -> Option<(Status, Fields)> {
  match &decided.answer {
    Answer::Refuse { status, fields } => Some((*status, fields.clone())),
    Answer::Serve { .. } => None,
  }
}""",
"""/// A refusal's status and fields, or `None` for a served or moved request.
fn refusal<E>(decided: &Decided<E>) -> Option<(Status, Fields)> {
  match &decided.answer {
    Answer::Refuse { status, fields } => Some((*status, fields.clone())),
    Answer::Serve { .. } | Answer::Move { .. } => None,
  }
}""")
swap("""  refusal(&site.answer(&head(&bytes), false)).map(|(status, _)| status)""",
     """  refusal(&ready(site.answer(&head(&bytes), false))).map(|(status, _)| status)""")
swap("""    let decided = site.answer(&head(&bytes), false);
    (refusal(&decided).unwrap().1, decided.proven)""",
"""    let decided = ready(site.answer(&head(&bytes), false));
    (refusal(&decided).unwrap().1, decided.proven)""")
swap("""/// Runs `future`, which the tests' exchanges finish without waiting.""",
     """/// Runs `future`, which the tests' sites and exchanges finish without waiting.""")
swap("""  match site.answer(&head(&bytes), false).answer {
    Answer::Serve {
      fields,
      head_only,
      exchange,
    } => {
      assert!(fields.is_empty() && !head_only);
      exchange
    }
    Answer::Refuse { status, .. } => panic!("{method} {target} refused {status:?}"),
  }""",
"""  match ready(site.answer(&head(&bytes), false)).answer {
    Answer::Serve {
      fields,
      head_only,
      exchange,
    } => {
      assert!(fields.is_empty() && !head_only);
      exchange
    }
    Answer::Refuse { status, .. } => panic!("{method} {target} refused {status:?}"),
    Answer::Move { shard } => panic!("{method} {target} moved to shard {shard}"),
  }""")
anchor = "/// Runs `future`, which the tests' sites and exchanges finish without waiting."
new_test = """/// A-650 part 1 on A-625 part 3's edge. Do: ask for [`AWAY`], a repository another runtime shard holds, as the owner,
/// with no credentials, and with a token the credential record did not answer for. Expect the owner's request moved to
/// that shard whole, proven (`Move`), and the others decided here before any move: `401`, and `503` for the record that
/// did not answer, as git's site answers them.
#[test]
fn a_request_for_a_repository_another_shard_holds_moves_there_once_proven() {
  let mut site = site(Vec::new());
  let mut decide = |fields: &[&str]| {
    let bytes = request("GET", "/away?cmd=capabilities", fields);
    let decided = ready(site.answer(&head(&bytes), false));
    let refused = refusal(&decided).map(|(status, _)| status);
    let moved = match &decided.answer {
      Answer::Move { shard } => Some(*shard),
      Answer::Serve { .. } | Answer::Refuse { .. } => None,
    };
    (moved, refused, decided.proven)
  };
  assert_eq!(
    [
      decide(&[OWNER_BEARER]),
      decide(&[]),
      decide(&[UNANSWERED_BEARER])
    ],
    [
      (Some(AWAY_SHARD), None, true),
      (None, Some(Status::UNAUTHORIZED), false),
      (None, Some(Status::SERVICE_UNAVAILABLE), false),
    ]
  );
}

"""
if text.count(anchor) != 1:
  sys.exit("anchor for the new test not found once")
text = text.replace(anchor, new_test + anchor)
open(PATH, "w", encoding="utf-8").write(text)
print("tests/hg.rs adapted")
