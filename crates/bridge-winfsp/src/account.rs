//! The account a WinFsp drive is served for, as the owner and group every object on it reports (§4.6 "Windows", the
//! stated SECURITY difference; A-350).
//!
//! NTFS records a new object's owner and group from its maker's token, and Windows programs read them back: git refuses
//! a repository whose owner is neither the user nor, for an administrator, the Administrators group ("detected dubious
//! ownership", `compat/mingw.c` `is_path_owned_by_current_sid`). The drive keeps no descriptor per object, and every
//! maker on it is one account: a Windows daemon's clients are its own user, since its rendezvous section and events
//! admit that user's SID alone (§4.6 "Rendezvous") and the account is recorded as uid 0 (A-237). So the drive's one
//! descriptor names that account as owner and its primary group as group, which is what NTFS gives a file the account
//! makes without elevation, and keeps the access list the volume's rule needs ([`DACL_SDDL`]).
//!
//! The SIDs come from the daemon's token as bytes; their text is written here, by the rules of MS-DTYP 2.4.2, so the
//! parse is pure and its hostile inputs are tested on every host. The token read is Windows' (`crate::host`).

/// Format: the revision every SID carries (MS-DTYP 2.4.2.2, `SID_REVISION`).
const SID_REVISION: u8 = 1;
/// Format: the most sub-authorities a SID holds (MS-DTYP 2.4.2.2, `SID_MAX_SUB_AUTHORITIES`).
const SID_MAX_SUB_AUTHORITIES: usize = 15;
/// Format: a SID's bytes before its sub-authorities: the revision, the sub-authority count and the identifier
/// authority (MS-DTYP 2.4.2.2).
const SID_HEADER_BYTES: usize = 8;
/// Format: where the identifier authority starts, after the revision and the count.
const AUTHORITY_AT: usize = 2;
/// Format: the identifier authority's bytes, a big-endian 48-bit number (MS-DTYP 2.4.2.2).
const AUTHORITY_BYTES: usize = SID_HEADER_BYTES - AUTHORITY_AT;
/// Format: the zero bytes that widen the identifier authority to a big-endian `u64`.
const AUTHORITY_PAD: usize = size_of::<u64>() - AUTHORITY_BYTES;
/// Format: the bytes of one sub-authority, a little-endian 32-bit word (MS-DTYP 2.4.2.2).
const SUB_AUTHORITY_BYTES: usize = 4;
/// Format: identifier authorities below this print in decimal, larger ones as `0x` and twelve hexadecimal digits
/// (MS-DTYP 2.4.2.1).
const DECIMAL_AUTHORITY_LIMIT: u64 = 1 << 32;

/// Format: the drive's discretionary access list in SDDL: protected (`P`, nothing inherited) and one entry granting
/// full access (`FA`) to Everyone (`WD`). WinFsp checks every open against a descriptor, and the volume's own rights are
/// the attachment's, enforced by the shard on every request, so the list lets the shard decide; an access list set or
/// asked for at a create is not kept (§4.6, the stated SECURITY difference).
pub const DACL_SDDL: &str = "D:P(A;;FA;;;WD)";

/// The SDDL of the drive's one descriptor: `owner` and `group`, each a SID's text, and [`DACL_SDDL`].
pub fn drive_sddl(owner: &str, group: &str) -> String {
  format!("O:{owner}G:{group}{DACL_SDDL}")
}

/// A SID's text (`S-1-5-21-…`, MS-DTYP 2.4.2.1) from the bytes it starts (MS-DTYP 2.4.2.2); bytes after it are not
/// read. `None` for bytes that start no SID: another revision, more sub-authorities than a SID holds, or fewer bytes
/// than its count names.
pub fn sid_text(bytes: &[u8]) -> Option<String> {
  let (&revision, rest) = bytes.split_first()?;
  let (&count, _) = rest.split_first()?;
  let count = usize::from(count);
  if revision != SID_REVISION || count > SID_MAX_SUB_AUTHORITIES {
    return None;
  }
  let mut widened = [0u8; size_of::<u64>()];
  widened
    .get_mut(AUTHORITY_PAD..)?
    .copy_from_slice(bytes.get(AUTHORITY_AT..SID_HEADER_BYTES)?);
  let authority = u64::from_be_bytes(widened);
  let mut text = if authority < DECIMAL_AUTHORITY_LIMIT {
    format!("S-{revision}-{authority}")
  } else {
    format!("S-{revision}-0x{authority:012X}")
  };
  let end = count
    .checked_mul(SUB_AUTHORITY_BYTES)?
    .checked_add(SID_HEADER_BYTES)?;
  for word in bytes
    .get(SID_HEADER_BYTES..end)?
    .chunks_exact(SUB_AUTHORITY_BYTES)
  {
    let sub_authority = u32::from_le_bytes(word.try_into().ok()?);
    text.push_str(&format!("-{sub_authority}"));
  }
  Some(text)
}

/// The text of the SID a token-information buffer's first word points at: `TOKEN_USER`'s `User.Sid` and
/// `TOKEN_PRIMARY_GROUP`'s `PrimaryGroup`, which `GetTokenInformation` writes inside the same buffer, after the
/// structure. `None` when the pointer lies outside the buffer or no SID starts there.
pub fn token_sid_text(buffer: &[u64]) -> Option<String> {
  let address = usize::try_from(*buffer.first()?).ok()?;
  let offset = address.checked_sub(buffer.as_ptr().addr())?;
  let bytes: Vec<u8> = buffer.iter().flat_map(|word| word.to_ne_bytes()).collect();
  sid_text(bytes.get(offset..)?)
}

#[cfg(test)]
mod tests {
  use super::*;

  /// The bytes of a SID with identifier authority `authority` and sub-authorities `subs`, as Windows lays them out.
  fn sid_bytes(authority: u64, subs: &[u32]) -> Vec<u8> {
    let mut bytes = vec![SID_REVISION, u8::try_from(subs.len()).unwrap()];
    bytes.extend_from_slice(&authority.to_be_bytes()[2..]);
    for sub in subs {
      bytes.extend_from_slice(&sub.to_le_bytes());
    }
    bytes
  }

  /// Golden vectors (MS-DTYP 2.4.2). Do: read the bytes of the account CI's Windows runner ran as (git printed it in
  /// CI run 44271637767), of Everyone, of the Administrators group and of a SID whose authority needs the hexadecimal
  /// form, each followed by bytes that are not the SID's. Expect each text exactly as Windows writes it.
  #[test]
  fn a_sid_reads_as_windows_writes_it() {
    let runner = sid_bytes(5, &[21, 3_289_802_278, 1_561_746_829, 1_498_425_070, 500]);
    let mut trailing = runner.clone();
    trailing.extend_from_slice(&[0xff; 7]);
    assert_eq!(
      sid_text(&trailing).as_deref(),
      Some("S-1-5-21-3289802278-1561746829-1498425070-500")
    );
    assert_eq!(sid_text(&sid_bytes(1, &[0])).as_deref(), Some("S-1-1-0"));
    assert_eq!(
      sid_text(&sid_bytes(5, &[32, 544])).as_deref(),
      Some("S-1-5-32-544")
    );
    assert_eq!(
      sid_text(&sid_bytes(0x0001_0000_0000, &[7])).as_deref(),
      Some("S-1-0x000100000000-7")
    );
  }

  /// Hostile input (CLAUDE.md §4). Do: read nothing, a header cut short, a SID of another revision, one naming sixteen
  /// sub-authorities, and one whose last sub-authority is cut short. Expect no text for each.
  #[test]
  fn bytes_that_start_no_sid_have_no_text() {
    let good = sid_bytes(5, &[21, 1, 2, 3, 1001]);
    assert_eq!(sid_text(&[]), None);
    assert_eq!(sid_text(good.get(..5).unwrap()), None);
    let mut revision = good.clone();
    revision[0] = SID_REVISION + 1;
    assert_eq!(sid_text(&revision), None);
    let mut count = sid_bytes(5, &[1; SID_MAX_SUB_AUTHORITIES]);
    count[1] = u8::try_from(SID_MAX_SUB_AUTHORITIES + 1).unwrap();
    count.extend_from_slice(&[0; SUB_AUTHORITY_BYTES]);
    assert_eq!(sid_text(&count), None);
    assert_eq!(sid_text(good.get(..good.len() - 1).unwrap()), None);
  }

  /// The token read's layout (`GetTokenInformation`: the structure first, its SID after it in the same buffer). Do: lay
  /// out a buffer whose first word points at a SID two words in, then point that word before the buffer and past its
  /// end. Expect the SID's text, then no text twice.
  #[test]
  fn the_sid_a_token_buffer_points_into_is_read_and_one_outside_it_is_not() {
    let sid = sid_bytes(5, &[21, 10, 20, 30, 513]);
    let words = 2 + sid.len().div_ceil(size_of::<u64>());
    let mut buffer = vec![0u64; words];
    let mut bytes: Vec<u8> = vec![0; words * size_of::<u64>()];
    bytes[2 * size_of::<u64>()..][..sid.len()].copy_from_slice(&sid);
    for (word, chunk) in buffer.iter_mut().zip(bytes.chunks_exact(size_of::<u64>())) {
      *word = u64::from_ne_bytes(chunk.try_into().unwrap());
    }
    let base = u64::try_from(buffer.as_ptr().addr()).unwrap();
    buffer[0] = base + 2 * 8;
    assert_eq!(
      token_sid_text(&buffer).as_deref(),
      Some("S-1-5-21-10-20-30-513")
    );
    buffer[0] = base - 8;
    assert_eq!(token_sid_text(&buffer), None);
    buffer[0] = base + u64::try_from(words * 8).unwrap();
    assert_eq!(token_sid_text(&buffer), None);
  }
}
