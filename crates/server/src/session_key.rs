//! A client's session key: what proves a client resuming its session under its id is the client the id was issued to
//! (§4.9 "a session outlives a daemon restart"; GAP-A9-9's threat model, a same-uid stranger).
//!
//! **Why.** A client's id names its seat: the completion records retained for it until acknowledged (§4.7), its
//! attachments, and every verb after. A client resumes under its id after a daemon restart (`Client::resume`), and the
//! rendezvous carries only the id it wants, which a stranger can name as well as its owner: they are small integers,
//! and on macOS and Windows the claim slot holding them is in an object any same-uid process maps. Before this the
//! daemon granted a wanted id to whoever named it first, so a stranger resuming under a dead client's id was served its
//! seat and read its completion records (`a_stranger_with_a_dead_clients_id_gets_neither_its_seat_nor_its_records`:
//! the victim's create answered the stranger with the victim's volume).
//!
//! **The rule.** Each client id has a 256-bit key, keyed BLAKE3 over the id under the node's session secret ([`key_of`]).
//! The secret is made by the first daemon under an anchor and kept in the anchor's supervision block
//! (`layout::SUP_SESSION_SECRET`), so it survives a daemon restart as the seats do; it is never sent. The key reaches
//! only the client, in its own region (`ClientRegion::session_key`), which only it is handed: the daemon writes it
//! there for an id it issues fresh, and a client resuming an id writes its own key there before its first request. The
//! shard that serves the client checks it once, before the first request ([`proves`], constant time), and refuses a
//! slot that does not prove its id `SessionUnproven`, with no completion recorded under the id and none of the id's
//! attachments touched; the id is given back for its owner to resume.
//!
//! **What it does not do.** A stranger that claims a dead id and sends nothing holds the slot until the reaper finds
//! its process gone, delaying the owner's resume; it reads nothing and changes nothing. Where a same-uid stranger can
//! map the anchor segment by name (macOS, the exposure recorded in GAPS for Ada), it can read the secret as it can read
//! every other byte of the daemon's memory there.

use slates_anchor::AnchorSegment;
use slates_anchor::layout::SESSION_SECRET_BYTES;

use crate::error::ServerError;

/// Format: the domain a session key is derived in, so no other keyed hash under the secret can equal one.
const CONTEXT: &[u8] = b"slates/client-session/v1";

/// Format: a client session key's width, a BLAKE3 output (256 bits).
pub const SESSION_KEY_BYTES: usize = slates_ipc::region::SESSION_KEY_BYTES;

/// Publishes a fresh session secret into `segment` unless a daemon before this one under its anchor already did.
pub fn adopt_or_mint(segment: &mut AnchorSegment) -> Result<(), ServerError> {
  if segment.session_secret()?.is_some() {
    return Ok(());
  }
  let mut secret = [0u8; SESSION_SECRET_BYTES];
  slates_transport::handshake::secure_random(&mut secret)
    .map_err(|e| ServerError::SessionSecret(e.to_string()))?;
  segment.publish_session_secret(&secret)?;
  secret.fill(0);
  Ok(())
}

/// The session secret `segment` holds; refused when none was published, which [`adopt_or_mint`] at start rules out.
pub fn secret_of(segment: &AnchorSegment) -> Result<[u8; SESSION_SECRET_BYTES], ServerError> {
  segment.session_secret()?.ok_or_else(|| {
    ServerError::SessionSecret("no session secret is published in the segment".to_owned())
  })
}

/// Client `client_id`'s session key under `secret`.
pub fn key_of(secret: &[u8; SESSION_SECRET_BYTES], client_id: u32) -> [u8; SESSION_KEY_BYTES] {
  let mut hasher = blake3::Hasher::new_keyed(secret);
  hasher.update(CONTEXT);
  hasher.update(&client_id.to_le_bytes());
  *hasher.finalize().as_bytes()
}

/// Whether `presented` is client `client_id`'s session key, compared in constant time (BLAKE3's `Hash` equality).
pub fn proves(
  secret: &[u8; SESSION_SECRET_BYTES],
  client_id: u32,
  presented: &[u8; SESSION_KEY_BYTES],
) -> bool {
  blake3::Hash::from(key_of(secret, client_id)) == blake3::Hash::from(*presented)
}

#[cfg(test)]
mod tests {
  use super::*;

  /// GAP-A9-9 (the seat). Do: derive two ids' keys under one secret and one id's under another. Expect: each key proves
  /// its own id under its own secret only; an all-zero key proves nothing.
  #[test]
  fn a_session_key_proves_its_own_id_under_its_own_secret_only() {
    let (secret, other) = ([7u8; SESSION_SECRET_BYTES], [9u8; SESSION_SECRET_BYTES]);
    let key = key_of(&secret, 3);
    assert!(proves(&secret, 3, &key));
    assert!(!proves(&secret, 4, &key));
    assert!(!proves(&other, 3, &key));
    assert!(!proves(&secret, 3, &[0u8; SESSION_KEY_BYTES]));
  }
}
