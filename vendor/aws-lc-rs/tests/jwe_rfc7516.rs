// mantle: RFC 7516's JWE example A.3, decrypted and re-encrypted with the vendored crate's
// AES key wrap and AES_128_CBC_HMAC_SHA_256 (aws-lc-rs#617; vendor/UPSTREAM.md).

use aws_lc_rs::aead::cbc_hmac::{Key, AES_128_CBC_HMAC_SHA_256, IV_LEN};
use aws_lc_rs::key_wrap::{AesKek, KeyWrap, AES_128};

/// RFC 7516 A.3.7's compact serialization: the protected header, the encrypted key, the IV,
/// the ciphertext and the tag, each BASE64URL-encoded, joined by `.`.
const JWE: &str = "eyJhbGciOiJBMTI4S1ciLCJlbmMiOiJBMTI4Q0JDLUhTMjU2In0.\
    6KB707dM9YTIgHtLvtgWQ8mKwboJW3of9locizkDTHzBC2IlrT1oOQ.\
    AxY8DCtDaGlsbGljb3RoZQ.\
    KDlTtXchhZTGufMYmOYGS4HffxPSUrfmqCHXaI9wOGY.\
    U0m_YmjN04DJvceFICbCVQ";

/// A.3.3's key-encryption key, the JWK `{"kty":"oct","k":"GawgguFyGrWKav7AX4VKUg"}`.
const KEK: &str = "GawgguFyGrWKav7AX4VKUg";

/// BASE64URL without padding (RFC 7515 §2).
fn base64url(text: &str) -> Vec<u8> {
    let value = |c: u8| -> u32 {
        u32::from(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => panic!("not base64url: {c}"),
        })
    };
    let mut out = Vec::new();
    for chunk in text.as_bytes().chunks(4) {
        let bits =
            chunk.iter().fold(0u32, |acc, &c| (acc << 6) | value(c)) << (6 * (4 - chunk.len()));
        out.extend_from_slice(&bits.to_be_bytes()[1..chunk.len()]);
    }
    out
}

#[test]
fn rfc_7516_a3_decrypts_and_reencrypts_byte_for_byte() {
    let parts: Vec<&str> = JWE.split('.').collect();
    assert_eq!(parts.len(), 5);
    assert_eq!(
        base64url(parts[0]),
        br#"{"alg":"A128KW","enc":"A128CBC-HS256"}"#
    );
    let encrypted_key = base64url(parts[1]);
    let iv: [u8; IV_LEN] = base64url(parts[2]).try_into().unwrap();
    let ciphertext = base64url(parts[3]);
    let tag = base64url(parts[4]);
    // A.3.5: the additional data is the protected header as sent, ASCII(BASE64URL(...)).
    let aad = parts[0].as_bytes();

    // A128KW unwraps the content-encryption key (A.3.3).
    let mut cek = [0u8; 32];
    let cek = AesKek::new(&AES_128, &base64url(KEK))
        .unwrap()
        .unwrap(&encrypted_key, &mut cek)
        .unwrap()
        .to_vec();

    // A128CBC-HS256 opens the content (A.3.6).
    let key = Key::new(&AES_128_CBC_HMAC_SHA_256, &cek).unwrap();
    let mut in_out = ciphertext.clone();
    let plaintext = key.open_in_place(&iv, aad, &mut in_out, &tag).unwrap();
    assert_eq!(plaintext, b"Live long and prosper.");

    // Both steps are deterministic, so encrypting the same inputs gives the same JWE (A.3.8).
    let mut sealed = b"Live long and prosper.".to_vec();
    let resealed_tag = key.less_safe_seal_in_place(iv, aad, &mut sealed).unwrap();
    assert_eq!(sealed, ciphertext);
    assert_eq!(resealed_tag.as_ref(), tag.as_slice());
    let mut rewrapped = [0u8; 40];
    let rewrapped = AesKek::new(&AES_128, &base64url(KEK))
        .unwrap()
        .wrap(&cek, &mut rewrapped)
        .unwrap();
    assert_eq!(rewrapped, encrypted_key.as_slice());
}

#[test]
fn a_jwe_with_a_changed_header_does_not_open() {
    let parts: Vec<&str> = JWE.split('.').collect();
    let mut cek = [0u8; 32];
    let cek = AesKek::new(&AES_128, &base64url(KEK))
        .unwrap()
        .unwrap(&base64url(parts[1]), &mut cek)
        .unwrap()
        .to_vec();
    let key = Key::new(&AES_128_CBC_HMAC_SHA_256, &cek).unwrap();
    let iv: [u8; IV_LEN] = base64url(parts[2]).try_into().unwrap();
    // The header names A256KW instead: authenticated as additional data, it no longer matches.
    let changed = parts[0].replace("BMTI4S1", "BMjU2S1");
    let mut in_out = base64url(parts[3]);
    assert!(key
        .open_in_place(&iv, changed.as_bytes(), &mut in_out, &base64url(parts[4]))
        .is_err());
}
