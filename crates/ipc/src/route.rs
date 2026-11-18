//! The daemon's socket route on macOS (§4.6; A-161, A-173): how a client receives a descriptor the daemon offers it, a
//! Shards stream's end (A-161) or the read-only descriptor of a direct map's copy of a file (A-173). Linux sends both over
//! the client's control socket (`rendezvous`); macOS has no such socket, and a descriptor reaches an unrelated process
//! there only through a Unix socket with a name, so the daemon listens on one socket file and nothing else on disk.
//!
//! **Where.** The socket is `slates-<16 hex>.sock` in the user's own temporary directory, `confstr`'s
//! `_CS_DARWIN_USER_TEMP_DIR` (`/var/folders/…/T/`, made by the system for the user alone, mode 0700), never `/tmp`;
//! the hex is a hash of the instance, so daemon and client name the same file and a path stays inside a Unix
//! socket's 104-byte address. The directory is checked to be the user's alone before the socket is bound, the node is
//! made owner-only (0600), and it is removed when the daemon stops; a node a dead daemon left is found unanswered and
//! replaced. Socket files are authorized for this when what crosses them is encrypted end to end (Ada, 2026-10-22).
//!
//! **What crosses it.** Nothing a listener could use. The reply on the client's ring (an attach's, a map's) carries the
//! offer's kind, its id (the attachment, the mapping) and a fresh 256-bit secret that the daemon keeps with the
//! descriptor as one offer. On the socket:
//! 1. the client sends a hello: the protocol's version, the offer's kind and id, and a fresh 32-byte nonce of its own;
//! 2. the daemon answers with a fresh 32-byte nonce of its own;
//! 3. each side derives two keys by HKDF-SHA256 (RFC 5869) from the secret, salted with both nonces, one per
//!    direction, so every exchange has keys no other exchange had and each key seals one message at a nonce of zero
//!    (SP 800-38D §8.2.1 asks a nonce never repeat under a key; here a key never seals twice);
//! 4. the client sends its proof, the id sealed with AES-256-GCM under its key, the transcript (the label, the kind,
//!    the id and both nonces) as additional data, so a proof made for one kind of offer never opens for the other;
//! 5. the daemon opens it, retires the offer (a secret claims once), and sends its confirmation, the id sealed under its
//!    own key, with the descriptor by `SCM_RIGHTS` in the same message; a stream's offer whose resync channel the
//!    daemon made sends that channel's device end beside the stream's, under a confirmation kind of its own that names
//!    two descriptors (A-339), so the client knows exactly what to expect and refuses anything else; the client opens
//!    the confirmation before it trusts a descriptor, and closes the route, which tells the daemon its receive has
//!    returned;
//! 6. only then does the daemon close its own copy of the descriptor. XNU flushes a Unix socket whose every reference is in a
//!    message in flight when its collector of such descriptors runs (`unp_gc`), so an end the daemon closed at once
//!    could arrive with its receive buffer emptied and its reads answering end of file (measured 2026-10-24; the bug
//!    record `docs/bugs/2026-10-24-a-stream-end-arrived-flushed-by-the-kernel-in-flight.md`). A client that still
//!    receives a flushed end, its receive buffer zero, refuses it `Flushed`.
//!
//! So the secret never crosses the socket, a recorded exchange replays to nothing (the daemon's nonce is new each
//! time), a second claim finds no offer, and a process without the secret, which can still connect (the socket is
//! the user's), learns nothing and is refused. Every refusal is one byte naming its kind, then the end.
//!
//! **What it does not change.** The secret is as private as the client's ring, which on macOS is a shared-memory
//! object a same-user process could open by name (GAP-A9-9): such a process could read file contents in the ring's
//! replies too, so the route adds no reach that it does not already have.

use std::ffi::OsStr;
use std::io::{IoSlice, IoSliceMut};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use aws_lc_rs::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use aws_lc_rs::hkdf;
use rustix::net::{
  AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
  SendAncillaryMessage, SendFlags, SocketAddrUnix, SocketFlags, SocketType,
};

use crate::error::{IpcError, RouteRefusal};

/// Format: the bytes of an offer's secret: 256 bits, drawn fresh for each attach.
pub const SECRET_BYTES: usize = 32;
/// Format: the bytes of each side's nonce: 256 bits, drawn fresh for each exchange.
pub const NONCE_BYTES: usize = 32;
/// Format: the version of the exchange a hello opens with: 2 since the hello names the offer's kind (A-173).
const VERSION: u8 = 2;
/// Format: the bytes of a hello's version.
const VERSION_BYTES: usize = 1;
/// Format: the bytes of a hello's offer kind.
const KIND_BYTES: usize = 1;
/// Format: the bytes of an offer's id (a little-endian `u64`: an attachment, a mapping).
const ID_BYTES: usize = 8;
/// Derived: where a hello's offer kind is, past its version.
const HELLO_KIND_AT: usize = VERSION_BYTES;
/// Derived: where a hello's id starts, past its version and kind.
const HELLO_ID_AT: usize = HELLO_KIND_AT + KIND_BYTES;
/// Derived: where a hello's nonce starts, past its version, kind and id.
const HELLO_NONCE_AT: usize = HELLO_ID_AT + ID_BYTES;
/// Derived: a hello: the version, the offer's kind and id, the client's nonce.
pub const HELLO_BYTES: usize = HELLO_NONCE_AT + NONCE_BYTES;
/// Format: an AES-GCM tag.
const TAG_BYTES: usize = 16;
/// Derived: a sealed id, a proof's or a confirmation's: the id and its tag.
pub const SEALED_BYTES: usize = ID_BYTES + TAG_BYTES;
/// Format: the code of a Shards stream's end offered (A-161).
const OFFER_STREAM: u8 = 1;
/// Format: the code of a direct map's copy offered (A-173).
const OFFER_MAP: u8 = 2;
/// Format: the kind byte that opens every message the daemon sends: its nonce follows.
pub const KIND_CHALLENGE: u8 = 0;
/// Format: the kind of the daemon's confirmation, which carries the offer's one descriptor.
pub const KIND_CONFIRM: u8 = 1;
/// Format: the kind of the daemon's confirmation of a stream's offer that carries two descriptors (A-339): the stream's
/// end, then the device's end of its resync channel. Only a stream's offer is confirmed so.
pub const KIND_CONFIRM_WITH_RESYNC: u8 = 6;
/// Format: the kinds of the daemon's refusals, each alone and then the end.
pub const KIND_NO_OFFER: u8 = 2;
/// Format: a refused proof.
pub const KIND_PROOF: u8 = 3;
/// Format: a message not the protocol's.
pub const KIND_MALFORMED: u8 = 4;
/// Format: the daemon at its bound of exchanges.
pub const KIND_BUSY: u8 = 5;
/// Format: the additional data's label, which the transcript opens with.
const AAD_LABEL: &[u8] = b"slates route v2";
/// Format: the HKDF info of the client's key.
const CLIENT_KEY: &[u8] = b"slates route v2 client to daemon";
/// Format: the HKDF info of the daemon's key.
const DAEMON_KEY: &[u8] = b"slates route v2 daemon to client";
/// Derived: the transcript: the label, the offer's kind and id, and both nonces.
const TRANSCRIPT_BYTES: usize = AAD_LABEL.len() + KIND_BYTES + ID_BYTES + 2 * NONCE_BYTES;
/// Format: an AES-256 key.
const KEY_BYTES: usize = 32;
/// Format: the AES-GCM nonce each key seals its one message at.
const ZERO_NONCE: [u8; 12] = [0; 12];
/// Format: a Unix socket address's path on macOS (`sockaddr_un.sun_path`), its terminating NUL included.
const SUN_PATH_BYTES: usize = 104;
/// Format: the socket file's name, around the instance's hash.
const NAME_PREFIX: &str = "slates-";
/// Format: the socket file's name's ending.
const NAME_SUFFIX: &str = ".sock";
/// Format: the bytes of the instance's hash a socket's name carries, as hex: 64 bits, so two instances of one user
/// meet only by a hash collision.
const NAME_HASH_BYTES: usize = 8;
/// Shape: the listen backlog (connections pending accept); the listener drains them at every wake, so it covers one
/// wake's arrivals, as the rendezvous's does.
const BACKLOG: i32 = 64;
/// Format: the largest path `confstr` is asked to write (`PATH_MAX`).
const PATH_MAX: usize = 1024;

/// What an offer holds: the descriptor a client claims on the route.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OfferKind {
  /// A Shards stream's end, offered for an attachment (A-161).
  Stream,
  /// The read-only descriptor of a direct map's copy of a file, offered for a mapping (A-173).
  Map,
}

impl OfferKind {
  /// Its code in a hello and in the transcript.
  pub const fn code(self) -> u8 {
    match self {
      OfferKind::Stream => OFFER_STREAM,
      OfferKind::Map => OFFER_MAP,
    }
  }

  /// The kind `code` names, if any.
  pub const fn of_code(code: u8) -> Option<OfferKind> {
    match code {
      OFFER_STREAM => Some(OfferKind::Stream),
      OFFER_MAP => Some(OfferKind::Map),
      _ => None,
    }
  }
}

/// A hello, the client's first message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hello {
  /// What the client claims.
  pub kind: OfferKind,
  /// The offer's id: the attachment of a stream, the mapping of a map.
  pub id: u64,
  /// The client's nonce.
  pub client_nonce: [u8; NONCE_BYTES],
}

impl Hello {
  /// Its bytes.
  pub fn encode(&self) -> [u8; HELLO_BYTES] {
    let mut bytes = [0u8; HELLO_BYTES];
    let (head, rest) = bytes.split_at_mut(HELLO_ID_AT);
    if let Some(byte) = head.get_mut(..VERSION_BYTES).and_then(<[u8]>::first_mut) {
      *byte = VERSION;
    }
    if let Some(byte) = head.get_mut(HELLO_KIND_AT) {
      *byte = self.kind.code();
    }
    let (id, nonce) = rest.split_at_mut(ID_BYTES);
    id.copy_from_slice(&self.id.to_le_bytes());
    nonce.copy_from_slice(&self.client_nonce);
    bytes
  }

  /// The hello `bytes` hold: refused `Malformed` for another length, another version or a kind no offer has.
  pub fn decode(bytes: &[u8]) -> Result<Hello, IpcError> {
    let malformed = IpcError::RouteRefused {
      refusal: RouteRefusal::Malformed,
    };
    if bytes.len() != HELLO_BYTES || bytes.first() != Some(&VERSION) {
      return Err(malformed);
    }
    let kind = bytes
      .get(HELLO_KIND_AT)
      .copied()
      .and_then(OfferKind::of_code)
      .ok_or(malformed.clone())?;
    let id = bytes
      .get(HELLO_ID_AT..HELLO_NONCE_AT)
      .and_then(|word| <[u8; ID_BYTES]>::try_from(word).ok())
      .map(u64::from_le_bytes)
      .ok_or(malformed.clone())?;
    let client_nonce = bytes
      .get(HELLO_NONCE_AT..)
      .and_then(|nonce| <[u8; NONCE_BYTES]>::try_from(nonce).ok())
      .ok_or(malformed)?;
    Ok(Hello {
      kind,
      id,
      client_nonce,
    })
  }
}

/// One exchange's keys and transcript: derived by both sides from the offer's secret and both nonces.
pub struct Exchange {
  client: LessSafeKey,
  daemon: LessSafeKey,
  transcript: [u8; TRANSCRIPT_BYTES],
  id: u64,
}

impl Exchange {
  /// The keys of the exchange that claims the `kind` offer `id` under `secret`, with the client's and the daemon's
  /// nonces.
  pub fn new(
    secret: &[u8; SECRET_BYTES],
    (kind, id): (OfferKind, u64),
    client_nonce: &[u8; NONCE_BYTES],
    daemon_nonce: &[u8; NONCE_BYTES],
  ) -> Result<Exchange, IpcError> {
    let mut salt = [0u8; 2 * NONCE_BYTES];
    let (first, second) = salt.split_at_mut(NONCE_BYTES);
    first.copy_from_slice(client_nonce);
    second.copy_from_slice(daemon_nonce);
    let mut transcript = [0u8; TRANSCRIPT_BYTES];
    let (label, rest) = transcript.split_at_mut(AAD_LABEL.len());
    label.copy_from_slice(AAD_LABEL);
    let (kind_byte, rest) = rest.split_at_mut(KIND_BYTES);
    if let Some(byte) = kind_byte.first_mut() {
      *byte = kind.code();
    }
    let (word, nonces) = rest.split_at_mut(ID_BYTES);
    word.copy_from_slice(&id.to_le_bytes());
    nonces.copy_from_slice(&salt);
    Ok(Exchange {
      client: key(secret, &salt, CLIENT_KEY)?,
      daemon: key(secret, &salt, DAEMON_KEY)?,
      transcript,
      id,
    })
  }

  /// The client's proof: the id sealed under the client's key.
  pub fn proof(&self) -> Result<[u8; SEALED_BYTES], IpcError> {
    seal(&self.client, &self.transcript, self.id)
  }

  /// Whether `proof` opens under the client's key to this exchange's id.
  pub fn proof_holds(&self, proof: &[u8]) -> bool {
    opens(&self.client, &self.transcript, self.id, proof)
  }

  /// The daemon's confirmation: the id sealed under the daemon's key.
  pub fn confirmation(&self) -> Result<[u8; SEALED_BYTES], IpcError> {
    seal(&self.daemon, &self.transcript, self.id)
  }

  /// Whether `confirmation` opens under the daemon's key to this exchange's id.
  pub fn confirmation_holds(&self, confirmation: &[u8]) -> bool {
    opens(&self.daemon, &self.transcript, self.id, confirmation)
  }
}

/// The AES-256-GCM key HKDF-SHA256 derives from `secret`, salted with `salt`, for `info`.
fn key(secret: &[u8; SECRET_BYTES], salt: &[u8], info: &[u8]) -> Result<LessSafeKey, IpcError> {
  /// The derived key's length, as HKDF asks it.
  struct Length;
  impl hkdf::KeyType for Length {
    fn len(&self) -> usize {
      KEY_BYTES
    }
  }
  let refused = |_| IpcError::OsRefused {
    call: "hkdf",
    code: None,
  };
  let mut bytes = [0u8; KEY_BYTES];
  let info = [info];
  hkdf::Salt::new(hkdf::HKDF_SHA256, salt)
    .extract(secret)
    .expand(&info, Length)
    .map_err(refused)?
    .fill(&mut bytes)
    .map_err(refused)?;
  let unbound = UnboundKey::new(&AES_256_GCM, &bytes).map_err(|_| IpcError::OsRefused {
    call: "aes-256-gcm key",
    code: None,
  })?;
  Ok(LessSafeKey::new(unbound))
}

/// `id` sealed under `key` at the zero nonce, `transcript` as additional data: its bytes and tag.
fn seal(key: &LessSafeKey, transcript: &[u8], id: u64) -> Result<[u8; SEALED_BYTES], IpcError> {
  let mut sealed = [0u8; SEALED_BYTES];
  let (body, tag_out) = sealed.split_at_mut(ID_BYTES);
  body.copy_from_slice(&id.to_le_bytes());
  let tag = key
    .seal_in_place_separate_tag(
      Nonce::assume_unique_for_key(ZERO_NONCE),
      Aad::from(transcript),
      body,
    )
    .map_err(|_| IpcError::OsRefused {
      call: "aes-256-gcm seal",
      code: None,
    })?;
  tag_out.copy_from_slice(tag.as_ref());
  Ok(sealed)
}

/// Whether `sealed` opens under `key` with `transcript` to `id`.
fn opens(key: &LessSafeKey, transcript: &[u8], id: u64, sealed: &[u8]) -> bool {
  if sealed.len() != SEALED_BYTES {
    return false;
  }
  let mut copy = [0u8; SEALED_BYTES];
  copy.copy_from_slice(sealed);
  key
    .open_in_place(
      Nonce::assume_unique_for_key(ZERO_NONCE),
      Aad::from(transcript),
      &mut copy,
    )
    .is_ok_and(|plain| plain == id.to_le_bytes().as_slice())
}

/// `N` random bytes from the system's generator (a secret, a nonce).
pub fn random<const N: usize>() -> Result<[u8; N], IpcError> {
  use aws_lc_rs::rand::SecureRandom;
  let mut bytes = [0u8; N];
  aws_lc_rs::rand::SystemRandom::new()
    .fill(&mut bytes)
    .map_err(|_| IpcError::OsRefused {
      call: "secure random",
      code: None,
    })?;
  Ok(bytes)
}

/// The route's socket for `instance`: `slates-<hash>.sock` in the user's own temporary directory. Refused
/// `PathTooLong` when it would not fit a socket address.
pub fn path(instance: &str) -> Result<PathBuf, IpcError> {
  let digest = blake3::hash(instance.as_bytes());
  let mut name = String::from(NAME_PREFIX);
  for byte in digest.as_bytes().iter().take(NAME_HASH_BYTES) {
    name.push_str(&format!("{byte:02x}"));
  }
  name.push_str(NAME_SUFFIX);
  let path = user_temp_dir()?.join(name);
  if path.as_os_str().len() >= SUN_PATH_BYTES {
    return Err(IpcError::RouteRefused {
      refusal: RouteRefusal::PathTooLong,
    });
  }
  Ok(path)
}

/// The user's own temporary directory, as the system names it (`confstr(_CS_DARWIN_USER_TEMP_DIR)`).
fn user_temp_dir() -> Result<PathBuf, IpcError> {
  let mut buffer = [0u8; PATH_MAX];
  // SAFETY: `buffer` is a live, writable array of `buffer.len()` bytes, the length passed; `confstr` writes at most
  // that many bytes (the value and its NUL, truncated to fit) and returns the length the whole value needs.
  let needed = unsafe {
    libc::confstr(
      libc::_CS_DARWIN_USER_TEMP_DIR,
      buffer.as_mut_ptr().cast(),
      buffer.len(),
    )
  };
  let refused = IpcError::OsRefused {
    call: "confstr(_CS_DARWIN_USER_TEMP_DIR)",
    code: None,
  };
  // Zero is no value; a value needing more than the buffer was cut and is not used.
  if needed == 0 || needed > buffer.len() {
    return Err(refused);
  }
  let value = buffer.get(..needed.saturating_sub(1)).ok_or(refused)?;
  Ok(PathBuf::from(OsStr::from_bytes(value)))
}

/// Refuses `SharedDirectory` unless `directory` is a directory the effective user owns that no one else may use.
fn owner_only(directory: &Path) -> Result<(), IpcError> {
  use rustix::fs::{FileType, Mode};
  let stat = rustix::fs::stat(directory).map_err(|e| refused("stat route directory", e))?;
  let mine = stat.st_uid == rustix::process::geteuid().as_raw();
  // No permission for the group or anyone else: the directory is the user's alone.
  let private = !Mode::from_raw_mode(stat.st_mode).intersects(Mode::RWXG | Mode::RWXO);
  let directory = FileType::from_raw_mode(stat.st_mode) == FileType::Directory;
  if mine && private && directory {
    Ok(())
  } else {
    Err(IpcError::RouteRefused {
      refusal: RouteRefusal::SharedDirectory,
    })
  }
}

fn refused(call: &'static str, e: rustix::io::Errno) -> IpcError {
  IpcError::OsRefused {
    call,
    code: Some(e.raw_os_error()),
  }
}

/// The daemon's listening socket: bound at its path, removed when dropped.
pub struct Listener {
  socket: OwnedFd,
  path: PathBuf,
}

impl Listener {
  /// Binds the route's socket for `instance` (see the module doc): the directory checked to be the user's alone, a
  /// node a dead daemon left replaced, a live daemon's refused `InUse`, the node made owner-only. The socket is
  /// non-blocking and close-on-exec.
  pub fn bind(instance: &str) -> Result<Listener, IpcError> {
    let path = path(instance)?;
    if let Some(directory) = path.parent() {
      owner_only(directory)?;
    }
    let address = SocketAddrUnix::new(&path).map_err(|e| refused("socket address", e))?;
    let socket = stream_socket()?;
    match rustix::net::bind(&socket, &address) {
      Ok(()) => {}
      Err(rustix::io::Errno::ADDRINUSE) => {
        if answers(&address) {
          return Err(IpcError::RouteRefused {
            refusal: RouteRefusal::InUse,
          });
        }
        // A node a dead daemon left: nothing answers it, so it is removed and bound again.
        remove(&path)?;
        rustix::net::bind(&socket, &address).map_err(|e| refused("bind route", e))?;
      }
      Err(e) => return Err(refused("bind route", e)),
    }
    let listener = Listener { socket, path };
    // The route's own socket node, made the user's alone (A-161; socket files authorized by Ada 2026-10-22 when what
    // crosses them is encrypted). structural: allow
    rustix::fs::chmod(
      &listener.path,
      rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    )
    .map_err(|e| refused("chmod route", e))?;
    rustix::net::listen(&listener.socket, BACKLOG).map_err(|e| refused("listen route", e))?;
    rustix::io::ioctl_fionbio(&listener.socket, true).map_err(|e| refused("ioctl_fionbio", e))?;
    Ok(listener)
  }

  /// The listening socket, for a readiness wait.
  pub fn socket(&self) -> BorrowedFd<'_> {
    self.socket.as_fd()
  }

  /// The socket's path.
  pub fn path(&self) -> &Path {
    &self.path
  }

  /// A connection waiting to be accepted, made non-blocking and close-on-exec, with `SIGPIPE` set aside; `None` when
  /// none waits.
  pub fn accept(&self) -> Result<Option<OwnedFd>, IpcError> {
    let connection = match rustix::net::accept(&self.socket) {
      Ok(connection) => connection,
      Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => return Ok(None),
      Err(e) => return Err(refused("accept route", e)),
    };
    rustix::io::fcntl_setfd(&connection, rustix::io::FdFlags::CLOEXEC)
      .map_err(|e| refused("fcntl_setfd", e))?;
    rustix::net::sockopt::set_socket_nosigpipe(&connection, true)
      .map_err(|e| refused("setsockopt(SO_NOSIGPIPE)", e))?;
    rustix::io::ioctl_fionbio(&connection, true).map_err(|e| refused("ioctl_fionbio", e))?;
    Ok(Some(connection))
  }
}

impl Drop for Listener {
  fn drop(&mut self) {
    // The node goes with the daemon. A removal refused leaves a node the next daemon finds unanswered and replaces.
    let _ = remove(&self.path);
  }
}

/// Removes the route's socket node at `path`.
fn remove(path: &Path) -> Result<(), IpcError> {
  // The route's own socket node, removed when its daemon stops or found left by a dead one (A-161; socket files
  // authorized by Ada 2026-10-22 when what crosses them is encrypted). structural: allow
  rustix::fs::unlink(path).map_err(|e| refused("unlink route", e))
}

/// A Unix stream socket, close-on-exec, with `SIGPIPE` set aside.
fn stream_socket() -> Result<OwnedFd, IpcError> {
  let socket = rustix::net::socket_with(
    AddressFamily::UNIX,
    SocketType::STREAM,
    SocketFlags::empty(),
    None,
  )
  .map_err(|e| refused("socket", e))?;
  rustix::io::fcntl_setfd(&socket, rustix::io::FdFlags::CLOEXEC)
    .map_err(|e| refused("fcntl_setfd", e))?;
  rustix::net::sockopt::set_socket_nosigpipe(&socket, true)
    .map_err(|e| refused("setsockopt(SO_NOSIGPIPE)", e))?;
  Ok(socket)
}

/// Whether a daemon answers at `address`.
fn answers(address: &SocketAddrUnix) -> bool {
  stream_socket().is_ok_and(|probe| rustix::net::connect(&probe, address).is_ok())
}

/// Refuses a direct map's descriptor that would authorize a peer more than the copy it is offered for allows (§4.6,
/// A-173, A-186, macOS). macOS has no seals, so the capability is the descriptor's own access. A read-only offer's
/// descriptor must be open read alone (`F_GETFL`), which refuses the peer a writable shared map and a write of it; a
/// writable offer's may be open read and write, since changing the copy's bytes is what it authorizes. Either way XNU
/// refuses any second sizing of the object whatever the mode (`EINVAL`), and the object must be exactly the
/// `authorized` bytes (`fstat`), the copy and nothing more. An object reached by no name (the daemon unlinks it at once)
/// cannot be opened again in another mode; a read-only descriptor of one is a read-only capability (measured on macOS
/// 26.4.1, `crates/mem/tests/peer_object.rs`). Refused `UnconfinedDescriptor` naming what it would have authorized.
pub fn confined_map(
  descriptor: BorrowedFd<'_>,
  authorized: u64,
  access: slates_mem::PeerAccess,
) -> Result<(), IpcError> {
  let object_bytes = rustix::fs::fstat(descriptor)
    .map(|stat| u64::try_from(stat.st_size).unwrap_or(u64::MAX))
    .map_err(|e| refused("fstat offered map", e))?;
  let mode = rustix::fs::fcntl_getfl(descriptor).map_err(|e| refused("fcntl(F_GETFL)", e))?;
  // structural: allow — the access modes `F_GETFL` answered, compared to refuse a writable descriptor; nothing is opened.
  let opened_to_write = mode.intersects(rustix::fs::OFlags::WRONLY | rustix::fs::OFlags::RDWR);
  let writable = opened_to_write && access == slates_mem::PeerAccess::Read;
  if writable || object_bytes != authorized {
    return Err(IpcError::UnconfinedDescriptor {
      object_bytes,
      authorized_bytes: authorized,
      writable,
    });
  }
  Ok(())
}

/// Format: the most descriptors a confirmation carries: a stream's end and its resync end (A-339).
const CONFIRMED_DESCRIPTORS: usize = 2;
/// Format: the descriptors a confirmation is read with room for: one more than the most it carries, so a message
/// carrying more is seen as such and refused rather than cut short.
const CONFIRMATION_ROOM: usize = CONFIRMED_DESCRIPTORS + 1;

/// Sends the daemon's confirmation on `connection`, the offered descriptor by `SCM_RIGHTS` in the same message, so it
/// arrives with the bytes that vouch for it; for a stream's offer with a resync channel, `resync`, that channel's device
/// end, rides beside it under [`KIND_CONFIRM_WITH_RESYNC`] (A-339).
pub fn send_confirmation(
  connection: BorrowedFd<'_>,
  confirmation: &[u8; SEALED_BYTES],
  stream: BorrowedFd<'_>,
  resync: Option<BorrowedFd<'_>>,
) -> Result<(), IpcError> {
  let mut message = [0u8; 1 + SEALED_BYTES];
  let (kind, body) = message.split_at_mut(1);
  if let Some(byte) = kind.first_mut() {
    *byte = if resync.is_some() {
      KIND_CONFIRM_WITH_RESYNC
    } else {
      KIND_CONFIRM
    };
  }
  body.copy_from_slice(confirmation);
  let both = [Some(stream), resync];
  let fds: Vec<BorrowedFd<'_>> = both.into_iter().flatten().collect();
  let mut space =
    [std::mem::MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(CONFIRMED_DESCRIPTORS))];
  let mut control = SendAncillaryBuffer::new(&mut space);
  if !control.push(SendAncillaryMessage::ScmRights(&fds)) {
    return Err(IpcError::Layout {
      reason: "the offered descriptors did not fit the ancillary buffer",
    });
  }
  let sent = rustix::net::sendmsg(
    connection,
    &[IoSlice::new(&message)],
    &mut control,
    SendFlags::empty(),
  )
  .map_err(|e| refused("sendmsg confirmation", e))?;
  if sent != message.len() {
    return Err(IpcError::Layout {
      reason: "the confirmation was sent short",
    });
  }
  Ok(())
}

/// The kind byte a refusal of `refusal` is sent as; `None` for one the daemon never sends (the route's own place).
pub fn refusal_kind(refusal: RouteRefusal) -> Option<u8> {
  match refusal {
    RouteRefusal::NoOffer => Some(KIND_NO_OFFER),
    RouteRefusal::Proof => Some(KIND_PROOF),
    RouteRefusal::Malformed => Some(KIND_MALFORMED),
    RouteRefusal::Busy => Some(KIND_BUSY),
    RouteRefusal::Confirmation
    | RouteRefusal::Flushed
    | RouteRefusal::SharedDirectory
    | RouteRefusal::PathTooLong
    | RouteRefusal::InUse => None,
  }
}

/// The refusal a daemon's kind byte names; `Malformed` for a byte that names none.
fn refusal_of(kind: u8) -> RouteRefusal {
  match kind {
    KIND_NO_OFFER => RouteRefusal::NoOffer,
    KIND_PROOF => RouteRefusal::Proof,
    KIND_BUSY => RouteRefusal::Busy,
    _ => RouteRefusal::Malformed,
  }
}

/// What a claim brings: the offer's descriptor (a stream's end, a map's copy), and for a stream's offer the device's
/// end of its resync channel when the daemon made one (A-339). Each close-on-exec.
#[derive(Debug)]
pub struct Claimed {
  /// The offered descriptor.
  pub descriptor: OwnedFd,
  /// The stream's resync end, beside it; none for a map, or a stream with no channel.
  pub resync: Option<OwnedFd>,
}

/// Claims the `kind` offer `id` from the daemon of `instance` with the offer's `secret` (the client's side of the
/// exchange in the module doc): its descriptor, and a stream's resync end beside it, once the daemon's confirmation
/// opens. Each socket read and write waits `within` at a time, and when that runs out asks `alive` whether the daemon
/// still lives: while it does, the step goes on waiting, as a client waits for a deferred reply (A-186), since a live
/// daemon always answers its route; once it does not, the claim is refused `DeadlineExceeded`. The daemon's refusals
/// arrive typed (`RouteRefused`); a confirmation that does not open is refused `Confirmation`, one carrying other
/// descriptors than its kind and the offer's name `Malformed`, and one carrying a socket the kernel flushed in flight
/// `Flushed`, every descriptor it carried closed.
pub fn claim(
  instance: &str,
  (kind, id): (OfferKind, u64),
  secret: &[u8; SECRET_BYTES],
  within: Duration,
  alive: &mut dyn FnMut() -> bool,
) -> Result<Claimed, IpcError> {
  let address = SocketAddrUnix::new(path(instance)?).map_err(|e| refused("socket address", e))?;
  let socket = stream_socket()?;
  for which in [
    rustix::net::sockopt::Timeout::Recv,
    rustix::net::sockopt::Timeout::Send,
  ] {
    rustix::net::sockopt::set_socket_timeout(&socket, which, Some(within))
      .map_err(|e| refused("setsockopt(SO_*TIMEO)", e))?;
  }
  rustix::net::connect(&socket, &address).map_err(|e| refused("connect route", e))?;
  let hello = Hello {
    kind,
    id,
    client_nonce: random()?,
  };
  write_all(socket.as_fd(), &hello.encode(), alive)?;
  let mut answer = [0u8; 1];
  read_exact(socket.as_fd(), &mut answer, alive)?;
  if answer != [KIND_CHALLENGE] {
    return Err(IpcError::RouteRefused {
      refusal: refusal_of(answer.first().copied().unwrap_or(KIND_MALFORMED)),
    });
  }
  let mut daemon_nonce = [0u8; NONCE_BYTES];
  read_exact(socket.as_fd(), &mut daemon_nonce, alive)?;
  let exchange = Exchange::new(secret, (kind, id), &hello.client_nonce, &daemon_nonce)?;
  write_all(socket.as_fd(), &exchange.proof()?, alive)?;
  let (confirmation, claimed) = receive_confirmation(socket.as_fd(), kind, alive)?;
  if !exchange.confirmation_holds(&confirmation) {
    return Err(IpcError::RouteRefused {
      refusal: RouteRefusal::Confirmation,
    });
  }
  // A flush empties a socket's receive buffer and zeroes its high-water mark; a live end's is the kernel's default. A
  // descriptor that is not a socket (a map's copy) answers no such size, and is not judged by it.
  let flushed = |descriptor: &OwnedFd| {
    rustix::net::sockopt::socket_recv_buffer_size(descriptor).is_ok_and(|bytes| bytes == 0)
  };
  if flushed(&claimed.descriptor) || claimed.resync.as_ref().is_some_and(flushed) {
    return Err(IpcError::RouteRefused {
      refusal: RouteRefusal::Flushed,
    });
  }
  // Closing `socket` on return tells the daemon this receive has returned, so it may close its copies of the ends.
  Ok(claimed)
}

/// Whether a step whose wait ran out may wait again: while `alive` says the daemon lives; otherwise the claim is
/// refused `DeadlineExceeded`.
fn wait_again(alive: &mut dyn FnMut() -> bool) -> Result<(), IpcError> {
  if alive() {
    Ok(())
  } else {
    Err(IpcError::DeadlineExceeded)
  }
}

/// The daemon's answer to a proof for a `kind` offer: its confirmation and the descriptors it carried, or its refusal.
/// Its wait runs out and is renewed as [`claim`] says.
fn receive_confirmation(
  socket: BorrowedFd<'_>,
  kind: OfferKind,
  alive: &mut dyn FnMut() -> bool,
) -> Result<([u8; SEALED_BYTES], Claimed), IpcError> {
  let mut message = [0u8; 1 + SEALED_BYTES];
  let mut space =
    [std::mem::MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(CONFIRMATION_ROOM))];
  let mut control = RecvAncillaryBuffer::new(&mut space);
  let received = loop {
    match rustix::net::recvmsg(
      socket,
      &mut [IoSliceMut::new(&mut message)],
      &mut control,
      RecvFlags::empty(),
    ) {
      Ok(received) => break received,
      Err(rustix::io::Errno::INTR) => {}
      Err(rustix::io::Errno::AGAIN) => wait_again(alive)?,
      Err(e) => return Err(refused("recvmsg confirmation", e)),
    }
  };
  let mut descriptors: Vec<OwnedFd> = Vec::new();
  for ancillary in control.drain() {
    if let RecvAncillaryMessage::ScmRights(iter) = ancillary {
      descriptors.extend(iter);
    }
  }
  // macOS has no `MSG_CMSG_CLOEXEC`, so each received descriptor is made close-on-exec here, as the client's promise
  // to its caller says: no child the client spawns inherits one by accident. A child another of the client's threads
  // forks between the receive and this call could still inherit it; a client that forks while it claims makes that
  // window.
  for descriptor in &descriptors {
    rustix::io::fcntl_setfd(descriptor, rustix::io::FdFlags::CLOEXEC)
      .map_err(|e| refused("fcntl_setfd received end", e))?;
  }
  let confirmed = message.first().copied().unwrap_or(KIND_MALFORMED);
  if received.bytes == 0 {
    return Err(IpcError::RouteRefused {
      refusal: RouteRefusal::Malformed,
    });
  }
  if !matches!(confirmed, KIND_CONFIRM | KIND_CONFIRM_WITH_RESYNC) {
    return Err(IpcError::RouteRefused {
      refusal: refusal_of(confirmed),
    });
  }
  let malformed = IpcError::RouteRefused {
    refusal: RouteRefusal::Malformed,
  };
  if received.bytes != message.len() {
    return Err(malformed);
  }
  let claimed = claimed_of(confirmed, kind, descriptors).ok_or(malformed.clone())?;
  let confirmation = message
    .get(1..)
    .and_then(|body| <[u8; SEALED_BYTES]>::try_from(body).ok())
    .ok_or(malformed)?;
  Ok((confirmation, claimed))
}

/// The descriptors a confirmation of kind `confirmed` carried for a `kind` offer, as the protocol names them: one for
/// [`KIND_CONFIRM`]; a stream's end then its resync end for [`KIND_CONFIRM_WITH_RESYNC`], which only a stream's offer is
/// confirmed with (A-339). `None` for any other count or pairing, every descriptor then closed.
fn claimed_of(confirmed: u8, kind: OfferKind, descriptors: Vec<OwnedFd>) -> Option<Claimed> {
  let with_resync = confirmed == KIND_CONFIRM_WITH_RESYNC;
  if with_resync && kind != OfferKind::Stream {
    return None;
  }
  match <[OwnedFd; 2]>::try_from(descriptors) {
    Ok([descriptor, resync]) if with_resync => Some(Claimed {
      descriptor,
      resync: Some(resync),
    }),
    Ok(_) => None,
    Err(descriptors) => match <[OwnedFd; 1]>::try_from(descriptors) {
      Ok([descriptor]) if !with_resync => Some(Claimed {
        descriptor,
        resync: None,
      }),
      _ => None,
    },
  }
}

/// Writes all of `bytes` on a blocking socket whose writes time out, each wait renewed as [`claim`] says.
fn write_all(
  socket: BorrowedFd<'_>,
  mut bytes: &[u8],
  alive: &mut dyn FnMut() -> bool,
) -> Result<(), IpcError> {
  while !bytes.is_empty() {
    match rustix::net::send(socket, bytes, SendFlags::empty()) {
      Ok(0) => {
        return Err(IpcError::RouteRefused {
          refusal: RouteRefusal::Malformed,
        });
      }
      Ok(sent) => bytes = bytes.get(sent..).unwrap_or(&[]),
      Err(rustix::io::Errno::INTR) => {}
      Err(rustix::io::Errno::AGAIN) => wait_again(alive)?,
      Err(e) => return Err(refused("send route", e)),
    }
  }
  Ok(())
}

/// Fills `buffer` from a blocking socket whose reads time out, each wait renewed as [`claim`] says; the daemon's
/// closing before it is full is `Malformed`.
fn read_exact(
  socket: BorrowedFd<'_>,
  buffer: &mut [u8],
  alive: &mut dyn FnMut() -> bool,
) -> Result<(), IpcError> {
  let mut filled = 0usize;
  while filled < buffer.len() {
    let Some(room) = buffer.get_mut(filled..) else {
      break;
    };
    match rustix::net::recv(socket, room, RecvFlags::empty()) {
      Ok((0, _)) => {
        return Err(IpcError::RouteRefused {
          refusal: RouteRefusal::Malformed,
        });
      }
      Ok((read, _)) => filled = filled.saturating_add(read),
      Err(rustix::io::Errno::INTR) => {}
      Err(rustix::io::Errno::AGAIN) => wait_again(alive)?,
      Err(e) => return Err(refused("recv route", e)),
    }
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Do: derive both sides of one exchange. Expect the proof and the confirmation to hold on the matching side, and
  /// a proof never to hold as a confirmation, nor a confirmation as a proof.
  #[test]
  fn an_exchange_holds_on_both_sides_and_its_directions_differ() {
    let secret = [7u8; SECRET_BYTES];
    let (client, daemon) = ([1u8; NONCE_BYTES], [2u8; NONCE_BYTES]);
    let ours = Exchange::new(&secret, (OfferKind::Stream, 42), &client, &daemon).unwrap();
    let theirs = Exchange::new(&secret, (OfferKind::Stream, 42), &client, &daemon).unwrap();
    let proof = ours.proof().unwrap();
    let confirmation = ours.confirmation().unwrap();
    assert!(theirs.proof_holds(&proof));
    assert!(theirs.confirmation_holds(&confirmation));
    assert!(
      !theirs.confirmation_holds(&proof),
      "the directions' keys differ"
    );
    assert!(!theirs.proof_holds(&confirmation));
  }

  /// Do: check one exchange's proof and confirmation against exchanges that differ in the secret, a nonce, the id or
  /// the offer's kind, then against the proof with each byte flipped and the proof cut short. Expect none to hold: a
  /// proof made to claim a stream never claims a map with the same id and secret (A-173).
  #[test]
  fn an_exchange_holds_only_under_its_own_secret_nonces_kind_and_id() {
    let secret = [7u8; SECRET_BYTES];
    let (client, daemon) = ([1u8; NONCE_BYTES], [2u8; NONCE_BYTES]);
    let stream = (OfferKind::Stream, 42);
    let ours = Exchange::new(&secret, stream, &client, &daemon).unwrap();
    let proof = ours.proof().unwrap();
    let confirmation = ours.confirmation().unwrap();
    for other in [
      Exchange::new(&[8u8; SECRET_BYTES], stream, &client, &daemon).unwrap(),
      Exchange::new(&secret, (OfferKind::Stream, 43), &client, &daemon).unwrap(),
      Exchange::new(&secret, (OfferKind::Map, 42), &client, &daemon).unwrap(),
      Exchange::new(&secret, stream, &[3u8; NONCE_BYTES], &daemon).unwrap(),
      Exchange::new(&secret, stream, &client, &[4u8; NONCE_BYTES]).unwrap(),
    ] {
      assert!(!other.proof_holds(&proof));
      assert!(!other.confirmation_holds(&confirmation));
    }
    for at in 0..SEALED_BYTES {
      let mut tampered = proof;
      tampered[at] ^= 1;
      assert!(!ours.proof_holds(&tampered), "byte {at}");
    }
    assert!(!ours.proof_holds(&proof[..SEALED_BYTES - 1]));
  }

  /// Do: encode a hello of each kind and decode it, then decode one cut short, one too long, one of another version and
  /// one naming a kind no offer has. Expect the same hello back, and each hostile one refused `Malformed`.
  #[test]
  fn a_hello_round_trips_and_hostile_ones_are_refused() {
    for kind in [OfferKind::Stream, OfferKind::Map] {
      let hello = Hello {
        kind,
        id: 0x0102_0304_0506_0708,
        client_nonce: [9u8; NONCE_BYTES],
      };
      assert_eq!(Hello::decode(&hello.encode()).unwrap(), hello);
    }
    let hello = Hello {
      kind: OfferKind::Map,
      id: 0x0102_0304_0506_0708,
      client_nonce: [9u8; NONCE_BYTES],
    };
    let bytes = hello.encode();
    let mut unknown_kind = bytes;
    unknown_kind[HELLO_KIND_AT] = 0;
    assert_eq!(
      Hello::decode(&unknown_kind),
      Err(IpcError::RouteRefused {
        refusal: RouteRefusal::Malformed
      })
    );
    let malformed = Err(IpcError::RouteRefused {
      refusal: RouteRefusal::Malformed,
    });
    assert_eq!(Hello::decode(&bytes[..HELLO_BYTES - 1]), malformed);
    let mut long = bytes.to_vec();
    long.push(0);
    assert_eq!(Hello::decode(&long), malformed);
    let mut other = bytes;
    other[0] = VERSION + 1;
    assert_eq!(Hello::decode(&other), malformed);
  }

  /// Do: compute the route's path for two instances. Expect each in the user's own temporary directory, which is the
  /// user's alone, inside a socket address, and the two different.
  #[test]
  fn the_path_is_in_the_users_own_directory_and_fits_an_address() {
    let first = path("one").unwrap();
    let second = path("two").unwrap();
    assert_ne!(first, second);
    assert!(first.as_os_str().len() < SUN_PATH_BYTES);
    let directory = first.parent().unwrap();
    assert!(!directory.starts_with("/tmp"));
    owner_only(directory).unwrap();
  }

  /// Reads `received`'s copy of a stream end: what its peer `peer` wrote reads back through it.
  fn reads_through(peer: &OwnedFd, received: &OwnedFd, bytes: &[u8]) -> bool {
    crate::stream::send(peer.as_fd(), bytes).unwrap();
    let mut back = vec![0u8; bytes.len()];
    rustix::io::read(received, &mut back).is_ok_and(|read| back.get(..read) == Some(bytes))
  }

  /// A-339. Do: send a stream's confirmation with its resync end beside its end, and one with its end alone; read each
  /// as a stream's claim and as a map's. Expect a stream's claim of the first to take the end and then the resync end,
  /// each the very socket sent (bytes written on its peer read back through it) and close-on-exec; a map's claim of it
  /// refused `Malformed`, since only a stream's offer carries a resync end; the second taken by either kind of claim with
  /// no resync end.
  #[test]
  fn a_streams_confirmation_carries_its_resync_end_beside_its_end_and_a_maps_never_does() {
    let confirmation = [7u8; SEALED_BYTES];
    let mut alive = || true;
    // Kept open past every receipt: a socket whose every copy rides a message is flushed by macOS's collector (A-161).
    let (stream_peer, stream_end) = crate::stream::pair().unwrap();
    let (resync_peer, resync_end) = crate::stream::pair().unwrap();
    let malformed = IpcError::RouteRefused {
      refusal: RouteRefusal::Malformed,
    };
    for kind in [OfferKind::Stream, OfferKind::Map] {
      let (daemon, client) = crate::stream::pair().unwrap();
      send_confirmation(
        daemon.as_fd(),
        &confirmation,
        stream_end.as_fd(),
        Some(resync_end.as_fd()),
      )
      .unwrap();
      match (kind, receive_confirmation(client.as_fd(), kind, &mut alive)) {
        (OfferKind::Stream, Ok((confirmed, claimed))) => {
          assert_eq!(confirmed, confirmation);
          let resync = claimed.resync.expect("the resync end rides beside the end");
          assert!(reads_through(&stream_peer, &claimed.descriptor, b"end"));
          assert!(reads_through(&resync_peer, &resync, b"resync"));
          for received in [&claimed.descriptor, &resync] {
            let flags = rustix::io::fcntl_getfd(received).unwrap();
            assert!(flags.contains(rustix::io::FdFlags::CLOEXEC));
          }
        }
        (OfferKind::Map, Err(refused)) => assert_eq!(refused, malformed),
        (kind, other) => panic!("a {kind:?} claim of a stream's two ends: {other:?}"),
      }
    }
    for kind in [OfferKind::Stream, OfferKind::Map] {
      let (daemon, client) = crate::stream::pair().unwrap();
      send_confirmation(daemon.as_fd(), &confirmation, stream_end.as_fd(), None).unwrap();
      let (_, claimed) = receive_confirmation(client.as_fd(), kind, &mut alive).unwrap();
      assert!(claimed.resync.is_none(), "one descriptor, no resync end");
      assert!(reads_through(&stream_peer, &claimed.descriptor, b"alone"));
    }
  }
}
