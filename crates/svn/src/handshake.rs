//! The ra_svn connection's setup and authentication, the server's side (§4.17.4 "5. The wire" and
//! "10. Who may do what", piece 7's first part; A-599): the greeting, the client's answer, the
//! authentication request and its mechanisms (`ANONYMOUS`, `CRAM-MD5`, `EXTERNAL`), and the
//! repository's information, as a state machine that does no I/O. The caller moves the bytes, looks
//! the client's URL up, draws a challenge's random number and reads the clock, and holds the users'
//! keys; everything the protocol decides is decided here.
//!
//! The exchange, as `svnserve` makes it (Subversion's `1.14.x` branch at `d434dc14`:
//! `subversion/libsvn_ra_svn/protocol` §2, `svnserve/serve.c` `construct_server_baton` 4209,
//! `send_mechs` 580, `auth` 649, `internal_auth_request` 693 and `find_repos` 3882, and
//! `libsvn_ra_svn/cram.c` 134; `docs/wip/research/vcs-servers-subversion.md` §2):
//! 1. The server greets, `( success ( 2 2 ( ) ( cap ... ) ) )`: version 2 to 2, an empty mechanism
//!    list (it belonged to version 1, which is gone), and `svnserve`'s capabilities without
//!    `svndiff1`, so no peer ever sends slates a zlib stream (§4.17.4 "5. The wire"). That one word
//!    is the greeting's only difference from `svnserve`'s.
//! 2. The client answers, `( version ( cap ... ) url ? ra-client ( ? client ) )`, read as `serve.c`
//!    reads it (`"nlc?c(?c)"` through `marshal.c` `vparse_tuple`): items past the prototype are
//!    ignored, a `ra-client` comes with the list after it or not at all, and a mistyped item
//!    refuses the answer. Then version 2 alone is taken, every capability must be a word, and
//!    `edit-pipeline` must be among them. `svnserve` ends the connection at each of these refusals
//!    with nothing written, and so does this ([`Refused`]).
//! 3. The caller canonicalizes the URL and looks it up ([`Progress::Opened`]), then refuses it with
//!    a command failure ([`Server::refuse`]) or names the repository ([`Server::open`]); a URL
//!    `svnserve` could not canonicalize ends its connection with nothing written, so the caller
//!    just closes. Named, the server asks for authentication, `( success ( ( mech ... ) realm ) )`,
//!    listing `ANONYMOUS`, `EXTERNAL` and `CRAM-MD5` in `send_mechs`' order as the repository
//!    allows each; a repository that allows none is refused "No access allowed to this repository",
//!    as `serve.c` refuses it before any request.
//! 4. The client answers, `( mech ( [ token ] ) )`. `EXTERNAL` takes the tunnel's user, failing a
//!    token that names another; `ANONYMOUS` takes no one and ignores its token; `CRAM-MD5` sends a
//!    challenge, `( step ( <nonce.time@host> ) )`, whose parts the caller gives
//!    ([`Server::challenge`]), reads the client's `user hexdigest` string, asks the caller for that
//!    user's key ([`Progress::KeyNeeded`], [`Server::key`]) and compares the digest. Any other
//!    mechanism is "Must authenticate with listed mechanism". A failed attempt is answered
//!    `( failure ( message ) )` and the client may answer again, as `internal_auth_request`'s loop
//!    lets it.
//! 5. Authenticated, the server writes `( success ( ) )` and the repository's information,
//!    `( success ( uuid url ( [ mergeinfo ] ) ) )`, and the handshake is done
//!    ([`Progress::Established`]): the client's next item is a command.
//!
//! The principal is the one the successful attempt proved, and nothing a failed attempt named is
//! kept.
//!
//! **Authentication for one command** ([`Server::for_command`]; A-656). Where a command of an
//! established session needs access its principal lacks and the client could authenticate,
//! `svnserve` asks again in the middle of the command (`must_have_access`, `serve.c` 807-850) with
//! `needs_username` set, since the rules must be asked again with a name (826-836): the request
//! lists `EXTERNAL` and `CRAM-MD5` where authenticated users have the access and never `ANONYMOUS`
//! (`send_mechs` 580-591), an `ANONYMOUS` attempt fails as unlisted (`auth` 649-690), the attempts
//! loop as at step 4 (`internal_auth_request` 693-721), and a successful one is answered
//! `( success ( ) )` alone: the repository's information belongs to the handshake. The session goes
//! on as the user the attempt proved.
//!
//! **Evidence.** Stock Subversion 1.15.0 (r1937767, Homebrew, on the Mac Studio, 2026-11-19), `svn`
//! against `svnserve`, every byte each way logged by a relay between them:
//! `crates/svn/fixtures/handshake/` (`make-fixtures.py`, `SHA256SUMS`). 1.15.0's `cram.c`,
//! `internal_auth.c` and protocol document are those of `d434dc14` byte for byte, and its `serve.c`
//! differs by a Windows include and a comment, so the sessions are the code cited above. The tests
//! replay every session and must write exactly what `svnserve` wrote, the greeting's `svndiff1`
//! aside: the stock sessions, and hand-made clients for each refusal of step 2, an unlisted
//! mechanism and an `EXTERNAL` token naming another user.
//!
//! What a stock client sends depends on its build: one with Cyrus SASL, as Homebrew's is, answers
//! `ANONYMOUS` with a token, base64 of `anonymous@` and its host's name followed by a line feed,
//! and `CRAM-MD5` with none, its response unencoded (`libsvn_ra_svn/cyrus_auth.c` 498-505 and
//! 553-555 at 1.15.0); one without sends `ANONYMOUS` an empty token (`internal_auth.c`). Both are
//! answered alike. A SASL client retrying with no further credentials sends its `( CRAM-MD5 ( ) )`
//! before it learns it has none, and ends at the challenge (the wrong-password session).
//!
//! **Where this is stricter than `svnserve`**, each a hostile input no stock client sends:
//! - a CRAM-MD5 response holding a NUL byte is malformed, as `cram.c` finds every such response
//!   (`strrchr` and `hex_decode` read a C string, so the digest after the last space before the NUL
//!   holds the NUL or is cut short);
//! - an `EXTERNAL` token is compared whole, so one holding a NUL byte names no one, where `strcmp`
//!   stops at the NUL; an absent token is an empty one, the protocol's `[ token:string ]` being
//!   optional;
//! - a list nested deeper than the handshake's prototypes, which `svnserve` reads to 64 levels
//!   before refusing the message, is refused at its third level (the bounds, below).
//!
//! **CRAM-MD5** is HMAC-MD5 (RFC 2104) keyed with the user's shared secret (RFC 2195 §2; `cram.c`
//! `compute_digest`, a secret longer than MD5's 64-byte block hashed first). The server holds the
//! keyed contexts, never the secret ([`CramContexts`], [`CramKey`]; RFC 2195 §4: storing the
//! "contexts" avoids "explicit cleartext storage of the shared secret", and the two "must both be
//! protected"), and compares a digest in constant time where `cram.c` uses `memcmp`. Since A-692 the
//! contexts are what a token's credential record keeps (§4.13), made when the token is enrolled, so
//! each digest is finished from a kept MD5 state: MD5 is written here from RFC 1321 (`rfc1321`), since
//! neither RustCrypto's `md-5` 0.10, which A-599 took with its `hmac`, nor the vendored aws-lc-rs
//! (MD5 as `digest::MD5_FOR_LEGACY_USE_ONLY`, a local change for S3, and HMAC over the SHA family
//! alone) lets a caller start from a kept state. Both libraries are its tests' oracles.
//!
//! **Bounds.** The client's items are read with the item codec ([`crate::ra_svn`]) within bounds
//! derived from the handshake's own prototypes and from the one bound the caller gives, the bytes
//! an item may take (the caller's, from the connection's memory share): the longest word a stock
//! parser takes, 24 (`marshal.c` `MAX_WORD_LENGTH`, 1.14 and 1.15 alike); the deepest list the
//! client's messages hold, 2; and the item count, the bytes over what one parsed item costs, so the
//! values a message parses to never take more memory than its bytes may.
//!
//! Every refusal is typed and names the Subversion error it stands for
//! ([`Refused::subversion_code`]). No I/O, no clock, no randomness, no panic.

use std::ffi::CStr;

use crate::ra_svn::{self, Bounds, ItemError, Parsed, Parser, Value};

mod rfc1321;

/// The one protocol version spoken, the greeting's least and greatest.
/// Format: `serve.c` greets "version 2 to 2" (`construct_server_baton`) and refuses any other
/// answer.
const PROTOCOL_VERSION: u64 = 2;

/// The capabilities the greeting announces, in `svnserve`'s order.
/// Format: `serve.c`'s greeting (`construct_server_baton` 4241-4256) without `svndiff1`, as §4.17.4
/// "5. The wire" decides: a sender uses zlib only for a peer that announced it (`marshal.c`), so
/// slates never receives zlib.
const SERVER_CAPABILITIES: [&str; 12] = [
  "edit-pipeline",
  "accepts-svndiff2",
  "absent-entries",
  "commit-revprops",
  "depth",
  "log-revprops",
  "atomic-revprops",
  "partial-replay",
  "inherited-props",
  "ephemeral-txnprops",
  "file-revs-reverse",
  "list",
];

/// The capability every client must announce.
/// Format: `SVN_RA_SVN_CAP_EDIT_PIPELINE` (`svn_ra_svn.h`); `serve.c` 4295 refuses a client without
/// it.
const EDIT_PIPELINE: &str = "edit-pipeline";

/// The repository capability `repos-info` announces when the repository keeps mergeinfo.
/// Format: `SVN_RA_SVN_CAP_MERGEINFO` (`svn_ra_svn.h`; `protocol` §2.1).
const MERGEINFO: &str = "mergeinfo";

/// The mechanism that takes no one.
/// Format: the SASL mechanism name `send_mechs` writes (`serve.c` 585).
const ANONYMOUS: &str = "ANONYMOUS";

/// The mechanism that takes the tunnel's user.
/// Format: the SASL mechanism name `send_mechs` writes (`serve.c` 587).
const EXTERNAL: &str = "EXTERNAL";

/// The mechanism that proves a shared secret.
/// Format: the SASL mechanism name `send_mechs` writes (`serve.c` 589).
const CRAM_MD5: &str = "CRAM-MD5";

/// The word a command response and a successful challenge start with.
/// Format: `command-response` and `challenge` (`protocol` §1 and §2).
const SUCCESS: &str = "success";

/// The word a failed command response and a failed challenge start with.
/// Format: `command-response` and `challenge` (`protocol` §1 and §2).
const FAILURE: &str = "failure";

/// The word a challenge that wants an answer starts with.
/// Format: `challenge` (`protocol` §2).
const STEP: &str = "step";

/// The longest word read.
/// Format: `marshal.c`'s `MAX_WORD_LENGTH` is 25 with the terminating byte, so a stock parser takes
/// 24 characters and no stock peer sends a server a longer word (§4.17.4 "5. The wire"; 1.15.0's
/// `marshal.c` keeps it).
const LONGEST_WORD: usize = 24;

/// The deepest list a client's handshake message holds.
/// Format: the answer `( version ( cap ... ) ... ( client ) )` and the auth-response
/// `( mech ( token ) )` (`protocol` §2) are lists of depth 2; the CRAM-MD5 response is a string.
const CLIENT_DEPTH: usize = 2;

/// The fewest bytes an item takes: a one-character word or number and its whitespace (`protocol`
/// §1).
/// Format: the grammar's shortest item.
const SMALLEST_ITEM_BYTES: usize = 2;

/// What one parsed item costs in memory: a [`Value`], whatever its kind.
/// Derived: `size_of::<Value>()`, measured by the compiler for the target, so it follows the type.
const PARSED_ITEM_BYTES: usize = size_of::<Value<'static>>();

/// The bytes of a CRAM-MD5 digest.
/// Format: an MD5 digest is 128 bits (RFC 1321 §3.5); `cram.c`'s `APR_MD5_DIGESTSIZE`.
pub const DIGEST_BYTES: usize = 16;

/// The bits a hexadecimal digit stands for.
/// Format: one digit is four bits.
const HEX_BITS: u32 = 4;

/// The value of the hexadecimal digit `a`, the first letter digit.
/// Format: `a` to `f` stand for ten to fifteen (`cram.c`'s `hex_to_int`).
const HEX_LETTER_BASE: u8 = 10;

/// The byte between a CRAM-MD5 response's user and its digest.
/// Format: `cram.c` 164 takes the last space (`strrchr`).
const RESPONSE_SEPARATOR: u8 = b' ';

/// The byte that ends a C string, where `cram.c`'s reading of a response stops.
/// Format: C's string terminator.
const C_STRING_END: u8 = 0;

/// The bytes that open a CRAM-MD5 challenge, before its random number.
/// Format: `cram.c` 154-156, `"<%" APR_UINT64_T_FMT ".%" APR_TIME_T_FMT "@%s>"`.
const CHALLENGE_OPEN: &str = "<";

/// The byte that closes a CRAM-MD5 challenge, after the host's name.
/// Format: as [`CHALLENGE_OPEN`].
const CHALLENGE_CLOSE: u8 = b'>';

/// `SVN_ERR_BAD_FILENAME`.
/// Format: the bad category's start, 120,000 + 1 × 5,000 = 125,000, plus 1 (`svn_error_codes.h`
/// 169).
const SVN_ERR_BAD_FILENAME: u32 = 125_001;

/// `SVN_ERR_BAD_URL`.
/// Format: the bad category's start plus 2 (`svn_error_codes.h` 173).
const SVN_ERR_BAD_URL: u32 = 125_002;

/// `SVN_ERR_RA_NOT_AUTHORIZED`.
/// Format: the RA category's start, 120,000 + 10 × 5,000 = 170,000, plus 1 (`svn_error_codes.h`
/// 967).
const SVN_ERR_RA_NOT_AUTHORIZED: u32 = 170_001;

/// `SVN_ERR_RA_SVN_MALFORMED_DATA`.
/// Format: the ra_svn category's start, 120,000 + 18 × 5,000 = 210,000, plus 4
/// (`svn_error_codes.h`).
const SVN_ERR_RA_SVN_MALFORMED_DATA: u32 = 210_004;

/// `SVN_ERR_RA_SVN_REPOS_NOT_FOUND`.
/// Format: the ra_svn category's start plus 5 (`svn_error_codes.h`).
const SVN_ERR_RA_SVN_REPOS_NOT_FOUND: u32 = 210_005;

/// `SVN_ERR_RA_SVN_BAD_VERSION`.
/// Format: the ra_svn category's start plus 6 (`svn_error_codes.h`).
const SVN_ERR_RA_SVN_BAD_VERSION: u32 = 210_006;

/// What the client said in answer to the greeting (`protocol` §2's `response`), borrowed from the
/// bytes it came in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientOpen<'a> {
  /// The protocol version the client chose: always 2 here, since any other is refused.
  pub version: u64,
  /// The client's capabilities, each a word.
  pub capabilities: Vec<&'a str>,
  /// The URL the client opens, as it sent it: the caller canonicalizes and resolves it.
  pub url: &'a [u8],
  /// The RA implementation, `SVN/1.15.0 (aarch64-apple-darwin25.6.0)` from a stock client, where it
  /// said.
  pub ra_client: Option<&'a [u8]>,
  /// The client's own string (`get_client_string`), where it said.
  pub client: Option<&'a [u8]>,
}

/// The repository the client's URL names, as the caller found it, and how its users may
/// authenticate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Repository<'a> {
  /// The repository's UUID, sent in `repos-info`.
  pub uuid: &'a [u8],
  /// The URL of the repository's root, sent in `repos-info`: `svnserve` takes the client's URL less
  /// the components of the path beneath the root (`find_repos`), and a client refuses one longer
  /// than its own URL.
  pub root_url: &'a [u8],
  /// The authentication realm (`svnserve`'s default is the UUID).
  pub realm: &'a [u8],
  /// Whether the repository keeps mergeinfo, which `repos-info` then announces.
  pub mergeinfo: bool,
  /// Whether anyone may read without authenticating (`ANONYMOUS`).
  pub anonymous: bool,
  /// The user the connection's tunnel logged in, where it came through one and authenticated users
  /// may read (`EXTERNAL`).
  pub tunnel_user: Option<&'a [u8]>,
  /// Whether authenticated users may read and the repository holds their keys (`CRAM-MD5`).
  pub cram_md5: bool,
}

/// A command failure the caller answers the client's URL with ([`Server::refuse`]), each as
/// `svnserve` words it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unserved<'a> {
  /// The URL is no `svn://` URL: "Non-svn URL passed to svn server: '`url`'" (`SVN_ERR_BAD_URL`).
  BadUrl {
    /// The URL as the caller canonicalized it.
    url: &'a [u8],
  },
  /// The URL's path climbs out of the server's root with `..`: "Couldn't determine repository path"
  /// (`SVN_ERR_BAD_FILENAME`).
  BadPath,
  /// No repository holds the URL's path: "No repository found in '`url`'"
  /// (`SVN_ERR_RA_SVN_REPOS_NOT_FOUND`).
  NoRepository {
    /// The URL as the caller canonicalized it.
    url: &'a [u8],
  },
  /// Neither anonymous nor authenticated users may read: "No access allowed to this repository"
  /// (`SVN_ERR_RA_NOT_AUTHORIZED`).
  NoAccess,
}

impl Unserved<'_> {
  /// The error `svnserve` answers the URL with, its number and message: what [`Server::refuse`]
  /// writes as the command failure, and what `svnserve -t` then ends with on its standard error
  /// (A-688: each recorded so from stock `svnserve -t` 1.15.0).
  pub fn failure(&self) -> (u32, Vec<u8>) {
    match *self {
      Unserved::BadUrl { url } => (
        SVN_ERR_BAD_URL,
        quoted(b"Non-svn URL passed to svn server: '", url),
      ),
      Unserved::BadPath => (
        SVN_ERR_BAD_FILENAME,
        b"Couldn't determine repository path".to_vec(),
      ),
      Unserved::NoRepository { url } => (
        SVN_ERR_RA_SVN_REPOS_NOT_FOUND,
        quoted(b"No repository found in '", url),
      ),
      Unserved::NoAccess => (
        SVN_ERR_RA_NOT_AUTHORIZED,
        b"No access allowed to this repository".to_vec(),
      ),
    }
  }
}

/// What [`Server::open`] answered the client's URL with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
  /// The authentication request is written: the client's mechanism is awaited.
  Asked,
  /// The repository allows no one: "No access allowed to this repository" is written, and the
  /// handshake has ended.
  Denied,
}

/// Who the client authenticated as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Principal {
  /// No one (`ANONYMOUS`).
  Anonymous,
  /// A named user: the tunnel's (`EXTERNAL`) or the one whose key proved the response (`CRAM-MD5`).
  User(Vec<u8>),
}

/// The capabilities a client announced in its answer that a command's answer depends on (`protocol`
/// §2.1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClientCapabilities {
  /// `svndiff1`: the client takes svndiff version 1, zlib, which slates never sends.
  pub svndiff1: bool,
  /// `accepts-svndiff2`: the client takes svndiff version 2, LZ4.
  pub accepts_svndiff2: bool,
  /// `absent-entries`: the client takes `absent-dir` and `absent-file`.
  pub absent_entries: bool,
  /// `depth`: the client sends and understands depths.
  pub depth: bool,
  /// `mergeinfo`: the client asks for mergeinfo.
  pub mergeinfo: bool,
  /// `log-revprops`: the client asks `log` for revision properties by name.
  pub log_revprops: bool,
}

impl ClientCapabilities {
  /// The capabilities among `words`, the client's announced list; a word it does not know is no
  /// capability.
  pub fn of(words: &[&str]) -> Self {
    let has = |capability: &str| words.contains(&capability);
    Self {
      svndiff1: has("svndiff1"),
      accepts_svndiff2: has("accepts-svndiff2"),
      absent_entries: has("absent-entries"),
      depth: has("depth"),
      mergeinfo: has("mergeinfo"),
      log_revprops: has("log-revprops"),
    }
  }
}

/// An established connection's session, what the handshake hands every command it serves: who the
/// client authenticated as, what it can take, and the repository it opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
  /// Who the client authenticated as.
  pub principal: Principal,
  /// What the client announced it can take.
  pub capabilities: ClientCapabilities,
  /// The repository's UUID.
  pub uuid: Vec<u8>,
  /// The URL of the repository's root, as `repos-info` sent it.
  pub root_url: Vec<u8>,
}

/// An authentication attempt that failed. The client may answer again, as `svnserve`'s loop lets
/// it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthFailure {
  /// A mechanism the server did not list: "Must authenticate with listed mechanism" (`serve.c`
  /// `auth`).
  NotListed,
  /// An `EXTERNAL` token naming another user than the tunnel's: "Requested username does not
  /// match".
  UsernameMismatch,
  /// A CRAM-MD5 response that is not `user`, a space and 32 lowercase hexadecimal digits:
  /// "Malformed client response in authentication" (`cram.c`).
  MalformedResponse,
  /// A CRAM-MD5 response that is no string: `cram.c` writes nothing for it ("Very wrong; don't
  /// report failure").
  NotAString,
  /// A CRAM-MD5 response naming a user the repository holds no key for: "Username not found".
  UnknownUser,
  /// A CRAM-MD5 digest the user's key does not give: "Password incorrect".
  WrongPassword,
  /// No challenge could be made (the caller found no random number or no host name): "Internal
  /// server error in authentication" (`cram.c` 153).
  Internal,
}

impl AuthFailure {
  /// The message the failure is written with, `( failure ( message ) )`; none for the one
  /// `svnserve` answers silently.
  pub fn message(self) -> Option<&'static str> {
    match self {
      Self::NotListed => Some("Must authenticate with listed mechanism"),
      Self::UsernameMismatch => Some("Requested username does not match"),
      Self::MalformedResponse => Some("Malformed client response in authentication"),
      Self::NotAString => None,
      Self::UnknownUser => Some("Username not found"),
      Self::WrongPassword => Some("Password incorrect"),
      Self::Internal => Some("Internal server error in authentication"),
    }
  }
}

/// Why the handshake was refused. The connection ends; nothing more is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
  /// The client's bytes are no item within the bounds (the codec's refusal).
  Item(ItemError),
  /// The client's answer does not match
  /// `( version:number ( cap ... ) url:string ? ra-client:string ( ? client:string ) )` as
  /// `vparse_tuple` reads it: "Malformed network data" (`SVN_ERR_RA_SVN_MALFORMED_DATA`).
  NotAnswer,
  /// A capability that is no word: "Capability entry is not a word"
  /// (`SVN_ERR_RA_SVN_MALFORMED_DATA`).
  CapabilityNotWord,
  /// A protocol version other than 2: "Unsupported ra_svn protocol version `n` (supported versions:
  /// \[2\])" (`SVN_ERR_RA_SVN_BAD_VERSION`).
  UnsupportedVersion(u64),
  /// No `edit-pipeline` among the client's capabilities: "Missing edit-pipeline capability"
  /// (`SVN_ERR_RA_SVN_BAD_VERSION`).
  NoEditPipeline,
  /// An auth-response that does not match `( mech:word ( ? token:string ) )`: "Malformed network
  /// data" (`SVN_ERR_RA_SVN_MALFORMED_DATA`).
  NotAuthResponse,
  /// A call the handshake does not take where it stands: the caller's error, not the client's.
  OutOfTurn,
}

impl Refused {
  /// The number of the Subversion error the refusal stands for; none for the caller's own error.
  pub fn subversion_code(self) -> Option<u32> {
    match self {
      Self::Item(error) => Some(error.subversion_code()),
      Self::NotAnswer | Self::CapabilityNotWord | Self::NotAuthResponse => {
        Some(SVN_ERR_RA_SVN_MALFORMED_DATA)
      }
      Self::UnsupportedVersion(_) | Self::NoEditPipeline => Some(SVN_ERR_RA_SVN_BAD_VERSION),
      Self::OutOfTurn => None,
    }
  }
}

/// What one call of [`Server::receive`] made of the client's bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Progress<'a> {
  /// The buffer ends inside the client's next item: at least `needed` bytes from its start are
  /// needed.
  NeedMore {
    /// How many bytes from the start of the buffer the item needs at least.
    needed: usize,
  },
  /// The client answered the greeting: the caller resolves its URL and calls [`Server::open`] or
  /// [`Server::refuse`].
  Opened {
    /// The answer.
    open: ClientOpen<'a>,
    /// How many bytes from the start of the buffer it took.
    consumed: usize,
  },
  /// The client chose CRAM-MD5: the caller gives the challenge's parts ([`Server::challenge`]), or
  /// says it has none ([`Server::challenge_failed`]).
  ChallengeNeeded {
    /// How many bytes from the start of the buffer the choice took.
    consumed: usize,
  },
  /// The client's CRAM-MD5 response named a user: the caller gives that user's key
  /// ([`Server::key`]).
  KeyNeeded {
    /// The user, as the response named it.
    user: &'a [u8],
    /// How many bytes from the start of the buffer the response took.
    consumed: usize,
  },
  /// An attempt failed and its failure is written (save for the one `svnserve` answers silently);
  /// the client may answer again.
  Failed {
    /// Why.
    failure: AuthFailure,
    /// How many bytes from the start of the buffer the attempt took.
    consumed: usize,
  },
  /// The client authenticated: its success is written, followed at the handshake by the
  /// repository's information, and the handshake, or the authentication for one command, is done.
  Established {
    /// Who.
    principal: Principal,
    /// How many bytes from the start of the buffer the attempt took.
    consumed: usize,
  },
}

/// What a CRAM-MD5 response's verification came to ([`Server::key`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
  /// The digest is the key's: its success is written, followed at the handshake by the
  /// repository's information, and the handshake, or the authentication for one command, is done.
  Established(Principal),
  /// The attempt failed and its failure is written; the client may answer again.
  Failed(AuthFailure),
}

/// Format: RFC 2104 §2's `ipad`, the byte the key is XORed with for the inner hash (`cram.c`
/// `compute_digest` 97).
const INNER_PAD: u8 = 0x36;

/// Format: RFC 2104 §2's `opad`, the byte the key is XORed with for the outer hash (`cram.c`
/// `compute_digest` 102).
const OUTER_PAD: u8 = 0x5c;

/// Format: the bytes a kept context has absorbed, its one pad block (RFC 2104 §2's B, MD5's block),
/// as Dovecot counts them when it loads one (`hmac_md5_set_cram_context`: `lo = 64`, `hi = 0`).
const CONTEXT_ABSORBED: u64 = 64;

/// HMAC-MD5's keyed contexts of one CRAM-MD5 secret (RFC 2104 §4; RFC 2195 §4's "contexts"): MD5's
/// chaining state after the key's inner pad block, and after its outer pad block, from which every
/// digest is finished without the secret. They answer CRAM-MD5 as the secret does, so they "must be
/// protected" as it is (RFC 2195 §4), but they do not give it back: a credential record keeps them,
/// never a token, which is also Basic's password and a bearer token (A-692). Their bytes
/// ([`CramContexts::to_bytes`]) are Dovecot's CRAM-MD5 scheme's (`hmac_md5_get_cram_context`,
/// `src/lib/hmac-cram-md5.c`): the outer state's words A, B, C and D, then the inner state's, each
/// low-order byte first.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct CramContexts {
  /// MD5's state after the key's outer pad block.
  outer: rfc1321::State,
  /// MD5's state after the key's inner pad block.
  inner: rfc1321::State,
}

impl std::fmt::Debug for CramContexts {
  /// Writes nothing of the contexts, which answer CRAM-MD5 as the secret does (RFC 2195 §4).
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter.write_str("CramContexts(..)")
  }
}

impl CramContexts {
  /// Format: the bytes of a pair of contexts (Dovecot's `CRAM_MD5_CONTEXTLEN`): two MD5 states of
  /// four 32-bit words each.
  pub const BYTES: usize = 2 * DIGEST_BYTES;

  /// The contexts of `secret` (RFC 2104 §2): the key is the secret, or its MD5 where the secret is
  /// longer than MD5's block, as `cram.c` makes it, zero-filled to the block; each pad block is
  /// compressed once from MD5's initial state.
  pub fn of_secret(secret: &[u8]) -> CramContexts {
    let hashed;
    let shortened: &[u8] = if secret.len() > rfc1321::BLOCK_BYTES {
      hashed = rfc1321::digest(secret);
      &hashed
    } else {
      secret
    };
    let mut key = [0u8; rfc1321::BLOCK_BYTES];
    for (slot, byte) in key.iter_mut().zip(shortened) {
      *slot = *byte;
    }
    CramContexts {
      outer: padded_state(&key, OUTER_PAD),
      inner: padded_state(&key, INNER_PAD),
    }
  }

  /// The contexts' bytes, in Dovecot's layout (the type's doc).
  pub fn to_bytes(&self) -> [u8; Self::BYTES] {
    let mut bytes = [0u8; Self::BYTES];
    let halves = bytes.as_chunks_mut::<DIGEST_BYTES>().0.iter_mut();
    for (half, state) in halves.zip([self.outer, self.inner]) {
      *half = rfc1321::bytes_of(state);
    }
    bytes
  }

  /// The contexts `bytes` holds, as [`CramContexts::to_bytes`] wrote them.
  pub fn from_bytes(bytes: &[u8; Self::BYTES]) -> CramContexts {
    let mut states = [rfc1321::INITIAL; 2];
    for (state, half) in states.iter_mut().zip(bytes.as_chunks::<DIGEST_BYTES>().0) {
      *state = rfc1321::state_of(half);
    }
    let [outer, inner] = states;
    CramContexts { outer, inner }
  }
}

/// MD5's state after one block of `key` XORed with `pad`, from its initial state.
fn padded_state(key: &[u8; rfc1321::BLOCK_BYTES], pad: u8) -> rfc1321::State {
  let mut state = rfc1321::INITIAL;
  rfc1321::compress(&mut state, &key.map(|byte| byte ^ pad));
  state
}

/// A user's CRAM-MD5 key: the shared secret's keyed contexts ([`CramContexts`]), the secret not
/// kept.
#[derive(Clone)]
pub struct CramKey {
  contexts: CramContexts,
}

impl std::fmt::Debug for CramKey {
  /// Writes nothing of the contexts, which are as strong as the secret (RFC 2195 §4).
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter.write_str("CramKey(..)")
  }
}

impl CramKey {
  /// The key whose contexts are `contexts`, as a credential record keeps them (A-692).
  pub fn from_contexts(contexts: CramContexts) -> Self {
    Self { contexts }
  }

  /// The key of `secret`, by its contexts ([`CramContexts::of_secret`]).
  pub fn from_secret(secret: &[u8]) -> Self {
    Self::from_contexts(CramContexts::of_secret(secret))
  }

  /// HMAC-MD5 of `challenge` under the key, finished from its contexts (RFC 2104 §4): MD5 of the
  /// challenge after the inner pad block, then MD5 of that digest after the outer pad block. The
  /// digest a client sends, as `cram.c`'s `compute_digest` makes it.
  pub fn digest(&self, challenge: &[u8]) -> [u8; DIGEST_BYTES] {
    let inner = rfc1321::finish(self.contexts.inner, CONTEXT_ABSORBED, challenge);
    rfc1321::finish(self.contexts.outer, CONTEXT_ABSORBED, &inner)
  }

  /// Whether `digest` is HMAC-MD5 of `challenge` under the key, compared in constant time (aws-lc's
  /// `CRYPTO_memcmp`).
  pub fn verifies(&self, challenge: &[u8], digest: &[u8; DIGEST_BYTES]) -> bool {
    aws_lc_rs::constant_time::verify_slices_are_equal(&self.digest(challenge), digest).is_ok()
  }
}

/// What the repository allows and tells, kept from [`Server::open`] (or [`Server::for_command`])
/// for the rest of the exchange.
#[derive(Clone, Debug)]
struct Offer {
  capabilities: ClientCapabilities,
  anonymous: bool,
  tunnel_user: Option<Vec<u8>>,
  cram_md5: bool,
  uuid: Vec<u8>,
  root_url: Vec<u8>,
  mergeinfo: bool,
  /// Whether the success is followed by the repository's information: at the handshake, not for one
  /// command.
  information: bool,
}

/// The mechanisms an authentication for one command offers ([`Server::for_command`]), as the
/// caller finds them for the access the command needs (`send_mechs` with `needs_username`,
/// `serve.c` 580-591): never `ANONYMOUS`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandMechanisms<'a> {
  /// The authentication realm, the handshake's.
  pub realm: &'a [u8],
  /// The user the connection's tunnel logged in, where it came through one and authenticated users
  /// have the access (`EXTERNAL`).
  pub tunnel_user: Option<&'a [u8]>,
  /// Whether authenticated users have the access and the repository holds their keys (`CRAM-MD5`).
  pub cram_md5: bool,
}

/// Where the handshake stands.
#[derive(Clone, Debug)]
enum State {
  /// The greeting is written; the client's answer is awaited.
  Greeted,
  /// The client answered with these capabilities; the caller's [`Server::open`] or
  /// [`Server::refuse`] is awaited.
  Opening(ClientCapabilities),
  /// The authentication request is written; the client's mechanism is awaited.
  Asked(Offer),
  /// The client chose CRAM-MD5; the caller's challenge is awaited.
  Challenging(Offer),
  /// The challenge is written; the client's response is awaited.
  Challenged { offer: Offer, challenge: Vec<u8> },
  /// The response named a user; the caller's key is awaited.
  Verifying {
    offer: Offer,
    challenge: Vec<u8>,
    user: Vec<u8>,
    digest: [u8; DIGEST_BYTES],
  },
  /// The repository's information is written: the handshake is done, and this is its session.
  Established(Session),
  /// A command failure is written, or the client was refused: the connection ends.
  Ended,
}

/// The server's side of one connection's handshake (module documentation).
#[derive(Clone, Debug)]
pub struct Server {
  state: State,
  parser: Parser,
  bounds: Bounds,
}

impl Server {
  /// A handshake whose greeting is appended to `out`, reading the client's items within
  /// `item_bytes` each.
  ///
  /// # Errors
  /// None in practice: the greeting's words are the grammar's.
  pub fn new(item_bytes: usize, out: &mut Vec<u8>) -> Result<Self, Refused> {
    let bounds = client_bounds(item_bytes);
    let capabilities = SERVER_CAPABILITIES
      .iter()
      .map(|word| Value::Word(word))
      .collect();
    let greeting = Value::List(vec![
      Value::Number(PROTOCOL_VERSION),
      Value::Number(PROTOCOL_VERSION),
      Value::List(Vec::new()),
      Value::List(capabilities),
    ]);
    write_response(SUCCESS, greeting, out)?;
    Ok(Self {
      state: State::Greeted,
      parser: Parser::new(bounds),
      bounds,
    })
  }

  /// The bounds the client's items are read within.
  pub fn bounds(&self) -> Bounds {
    self.bounds
  }

  /// Whether the handshake, or the authentication for one command, is done: its success is
  /// written, and the client's next item is a command's.
  pub fn established(&self) -> bool {
    matches!(self.state, State::Established(_))
  }

  /// The session the handshake established, or the one an authentication for one command left,
  /// once it is done; every command the connection serves takes it.
  pub fn session(&self) -> Option<&Session> {
    match &self.state {
      State::Established(session) => Some(session),
      _ => None,
    }
  }

  /// Reads on in `buffer`, the same buffer with more at its end, as the item codec takes it
  /// ([`Parser::parse`]): the client's next message, appending to `out` whatever it is answered
  /// with at once. After a [`Progress`] with `consumed`, the caller drops that many bytes from the
  /// buffer's start once it has answered what the progress asks.
  ///
  /// # Errors
  /// Every [`Refused`] but `OutOfTurn` for the client's bytes, which end the handshake; `OutOfTurn`
  /// where the handshake awaits the caller, or is over.
  pub fn receive<'a>(
    &mut self,
    buffer: &'a [u8],
    out: &mut Vec<u8>,
  ) -> Result<Progress<'a>, Refused> {
    if !self.awaits_client() {
      return Err(Refused::OutOfTurn);
    }
    let (value, consumed) = match self.parser.parse(buffer) {
      Ok(Parsed::NeedMore { needed }) => return Ok(Progress::NeedMore { needed }),
      Ok(Parsed::Item { value, consumed }) => (value, consumed),
      Err(error) => {
        self.state = State::Ended;
        return Err(Refused::Item(error));
      }
    };
    match std::mem::replace(&mut self.state, State::Ended) {
      State::Greeted => {
        let open = answer(value)?;
        self.state = State::Opening(ClientCapabilities::of(&open.capabilities));
        Ok(Progress::Opened { open, consumed })
      }
      State::Asked(offer) => self.mechanism(offer, &value, consumed, out),
      State::Challenged { offer, challenge } => {
        self.cram_response(offer, challenge, value, consumed, out)
      }
      other => {
        self.state = other;
        Err(Refused::OutOfTurn)
      }
    }
  }

  /// Answers the client's URL with `repository`: the authentication request is appended to `out`,
  /// listing the mechanisms the repository allows ([`Access::Asked`]). A repository that allows
  /// none is refused "No access allowed to this repository" instead, as `serve.c` 4345-4351 refuses
  /// it before any request ([`Access::Denied`]).
  ///
  /// # Errors
  /// `OutOfTurn` unless the client's answer was just read ([`Progress::Opened`]).
  pub fn open(
    &mut self,
    repository: &Repository<'_>,
    out: &mut Vec<u8>,
  ) -> Result<Access, Refused> {
    let State::Opening(capabilities) = self.state else {
      return Err(Refused::OutOfTurn);
    };
    let mechanisms = offered_mechanisms(repository);
    if mechanisms.is_empty() {
      self.refuse(Unserved::NoAccess, out)?;
      return Ok(Access::Denied);
    }
    let request = Value::List(vec![
      Value::List(mechanisms),
      Value::String(repository.realm),
    ]);
    write_response(SUCCESS, request, out)?;
    self.state = State::Asked(Offer {
      capabilities,
      anonymous: repository.anonymous,
      tunnel_user: repository.tunnel_user.map(<[u8]>::to_vec),
      cram_md5: repository.cram_md5,
      uuid: repository.uuid.to_vec(),
      root_url: repository.root_url.to_vec(),
      mergeinfo: repository.mergeinfo,
      information: true,
    });
    Ok(Access::Asked)
  }

  /// An authentication for one command of the established `session`, as `must_have_access` asks for
  /// it where the session's principal lacks the access the command needs and the client could
  /// authenticate (`serve.c` 826-836): `internal_auth_request` with `needs_username`
  /// (`serve.c` 693-721), its request, `( success ( ( mech ... ) realm ) )`, listing `EXTERNAL` and
  /// `CRAM-MD5` as `mechanisms` allows each, appended to `out`. The client's attempts are then
  /// answered as the handshake's are, `ANONYMOUS` among the unlisted, and a successful one with
  /// `( success ( ) )` alone; established, [`Server::session`] is `session` as the user the attempt
  /// proved. The client's items are read within `item_bytes` each.
  ///
  /// # Errors
  /// `OutOfTurn` when `mechanisms` allows none, nothing written: `must_have_access` asks only where
  /// the client could authenticate, so this is the caller's error.
  pub fn for_command(
    session: Session,
    mechanisms: &CommandMechanisms<'_>,
    item_bytes: usize,
    out: &mut Vec<u8>,
  ) -> Result<Self, Refused> {
    let offered = offered_mechanisms(&Repository {
      uuid: &session.uuid,
      root_url: &session.root_url,
      realm: mechanisms.realm,
      mergeinfo: false,
      anonymous: false,
      tunnel_user: mechanisms.tunnel_user,
      cram_md5: mechanisms.cram_md5,
    });
    if offered.is_empty() {
      return Err(Refused::OutOfTurn);
    }
    let request = Value::List(vec![Value::List(offered), Value::String(mechanisms.realm)]);
    write_response(SUCCESS, request, out)?;
    let bounds = client_bounds(item_bytes);
    Ok(Self {
      state: State::Asked(Offer {
        capabilities: session.capabilities,
        anonymous: false,
        tunnel_user: mechanisms.tunnel_user.map(<[u8]>::to_vec),
        cram_md5: mechanisms.cram_md5,
        uuid: session.uuid,
        root_url: session.root_url,
        mergeinfo: false,
        information: false,
      }),
      parser: Parser::new(bounds),
      bounds,
    })
  }

  /// Answers the client's URL with the command failure `unserved`, appended to `out`; the handshake
  /// ends.
  ///
  /// # Errors
  /// `OutOfTurn` unless the client's answer was just read ([`Progress::Opened`]).
  pub fn refuse(&mut self, unserved: Unserved<'_>, out: &mut Vec<u8>) -> Result<(), Refused> {
    if !matches!(self.state, State::Opening(_)) {
      return Err(Refused::OutOfTurn);
    }
    let (code, message) = unserved.failure();
    self.state = State::Ended;
    write_command_failure(code, &message, out)
  }

  /// Sends the CRAM-MD5 challenge `<nonce.time@host>` (`cram.c`'s form: a random number, the time
  /// in microseconds, the host's name), appended to `out`. The caller gives the parts, a fresh
  /// random number each time; the host's name is a C string, as `apr_gethostname` gives it, so the
  /// challenge holds no NUL that would cut a client's digest short.
  ///
  /// # Errors
  /// `OutOfTurn` unless the client just chose CRAM-MD5 ([`Progress::ChallengeNeeded`]).
  pub fn challenge(
    &mut self,
    nonce: u64,
    time_micros: i64,
    host: &CStr,
    out: &mut Vec<u8>,
  ) -> Result<(), Refused> {
    let offer = match std::mem::replace(&mut self.state, State::Ended) {
      State::Challenging(offer) => offer,
      other => {
        self.state = other;
        return Err(Refused::OutOfTurn);
      }
    };
    let mut challenge = format!("{CHALLENGE_OPEN}{nonce}.{time_micros}@").into_bytes();
    challenge.extend_from_slice(host.to_bytes());
    challenge.push(CHALLENGE_CLOSE);
    let step = Value::List(vec![
      Value::Word(STEP),
      Value::List(vec![Value::String(&challenge)]),
    ]);
    ra_svn::write(&step, out).map_err(Refused::Item)?;
    self.state = State::Challenged { offer, challenge };
    Ok(())
  }

  /// Fails the CRAM-MD5 attempt the client just chose because no challenge could be made, as
  /// `cram.c` fails it when it draws no random number or finds no host name: "Internal server error
  /// in authentication" is appended to `out`, and the client may answer again.
  ///
  /// # Errors
  /// `OutOfTurn` unless the client just chose CRAM-MD5 ([`Progress::ChallengeNeeded`]).
  pub fn challenge_failed(&mut self, out: &mut Vec<u8>) -> Result<(), Refused> {
    let offer = match std::mem::replace(&mut self.state, State::Ended) {
      State::Challenging(offer) => offer,
      other => {
        self.state = other;
        return Err(Refused::OutOfTurn);
      }
    };
    write_challenge_failure(AuthFailure::Internal, out)?;
    self.state = State::Asked(offer);
    Ok(())
  }

  /// Verifies the CRAM-MD5 response with the named user's key, `None` where the repository holds no
  /// key for the user, appending the success and the repository's information, or the failure, to
  /// `out`.
  ///
  /// # Errors
  /// `OutOfTurn` unless a response naming a user was just read ([`Progress::KeyNeeded`]).
  pub fn key(&mut self, key: Option<&CramKey>, out: &mut Vec<u8>) -> Result<Verdict, Refused> {
    let (offer, challenge, user, digest) = match std::mem::replace(&mut self.state, State::Ended) {
      State::Verifying {
        offer,
        challenge,
        user,
        digest,
      } => (offer, challenge, user, digest),
      other => {
        self.state = other;
        return Err(Refused::OutOfTurn);
      }
    };
    let failure = match key {
      None => AuthFailure::UnknownUser,
      Some(key) if !key.verifies(&challenge, &digest) => AuthFailure::WrongPassword,
      Some(_) => {
        let principal = self.establish(offer, Principal::User(user), out)?;
        return Ok(Verdict::Established(principal));
      }
    };
    write_challenge_failure(failure, out)?;
    self.state = State::Asked(offer);
    Ok(Verdict::Failed(failure))
  }

  /// Whether the handshake waits on the client's bytes, rather than on its caller or on nothing.
  fn awaits_client(&self) -> bool {
    matches!(
      self.state,
      State::Greeted | State::Asked(_) | State::Challenged { .. }
    )
  }

  /// The client's auth-response read where the server asked for one: a mechanism it offered taken,
  /// or the attempt failed.
  fn mechanism<'a>(
    &mut self,
    offer: Offer,
    value: &Value<'a>,
    consumed: usize,
    out: &mut Vec<u8>,
  ) -> Result<Progress<'a>, Refused> {
    let (mechanism, token) = auth_response(value)?;
    if mechanism == EXTERNAL
      && let Some(user) = offer.tunnel_user.clone()
    {
      if token.is_some_and(|token| !token.is_empty() && token != user.as_slice()) {
        return self.failed(offer, AuthFailure::UsernameMismatch, consumed, out);
      }
      let principal = self.establish(offer, Principal::User(user), out)?;
      return Ok(Progress::Established {
        principal,
        consumed,
      });
    }
    if mechanism == ANONYMOUS && offer.anonymous {
      let principal = self.establish(offer, Principal::Anonymous, out)?;
      return Ok(Progress::Established {
        principal,
        consumed,
      });
    }
    if mechanism == CRAM_MD5 && offer.cram_md5 {
      self.state = State::Challenging(offer);
      return Ok(Progress::ChallengeNeeded { consumed });
    }
    self.failed(offer, AuthFailure::NotListed, consumed, out)
  }

  /// The client's CRAM-MD5 response read where the challenge awaited it.
  fn cram_response<'a>(
    &mut self,
    offer: Offer,
    challenge: Vec<u8>,
    value: Value<'a>,
    consumed: usize,
    out: &mut Vec<u8>,
  ) -> Result<Progress<'a>, Refused> {
    let Value::String(response) = value else {
      return self.failed(offer, AuthFailure::NotAString, consumed, out);
    };
    let Some((user, digest)) = split_response(response) else {
      return self.failed(offer, AuthFailure::MalformedResponse, consumed, out);
    };
    self.state = State::Verifying {
      offer,
      challenge,
      user: user.to_vec(),
      digest,
    };
    Ok(Progress::KeyNeeded { user, consumed })
  }

  /// An attempt failed: its failure is appended to `out` (save for the one written silently) and
  /// the client may answer again.
  fn failed<'a>(
    &mut self,
    offer: Offer,
    failure: AuthFailure,
    consumed: usize,
    out: &mut Vec<u8>,
  ) -> Result<Progress<'a>, Refused> {
    write_challenge_failure(failure, out)?;
    self.state = State::Asked(offer);
    Ok(Progress::Failed { failure, consumed })
  }

  /// The client authenticated as `principal`: `( success ( ) )` is appended to `out`, at the
  /// handshake with the repository's information, `( success ( uuid url ( [ mergeinfo ] ) ) )`
  /// (`serve.c` 4386-4392), and the handshake, or the authentication for one command, is done.
  fn establish(
    &mut self,
    offer: Offer,
    principal: Principal,
    out: &mut Vec<u8>,
  ) -> Result<Principal, Refused> {
    write_response(SUCCESS, Value::List(Vec::new()), out)?;
    if offer.information {
      let capabilities = if offer.mergeinfo {
        vec![Value::Word(MERGEINFO)]
      } else {
        Vec::new()
      };
      let information = Value::List(vec![
        Value::String(&offer.uuid),
        Value::String(&offer.root_url),
        Value::List(capabilities),
      ]);
      write_response(SUCCESS, information, out)?;
    }
    self.state = State::Established(Session {
      principal: principal.clone(),
      capabilities: offer.capabilities,
      uuid: offer.uuid,
      root_url: offer.root_url,
    });
    Ok(principal)
  }
}

/// The bounds a client's items are read within, from the bytes one may take: the longest word, the
/// deepest list the client's messages hold, and the item count the bytes allow (module
/// documentation, **Bounds**).
fn client_bounds(item_bytes: usize) -> Bounds {
  Bounds {
    item_bytes,
    word_bytes: LONGEST_WORD,
    depth: CLIENT_DEPTH,
    nodes: item_bytes
      .checked_div(PARSED_ITEM_BYTES.max(SMALLEST_ITEM_BYTES))
      .unwrap_or(0),
  }
}

/// The mechanisms `repository` allows, in `send_mechs`' order: `ANONYMOUS`, `EXTERNAL`, `CRAM-MD5`.
fn offered_mechanisms(repository: &Repository<'_>) -> Vec<Value<'static>> {
  let mut mechanisms = Vec::new();
  if repository.anonymous {
    mechanisms.push(Value::Word(ANONYMOUS));
  }
  if repository.tunnel_user.is_some() {
    mechanisms.push(Value::Word(EXTERNAL));
  }
  if repository.cram_md5 {
    mechanisms.push(Value::Word(CRAM_MD5));
  }
  mechanisms
}

/// Appends a command response, `( word params )`, to `out` (`svn_ra_svn__write_cmd_response`).
fn write_response(word: &str, params: Value<'_>, out: &mut Vec<u8>) -> Result<(), Refused> {
  ra_svn::write(&Value::List(vec![Value::Word(word), params]), out).map_err(Refused::Item)
}

/// Appends a challenge's failure, `( failure ( message ) )`, where the failure is written at all.
fn write_challenge_failure(failure: AuthFailure, out: &mut Vec<u8>) -> Result<(), Refused> {
  let Some(message) = failure.message() else {
    return Ok(());
  };
  write_response(
    FAILURE,
    Value::List(vec![Value::String(message.as_bytes())]),
    out,
  )
}

/// Appends a command failure of one error, `( failure ( ( code message file line ) ) )`
/// (`svn_ra_svn__write_cmd_failure`), with no source file and line, as a release build of
/// `svnserve` sends them.
pub(crate) fn write_command_failure(
  code: u32,
  message: &[u8],
  out: &mut Vec<u8>,
) -> Result<(), Refused> {
  let error = Value::List(vec![
    Value::Number(u64::from(code)),
    Value::String(message),
    Value::String(&[]),
    Value::Number(0),
  ]);
  write_response(FAILURE, Value::List(vec![error]), out)
}

/// `prefix`, `url` and a closing quote: `svnserve`'s quoting of a URL in a message.
fn quoted(prefix: &[u8], url: &[u8]) -> Vec<u8> {
  let mut message = prefix.to_vec();
  message.extend_from_slice(url);
  message.push(b'\'');
  message
}

/// The answer's items as `serve.c` reads them (`"nlc?c(?c)"`), before any is judged.
struct AnswerItems<'a> {
  version: u64,
  capabilities: Vec<Value<'a>>,
  url: &'a [u8],
  ra_client: Option<&'a [u8]>,
  client: Option<&'a [u8]>,
}

/// Reads the client's answer to the greeting, refusing what `serve.c` refuses in its order: the
/// form, the version, the capabilities' kinds, then `edit-pipeline`.
fn answer(value: Value<'_>) -> Result<ClientOpen<'_>, Refused> {
  let items = answer_items(value)?;
  if items.version != PROTOCOL_VERSION {
    return Err(Refused::UnsupportedVersion(items.version));
  }
  let capabilities = items
    .capabilities
    .into_iter()
    .map(|capability| match capability {
      Value::Word(word) => Ok(word),
      _ => Err(Refused::CapabilityNotWord),
    })
    .collect::<Result<Vec<_>, _>>()?;
  if !capabilities.contains(&EDIT_PIPELINE) {
    return Err(Refused::NoEditPipeline);
  }
  Ok(ClientOpen {
    version: items.version,
    capabilities,
    url: items.url,
    ra_client: items.ra_client,
    client: items.client,
  })
}

/// The answer's form,
/// `( version:number ( cap ... ) url:string ? ra-client:string ( ? client:string ) )`, as
/// `vparse_tuple` matches it: items past the prototype ignored, every one it reaches of the kind it
/// names.
fn answer_items(value: Value<'_>) -> Result<AnswerItems<'_>, Refused> {
  let Value::List(items) = value else {
    return Err(Refused::NotAnswer);
  };
  let mut items = items.into_iter();
  let (Some(Value::Number(version)), Some(Value::List(capabilities)), Some(Value::String(url))) =
    (items.next(), items.next(), items.next())
  else {
    return Err(Refused::NotAnswer);
  };
  let (ra_client, client) = client_strings(items.next(), items.next())?;
  Ok(AnswerItems {
    version,
    capabilities,
    url,
    ra_client,
    client,
  })
}

/// The client's `ra-client` string and its first `client` string, each absent when the answer
/// leaves it out.
type ClientStrings<'a> = (Option<&'a [u8]>, Option<&'a [u8]>);

/// The answer's optional tail, `? ra-client:string ( ? client:string )`: absent whole, or the
/// string and the list after it, the list empty or a string first. `vparse_tuple` stops at the `?`
/// only before the string, so a `ra-client` with no list after it is malformed (the hostile session
/// `hostile-ra-client-without-client-list`).
fn client_strings<'a>(
  ra_client: Option<Value<'a>>,
  client: Option<Value<'a>>,
) -> Result<ClientStrings<'a>, Refused> {
  match (ra_client, client) {
    (None, _) => Ok((None, None)),
    (Some(Value::String(ra_client)), Some(Value::List(inner))) => match inner.into_iter().next() {
      None => Ok((Some(ra_client), None)),
      Some(Value::String(client)) => Ok((Some(ra_client), Some(client))),
      Some(_) => Err(Refused::NotAnswer),
    },
    _ => Err(Refused::NotAnswer),
  }
}

/// Reads an auth-response, `( mech:word ( ? token:string ) )` (`serve.c` 711 reads it as `w(?c)`).
fn auth_response<'a>(value: &Value<'a>) -> Result<(&'a str, Option<&'a [u8]>), Refused> {
  let Value::List(items) = value else {
    return Err(Refused::NotAuthResponse);
  };
  let mut items = items.iter();
  let (Some(Value::Word(mechanism)), Some(Value::List(token))) = (items.next(), items.next())
  else {
    return Err(Refused::NotAuthResponse);
  };
  let token = match token.first() {
    None => None,
    Some(Value::String(token)) => Some(*token),
    Some(_) => return Err(Refused::NotAuthResponse),
  };
  Ok((*mechanism, token))
}

/// Splits a CRAM-MD5 response into its user and digest as `cram.c` 164-168 does: at the last space,
/// the digest exactly 32 lowercase hexadecimal digits. A response holding a NUL byte is refused
/// whole, as `cram.c` refuses every one: it reads the response as a C string, so the digest after
/// the last space before the NUL holds the NUL or is too short.
fn split_response(response: &[u8]) -> Option<(&[u8], [u8; DIGEST_BYTES])> {
  if response.contains(&C_STRING_END) {
    return None;
  }
  let separator = response
    .iter()
    .rposition(|byte| *byte == RESPONSE_SEPARATOR)?;
  let user = response.get(..separator)?;
  let digest = decode_digest(response.get(separator.checked_add(1)?..)?)?;
  Some((user, digest))
}

/// A digest's 32 lowercase hexadecimal digits decoded, as `cram.c`'s `hex_decode` decodes them;
/// `None` for any other length or byte.
fn decode_digest(hex: &[u8]) -> Option<[u8; DIGEST_BYTES]> {
  if hex.len() != DIGEST_BYTES.checked_mul(2)? {
    return None;
  }
  let mut digest = [0; DIGEST_BYTES];
  for (slot, [high, low]) in digest.iter_mut().zip(hex.as_chunks::<2>().0) {
    *slot = hex_value(*high)?.checked_shl(HEX_BITS)? | hex_value(*low)?;
  }
  Some(digest)
}

/// The value of a lowercase hexadecimal digit (`cram.c`'s `hex_to_int` takes no uppercase).
fn hex_value(digit: u8) -> Option<u8> {
  match digit {
    // Format: the decimal digits.
    b'0'..=b'9' => digit.checked_sub(b'0'),
    b'a'..=b'f' => digit.checked_sub(b'a')?.checked_add(HEX_LETTER_BASE),
    _ => None,
  }
}

#[cfg(test)]
mod tests;
