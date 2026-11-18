//! The launcher's own authority (§4.12 "The consumer contract", §4.13 "Grants"; A-299, agreed with A-285's owner on
//! 2026-11-18): a secret the host that starts an instance provides at every start and keeps, so its authority — approving
//! what an ask mode holds, setting write policies, enrolling and revoking consumers — outlives a daemon's restart and its
//! own.
//!
//! **Why the launcher provides it.** The issuer secret the daemon mints at every start lives in the anchor's segment,
//! which on Linux is a memfd only the anchor's children hold. Handing the starter that descriptor would make its
//! authority die with its process: kiln runs its instance in a systemd scope so the instance outlives kiln, and after
//! kiln's restart (an upgrade) no approval or policy could be proven without restarting the instance, which loses every
//! volume (they live in its memory, R1). The issuer secret cannot simply be kept instead: the client regions' names and
//! the boot incarnation derive from it, so it must stay fresh at every boot (A-176). So the launcher holds a secret of
//! its own ([`LauncherSecret`]), the anchor publishes it into its segment's supervision block before its first daemon
//! (`AnchorSegment::publish_launcher_secret`), and the daemon verifies a launcher-class proof under it as under the
//! issuer secret (A-285's owner's piece). [`crate::Issuer::from_launcher_secret`] makes the proofs.
//!
//! **How it reaches the anchor.** On the anchor's standard input, as one record written into a fresh pipe whose write
//! end is closed before the spawn ([`crate::StartSpec::anchor_command`]), so the anchor's read always ends: the record,
//! then the end of the pipe, never a wait on a writer. An environment variable would show the secret to every process of
//! the user (`ps -E`, `/proc/<pid>/environ`), which is why `slates_ipc::delivery` delivers a consumer's capability on an
//! inherited descriptor too; standard input is that descriptor, and it passes unchanged through a wrapper that execs the
//! command (`systemd-run --scope`). The anchor reads the record only when [`ENV_LAUNCHER`] says one is there, since an
//! anchor run from a terminal has the terminal as its standard input.
//!
//! **The record** ([`record_of`], [`secret_of_record`]): a magic word, the secret, and the CRC32C of both, 40 bytes; the
//! anchor reads at most one byte past it, refuses anything else by its typed [`LauncherFault`], and refuses an all-zero
//! secret, which is no authority.

use std::fmt;

use crate::error::{Error, Unsupported};

/// Format: the environment variable that tells the anchor its standard input carries the launcher's record, and its one
/// value. It carries no secret.
pub const ENV_LAUNCHER: &str = "SLATES_LAUNCHER";
/// Format: [`ENV_LAUNCHER`]'s value.
pub const LAUNCHER_ON_STDIN: &str = "stdin";

/// Format: the secret's width, the anchor segment's `LAUNCHER_SECRET_BYTES` (a BLAKE3 key, the width of the issuer
/// secret's, so the same proof formulas take either).
pub const LAUNCHER_SECRET_BYTES: usize = slates_anchor::layout::LAUNCHER_SECRET_BYTES;
/// Format: the record's magic, `SLLA` in little-endian ASCII ("slates launcher").
const MAGIC: u32 = 0x414C_4C53;
/// Format: the magic's width.
const MAGIC_BYTES: usize = size_of::<u32>();
/// Format: the checksum's width.
const CHECKSUM_BYTES: usize = size_of::<u32>();
/// Format: the record: the magic (4), the secret (32), the CRC32C of the 36 bytes before it (4).
pub const RECORD_BYTES: usize = MAGIC_BYTES + LAUNCHER_SECRET_BYTES + CHECKSUM_BYTES;

/// The launcher's secret (see the module doc). The host keeps it where it keeps its other secrets ([`Self::bytes`],
/// [`Self::from_bytes`]) and provides it at every start of its instance. Its `Debug` leaves the bytes out.
#[derive(Clone, PartialEq, Eq)]
pub struct LauncherSecret([u8; LAUNCHER_SECRET_BYTES]);

impl LauncherSecret {
  /// A fresh secret from the TLS provider's secure random (AWS-LC, A-66), as the daemon mints its own. Refused
  /// `Unsupported(SecureRandom)` when the provider cannot give one.
  pub fn generate() -> Result<LauncherSecret, Error> {
    let mut secret = [0u8; LAUNCHER_SECRET_BYTES];
    slates_transport::handshake::secure_random(&mut secret)
      .map_err(|_| Error::Unsupported(Unsupported::SecureRandom))?;
    Ok(LauncherSecret(secret))
  }

  /// The secret a host kept. Refused [`LauncherFault::Zero`] for all-zero bytes, which are no authority.
  pub fn from_bytes(bytes: [u8; LAUNCHER_SECRET_BYTES]) -> Result<LauncherSecret, LauncherFault> {
    if bytes.iter().all(|byte| *byte == 0) {
      return Err(LauncherFault::Zero);
    }
    Ok(LauncherSecret(bytes))
  }

  /// The secret's bytes, for the host to keep.
  pub fn bytes(&self) -> &[u8; LAUNCHER_SECRET_BYTES] {
    &self.0
  }
}

impl fmt::Debug for LauncherSecret {
  /// A secret is never rendered.
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str("LauncherSecret(..)")
  }
}

/// Why a launcher record or secret was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LauncherFault {
  /// The record is not [`RECORD_BYTES`] long: cut short, or followed by more.
  Length,
  /// The record does not open with the launcher's magic.
  Magic,
  /// The record's checksum does not match its bytes.
  Checksum,
  /// The secret is all zero, which is no authority.
  Zero,
  /// [`ENV_LAUNCHER`] holds a value other than [`LAUNCHER_ON_STDIN`].
  Marker,
  /// The anchor's standard input could not be read; the OS error number, when it gave one.
  Read {
    /// The OS error number.
    errno: Option<i32>,
  },
}

impl fmt::Display for LauncherFault {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Length => write!(f, "the launcher's record is not {RECORD_BYTES} bytes long"),
      Self::Magic => f.write_str("the launcher's record does not open with its magic"),
      Self::Checksum => f.write_str("the launcher's record's checksum does not match"),
      Self::Zero => f.write_str("the launcher's secret is all zero, which is no authority"),
      Self::Marker => write!(f, "{ENV_LAUNCHER} must be {LAUNCHER_ON_STDIN:?} or absent"),
      Self::Read { errno } => write!(f, "reading the launcher's record: errno {errno:?}"),
    }
  }
}

/// The record that carries `secret` to the anchor.
pub(crate) fn record_of(secret: &LauncherSecret) -> [u8; RECORD_BYTES] {
  let mut record = [0u8; RECORD_BYTES];
  let (head, checksum) = record.split_at_mut(MAGIC_BYTES + LAUNCHER_SECRET_BYTES);
  let (magic, bytes) = head.split_at_mut(MAGIC_BYTES);
  magic.copy_from_slice(&MAGIC.to_le_bytes());
  bytes.copy_from_slice(secret.bytes());
  checksum.copy_from_slice(&slates_wire::crc32c::crc32c(head).to_le_bytes());
  record
}

/// The secret a record carries: its length, magic and checksum checked before a byte of the secret is taken.
pub(crate) fn secret_of_record(record: &[u8]) -> Result<LauncherSecret, LauncherFault> {
  if record.len() != RECORD_BYTES {
    return Err(LauncherFault::Length);
  }
  let (head, checksum) = record.split_at(MAGIC_BYTES + LAUNCHER_SECRET_BYTES);
  let (magic, bytes) = head.split_at(MAGIC_BYTES);
  if magic != MAGIC.to_le_bytes() {
    return Err(LauncherFault::Magic);
  }
  if checksum != slates_wire::crc32c::crc32c(head).to_le_bytes() {
    return Err(LauncherFault::Checksum);
  }
  let secret: [u8; LAUNCHER_SECRET_BYTES] = bytes.try_into().map_err(|_| LauncherFault::Length)?;
  LauncherSecret::from_bytes(secret)
}

/// The anchor's side (A-299): the launcher's secret from standard input when [`ENV_LAUNCHER`] says one is there, read
/// to its end and at most one byte past the record, or none when the variable is absent. Refused by its fault otherwise:
/// an anchor whose starter meant to give it authority and failed must not run without it.
pub(crate) fn from_standard_input() -> Result<Option<LauncherSecret>, LauncherFault> {
  use std::io::Read;
  let Some(marker) = std::env::var_os(ENV_LAUNCHER) else {
    return Ok(None);
  };
  if marker != LAUNCHER_ON_STDIN {
    return Err(LauncherFault::Marker);
  }
  let bound = u64::try_from(RECORD_BYTES)
    .map_err(|_| LauncherFault::Length)?
    .saturating_add(1);
  let mut record = Vec::with_capacity(RECORD_BYTES.saturating_add(1));
  std::io::stdin()
    .lock()
    .take(bound)
    .read_to_end(&mut record)
    .map_err(|error| LauncherFault::Read {
      errno: error.raw_os_error(),
    })?;
  let secret = secret_of_record(&record);
  record.fill(0);
  secret.map(Some)
}

#[cfg(test)]
mod tests {
  use super::{LauncherFault, LauncherSecret, RECORD_BYTES, record_of, secret_of_record};

  /// A-299: do carry a secret in its record and read it back, then read the record cut short, followed by a byte, with
  /// its magic, a secret byte and its checksum each flipped, and a record of an all-zero secret; expect the secret, then
  /// each refusal by its own fault — a record is taken only whole and as written.
  #[test]
  fn a_launcher_record_is_read_only_whole_and_as_written() {
    let secret = LauncherSecret::from_bytes([3; 32]).unwrap();
    let record = record_of(&secret);
    assert_eq!(secret_of_record(&record), Ok(secret.clone()));
    assert_eq!(
      secret_of_record(&record[..RECORD_BYTES - 1]),
      Err(LauncherFault::Length)
    );
    let mut longer = record.to_vec();
    longer.push(0);
    assert_eq!(secret_of_record(&longer), Err(LauncherFault::Length));
    let flipped = |at: usize| {
      let mut bytes = record;
      bytes[at] ^= 1;
      secret_of_record(&bytes)
    };
    assert_eq!(flipped(0), Err(LauncherFault::Magic));
    assert_eq!(flipped(10), Err(LauncherFault::Checksum));
    assert_eq!(flipped(RECORD_BYTES - 1), Err(LauncherFault::Checksum));
    assert_eq!(
      LauncherSecret::from_bytes([0; 32]),
      Err(LauncherFault::Zero)
    );
    assert_eq!(
      format!("{secret:?}"),
      "LauncherSecret(..)",
      "never rendered"
    );
  }

  /// A-299: do generate two secrets; expect each usable (not all zero) and the two different.
  #[test]
  fn generated_secrets_are_usable_and_distinct() {
    let first = LauncherSecret::generate().unwrap();
    let second = LauncherSecret::generate().unwrap();
    assert!(LauncherSecret::from_bytes(*first.bytes()).is_ok());
    assert_ne!(first, second);
  }
}
