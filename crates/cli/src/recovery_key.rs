//! Operator-provisioned quorum recovery authority (§4.8, AUD-07). The CLI reads an exact
//! 256-bit key from the named read-only secret mount on any platform. A configured but
//! unreadable or malformed key refuses; it never changes to another authority source.

use std::io::Read;

use crate::Failure;

/// Format: a BLAKE3 keyed-hash key is 256 bits.
const KEY_BYTES: usize = 32;

#[allow(clippy::disallowed_methods)] // Read-only operator input, like the fleet's certificate and key files.
pub(crate) fn load() -> Result<Option<slates_server::RecoveryKey>, Failure> {
  let Some(path) = std::env::var_os("SLATES_RECOVERY_KEY") else {
    return Ok(None);
  };
  let invalid = || {
    Failure::Refused(
      "SLATES_RECOVERY_KEY must name a readable, nonzero 32-byte node recovery key".to_owned(),
    )
  };
  let file = std::fs::File::open(path).map_err(|_| invalid())?;
  let mut bytes = Vec::with_capacity(KEY_BYTES + 1);
  file
    .take((KEY_BYTES + 1) as u64)
    .read_to_end(&mut bytes)
    .map_err(|_| invalid())?;
  let key = slates_server::RecoveryKey::from_bytes(bytes.try_into().map_err(|_| invalid())?)
    .ok_or_else(invalid)?;
  Ok(Some(key))
}
