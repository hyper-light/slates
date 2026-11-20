//! The listeners an anchor shares with each daemon it spawns on Windows (A-748; A-625 part 3's decision 3 amended,
//! §4.6): the bytes of the pipe that carries them, pure and cfg-free so the format is tested on every host.
//!
//! On Unix a held listener crosses a spawn as an inherited descriptor at a number the environment names. Windows
//! inherits no socket (A-183), so there the anchor holds each listener for its whole life and, once a daemon is
//! spawned, duplicates it for that daemon's process alone (`WSADuplicateSocketW`, `slates_rt::tcp::share_listener`),
//! an explicit duplicate never an inherited handle. Each duplicate is a protocol record the daemon makes into its own
//! socket (`slates_rt::tcp::TcpListener::from_share`). The records travel down an anonymous pipe whose reading end is
//! the one handle of the set handed to the daemon by the spawn's explicit handle list, named by [`ENV_SHARED_LISTENERS`];
//! the anchor writes them after the spawn, when the daemon's process id is known, and closes its end, so the daemon's
//! read ends at the pipe's end. The kernel keeps the listening socket, and the connections queued on it, across a
//! restart, as on Unix: a client's URL survives the daemon.
//!
//! A record is the environment variable the listener answers to (`ENV_VCS_HTTP`, `ENV_VCS_SSH`, `ENV_VCS_SVN`), one
//! byte of its length and its bytes, then the share's length in two little-endian bytes and its bytes. The pipe's bytes
//! cross a process boundary, so a reader checks every length against what is left and refuses a pipe that ends inside
//! a record or holds more records than it allows.

/// Format: the environment variable naming the reading end of the pipe the shares arrive on, its handle value in
/// decimal (Windows only).
pub const ENV_SHARED_LISTENERS: &str = "SLATES_ANCHOR_SHARED_LISTENERS";

/// Why a pipe's bytes are not a list of shares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SharesRefusal {
  /// A name or a share longer than its length field can say.
  TooLong,
  /// The bytes end inside a record.
  Truncated,
  /// A name that is not UTF-8.
  Name,
  /// More records than the reader allows.
  TooMany,
}

/// One listener's record: the variable it answers to and its share's bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Share {
  /// The environment variable the listener answers to.
  pub variable: String,
  /// The share's bytes (`slates_rt::tcp::share_listener`).
  pub bytes: Vec<u8>,
}

/// The pipe's bytes for `shares`.
///
/// # Errors
/// [`SharesRefusal::TooLong`] for a name past 255 bytes or a share past 65,535.
pub fn encode(shares: &[Share]) -> Result<Vec<u8>, SharesRefusal> {
  let mut bytes = Vec::new();
  for share in shares {
    let name = u8::try_from(share.variable.len()).map_err(|_| SharesRefusal::TooLong)?;
    let len = u16::try_from(share.bytes.len()).map_err(|_| SharesRefusal::TooLong)?;
    bytes.push(name);
    bytes.extend_from_slice(share.variable.as_bytes());
    bytes.extend_from_slice(&len.to_le_bytes());
    bytes.extend_from_slice(&share.bytes);
  }
  Ok(bytes)
}

/// The shares the pipe's `bytes` hold, at most `most`.
///
/// # Errors
/// [`SharesRefusal`] for bytes that end inside a record, a name that is not UTF-8, or more than `most` records.
pub fn decode(bytes: &[u8], most: usize) -> Result<Vec<Share>, SharesRefusal> {
  let mut rest = bytes;
  let mut shares = Vec::new();
  while let Some((&name_len, after)) = rest.split_first() {
    if shares.len() >= most {
      return Err(SharesRefusal::TooMany);
    }
    let (name, after) = after
      .split_at_checked(usize::from(name_len))
      .ok_or(SharesRefusal::Truncated)?;
    let (len, after) = after
      .split_first_chunk::<2>()
      .ok_or(SharesRefusal::Truncated)?;
    let (share, after) = after
      .split_at_checked(usize::from(u16::from_le_bytes(*len)))
      .ok_or(SharesRefusal::Truncated)?;
    shares.push(Share {
      variable: String::from_utf8(name.to_vec()).map_err(|_| SharesRefusal::Name)?,
      bytes: share.to_vec(),
    });
    rest = after;
  }
  Ok(shares)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn share(variable: &str, bytes: &[u8]) -> Share {
    Share {
      variable: variable.to_owned(),
      bytes: bytes.to_vec(),
    }
  }

  /// A-748. Do: encode three shares, decode them; decode every truncation of the bytes; decode with room for two;
  /// encode a share past its length field. Expect: the three back in order; every strict prefix that ends inside a
  /// record refused `Truncated`; room for two refused `TooMany`; the long share refused `TooLong`.
  #[test]
  fn shares_cross_the_pipe_whole_or_are_refused() {
    let shares = vec![
      share(crate::ENV_VCS_HTTP, &[1; 628]),
      share(crate::ENV_VCS_SSH, &[2; 628]),
      share(crate::ENV_VCS_SVN, &[]),
    ];
    let bytes = encode(&shares).unwrap();
    assert_eq!(decode(&bytes, 3).unwrap(), shares);
    // Where each record ends: a cut there is a shorter list, not a truncation.
    let mut ends = Vec::new();
    let mut at = 0;
    for share in &shares {
      at += 1 + share.variable.len() + 2 + share.bytes.len();
      ends.push(at);
    }
    for cut in 1..bytes.len() {
      if ends.contains(&cut) {
        assert_eq!(
          decode(&bytes[..cut], 3).unwrap().len(),
          ends.iter().position(|end| *end == cut).unwrap() + 1
        );
      } else {
        assert_eq!(
          decode(&bytes[..cut], 3),
          Err(SharesRefusal::Truncated),
          "cut at {cut}"
        );
      }
    }
    assert_eq!(decode(&bytes, 2), Err(SharesRefusal::TooMany));
    assert_eq!(decode(&[], 0), Ok(Vec::new()));
    assert_eq!(
      encode(&[share("x", &vec![0; 65_536])]),
      Err(SharesRefusal::TooLong)
    );
  }
}
