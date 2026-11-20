//! git's tree, commit and tag objects (A-590; §4.17.2 "Pieces (git)", piece 4): parsed and written back byte for byte,
//! and checked as git's fsck checks them. A push checks every new object, strictly, as `transfer.fsckObjects` does
//! (§4.17.2 "A push", step 4). A tree git once wrote with a mode or an order fsck now only warns of is still served as
//! written ("The repository in git's terms", item 5), so a parsed object keeps the bytes it was read from, and writing
//! it gives them back.
//!
//! - [`tree`]: entries (mode, name, id) as git's `decode_tree_entry` reads them; git's order; the five modes git writes
//!   and the others fsck flags; `fsck_tree`'s checks.
//! - [`commit`]: headers (`tree`, `parent`, `author`, `committer`, `encoding`, `gpgsig`, `gpgsig-sha256`, `mergetag`
//!   and any other), each with its continuation lines, and the message; the payload a signature covers;
//!   `fsck_commit`'s checks.
//! - [`tag`]: headers (`object`, `type`, `tag`, `tagger`) and the message, with the signature block that may end it;
//!   `fsck_tag_standalone`'s checks.
//! - [`fsck`]: every message id of git's `fsck.h` at its default severity, and a check's [`Report`]: each finding a
//!   typed refusal or a typed warning, in the order git reports it.
//!
//! **Sources** (git v2.50.1, read 2026-11-19). The code \[C\]: `fsck.h` (`FOREACH_FSCK_MSG_ID` and each id's
//! severity), `fsck.c` (`fsck_tree`, `verify_ordered`, `verify_headers`, `fsck_ident`, `fsck_commit`,
//! `fsck_tag_standalone`, `report`), `tree-walk.c` (`decode_tree_entry`), `object.h` (`parse_mode`), `utf8.c`
//! (`is_hfs_dot_generic`), `path.c` (`is_ntfs_dotgit`, `is_ntfs_dot_generic`), `refs.c` (`check_refname_format`),
//! `date.c` (`date_overflows`), `commit.c` (`parse_buffer_signed_by_header`), `gpg-interface.c`
//! (`parse_signed_buffer`), and `builtin/index-pack.c` and `builtin/receive-pack.c` for how a push is checked; the
//! documents \[B\]: `gitformat-signature` (signed commits and tags, `mergetag`).
//!
//! **Strictness.** `git fsck` checks with warnings as warnings ([`Strictness::Default`]). A push that git checks
//! (`receive.fsckObjects`, or `transfer.fsckObjects`) runs `index-pack --strict`, whose options are
//! `FSCK_OPTIONS_MISSING_GITMODULES`, strict: a warning refuses ([`Strictness::Strict`]). Under either, an info is
//! reported as a warning and an ignored id is not reported.
//!
//! **Where slates and git part.** In one place, on purpose. git's tag check reads the tagger, then, without looking at
//! the tagger's result, checks for an extra header after it; that finding is ignored by default, and its result
//! overwrites the tagger's with success. So git prints a malformed tagger's error and accepts the tag anyway: stock
//! `git hash-object` writes `fixtures/fsck` `bad-tag-tagger-extra-header`. Here the tagger's error refuses the tag. And
//! one choice among git's builds: git for Windows also takes a backslash to end a name in its HFS+ check; here, as
//! git on Linux and macOS, only `/` does (HFS+ itself never reads a backslash as a separator).
//!
//! **Golden vectors.** `fixtures/fsck` holds what stock git 2.50.1 (Apple Git-155) wrote and said: in a SHA-1 and a
//! SHA-256 repository, trees, commits and tags made by git's own commands, and the malformed ones git's own
//! `t/t1450-fsck.sh` makes, one more for each check; and in the SHA-1 repository one object for every spelling of
//! `.gitmodules`, `.gitattributes`, `.gitignore` and `.mailmap` git's `t/t0060-path-utils.sh` judges, for spellings of
//! `.git`, and for tag names on either side of `refs.c`'s rules. For each object the manifest keeps what `git fsck`,
//! `git fsck --strict` and `git hash-object` said of it. `fixtures/fsck/make-fsck-fixtures.sh` made them on
//! 2026-11-19 (two runs, byte for byte the same); `SHA256SUMS` holds each file's SHA-256, and the tests check every
//! file against it and every object's id against its bytes.
//!
//! **Hostile input (§4.9).** Every byte is untrusted: every length is checked against the bytes left before it is
//! read, nothing recurses, a parse that cannot hold the bytes is a typed refusal naming where it stopped, and what a
//! parse keeps beside the bytes it borrows is charged to the caller's [`crate::Admission`] first. An object id in a
//! tree comes in as raw bytes of the repository's hash length; a commit's and a tag's ids are text, read as git reads
//! them. Hashing is [`crate::object_name`]'s, never done here.

pub mod commit;
mod dotgit;
pub mod fsck;
mod headers;
mod hex;
mod ident;
pub mod tag;
pub mod tree;

#[cfg(test)]
#[path = "../tests/objects/mod.rs"]
mod tests;

pub use fsck::{FsckId, Report, Severity, Strictness};
pub use headers::Header;
pub use ident::Ident;

use crate::PastAdmission;

/// Why a parse could not hold an object's bytes: each the reason git's own reading gives for the same bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
  /// A NUL before the end of a commit's or a tag's headers (`nulInHeader`, fatal in git).
  NulInHeader,
  /// A commit's or a tag's headers end with no newline (`unterminatedHeader`, fatal in git).
  UnterminatedHeader,
  /// A tree's last bytes are not a NUL and an object id ("too-short tree object"; `badTree`).
  TooShortTree,
  /// A tree entry's mode is not octal digits ending at a space ("malformed mode in tree entry"; `badTree`).
  MalformedMode,
  /// A tree entry with no name ("empty filename in tree entry"; `badTree`).
  EmptyName,
  /// What the parse would keep is more than the caller's admission holds: refused before anything was kept.
  PastAdmission(PastAdmission),
}

impl Refusal {
  /// The fsck message git reports for the same bytes; none for the caller's admission.
  pub fn fsck_id(self) -> Option<FsckId> {
    match self {
      Refusal::NulInHeader => Some(FsckId::NulInHeader),
      Refusal::UnterminatedHeader => Some(FsckId::UnterminatedHeader),
      Refusal::TooShortTree | Refusal::MalformedMode | Refusal::EmptyName => Some(FsckId::BadTree),
      Refusal::PastAdmission(_) => None,
    }
  }
}

/// A parse refused, at the offset into the object where it stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObjectRefused {
  /// The offset of the byte the parse stopped at: an entry's or a header's start, or the object's length when the
  /// bytes ended too soon.
  pub at: usize,
  /// Why.
  pub refusal: Refusal,
}
