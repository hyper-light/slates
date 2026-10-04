// mantle: AES-CBC with HMAC-SHA-2, the content encryption JWE uses (aws-lc-rs#617;
// vendor/UPSTREAM.md).

//! Authenticated encryption with AES-CBC and HMAC-SHA-2, as RFC 7518 §5.2 defines it for
//! JSON Web Encryption: `A128CBC-HS256`, `A192CBC-HS384` and `A256CBC-HS512`.
//!
//! The key's first half keys HMAC and its second half AES. Sealing pads the plaintext with
//! PKCS #7 and encrypts it with AES-CBC under a 128-bit IV. The tag is the HMAC of the
//! additional data, the IV, the ciphertext, and the additional data's length in bits as a
//! 64-bit big-endian number, cut to half the HMAC's output. Opening checks the tag, in
//! constant time, before it decrypts anything.
//!
//! CBC needs an IV that cannot be predicted, so [`Key::seal_in_place`] draws one at random.
//! [`Key::less_safe_seal_in_place`] takes the caller's, for known-answer tests.

use crate::cipher::{
    self, DecryptionContext, EncryptionContext, PaddedBlockDecryptingKey, PaddedBlockEncryptingKey,
    UnboundCipherKey,
};
use crate::error::Unspecified;
use crate::iv::FixedLength;
use crate::{constant_time, hmac, rand};
use core::fmt::Debug;

/// The length of an IV.
pub const IV_LEN: usize = 16;

/// The longest tag, `A256CBC-HS512`'s.
const MAX_TAG_LEN: usize = 32;

/// An AES-CBC with HMAC-SHA-2 algorithm.
pub struct Algorithm {
    name: &'static str,
    key_len: usize,
    cipher: &'static cipher::Algorithm,
    mac: hmac::Algorithm,
    tag_len: usize,
}

impl Algorithm {
    /// The key's length in bytes: the HMAC key's and the AES key's together.
    #[must_use]
    pub fn key_len(&self) -> usize {
        self.key_len
    }

    /// The tag's length in bytes.
    #[must_use]
    pub fn tag_len(&self) -> usize {
        self.tag_len
    }
}

impl Debug for Algorithm {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.name)
    }
}

/// `A128CBC-HS256`: AES-128-CBC and HMAC-SHA-256, a 32-byte key and a 16-byte tag.
pub const AES_128_CBC_HMAC_SHA_256: Algorithm = Algorithm {
    name: "AES_128_CBC_HMAC_SHA_256",
    key_len: 32,
    cipher: &cipher::AES_128,
    mac: hmac::HMAC_SHA256,
    tag_len: 16,
};

/// `A192CBC-HS384`: AES-192-CBC and HMAC-SHA-384, a 48-byte key and a 24-byte tag.
pub const AES_192_CBC_HMAC_SHA_384: Algorithm = Algorithm {
    name: "AES_192_CBC_HMAC_SHA_384",
    key_len: 48,
    cipher: &cipher::AES_192,
    mac: hmac::HMAC_SHA384,
    tag_len: 24,
};

/// `A256CBC-HS512`: AES-256-CBC and HMAC-SHA-512, a 64-byte key and a 32-byte tag.
pub const AES_256_CBC_HMAC_SHA_512: Algorithm = Algorithm {
    name: "AES_256_CBC_HMAC_SHA_512",
    key_len: 64,
    cipher: &cipher::AES_256,
    mac: hmac::HMAC_SHA512,
    tag_len: 32,
};

/// An authentication tag.
#[derive(Clone, Copy)]
pub struct Tag {
    bytes: [u8; MAX_TAG_LEN],
    len: usize,
}

impl AsRef<[u8]> for Tag {
    fn as_ref(&self) -> &[u8] {
        self.bytes.get(..self.len).unwrap_or(&[])
    }
}

impl Debug for Tag {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Tag").finish_non_exhaustive()
    }
}

/// An AES-CBC with HMAC-SHA-2 key, for sealing and opening.
pub struct Key {
    algorithm: &'static Algorithm,
    mac: hmac::Key,
    encrypting: PaddedBlockEncryptingKey,
    decrypting: PaddedBlockDecryptingKey,
}

impl Key {
    /// A key for `algorithm` from `key_bytes`, `algorithm.key_len()` bytes: the HMAC key, then
    /// the AES key.
    ///
    /// # Errors
    /// `error::Unspecified` if `key_bytes` has the wrong length or a key cannot be made.
    pub fn new(algorithm: &'static Algorithm, key_bytes: &[u8]) -> Result<Self, Unspecified> {
        if key_bytes.len() != algorithm.key_len {
            return Err(Unspecified);
        }
        let (mac_key, enc_key) = key_bytes.split_at(algorithm.key_len / 2);
        Ok(Self {
            algorithm,
            mac: hmac::Key::try_new(algorithm.mac, mac_key)?,
            encrypting: PaddedBlockEncryptingKey::cbc_pkcs7(UnboundCipherKey::new(
                algorithm.cipher,
                enc_key,
            )?)?,
            decrypting: PaddedBlockDecryptingKey::cbc_pkcs7(UnboundCipherKey::new(
                algorithm.cipher,
                enc_key,
            )?)?,
        })
    }

    /// Seals `in_out` under a random IV: pads and encrypts the plaintext in place, and returns
    /// the IV and the tag.
    ///
    /// # Errors
    /// `error::Unspecified` if no random IV can be drawn or sealing fails.
    pub fn seal_in_place<InOut>(
        &self,
        aad: &[u8],
        in_out: &mut InOut,
    ) -> Result<([u8; IV_LEN], Tag), Unspecified>
    where
        InOut: AsMut<[u8]> + for<'a> Extend<&'a u8>,
    {
        let mut iv = [0u8; IV_LEN];
        rand::fill(&mut iv)?;
        let tag = self.less_safe_seal_in_place(iv, aad, in_out)?;
        Ok((iv, tag))
    }

    /// Seals `in_out` under the caller's `iv`, which must not be predictable, as CBC
    /// requires: pads and encrypts the plaintext in place, and returns the tag.
    ///
    /// # Errors
    /// `error::Unspecified` if sealing fails.
    pub fn less_safe_seal_in_place<InOut>(
        &self,
        iv: [u8; IV_LEN],
        aad: &[u8],
        in_out: &mut InOut,
    ) -> Result<Tag, Unspecified>
    where
        InOut: AsMut<[u8]> + for<'a> Extend<&'a u8>,
    {
        self.encrypting
            .less_safe_encrypt(in_out, EncryptionContext::Iv128(FixedLength::from(iv)))?;
        self.tag(&iv, aad, in_out.as_mut())
    }

    /// Opens `in_out`, a ciphertext sealed under `iv` with `aad`, if `tag` authenticates them,
    /// and returns the plaintext, a prefix of `in_out`. Nothing is decrypted unless the tag
    /// matches.
    ///
    /// # Errors
    /// `error::Unspecified` if the tag does not match, the ciphertext is not whole blocks, or
    /// its padding is not PKCS #7's.
    pub fn open_in_place<'in_out>(
        &self,
        iv: &[u8; IV_LEN],
        aad: &[u8],
        in_out: &'in_out mut [u8],
        tag: &[u8],
    ) -> Result<&'in_out mut [u8], Unspecified> {
        let expected = self.tag(iv, aad, in_out)?;
        constant_time::verify_slices_are_equal(expected.as_ref(), tag)?;
        self.decrypting
            .decrypt(in_out, DecryptionContext::Iv128(FixedLength::from(*iv)))
    }

    /// The key's algorithm.
    #[must_use]
    pub fn algorithm(&self) -> &'static Algorithm {
        self.algorithm
    }

    /// `HMAC(MAC_KEY, A || IV || E || AL)` cut to the tag's length (RFC 7518 §5.2.2.1).
    fn tag(&self, iv: &[u8; IV_LEN], aad: &[u8], ciphertext: &[u8]) -> Result<Tag, Unspecified> {
        let aad_bits = u64::try_from(aad.len())
            .ok()
            .and_then(|len| len.checked_mul(8))
            .ok_or(Unspecified)?;
        let mut mac = hmac::Context::try_with_key(&self.mac)?;
        mac.try_update(aad)?;
        mac.try_update(iv)?;
        mac.try_update(ciphertext)?;
        mac.try_update(&aad_bits.to_be_bytes())?;
        let full = mac.try_sign()?;
        let len = self.algorithm.tag_len;
        let mut tag = Tag {
            bytes: [0u8; MAX_TAG_LEN],
            len,
        };
        tag.bytes
            .get_mut(..len)
            .ok_or(Unspecified)?
            .copy_from_slice(full.as_ref().get(..len).ok_or(Unspecified)?);
        Ok(tag)
    }
}

impl Debug for Key {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Key")
            .field("algorithm", &self.algorithm)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Algorithm, Key, AES_128_CBC_HMAC_SHA_256, AES_192_CBC_HMAC_SHA_384,
        AES_256_CBC_HMAC_SHA_512, IV_LEN,
    };
    use crate::test::from_hex;

    // RFC 7518 Appendix B: K, P, IV and A, and the ciphertext E and tag T they give.
    const P: &str =
        "41206369706865722073797374656d206d757374206e6f7420626520726571756972656420746f20\
        6265207365637265742c20616e64206974206d7573742062652061626c6520746f2066616c6c2069\
        6e746f207468652068616e6473206f662074686520656e656d7920776974686f757420696e636f6e\
        76656e69656e6365";
    const IV: &str = "1af38c2dc2b96ffdd86694092341bc04";
    const A: &str =
        "546865207365636f6e64207072696e6369706c65206f662041756775737465204b6572636b686f66\
        6673";

    struct Case {
        algorithm: &'static Algorithm,
        k: &'static str,
        e: &'static str,
        t: &'static str,
    }

    const CASES: [Case; 3] = [
        Case {
            algorithm: &AES_128_CBC_HMAC_SHA_256,
            k: K_B1,
            e: E_B1,
            t: T_B1,
        },
        Case {
            algorithm: &AES_192_CBC_HMAC_SHA_384,
            k: K_B2,
            e: E_B2,
            t: T_B2,
        },
        Case {
            algorithm: &AES_256_CBC_HMAC_SHA_512,
            k: K_B3,
            e: E_B3,
            t: T_B3,
        },
    ];

    const K_B1: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
    const E_B1: &str =
        "c80edfa32ddf39d5ef00c0b468834279a2e46a1b8049f792f76bfe54b903a9c9a94ac9b47ad2655c\
        5f10f9aef71427e2fc6f9b3f399a221489f16362c703233609d45ac69864e3321cf82935ac4096c8\
        6e133314c54019e8ca7980dfa4b9cf1b384c486f3a54c51078158ee5d79de59fbd34d848b3d69550\
        a67646344427ade54b8851ffb598f7f80074b9473c82e2db";
    const T_B1: &str = "652c3fa36b0a7c5b3219fab3a30bc1c4";
    const K_B2: &str =
        "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f2021222324252627\
        28292a2b2c2d2e2f";
    const E_B2: &str =
        "ea65da6b59e61edb419be62d19712ae5d303eeb50052d0dfd6697f77224c8edb000d279bdc14c107\
        2654bd30944230c657bed4ca0c9f4a8466f22b226d1746214bf8cfc2400add9f5126e479663fc90b\
        3bed787a2f0ffcbf3904be2a641d5c2105bfe591bae23b1d7449e532eef60a9ac8bb6c6b01d35d49\
        787bcd57ef484927f280adc91ac0c4e79c7b11efc60054e3";
    const T_B2: &str = "8490ac0e58949bfe51875d733f93ac2075168039ccc733d7";
    const K_B3: &str =
        "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f2021222324252627\
        28292a2b2c2d2e2f303132333435363738393a3b3c3d3e3f";
    const E_B3: &str =
        "4affaaadb78c31c5da4b1b590d10ffbd3dd8d5d302423526912da037ecbcc7bd822c301dd67c373b\
        ccb584ad3e9279c2e6d12a1374b77f077553df829410446b36ebd97066296ae6427ea75c2e0846a1\
        1a09ccf5370dc80bfecbad28c73f09b3a3b75e662a2594410ae496b2e2e6609e31e6e02cc837f053\
        d21f37ff4f51950bbe2638d09dd7a4930930806d0703b1f6";
    const T_B3: &str = "4dd3b4c088a7f45c216839645b2012bf2e6269a8c56a816dbc1b267761955bc5";

    fn iv() -> [u8; IV_LEN] {
        from_hex(IV).unwrap().try_into().unwrap()
    }

    #[test]
    fn rfc_7518_appendix_b() {
        for case in &CASES {
            let key = Key::new(case.algorithm, &from_hex(case.k).unwrap()).unwrap();
            let aad = from_hex(A).unwrap();
            let mut in_out = from_hex(P).unwrap();
            let tag = key
                .less_safe_seal_in_place(iv(), &aad, &mut in_out)
                .unwrap();
            assert_eq!(in_out, from_hex(case.e).unwrap(), "{:?}", case.algorithm);
            assert_eq!(tag.as_ref(), from_hex(case.t).unwrap().as_slice());
            assert_eq!(tag.as_ref().len(), case.algorithm.tag_len());

            let opened = key
                .open_in_place(&iv(), &aad, &mut in_out, tag.as_ref())
                .unwrap();
            assert_eq!(opened, from_hex(P).unwrap().as_slice());
        }
    }

    #[test]
    fn a_changed_byte_anywhere_is_refused_before_decrypting() {
        for case in &CASES {
            let key = Key::new(case.algorithm, &from_hex(case.k).unwrap()).unwrap();
            let aad = from_hex(A).unwrap();
            let sealed = from_hex(case.e).unwrap();
            let tag = from_hex(case.t).unwrap();

            let mut changed = sealed.clone();
            changed[20] ^= 1;
            assert!(key.open_in_place(&iv(), &aad, &mut changed, &tag).is_err());
            assert_eq!(changed[21..], sealed[21..], "decrypted despite a bad tag");

            let mut bad_iv = iv();
            bad_iv[0] ^= 1;
            assert!(key
                .open_in_place(&bad_iv, &aad, &mut sealed.clone(), &tag)
                .is_err());
            let mut bad_aad = aad.clone();
            bad_aad[0] ^= 1;
            assert!(key
                .open_in_place(&iv(), &bad_aad, &mut sealed.clone(), &tag)
                .is_err());
            let mut bad_tag = tag.clone();
            bad_tag[0] ^= 1;
            assert!(key
                .open_in_place(&iv(), &aad, &mut sealed.clone(), &bad_tag)
                .is_err());
            // A truncated tag, or the untruncated HMAC, is not the tag.
            assert!(key
                .open_in_place(&iv(), &aad, &mut sealed.clone(), &tag[..tag.len() - 1])
                .is_err());
        }
    }

    #[test]
    fn random_ivs_round_trip_every_length() {
        for case in &CASES {
            let key = Key::new(case.algorithm, &from_hex(case.k).unwrap()).unwrap();
            let mut ivs = Vec::new();
            for len in [0usize, 1, 15, 16, 17, 100] {
                let plaintext: Vec<u8> = (0..len).map(|i| u8::try_from(i % 251).unwrap()).collect();
                let mut in_out = plaintext.clone();
                let (iv, tag) = key.seal_in_place(b"header", &mut in_out).unwrap();
                assert_eq!(in_out.len(), (len / 16 + 1) * 16, "PKCS #7 always pads");
                let opened = key
                    .open_in_place(&iv, b"header", &mut in_out, tag.as_ref())
                    .unwrap();
                assert_eq!(opened, plaintext.as_slice());
                ivs.push(iv);
            }
            ivs.sort_unstable();
            ivs.dedup();
            assert_eq!(ivs.len(), 6, "an IV repeated");
        }
    }

    #[test]
    fn keys_of_the_wrong_length_are_refused() {
        for case in &CASES {
            let len = case.algorithm.key_len();
            assert!(Key::new(case.algorithm, &vec![0u8; len - 1]).is_err());
            assert!(Key::new(case.algorithm, &vec![0u8; len + 1]).is_err());
            assert!(Key::new(case.algorithm, &vec![0u8; len]).is_ok());
        }
    }

    #[test]
    fn ciphertext_that_is_not_whole_blocks_is_refused() {
        let key = Key::new(&AES_128_CBC_HMAC_SHA_256, &[7u8; 32]).unwrap();
        let mut in_out = b"seventeen bytes!!".to_vec();
        let (iv, _) = key.seal_in_place(b"", &mut in_out).unwrap();
        in_out.pop();
        let tag = key.tag(&iv, b"", &in_out).unwrap();
        assert!(key
            .open_in_place(&iv, b"", &mut in_out, tag.as_ref())
            .is_err());
    }
}
