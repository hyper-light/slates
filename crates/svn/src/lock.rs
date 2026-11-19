//! Locks (§4.17.4 item 7 and piece 8; item 3.4, "Locks at the verdict"; A-602): a repository's lock table, the four
//! lock verbs, and the check a commit meets, each rule as Subversion's FSFS and repository layer state it.
//!
//! A lock grants one principal the right to change one file: the owner alone may change, delete or move it, which
//! includes "renaming or deleting any of its parent directories" (`notes/locking/locking-functional-spec.txt`). Each
//! lock is an entry of the repository's register (§4.17.4 item 7): its path, its token (`opaquelocktoken:` and a UUID),
//! its owner, a comment, and its creation and expiry times. A lock is data the verdict reads, never a lock in the sense
//! of §2 bans 1 and 10: nothing waits on it, and a commit that meets someone else's lock is refused at once.
//!
//! The rules, read in Subversion's `1.14.x` branch at `d434dc14` (`docs/wip/research/vcs-servers-subversion.md` §7):
//! - **Locking** (`libsvn_fs_fs/lock.c` `check_lock` and `lock_body`; `libsvn_fs/fs-loader.c` `svn_fs_lock_many`;
//!   `libsvn_repos/fs-wrap.c` `svn_repos_fs_lock_many`). A request needs an authenticated principal (`E160034`), a
//!   comment that XML can carry (`E130004`) and an expiry that is not negative (`E200004`). Then each path on its own:
//!   a token the request supplies must be `opaquelocktoken:` and printable ASCII (`E160037`); only a file is locked
//!   (`E160017`); a path the head does not hold is `E160013`, or `E160042` when the client named the revision it holds;
//!   that revision past the head is `E160006`, and older than the file's last change `E160042`; a path locked already
//!   is `E160035` unless the request steals it, which replaces the lock and its token. A new lock's token is the one
//!   supplied or one the owner draws, in path order among the paths it locks.
//! - **Unlocking** (`check_unlock`, `unlock_body`, `svn_fs_fs__unlock`). It needs a principal even to break a lock
//!   (`E160034`). No lock at the path is `E160040`; then, unless the request breaks it, a token other than the lock's
//!   is `E160040` and another principal's lock `E160039`.
//! - **At the verdict** (`libsvn_fs_fs/transaction.c` `verify_locks`; `lock.c` `verify_lock` and
//!   `svn_fs_fs__allow_locked_operation`). A path the work modifies meets its own lock; a path it adds, deletes or
//!   replaces meets every lock at or beneath it, since "for paths being added or removed, we need to hold the locks for
//!   that path and any children of that path". Each lock met must be the committer's (`E160039`, checked first; no
//!   principal at all is `E160034`) and its token among those the commit presented (`E160037`), wherever in the commit
//!   the token was presented. The first lock that fails answers, in the order `verify_locks` meets them: the changed
//!   paths in Subversion's path order, a path's children right after it (A-617). An accepted commit then releases every presented lock whose token and owner match, the
//!   paths it did not change among them, unless the client asked to keep them (`svnserve/serve.c` `commit` and
//!   `unlock_paths`).
//! - **Expiry** (`lock.c` `lock_expired`). A lock expires once the time is past its expiry; one without an expiry never
//!   does. An expired lock is no lock to every rule, and a read does not show it.
//!
//! Guarded, then applied, as `crates/db/src/partition.rs` keeps its records. [`LockTable::lock`], [`LockTable::unlock`]
//! and [`LockTable::verdict`] are the owner's guards: each reads the table, the head ([`HeadView`]) and the instant the
//! owner's clock gave, and returns the entries to append ([`LockEntry`]) with the answers to give. [`LockTable::apply`]
//! applies an entry unconditionally, so a holder replaying the register reaches the owner's table without reading a
//! clock or drawing a token, and an expiry is judged only when a lock is used, on the owner (§4.17.4 item 7). The
//! owner reads its clock through the codebase's seam (`slates_vfs::clock::Clock::wall_ns`, made an [`AprTime`] by
//! [`AprTime::from_wall_ns`]) and draws a token's bytes from its random source; this module does neither.
//!
//! Where FSFS removes an expired lock it meets under its write lock, the guard records the removal, so the judgment
//! stands on every holder whatever their clocks say: a lock request replaces it, an unlock answers `E160041` and
//! removes it, and an accepted commit removes every expired lock its check met. A refused commit records nothing: FSFS
//! removes the expired locks its walk met before the refusing one, in the order of a hash table, which leaves which of
//! them it removed unspecified; here none is, and the next use judges each again.
//!
//! What slates adds to Subversion's rules, each stated in §4.17.4 item 7:
//! - **The room is the caller's**: the most locks the table may hold, which the owner derives from its shard's room
//!   (§4.17.4 item 12). A lock request whose paths could take the table past it first lets go of every expired lock;
//!   then a new lock past it is refused (`E160000`: Subversion has no closer error).
//! - **Release in the verdict's one step.** `svnserve` unlocks a commit's locks after the commit; here the check and
//!   the release are one step on the owner (item 3.4), so no lock changes between them.
//! - **A comment or a token is the whole string the client sent.** C reads both up to their first NUL byte; here a NUL
//!   is a control character like any other, so a comment holding one is `E130004` and a token `E160037`.
//! - **A path is in Subversion's canonical form** (`E160005` otherwise), as the server's wire layer makes every path.
//!
//! The oracle in the module's tests transcribes FSFS's functions named above into a model that changes its map of locks
//! in place, as FSFS changes its digest files, and the table must equal it on generated histories.
//!
//! No I/O, no clock, no randomness, no panic.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use crate::index::{Action, NodeKind, Revision, is_canonical};

/// The start of every lock token: the URI scheme of a DAV lock token, which FSFS writes and requires of a token a
/// request supplies.
/// Format: `"opaquelocktoken:"` (`libsvn_fs_fs/lock.c` `svn_fs_fs__generate_lock_token`; `libsvn_fs/fs-loader.c`
/// `svn_fs_lock_many`, "has bad scheme; expected 'opaquelocktoken'").
pub const TOKEN_SCHEME: &str = "opaquelocktoken:";

/// The bytes of a UUID, which the owner draws for a new lock's token.
/// Format: a UUID is 128 bits (RFC 9562 §4).
pub const UUID_BYTES: usize = 16;

/// The bytes of a UUID before each of its text's hyphens: its hex digits are grouped 8-4-4-4-12.
/// Format: RFC 9562 §4, the string representation; `apr_uuid_format`, which `svn_uuid_generate` uses.
const UUID_HYPHENS_BEFORE: [usize; 4] = [4, 6, 8, 10];

/// The byte of a UUID that holds its version in its high four bits.
/// Format: RFC 9562 §5.4, the version field, octet 6.
const UUID_VERSION_BYTE: usize = 6;

/// The low four bits of the version byte, which stay random.
/// Format: RFC 9562 §5.4, `random_a` beside the version field.
const UUID_VERSION_KEPT: u8 = 0x0F;

/// Version 4, a random UUID, in the version byte's high four bits.
/// Format: RFC 9562 §5.4, `ver` = 0b0100.
const UUID_VERSION_RANDOM: u8 = 0x40;

/// The byte of a UUID that holds its variant in its high two bits.
/// Format: RFC 9562 §4.1, the variant field, octet 8.
const UUID_VARIANT_BYTE: usize = 8;

/// The low six bits of the variant byte, which stay random.
/// Format: RFC 9562 §5.4, `random_b` beside the variant field.
const UUID_VARIANT_KEPT: u8 = 0x3F;

/// The variant RFC 9562 defines, in the variant byte's high two bits.
/// Format: RFC 9562 §4.1, `var` = 0b10.
const UUID_VARIANT_RFC: u8 = 0x80;

/// The control characters XML carries, which a comment may hold: tab, line feed, carriage return and delete.
/// Format: `libsvn_subr/xml.c` `svn_xml_is_xml_safe`, which refuses every other byte `svn_ctype_iscntrl` names (0 to
/// 31 and 127, `libsvn_subr/ctype.c`, the same bytes as `u8::is_ascii_control`).
const XML_SAFE_CONTROLS: [u8; 4] = [b'\t', b'\n', b'\r', b'\x7f'];

/// Nanoseconds in a microsecond: the clock seam reads nanoseconds and Subversion keeps microseconds.
/// Format: `apr_time_t` counts microseconds (`apr_time.h`, `APR_USEC_PER_SEC`).
const NANOSECONDS_PER_MICROSECOND: i64 = 1_000;

/// The root directory's path, the one path every other path is beneath.
/// Format: `svn_fspath__is_canonical`.
const ROOT: &str = "/";

/// The character between a path's names.
/// Format: `svn_fspath__join`.
const SEPARATOR: char = '/';

/// `SVN_ERR_XML_UNESCAPABLE_DATA`, "Data cannot be safely XML-escaped".
/// Format: the XML category's start, 120,000 + 2 × 5,000 = 130,000, plus 4 (`svn_error_codes.h`).
const SVN_ERR_XML_UNESCAPABLE_DATA: u32 = 130_004;

/// `SVN_ERR_FS_GENERAL`, "General filesystem error".
/// Format: the filesystem category's start, 120,000 + 8 × 5,000 = 160,000, plus 0 (`svn_error_codes.h`).
const SVN_ERR_FS_GENERAL: u32 = 160_000;

/// `SVN_ERR_FS_CORRUPT`.
/// Format: the filesystem category's start plus 4 (`svn_error_codes.h`).
const SVN_ERR_FS_CORRUPT: u32 = 160_004;

/// `SVN_ERR_FS_PATH_SYNTAX`.
/// Format: the filesystem category's start plus 5 (`svn_error_codes.h`).
const SVN_ERR_FS_PATH_SYNTAX: u32 = 160_005;

/// `SVN_ERR_FS_NO_SUCH_REVISION`.
/// Format: the filesystem category's start plus 6 (`svn_error_codes.h`).
const SVN_ERR_FS_NO_SUCH_REVISION: u32 = 160_006;

/// `SVN_ERR_FS_NOT_FOUND`, "Filesystem has no item".
/// Format: the filesystem category's start plus 13 (`svn_error_codes.h`).
const SVN_ERR_FS_NOT_FOUND: u32 = 160_013;

/// `SVN_ERR_FS_NOT_FILE`, "Name does not refer to a filesystem file".
/// Format: the filesystem category's start plus 17 (`svn_error_codes.h`).
const SVN_ERR_FS_NOT_FILE: u32 = 160_017;

/// `SVN_ERR_FS_NO_USER`, "No user associated with filesystem".
/// Format: the filesystem category's start plus 34 (`svn_error_codes.h`).
const SVN_ERR_FS_NO_USER: u32 = 160_034;

/// `SVN_ERR_FS_PATH_ALREADY_LOCKED`, "Path is already locked".
/// Format: the filesystem category's start plus 35 (`svn_error_codes.h`).
const SVN_ERR_FS_PATH_ALREADY_LOCKED: u32 = 160_035;

/// `SVN_ERR_FS_BAD_LOCK_TOKEN`, "Lock token is incorrect".
/// Format: the filesystem category's start plus 37 (`svn_error_codes.h`).
const SVN_ERR_FS_BAD_LOCK_TOKEN: u32 = 160_037;

/// `SVN_ERR_FS_LOCK_OWNER_MISMATCH`, "Username does not match lock owner".
/// Format: the filesystem category's start plus 39 (`svn_error_codes.h`).
const SVN_ERR_FS_LOCK_OWNER_MISMATCH: u32 = 160_039;

/// `SVN_ERR_FS_NO_SUCH_LOCK`, "Filesystem has no such lock".
/// Format: the filesystem category's start plus 40 (`svn_error_codes.h`).
const SVN_ERR_FS_NO_SUCH_LOCK: u32 = 160_040;

/// `SVN_ERR_FS_LOCK_EXPIRED`, "Lock has expired".
/// Format: the filesystem category's start plus 41 (`svn_error_codes.h`).
const SVN_ERR_FS_LOCK_EXPIRED: u32 = 160_041;

/// `SVN_ERR_FS_OUT_OF_DATE`, "Item is out of date".
/// Format: the filesystem category's start plus 42 (`svn_error_codes.h`).
const SVN_ERR_FS_OUT_OF_DATE: u32 = 160_042;

/// `SVN_ERR_INCORRECT_PARAMS`, "Incorrect parameters given".
/// Format: the miscellaneous category's start, 120,000 + 16 × 5,000 = 200,000, plus 4 (`svn_error_codes.h`).
const SVN_ERR_INCORRECT_PARAMS: u32 = 200_004;

/// A time as Subversion keeps one (`apr_time_t`): microseconds since the Unix epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AprTime(pub i64);

impl AprTime {
  /// The time a reading of the codebase's clock seam gives (`slates_vfs::clock::Clock::wall_ns`, nanoseconds since the
  /// epoch), in the whole microseconds Subversion keeps.
  pub fn from_wall_ns(wall_ns: i64) -> Self {
    Self(wall_ns / NANOSECONDS_PER_MICROSECOND)
  }
}

/// A lock's token: `opaquelocktoken:` and a UUID (§4.17.4 item 7).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LockToken(String);

impl LockToken {
  /// The token of a new lock made from sixteen bytes the owner drew from its random source: `opaquelocktoken:` and the
  /// bytes as a random UUID (RFC 9562 version 4) in lower-case hex, as `svn_fs_fs__generate_lock_token` writes one
  /// from `svn_uuid_generate`. The owner draws before it appends, so every holder replays the same token.
  pub fn drawn(random: [u8; UUID_BYTES]) -> Self {
    let mut text = String::from(TOKEN_SCHEME);
    for (at, byte) in random.into_iter().enumerate() {
      if UUID_HYPHENS_BEFORE.contains(&at) {
        text.push('-');
      }
      let byte = match at {
        UUID_VERSION_BYTE => (byte & UUID_VERSION_KEPT) | UUID_VERSION_RANDOM,
        UUID_VARIANT_BYTE => (byte & UUID_VARIANT_KEPT) | UUID_VARIANT_RFC,
        _ => byte,
      };
      text.push_str(&format!("{byte:02x}"));
    }
    Self(text)
  }

  /// A token a request supplies (a DAV client refreshing its lock, a pre-lock hook), checked as `svn_fs_lock_many`
  /// checks it: `opaquelocktoken:` first, then nothing but printable ASCII, every byte below 128 and none a control
  /// character. Refused `E160037` otherwise.
  pub fn supplied(token: &str) -> Result<Self, LockRefusal> {
    let printable = token
      .bytes()
      .all(|byte| byte.is_ascii() && !byte.is_ascii_control());
    if token.starts_with(TOKEN_SCHEME) && printable {
      Ok(Self(token.to_owned()))
    } else {
      Err(LockRefusal::MalformedToken {
        token: token.to_owned(),
      })
    }
  }

  /// The token's text.
  pub fn as_str(&self) -> &str {
    &self.0
  }
}

/// One lock (`svn_types.h` `svn_lock_t`; §4.17.4 item 7).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lock {
  /// The locked path: a file's, when the lock was made.
  pub path: String,
  /// Its token.
  pub token: LockToken,
  /// The principal who holds it.
  pub owner: String,
  /// The comment made with it, if any.
  pub comment: Option<String>,
  /// Whether the comment came as a DAV lock request's owner element, which a DAV answer gives back as it was written.
  pub is_dav_comment: bool,
  /// When it was made, on the owner's clock.
  pub created: AprTime,
  /// When it expires, if it ever does.
  pub expires: Option<AprTime>,
}

impl Lock {
  /// Whether the lock has expired at `now`: it has an expiry and `now` is past it (`lock.c` `lock_expired`).
  pub fn expired(&self, now: AprTime) -> bool {
    self.expires.is_some_and(|at| now > at)
  }
}

/// What the lock table reads of the repository's head: its youngest revision, and the node at a path there (the
/// index's node rows, §4.17.4 item 2). The server answers it from the index; the module's tests from a model.
pub trait HeadView {
  /// The youngest revision.
  fn youngest(&self) -> Revision;

  /// The node at `path` in the youngest revision, its kind and the revision it was created in, or `None` when the path
  /// holds nothing there.
  fn node(&self, path: &str) -> Option<(NodeKind, Revision)>;
}

/// One path a lock request names (`svn_fs_lock_target_t`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LockTarget {
  /// The revision of the file the client's working copy holds, when it sent one (`current-rev`).
  pub current: Option<Revision>,
  /// A token the request supplies, checked by [`LockToken::supplied`]; without one the owner draws one.
  pub token: Option<String>,
}

/// A lock request: `lock` or `lock-many` over `svn://`, a LOCK over HTTP (`svn_repos_fs_lock_many`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LockRequest {
  /// The authenticated principal; `None` for an anonymous one.
  pub user: Option<String>,
  /// The comment, as the client's bytes.
  pub comment: Option<Vec<u8>>,
  /// Whether the comment came as a DAV lock request's owner element.
  pub is_dav_comment: bool,
  /// When the locks expire. `svn://` never names a time; a DAV LOCK's `Timeout` does. Zero is none, as FSFS keeps it.
  pub expires: Option<AprTime>,
  /// Whether a lock someone holds is stolen, replaced by a new one (`steal-lock`, `svn lock --force`).
  pub steal: bool,
  /// The paths, each once.
  pub targets: BTreeMap<String, LockTarget>,
}

/// An unlock request: `unlock` or `unlock-many` over `svn://`, an UNLOCK over HTTP (`svn_repos_fs_unlock_many`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UnlockRequest {
  /// The authenticated principal; `None` for an anonymous one.
  pub user: Option<String>,
  /// Whether whoever's lock it is is broken, its token unasked (`break-lock`, `svn unlock --force`).
  pub break_lock: bool,
  /// The paths, each with the token the client sent, if it sent one.
  pub targets: BTreeMap<String, Option<String>>,
}

/// What a commit brings to the verdict's lock check (`commit`'s `lock-tokens` and `keep-locks`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommitLocks {
  /// The authenticated principal; `None` for an anonymous one.
  pub user: Option<String>,
  /// The lock tokens the commit presented: each path with the token the client sent for it.
  pub presented: BTreeMap<String, String>,
  /// Whether the client asked to keep its locks (`svn commit --no-unlock`).
  pub keep: bool,
}

/// How far beneath a path `get-locks` reaches (`svn_depth_t`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Depth {
  /// The path alone.
  Empty,
  /// The path and the files directly in it.
  Files,
  /// The path and everything directly in it; for locks, which only files hold, the same as `Files`.
  Immediates,
  /// The path and everything beneath it.
  Infinity,
}

impl Depth {
  /// Whether a lock at `path` is reached from `base` (`lock.c` `get_locks_filter_func`).
  fn reaches(self, base: &str, path: &str) -> bool {
    match self {
      Self::Infinity => true,
      Self::Empty => path == base,
      Self::Files | Self::Immediates => {
        path == base || relative(base, path).is_some_and(|rest| !rest.contains(SEPARATOR))
      }
    }
  }
}

/// A change to the lock table, as one entry of the repository's register records it (§4.17.4 item 7).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LockEntry {
  /// A lock set: a new one, or one replacing a stolen or an expired lock at its path.
  Set(Lock),
  /// The lock at a path removed: unlocked by its owner, broken, released by a commit, or met expired.
  Removed {
    /// The path.
    path: String,
    /// The removed lock's token, which the lock at the path holds when the entry is applied.
    token: LockToken,
  },
}

/// What a guard decided: the entries the owner appends, then each path's answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decision<Answer> {
  /// The entries, in the order they are applied.
  pub entries: Vec<LockEntry>,
  /// Each path's answer, in path order.
  pub answers: Vec<(String, Answer)>,
}

/// A path's answer to a lock request: its new lock, or why it has none.
pub type LockAnswer = Result<Lock, LockRefusal>;

/// A path's answer to an unlock request.
pub type UnlockAnswer = Result<(), LockRefusal>;

/// Why a lock verb or the verdict refused, each answered with a Subversion error ([`LockRefusal::subversion_code`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LockRefusal {
  /// A path out of Subversion's canonical form (`SVN_ERR_FS_PATH_SYNTAX`).
  PathSyntax {
    /// The path.
    path: String,
  },
  /// No authenticated principal (`SVN_ERR_FS_NO_USER`).
  NoUser {
    /// The path of the lock a commit could not verify; `None` for a lock or an unlock request.
    path: Option<String>,
  },
  /// A comment that is not UTF-8, or that holds a control character other than tab, line feed, carriage return and
  /// delete (`SVN_ERR_XML_UNESCAPABLE_DATA`).
  UnescapableComment,
  /// An expiry before the epoch (`SVN_ERR_INCORRECT_PARAMS`).
  NegativeExpiry {
    /// The expiry.
    expires: AprTime,
  },
  /// A supplied token that is not `opaquelocktoken:` and printable ASCII (`SVN_ERR_FS_BAD_LOCK_TOKEN`).
  MalformedToken {
    /// The token.
    token: String,
  },
  /// A path whose node at the head is a directory: only a file is locked (`SVN_ERR_FS_NOT_FILE`).
  NotFile {
    /// The path.
    path: String,
  },
  /// A path the head does not hold, the client naming no revision (`SVN_ERR_FS_NOT_FOUND`).
  NotFound {
    /// The path.
    path: String,
  },
  /// A path the head does not hold, the client naming the revision it holds; or a file changed after that revision
  /// (`SVN_ERR_FS_OUT_OF_DATE`).
  OutOfDate {
    /// The path.
    path: String,
  },
  /// A revision past the head (`SVN_ERR_FS_NO_SUCH_REVISION`).
  NoSuchRevision {
    /// The revision.
    revision: Revision,
  },
  /// A path locked already, the request not stealing (`SVN_ERR_FS_PATH_ALREADY_LOCKED`).
  AlreadyLocked {
    /// The path.
    path: String,
    /// The lock's owner.
    owner: String,
  },
  /// No lock at the path, or a lock whose token is not the one sent (`SVN_ERR_FS_NO_SUCH_LOCK`).
  NoSuchLock {
    /// The path.
    path: String,
  },
  /// The lock at the path has expired, and is removed (`SVN_ERR_FS_LOCK_EXPIRED`).
  Expired {
    /// The path.
    path: String,
  },
  /// A lock another principal holds (`SVN_ERR_FS_LOCK_OWNER_MISMATCH`).
  OwnerMismatch {
    /// The lock's path.
    path: String,
    /// The lock's owner.
    owner: String,
  },
  /// A lock a commit meets whose token it did not present (`SVN_ERR_FS_BAD_LOCK_TOKEN`).
  TokenNotPresented {
    /// The lock's path.
    path: String,
  },
  /// A new lock past the table's room, once every expired lock was let go (`SVN_ERR_FS_GENERAL`: Subversion has no
  /// closer error).
  Room {
    /// The room given.
    room: usize,
  },
  /// An entry removing a lock the table does not hold, which no guard writes (`SVN_ERR_FS_CORRUPT`).
  Unrecorded {
    /// The entry's path.
    path: String,
  },
}

impl LockRefusal {
  /// The number of the Subversion error a client is answered with.
  pub fn subversion_code(&self) -> u32 {
    match self {
      Self::PathSyntax { .. } => SVN_ERR_FS_PATH_SYNTAX,
      Self::NoUser { .. } => SVN_ERR_FS_NO_USER,
      Self::UnescapableComment => SVN_ERR_XML_UNESCAPABLE_DATA,
      Self::NegativeExpiry { .. } => SVN_ERR_INCORRECT_PARAMS,
      Self::MalformedToken { .. } | Self::TokenNotPresented { .. } => SVN_ERR_FS_BAD_LOCK_TOKEN,
      Self::NotFile { .. } => SVN_ERR_FS_NOT_FILE,
      Self::NotFound { .. } => SVN_ERR_FS_NOT_FOUND,
      Self::OutOfDate { .. } => SVN_ERR_FS_OUT_OF_DATE,
      Self::NoSuchRevision { .. } => SVN_ERR_FS_NO_SUCH_REVISION,
      Self::AlreadyLocked { .. } => SVN_ERR_FS_PATH_ALREADY_LOCKED,
      Self::NoSuchLock { .. } => SVN_ERR_FS_NO_SUCH_LOCK,
      Self::Expired { .. } => SVN_ERR_FS_LOCK_EXPIRED,
      Self::OwnerMismatch { .. } => SVN_ERR_FS_LOCK_OWNER_MISMATCH,
      Self::Room { .. } => SVN_ERR_FS_GENERAL,
      Self::Unrecorded { .. } => SVN_ERR_FS_CORRUPT,
    }
  }
}

/// A repository's lock table: every lock it holds, by path, expired ones included until an entry removes them
/// (§4.17.4 item 7).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LockTable {
  /// The locks, by path.
  locks: BTreeMap<String, Lock>,
}

impl LockTable {
  /// An empty table, a new repository's.
  pub fn new() -> Self {
    Self::default()
  }

  /// Every lock the table holds, expired ones too, in path order: what a holder's replay compares with the owner's.
  pub fn locks(&self) -> impl Iterator<Item = &Lock> {
    self.locks.values()
  }

  /// The lock at `path` that has not expired at `now` (`get-lock`; `lock.c` `svn_fs_fs__get_lock`). A read removes
  /// nothing.
  pub fn get_lock(&self, path: &str, now: AprTime) -> Option<&Lock> {
    self.locks.get(path).filter(|lock| !lock.expired(now))
  }

  /// The locks at and beneath `path` that `depth` reaches and that have not expired at `now`, in path order
  /// (`get-locks`; `lock.c` `svn_fs_fs__get_locks`). A read removes nothing.
  pub fn get_locks(&self, path: &str, depth: Depth, now: AprTime) -> Vec<&Lock> {
    self
      .at_or_beneath(path)
      .into_iter()
      .filter(|lock| !lock.expired(now) && depth.reaches(path, &lock.path))
      .collect()
  }

  /// The guard of a lock request (module documentation, "Locking"): judged at `now` against `head`, within `room`
  /// locks, each new token drawn from `draw` in path order. The request is refused whole for no principal, a comment
  /// XML cannot carry or a negative expiry; otherwise each path has its answer, and the entries set each new lock after
  /// letting go of any expired locks the room needed.
  pub fn lock(
    &self,
    request: &LockRequest,
    head: &impl HeadView,
    now: AprTime,
    room: usize,
    draw: &mut impl FnMut() -> [u8; UUID_BYTES],
  ) -> Result<Decision<LockAnswer>, LockRefusal> {
    let owner = request
      .user
      .as_deref()
      .ok_or(LockRefusal::NoUser { path: None })?;
    let comment = request.comment.as_deref().map(xml_safe).transpose()?;
    let expires = expiry(request.expires)?;
    let entries = self.let_go_for(request.targets.len(), room, now);
    let mut walk = Walk {
      table: self,
      room,
      held: self.locks.len().saturating_sub(entries.len()),
      let_go: entries.iter().filter_map(removed_path).collect(),
    };
    let mut decision = Decision {
      entries,
      answers: Vec::with_capacity(request.targets.len()),
    };
    for (path, target) in &request.targets {
      let answer = self
        .admit(path, target, request.steal, head, now)
        .and_then(|supplied| walk.make_room(path).map(|()| supplied))
        .map(|supplied| Lock {
          path: path.clone(),
          token: supplied.unwrap_or_else(|| LockToken::drawn(draw())),
          owner: owner.to_owned(),
          comment: comment.clone(),
          is_dav_comment: request.is_dav_comment,
          created: now,
          expires,
        });
      if let Ok(lock) = &answer {
        decision.entries.push(LockEntry::Set(lock.clone()));
      }
      decision.answers.push((path.clone(), answer));
    }
    Ok(decision)
  }

  /// The guard of an unlock request (module documentation, "Unlocking"), judged at `now`. Refused whole without a
  /// principal; otherwise each path has its answer, and the entries remove each lock unlocked, broken or met expired.
  pub fn unlock(
    &self,
    request: &UnlockRequest,
    now: AprTime,
  ) -> Result<Decision<UnlockAnswer>, LockRefusal> {
    let user = request
      .user
      .as_deref()
      .ok_or(LockRefusal::NoUser { path: None })?;
    let mut decision = Decision {
      entries: Vec::new(),
      answers: Vec::with_capacity(request.targets.len()),
    };
    for (path, token) in &request.targets {
      let (entry, answer) = self.unlock_one(path, token.as_deref(), user, request.break_lock, now);
      decision.entries.extend(entry);
      decision.answers.push((path.clone(), answer));
    }
    Ok(decision)
  }

  /// Locks at the verdict (§4.17.4 item 3.4; module documentation, "At the verdict"): the work's `changes`, each a path
  /// and what the work did to it, checked at `now` against the locks they meet, refused at the first lock the commit may
  /// not pass in the order `verify_locks` meets them (Subversion's path order, A-617). Accepted, the entries remove the expired locks the check met and, unless the commit
  /// keeps its locks, every presented lock whose token and owner match; the merge task appends them in the verdict's
  /// one step. A refused commit records nothing.
  pub fn verdict<'c>(
    &self,
    changes: impl IntoIterator<Item = (&'c str, Action)>,
    commit: &CommitLocks,
    now: AprTime,
  ) -> Result<Vec<LockEntry>, LockRefusal> {
    let presented: BTreeSet<&str> = commit.presented.values().map(String::as_str).collect();
    let mut removed = BTreeMap::new();
    for lock in self.met(changes)? {
      if lock.expired(now) {
        removed.insert(lock.path.as_str(), lock);
      } else {
        verify(lock, commit.user.as_deref(), &presented)?;
      }
    }
    if !commit.keep {
      removed.extend(self.released(commit, now));
    }
    Ok(removed.into_values().map(removal).collect())
  }

  /// Applies an entry the owner's guard wrote, unconditionally, so a replay reaches the owner's table. An entry
  /// removing a lock the table does not hold is a history no guard wrote, refused typed with the table unchanged.
  pub fn apply(&mut self, entry: &LockEntry) -> Result<(), LockRefusal> {
    match entry {
      LockEntry::Set(lock) => {
        self.locks.insert(lock.path.clone(), lock.clone());
        Ok(())
      }
      LockEntry::Removed { path, token } => match self.locks.get(path) {
        Some(lock) if lock.token == *token => {
          self.locks.remove(path);
          Ok(())
        }
        _ => Err(LockRefusal::Unrecorded { path: path.clone() }),
      },
    }
  }

  /// One path of a lock request checked as FSFS checks it, in its order (`svn_fs_lock_many`'s token check, then
  /// `check_lock`): the supplied token, if any, when every check passes.
  fn admit(
    &self,
    path: &str,
    target: &LockTarget,
    steal: bool,
    head: &impl HeadView,
    now: AprTime,
  ) -> Result<Option<LockToken>, LockRefusal> {
    checked(path)?;
    let supplied = target
      .token
      .as_deref()
      .map(LockToken::supplied)
      .transpose()?;
    let created = file_at_head(path, target.current, head)?;
    if let Some(current) = target.current {
      if current > head.youngest() {
        return Err(LockRefusal::NoSuchRevision { revision: current });
      }
      if current < created {
        return Err(LockRefusal::OutOfDate {
          path: path.to_owned(),
        });
      }
    }
    match self.get_lock(path, now) {
      Some(held) if !steal => Err(LockRefusal::AlreadyLocked {
        path: path.to_owned(),
        owner: held.owner.clone(),
      }),
      _ => Ok(supplied),
    }
  }

  /// The entries letting go of every expired lock, when a request naming `targets` paths could take the table past
  /// `room`; none otherwise.
  fn let_go_for(&self, targets: usize, room: usize, now: AprTime) -> Vec<LockEntry> {
    if self.locks.len().saturating_add(targets) <= room {
      return Vec::new();
    }
    self
      .locks
      .values()
      .filter(|lock| lock.expired(now))
      .map(removal)
      .collect()
  }

  /// One path of an unlock request (`lock.c` `check_unlock` and `unlock_body`): the entry it writes, if any, and its
  /// answer.
  fn unlock_one(
    &self,
    path: &str,
    token: Option<&str>,
    user: &str,
    break_lock: bool,
    now: AprTime,
  ) -> (Option<LockEntry>, UnlockAnswer) {
    if let Err(refusal) = checked(path) {
      return (None, Err(refusal));
    }
    let Some(lock) = self.locks.get(path) else {
      return (
        None,
        Err(LockRefusal::NoSuchLock {
          path: path.to_owned(),
        }),
      );
    };
    if lock.expired(now) {
      let refusal = LockRefusal::Expired {
        path: path.to_owned(),
      };
      return (Some(removal(lock)), Err(refusal));
    }
    match unlocks(lock, token, user, break_lock) {
      Ok(()) => (Some(removal(lock)), Ok(())),
      Err(refusal) => (None, Err(refusal)),
    }
  }

  /// The locks the work's changes meet, each once, in the order `transaction.c` `verify_locks` meets them (3527-3593):
  /// the changed paths in Subversion's path order (`svn_sort_compare_items_as_paths`, 3539), a modified path's own lock
  /// and every lock at or beneath a path added, deleted or replaced, a path beneath the last one so read skipped (3564),
  /// since its locks were met there. Beneath a path read whole, FSFS walks the lock digests in a hash table's order,
  /// which nothing fixes; here in the same path order, the path's own lock first, as FSFS's walk sends it first.
  fn met<'c>(
    &self,
    changes: impl IntoIterator<Item = (&'c str, Action)>,
  ) -> Result<Vec<&Lock>, LockRefusal> {
    let mut met: Vec<&Lock> = Vec::new();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut read_whole: Option<&str> = None;
    for (path, action) in in_path_order(changes)? {
      if read_whole.is_some_and(|whole| path == whole || relative(whole, path).is_some()) {
        continue;
      }
      let mut reached: Vec<&Lock> = match action {
        Action::Modify => self.locks.get(path).into_iter().collect(),
        Action::Add | Action::Delete | Action::Replace => {
          read_whole = Some(path);
          self.at_or_beneath(path)
        }
      };
      reached.sort_by(|first, second| path_order(&first.path, &second.path));
      met.extend(
        reached
          .into_iter()
          .filter(|lock| seen.insert(lock.path.as_str())),
      );
    }
    Ok(met)
  }

  /// The presented locks an accepted commit releases (`serve.c` `unlock_paths`, which unlocks each presented path with
  /// its token, never breaking): each whose token and owner match, and each met expired, which FSFS removes as it
  /// looks. Without a principal, none (`svn_repos_fs_unlock_many` refuses the whole release).
  fn released(&self, commit: &CommitLocks, now: AprTime) -> Vec<(&str, &Lock)> {
    let Some(user) = commit.user.as_deref() else {
      return Vec::new();
    };
    commit
      .presented
      .iter()
      .filter_map(|(path, token)| {
        self.locks.get(path).filter(|lock| {
          lock.expired(now) || (lock.token.as_str() == token.as_str() && lock.owner == user)
        })
      })
      .map(|lock| (lock.path.as_str(), lock))
      .collect()
  }

  /// Every lock at `path` or beneath it, in path order, expired ones too.
  fn at_or_beneath(&self, path: &str) -> Vec<&Lock> {
    let (own, prefix) = if path == ROOT {
      (None, ROOT.to_owned())
    } else {
      (self.locks.get(path), format!("{path}{SEPARATOR}"))
    };
    own
      .into_iter()
      .chain(
        self
          .locks
          .range(prefix.clone()..)
          .take_while(|(beneath, _)| beneath.starts_with(prefix.as_str()))
          .map(|(_, lock)| lock),
      )
      .collect()
  }
}

/// A lock request's walk over its paths, keeping the table within its room as new locks are made.
struct Walk<'t> {
  /// The table as it stood before the request.
  table: &'t LockTable,
  /// The most locks the table may hold.
  room: usize,
  /// The locks the table holds as the walk stands: before the request, less those let go, plus the new ones made.
  held: usize,
  /// The paths whose expired locks the request let go.
  let_go: BTreeSet<String>,
}

impl Walk<'_> {
  /// Makes room for a lock at `path`: none is needed to replace a lock the table still holds; a new one is refused past
  /// the room, and otherwise counted.
  fn make_room(&mut self, path: &str) -> Result<(), LockRefusal> {
    let replaces = self.table.locks.contains_key(path) && !self.let_go.contains(path);
    if replaces {
      return Ok(());
    }
    if self.held >= self.room {
      return Err(LockRefusal::Room { room: self.room });
    }
    self.held = self.held.saturating_add(1);
    Ok(())
  }
}

/// `path`, refused unless in Subversion's canonical form.
fn checked(path: &str) -> Result<(), LockRefusal> {
  if is_canonical(path) {
    Ok(())
  } else {
    Err(LockRefusal::PathSyntax {
      path: path.to_owned(),
    })
  }
}

/// The revision the file at `path` was created in at the head, or why it cannot be locked (`check_lock`): a directory,
/// or nothing there, which is out of date when the client named the revision it holds.
fn file_at_head(
  path: &str,
  current: Option<Revision>,
  head: &impl HeadView,
) -> Result<Revision, LockRefusal> {
  let path = path.to_owned();
  match head.node(&path) {
    Some((NodeKind::File, created)) => Ok(created),
    Some((NodeKind::Directory, _)) => Err(LockRefusal::NotFile { path }),
    None if current.is_some() => Err(LockRefusal::OutOfDate { path }),
    None => Err(LockRefusal::NotFound { path }),
  }
}

/// A comment checked as `svn_xml_is_xml_safe` checks it: UTF-8, and no control character but tab, line feed, carriage
/// return and delete.
fn xml_safe(comment: &[u8]) -> Result<String, LockRefusal> {
  let text = std::str::from_utf8(comment).map_err(|_| LockRefusal::UnescapableComment)?;
  let unsafe_control = text
    .bytes()
    .any(|byte| byte.is_ascii_control() && !XML_SAFE_CONTROLS.contains(&byte));
  if unsafe_control {
    return Err(LockRefusal::UnescapableComment);
  }
  Ok(text.to_owned())
}

/// The expiry a lock is made with: none for none or zero, refused before the epoch (`svn_fs_lock_many`).
fn expiry(requested: Option<AprTime>) -> Result<Option<AprTime>, LockRefusal> {
  match requested {
    Some(expires) if expires.0 < 0 => Err(LockRefusal::NegativeExpiry { expires }),
    Some(AprTime(0)) | None => Ok(None),
    Some(expires) => Ok(Some(expires)),
  }
}

/// Whether `user`, sending `token`, may unlock `lock` (`check_unlock`): a break may; otherwise the token must be the
/// lock's (`E160040`) and the user its owner (`E160039`).
fn unlocks(lock: &Lock, token: Option<&str>, user: &str, break_lock: bool) -> UnlockAnswer {
  if break_lock {
    return Ok(());
  }
  if token != Some(lock.token.as_str()) {
    return Err(LockRefusal::NoSuchLock {
      path: lock.path.clone(),
    });
  }
  if user != lock.owner {
    return Err(LockRefusal::OwnerMismatch {
      path: lock.path.clone(),
      owner: lock.owner.clone(),
    });
  }
  Ok(())
}

/// Whether a commit by `user` presenting `presented` may pass `lock` (`lock.c` `verify_lock`, in its order).
fn verify(lock: &Lock, user: Option<&str>, presented: &BTreeSet<&str>) -> Result<(), LockRefusal> {
  let Some(user) = user else {
    return Err(LockRefusal::NoUser {
      path: Some(lock.path.clone()),
    });
  };
  if user != lock.owner {
    return Err(LockRefusal::OwnerMismatch {
      path: lock.path.clone(),
      owner: lock.owner.clone(),
    });
  }
  if !presented.contains(lock.token.as_str()) {
    return Err(LockRefusal::TokenNotPresented {
      path: lock.path.clone(),
    });
  }
  Ok(())
}

/// The entry removing `lock`.
fn removal(lock: &Lock) -> LockEntry {
  LockEntry::Removed {
    path: lock.path.clone(),
    token: lock.token.clone(),
  }
}

/// The path an entry removes a lock at, if it removes one.
fn removed_path(entry: &LockEntry) -> Option<String> {
  match entry {
    LockEntry::Removed { path, .. } => Some(path.clone()),
    LockEntry::Set(_) => None,
  }
}

/// The changed paths, each checked canonical (`E160005`), sorted as `verify_locks` sorts them ([`path_order`]).
fn in_path_order<'c>(
  changes: impl IntoIterator<Item = (&'c str, Action)>,
) -> Result<Vec<(&'c str, Action)>, LockRefusal> {
  let mut ordered = Vec::new();
  for (path, action) in changes {
    checked(path)?;
    ordered.push((path, action));
  }
  ordered.sort_by(|(first, _), (second, _)| path_order(first, second));
  Ok(ordered)
}

/// Subversion's order of two canonical paths (`path.c` `svn_path_compare_paths` 418-453, by which
/// `svn_sort_compare_items_as_paths` sorts): "Children of paths are greater than their parents, but less than greater
/// siblings of their parents", which is their names compared one by one (`/a/b` before `/a-b`, which byte order puts
/// first).
fn path_order(first: &str, second: &str) -> Ordering {
  first.split(SEPARATOR).cmp(second.split(SEPARATOR))
}

/// `path` relative to `base` when it lies beneath it (`svn_fspath__skip_ancestor`, less the path itself).
fn relative<'p>(base: &str, path: &'p str) -> Option<&'p str> {
  if base == ROOT {
    return path.strip_prefix(ROOT).filter(|rest| !rest.is_empty());
  }
  path
    .strip_prefix(base)
    .and_then(|rest| rest.strip_prefix(SEPARATOR))
}

#[cfg(test)]
mod tests;
