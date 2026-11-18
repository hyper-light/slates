//! The library's closed refusal taxonomy (§4.4's refusals as the daemon sends them, the channel's own, and this crate's:
//! a mount the OS refused, an issuer authority this process does not hold, a path the wire cannot carry). Every
//! failure of every verb is one of these variants; none is a panic and none is prose alone (CLAUDE.md §3 "Errors, not
//! panics"). A host decides on the variant: [`Error::Refused`] is the daemon's typed answer (retry, re-plan, or show
//! it), [`Error::Unavailable`] means no daemon answers at the instance (start one, or report it), and the rest name
//! what failed in this process.

use std::fmt;

use slates_client::ClientError;
use slates_ipc::protocol::Refusal;

/// A typed failure of one of this crate's verbs. (`Eq` is not derived: a daemon [`Refusal`] may carry measured
/// probabilities.)
#[derive(Debug, Clone, PartialEq)]
pub enum Error {
  /// The daemon refused the verb (§4.4's taxonomy, each refusal counted by the daemon).
  Refused(Refusal),
  /// No daemon answers at the instance: its rendezvous is absent, its claim went unanswered, or the daemon went away
  /// and did not come back within the reconnect budget (§4.7 "Failure matrix").
  Unavailable {
    /// The instance asked for.
    instance: String,
    /// What the client found.
    cause: ClientError,
  },
  /// The channel to a live daemon failed otherwise (typed by the client: a full ring, a session another client
  /// holds, a reply the verb does not expect).
  Channel(ClientError),
  /// A host mount could not be made or removed.
  Mount(MountFault),
  /// This process holds no issuer authority, or the authority it was given does not attach.
  Issuer(IssuerFault),
  /// A path the verb takes cannot be carried to the daemon as it is.
  Path {
    /// The path, as text (lossily, when it was not Unicode).
    path: String,
    /// What is wrong with it.
    fault: PathFault,
  },
}

impl Error {
  /// The daemon's refusal, when that is what this is.
  pub fn refusal(&self) -> Option<&Refusal> {
    match self {
      Self::Refused(refusal) => Some(refusal),
      _ => None,
    }
  }

  /// Whether no daemon answers at the instance.
  pub fn is_unavailable(&self) -> bool {
    matches!(self, Self::Unavailable { .. })
  }

  /// A client failure of a verb sent to `instance`, typed: the daemon's refusal, no daemon, or the channel's own.
  pub(crate) fn of(instance: &str, error: ClientError) -> Error {
    match error {
      ClientError::Refused(refusal) => Error::Refused(refusal),
      ClientError::Ipc(slates_ipc::IpcError::DaemonUnavailable { .. })
      | ClientError::DaemonGone { .. } => Error::Unavailable {
        instance: instance.to_owned(),
        cause: error,
      },
      other => Error::Channel(other),
    }
  }
}

impl fmt::Display for Error {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Refused(Refusal::ConsensusNotInitialized) => f.write_str(
        "refused: ConsensusNotInitialized: this node has no consensus group yet; create it once (`bootstrap`), or, on \
         a fleet node, wait for it to import the groups",
      ),
      Self::Refused(refusal) => write!(f, "refused: {refusal:?}"),
      Self::Unavailable { instance, cause } => {
        write!(f, "no daemon at instance {instance}: {cause}")
      }
      Self::Channel(error) => write!(f, "{error}"),
      Self::Mount(fault) => write!(f, "{fault}"),
      Self::Issuer(fault) => write!(f, "{fault}"),
      Self::Path { path, fault } => write!(f, "{path}: {fault}"),
    }
  }
}

impl std::error::Error for Error {}

/// What is wrong with a path a verb takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathFault {
  /// It is relative: the daemon would resolve it against its own working directory, not the caller's.
  NotAbsolute,
  /// It is not Unicode, and the wire carries paths as UTF-8.
  NotUnicode,
}

impl fmt::Display for PathFault {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::NotAbsolute => f.write_str(
        "a relative path; the daemon resolves paths against its own working directory, so pass an absolute one",
      ),
      Self::NotUnicode => f.write_str("not Unicode; the wire carries paths as UTF-8"),
    }
  }
}

/// Why a host mount could not be made or removed (§4.6). An OS refusal carries its error number, never a message
/// alone, so a host decides on it.
#[derive(Debug, Clone, PartialEq)]
pub enum MountFault {
  /// The mount point could not be resolved to the real path the kernel records (it does not exist, or is not
  /// reachable); the error number.
  MountPoint {
    /// The mount point as given.
    path: String,
    /// The OS error number, when the OS gave one.
    errno: Option<i32>,
  },
  /// This host has no mount for the form asked: no loopback mount mechanism (`mount_nfs` missing on a BSD), or an
  /// option this platform's mount does not take.
  Unsupported {
    /// What is missing, in a sentence a person can act on.
    reason: &'static str,
  },
  /// The daemon serves no NFS loopback listener (its bind failed), so no loopback mount can reach it.
  NoNfsListener,
  /// The daemon issued no mount capability for the mount's attachment (§4.13; AUD-01), so nothing could present one.
  NoCapability,
  /// The daemon's MOUNT service did not hand over the export's root handle (macOS, §4.6 A-34).
  RootHandle {
    /// The OS error number of the loopback exchange, when the socket failed.
    errno: Option<i32>,
    /// The MOUNT status the daemon refused with (`mountstat3`), when it answered one.
    status: Option<u32>,
  },
  /// The kernel refused the mount; the error number.
  Refused {
    /// The mount point.
    path: String,
    /// The OS error number, when the OS gave one.
    errno: Option<i32>,
  },
  /// The mount was made but its record could not name it (§4.4 `Binding → Bound`), and taking it down again failed
  /// too: the mount stays until its user unmounts it.
  Unbound {
    /// The daemon's refusal of the bind.
    bind: Box<Error>,
    /// Why taking the mount down again failed.
    unmount: Box<MountFault>,
  },
  /// The OS program that removes a mount (`umount`, `fusermount3 -u`) could not run, or refused: the path is still
  /// in use, or nothing is mounted there.
  Unmount {
    /// The mount point.
    path: String,
    /// The program's exit code, when it ran and exited.
    status: Option<i32>,
    /// The OS error number of starting it, when it could not run.
    errno: Option<i32>,
  },
  /// No volume that reported its mounts is mounted at the path (Windows, where a drive is the daemon's own and is
  /// removed by detaching its attachment): the volumes that could not report theirs, by name and refusal.
  NotMounted {
    /// The path asked.
    path: String,
    /// Each volume whose status was refused, as `name (id) refused <refusal>`.
    unreported: Vec<String>,
  },
}

impl fmt::Display for MountFault {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::MountPoint { path, errno } => {
        write!(f, "the mount point {path}: {}", errno_text(*errno))
      }
      Self::Unsupported { reason } => write!(f, "cannot mount here: {reason}"),
      Self::NoNfsListener => f.write_str(
        "the daemon is not serving NFS (its loopback listener did not bind); cannot mount",
      ),
      Self::NoCapability => f.write_str(
        "the daemon issued no mount capability for this attachment; the volume cannot be mounted",
      ),
      Self::RootHandle { errno, status } => match (errno, status) {
        (_, Some(status)) => write!(
          f,
          "the daemon's MOUNT service refused the export (mountstat3 {status})"
        ),
        (errno, None) => write!(f, "the daemon's MOUNT service: {}", errno_text(*errno)),
      },
      Self::Refused { path, errno } => {
        write!(f, "mounting at {path}: {}", errno_text(*errno))
      }
      Self::Unbound { bind, unmount } => write!(
        f,
        "the mount's record could not name it ({bind}), and unmounting it again failed ({unmount})"
      ),
      Self::Unmount {
        path,
        status,
        errno,
      } => match (status, errno) {
        (Some(status), _) => write!(
          f,
          "unmounting {path} failed (exit {status}); is the path still in use?"
        ),
        (None, errno) => write!(
          f,
          "unmounting {path}: the unmount program could not run: {}",
          errno_text(*errno)
        ),
      },
      Self::NotMounted { path, unreported } if unreported.is_empty() => {
        write!(f, "no slates volume is mounted at {path}")
      }
      Self::NotMounted { path, unreported } => write!(
        f,
        "no slates volume that reported its mounts is mounted at {path}; {} did not report: {}",
        unreported.len(),
        unreported.join("; ")
      ),
    }
  }
}

/// Why this process cannot issue a grant (§4.13 "Grants"): the authority is the anchor's segment, which only the
/// anchor's own tree and, on macOS and Windows, a process given its name hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssuerFault {
  /// No anchor handoff is in this process's environment (or in the pairs given): the process was not started from
  /// the anchor's session, and the MCP server and the SDKs never are.
  NoHandoff,
  /// The handoff is incomplete or malformed: `SLATES_ANCHOR_LEN` missing, or not a number.
  Malformed {
    /// What is wrong.
    reason: &'static str,
  },
  /// The segment the handoff names could not be attached or read (gone, another machine's, another layout).
  Attach(slates_anchor::AnchorError),
  /// The daemon has not published an issuer secret yet (an all-zero secret is no authority): it is still starting.
  NotPublished,
}

impl fmt::Display for IssuerFault {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::NoHandoff => f.write_str(
        "no anchor in this environment: grants, enrollments, revocations and recoveries are the anchor user's \
         surface; run them from the session that started the daemon, with the variables the anchor printed exported \
         (the MCP server and the SDKs cannot issue them)",
      ),
      Self::Malformed { reason } => write!(f, "the anchor's handoff is malformed: {reason}"),
      Self::Attach(error) => write!(f, "attaching the anchor: {error}"),
      Self::NotPublished => f.write_str("the daemon has published no issuer secret yet"),
    }
  }
}

/// An OS error number as a person reads it.
fn errno_text(errno: Option<i32>) -> String {
  match errno {
    Some(errno) => std::io::Error::from_raw_os_error(errno).to_string(),
    None => "the OS gave no error number".to_owned(),
  }
}
