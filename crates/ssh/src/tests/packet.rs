//! The binary packet protocol (A-594; RFC 4253 §6; RFC 5647 §7; the chacha20-poly1305 draft's Appendix A).

use aws_lc_rs::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use proptest::prelude::*;

use crate::packet::{
  AES_BLOCK_LEN, CLEAR_ALIGNMENT, Cipher, LENGTH_LEN, MAX_PACKET_EVERY_PEER_ACCEPTS, MIN_PADDING,
  Opener, PacketRefusal, SealRefusal, Sealer, TAG_LEN, Usage,
};
use crate::support::{Scripted, Seeded, hex};

/// The draft's client-to-server key material (Figure 5).
const DRAFT_KEY: &str = "8bbff6855fc102338c373e73aac0c914f076a905b2444a32eecaffeae22becc5
                         e9b7a7a5825a8249346ec1c28301cf394543fc7569887d76e168f37562ac0740";

/// The draft's packet on the wire (Figure 18).
const DRAFT_WIRE: &str = "2c3ecce4a5bc05895bf07a7ba956b6c68829ac7c83b780b7000ecde745afc705
                          bbc378ce03a280236b87b53bed5839662302b164b6286a48cd1e097138e3cb90
                          9b8b2b829dd18d2a35ff82d995349e855bf02c298ef775f2d1a7e8b8";

/// The draft's payload: `SSH2_MSG_CHANNEL_DATA`, channel 0, "Lorem ipsum dolor sit amet, consectetur adipisicing elit"
/// (Figure 2).
fn draft_payload() -> Vec<u8> {
  let text = b"Lorem ipsum dolor sit amet, consectetur adipisicing elit";
  let mut payload = vec![0x5e, 0, 0, 0, 0];
  payload.extend_from_slice(&(text.len() as u32).to_be_bytes());
  payload.extend_from_slice(text);
  payload
}

/// Opens `wire` as one whole packet with `opener`, framing it from its first four bytes first.
fn open_wire(opener: &mut Opener, wire: &[u8], cap: usize) -> Result<Vec<u8>, PacketRefusal> {
  let have = wire.len() as u64;
  let incomplete = PacketRefusal::Incomplete {
    have,
    need: LENGTH_LEN as u64,
  };
  let head: [u8; LENGTH_LEN] = wire
    .get(..LENGTH_LEN)
    .and_then(|head| head.try_into().ok())
    .ok_or(incomplete)?;
  let size = opener.frame(head, cap)?;
  if size != wire.len() {
    return Err(PacketRefusal::Incomplete {
      have,
      need: size as u64,
    });
  }
  let mut packet = wire.to_vec();
  let opened = opener.open(&mut packet)?;
  packet
    .get(opened.payload)
    .map(<[u8]>::to_vec)
    .ok_or(incomplete)
}

/// A sealer and an opener under `cipher` (none in the clear) with fixed key material, and the cipher's alignment and
/// tag length.
fn pair(cipher: Option<Cipher>) -> (Sealer, Opener, usize, usize) {
  match cipher {
    None => (Sealer::clear(), Opener::clear(), CLEAR_ALIGNMENT, 0),
    Some(cipher) => {
      let key = vec![0x42; cipher.key_len()];
      let iv = vec![0x24; cipher.iv_len()];
      let alignment = if cipher == Cipher::Aes256Gcm {
        AES_BLOCK_LEN
      } else {
        CLEAR_ALIGNMENT
      };
      let sealer = Sealer::new(cipher, &key, &iv).unwrap();
      (
        sealer,
        Opener::new(cipher, &key, &iv).unwrap(),
        alignment,
        TAG_LEN,
      )
    }
  }
}

/// A-594 golden (`draft-ietf-sshm-chacha20-poly1305-04` Appendix A): do seal the draft's packet under its key at its
/// sequence number 7 with its six padding bytes, then open the draft's wire bytes the same way. Expect Figure 18's 92
/// bytes exactly, and the draft's payload back.
#[test]
fn chacha_seals_and_opens_the_drafts_worked_packet() {
  let key = hex(DRAFT_KEY);
  let mut sealer = Sealer::new(Cipher::ChaCha20Poly1305, &key, &[]).unwrap();
  sealer.set_sequence(7);
  let mut padding = Scripted::new(&hex("4e43e804dc6c"));
  let mut out = Vec::new();
  sealer
    .seal(&draft_payload(), &mut padding, &mut out)
    .unwrap();
  assert_eq!(out, hex(DRAFT_WIRE));
  assert_eq!(sealer.sequence(), 8);
  assert_eq!(
    sealer.usage(),
    Usage {
      packets: 1,
      blocks: 0
    }
  );
  let mut opener = Opener::new(Cipher::ChaCha20Poly1305, &key, &[]).unwrap();
  opener.set_sequence(7);
  assert_eq!(
    open_wire(&mut opener, &hex(DRAFT_WIRE), MAX_PACKET_EVERY_PEER_ACCEPTS),
    Ok(draft_payload())
  );
  assert_eq!(opener.sequence(), 8);
}

/// A-594 hostile: do open the draft's packet with one bit flipped in its tag, in its body, under the wrong sequence
/// number, and framed under a cap one byte short. Expect a tag mismatch for the first three, refused before decrypting,
/// and the cap refused from the first four bytes.
#[test]
fn chacha_refuses_a_forged_or_misplaced_packet() {
  let key = hex(DRAFT_KEY);
  let wire = hex(DRAFT_WIRE);
  for flip in [wire.len() - 1, 20] {
    let mut forged = wire.clone();
    forged[flip] ^= 0x01;
    let mut opener = Opener::new(Cipher::ChaCha20Poly1305, &key, &[]).unwrap();
    opener.set_sequence(7);
    assert_eq!(
      open_wire(&mut opener, &forged, MAX_PACKET_EVERY_PEER_ACCEPTS),
      Err(PacketRefusal::TagMismatch)
    );
  }
  let mut opener = Opener::new(Cipher::ChaCha20Poly1305, &key, &[]).unwrap();
  opener.set_sequence(7);
  let head: [u8; 4] = wire[..4].try_into().unwrap();
  assert_eq!(
    opener.frame(head, wire.len() - 1),
    Err(PacketRefusal::LengthPastCap { size: 92, cap: 91 })
  );
  let mut packet = wire.clone();
  opener.set_sequence(8);
  assert!(
    opener.open(&mut packet).is_err(),
    "another sequence number decrypts another length or fails the tag"
  );
}

/// A-594 oracle (RFC 5647 §7.1, §7.2): do seal three packets under AES-256-GCM with a derived IV, and seal the same
/// plaintexts by hand with AWS-LC: the nonce the IV's fixed field and its invocation counter plus 0, 1 and 2, the
/// length as the additional data. Expect the same bytes, the length in the clear, each packet opened back, and a flipped
/// tag or body refused.
#[test]
fn gcm_follows_rfc_5647s_nonce_and_additional_data() {
  let key = hex("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f");
  let iv = hex("a0a1a2a3 fffffffffffffffe");
  let mut sealer = Sealer::new(Cipher::Aes256Gcm, &key, &iv).unwrap();
  let mut opener = Opener::new(Cipher::Aes256Gcm, &key, &iv).unwrap();
  let by_hand = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &key).unwrap());
  let payloads: [&[u8]; 3] = [b"\x05\x00\x00\x00\x0cssh-userauth", b"\x5e", &[0x5e; 100]];
  for (index, payload) in payloads.iter().enumerate() {
    let mut entropy = Seeded::new(index as u64 + 1);
    let mut out = Vec::new();
    sealer.seal(payload, &mut entropy, &mut out).unwrap();
    let length = u32::from_be_bytes(out[..4].try_into().unwrap()) as usize;
    assert_eq!(
      length % AES_BLOCK_LEN,
      0,
      "the plaintext after the length is a multiple of 16"
    );
    assert_eq!(out.len(), LENGTH_LEN + length + TAG_LEN);
    let padding = (length - 1 - payload.len()) as u8;
    let mut plain = vec![padding];
    plain.extend_from_slice(payload);
    let mut padding_bytes = vec![0u8; padding as usize];
    crate::entropy::Entropy::fill(&mut Seeded::new(index as u64 + 1), &mut padding_bytes).unwrap();
    plain.extend_from_slice(&padding_bytes);
    let mut nonce = [0u8; 12];
    nonce[..4].copy_from_slice(&iv[..4]);
    nonce[4..].copy_from_slice(
      &0xffff_ffff_ffff_fffe_u64
        .wrapping_add(index as u64)
        .to_be_bytes(),
    );
    let tag = by_hand
      .seal_in_place_separate_tag(
        Nonce::assume_unique_for_key(nonce),
        Aad::from((length as u32).to_be_bytes()),
        &mut plain,
      )
      .unwrap();
    let mut expected = (length as u32).to_be_bytes().to_vec();
    expected.extend_from_slice(&plain);
    expected.extend_from_slice(tag.as_ref());
    assert_eq!(out, expected, "packet {index}");
    assert_eq!(
      open_wire(&mut opener, &out, MAX_PACKET_EVERY_PEER_ACCEPTS),
      Ok(payload.to_vec())
    );
  }
  assert_eq!(sealer.usage().packets, 3);
  assert_eq!(sealer.usage(), opener.usage());
  let mut out = Vec::new();
  Sealer::new(Cipher::Aes256Gcm, &key, &iv)
    .unwrap()
    .seal(b"\x5e", &mut Seeded::new(9), &mut out)
    .unwrap();
  for flip in [out.len() - 1, LENGTH_LEN + 1] {
    let mut forged = out.clone();
    forged[flip] ^= 0x80;
    let mut fresh = Opener::new(Cipher::Aes256Gcm, &key, &iv).unwrap();
    assert_eq!(
      open_wire(&mut fresh, &forged, MAX_PACKET_EVERY_PEER_ACCEPTS),
      Err(PacketRefusal::TagMismatch)
    );
  }
}

/// A-594: do seal and open packets in the clear (before the first NEWKEYS). Expect the whole packet, length included, a
/// multiple of 8 with at least four bytes of padding and no tag, and each payload back.
#[test]
fn clear_packets_align_their_length_too() {
  let mut sealer = Sealer::clear();
  let mut opener = Opener::clear();
  for payload_len in [1usize, 2, 3, 10, 11, 12, 255] {
    let payload = vec![0x14; payload_len];
    let mut out = Vec::new();
    sealer
      .seal(&payload, &mut Seeded::new(3), &mut out)
      .unwrap();
    assert_eq!(out.len() % CLEAR_ALIGNMENT, 0);
    assert!(out[4] as usize >= MIN_PADDING);
    assert_eq!(
      open_wire(&mut opener, &out, MAX_PACKET_EVERY_PEER_ACCEPTS),
      Ok(payload)
    );
  }
  assert_eq!(
    sealer.usage(),
    Usage {
      packets: 7,
      blocks: 0
    }
  );
}

/// A-594 hostile: do frame lengths a client could send in the clear: too short to hold a payload, unaligned, `u32::MAX`,
/// and an aligned length past the cap. Expect each refused by its kind from the first four bytes, so nothing past them
/// is read or held.
#[test]
fn bad_lengths_are_refused_from_the_first_four_bytes() {
  let opener = Opener::clear();
  let cap = MAX_PACKET_EVERY_PEER_ACCEPTS;
  assert_eq!(
    opener.frame(4u32.to_be_bytes(), cap),
    Err(PacketRefusal::LengthTooShort { length: 4 })
  );
  assert_eq!(
    opener.frame(13u32.to_be_bytes(), cap),
    Err(PacketRefusal::LengthUnaligned {
      length: 13,
      alignment: 8
    })
  );
  assert_eq!(
    opener.frame(u32::MAX.to_be_bytes(), cap),
    Err(PacketRefusal::LengthUnaligned {
      length: u32::MAX,
      alignment: 8
    })
  );
  assert_eq!(
    opener.frame(0xffff_fffc_u32.to_be_bytes(), cap),
    Err(PacketRefusal::LengthPastCap {
      size: 0x1_0000_0000,
      cap: cap as u64
    })
  );
  assert_eq!(opener.frame(12u32.to_be_bytes(), cap), Ok(16));
}

/// A-594 hostile: do open clear packets with three bytes of padding, with padding that leaves no payload, and a packet
/// shorter than its length says. Expect each refused by its kind.
#[test]
fn bad_padding_is_refused() {
  let mut opener = Opener::clear();
  let mut short = 12u32.to_be_bytes().to_vec();
  short.extend_from_slice(&[3, 0x14, 0x14, 0x14, 0x14, 0x14, 0x14, 0x14, 0, 0, 0]);
  short.push(0);
  assert_eq!(
    opener.open(&mut short),
    Err(PacketRefusal::PaddingTooShort { padding: 3 })
  );
  let mut empty = 12u32.to_be_bytes().to_vec();
  empty.extend_from_slice(&[11; 12]);
  assert_eq!(
    opener.open(&mut empty),
    Err(PacketRefusal::PaddingPastPacket {
      padding: 11,
      length: 12
    })
  );
  let mut cut = 12u32.to_be_bytes().to_vec();
  cut.extend_from_slice(&[4; 8]);
  assert_eq!(
    opener.open(&mut cut),
    Err(PacketRefusal::Incomplete { have: 12, need: 16 })
  );
}

/// A-594: do seal a payload too large for RFC 4253 §6.1's 35000-byte packet, and a sequence number at `u32::MAX`. Expect
/// the first refused with nothing written, and the second sealed with the count wrapped to zero (§6.4).
#[test]
fn oversized_payloads_are_refused_and_sequence_numbers_wrap() {
  let mut sealer = Sealer::clear();
  let mut out = Vec::new();
  assert_eq!(
    sealer.seal(
      &vec![0x5e; MAX_PACKET_EVERY_PEER_ACCEPTS],
      &mut Seeded::new(1),
      &mut out
    ),
    Err(SealRefusal::TooLarge {
      payload: MAX_PACKET_EVERY_PEER_ACCEPTS as u64,
      ceiling: MAX_PACKET_EVERY_PEER_ACCEPTS as u64
    })
  );
  assert!(out.is_empty());
  sealer.set_sequence(u32::MAX);
  sealer.seal(b"\x5e", &mut Seeded::new(1), &mut out).unwrap();
  assert_eq!(sealer.sequence(), 0);
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

  /// A-594: do seal any payload up to 34899 bytes under each cipher and in the clear, then open it. Expect a packet no
  /// larger than 35000 bytes, aligned to its cipher's block, with four to a block plus three bytes of padding, opened
  /// back to the payload, and each side's usage counted alike.
  #[test]
  fn any_payload_seals_aligned_and_opens_back(
    payload_len in 1usize..34_900,
    seed in any::<u64>(),
    selector in 0usize..3,
  ) {
    let cipher = [None, Some(Cipher::ChaCha20Poly1305), Some(Cipher::Aes256Gcm)][selector];
    let payload: Vec<u8> = (0..payload_len).map(|index| index as u8).collect();
    let (mut sealer, mut opener, alignment, tag) = pair(cipher);
    let mut out = Vec::new();
    prop_assert_eq!(sealer.seal(&payload, &mut Seeded::new(seed), &mut out), Ok(()));
    prop_assert!(out.len() <= MAX_PACKET_EVERY_PEER_ACCEPTS);
    let length = out.len() - LENGTH_LEN - tag;
    let aligned = if cipher.is_none() { length + LENGTH_LEN } else { length };
    prop_assert_eq!(aligned % alignment, 0);
    let padding = length - 1 - payload_len;
    prop_assert!((MIN_PADDING..alignment + MIN_PADDING).contains(&padding), "padding {}", padding);
    let opened = open_wire(&mut opener, &out, MAX_PACKET_EVERY_PEER_ACCEPTS);
    prop_assert_eq!(opened, Ok(payload));
    prop_assert_eq!(sealer.usage(), opener.usage());
  }
}
