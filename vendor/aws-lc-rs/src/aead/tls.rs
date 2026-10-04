// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0 OR ISC

use super::aead_ctx::{self, AeadCtx};
use super::{Aad, Algorithm, AlgorithmID, Nonce, Tag, UnboundKey};
use crate::error::Unspecified;
use core::fmt::Debug;
use core::ops::RangeFrom;

/// The Transport Layer Security (TLS) protocol version.
#[allow(clippy::module_name_repetitions)]
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
#[non_exhaustive]
pub enum TlsProtocolId {
    /// TLS 1.2 (RFC 5246)
    TLS12,

    /// TLS 1.3 (RFC 8446)
    TLS13,
}

/// AEAD Encryption key used for TLS protocol record encryption.
///
/// This type encapsulates encryption operations for TLS AEAD algorithms.
/// It validates that the provides nonce values are monotonically increasing for each invocation.
///
/// The following algorithms are supported:
/// * `AES_128_GCM`
/// * `AES_256_GCM`
///
/// Prefer this type in place of `LessSafeKey`, `OpeningKey`, `SealingKey` for TLS protocol implementations.
#[allow(clippy::module_name_repetitions)]
pub struct TlsRecordSealingKey {
    // The TLS-specific AEAD seal constructions in AWS-LC maintain internal mutable state
    // (nonce counter). The seal methods take `&mut self` to prevent concurrent access.
    key: UnboundKey,
    protocol: TlsProtocolId,
}

impl TlsRecordSealingKey {
    /// New TLS record sealing key. Only supports `AES_128_GCM` and `AES_256_GCM`.
    ///
    /// # Errors
    /// * `Unspecified`: Returned if the length of `key_bytes` does not match the chosen algorithm,
    ///   or if an unsupported algorithm is provided.
    pub fn new(
        algorithm: &'static Algorithm,
        protocol: TlsProtocolId,
        key_bytes: &[u8],
    ) -> Result<Self, Unspecified> {
        let ctx = match (algorithm.id, protocol) {
            (AlgorithmID::AES_128_GCM, TlsProtocolId::TLS12) => AeadCtx::aes_128_gcm_tls12(
                key_bytes,
                algorithm.tag_len(),
                aead_ctx::AeadDirection::Seal,
            ),
            (AlgorithmID::AES_128_GCM, TlsProtocolId::TLS13) => AeadCtx::aes_128_gcm_tls13(
                key_bytes,
                algorithm.tag_len(),
                aead_ctx::AeadDirection::Seal,
            ),
            (AlgorithmID::AES_256_GCM, TlsProtocolId::TLS12) => AeadCtx::aes_256_gcm_tls12(
                key_bytes,
                algorithm.tag_len(),
                aead_ctx::AeadDirection::Seal,
            ),
            (AlgorithmID::AES_256_GCM, TlsProtocolId::TLS13) => AeadCtx::aes_256_gcm_tls13(
                key_bytes,
                algorithm.tag_len(),
                aead_ctx::AeadDirection::Seal,
            ),
            (
                AlgorithmID::AES_128_GCM_SIV
                | AlgorithmID::AES_192_GCM
                | AlgorithmID::AES_256_GCM_SIV
                | AlgorithmID::CHACHA20_POLY1305,
                _,
            ) => Err(Unspecified),
        }?;
        Ok(Self {
            key: UnboundKey::from(ctx),
            protocol,
        })
    }

    /// Accepts a `Nonce` and `Aad` construction that is unique for this key and
    /// TLS record sealing operation for the configured TLS protocol version.
    ///
    /// `nonce` must be unique and incremented per each sealing operation,
    /// otherwise an error is returned.
    ///
    /// # Errors
    /// `error::Unspecified` if encryption operation fails.
    #[inline]
    #[allow(clippy::needless_pass_by_value)]
    pub fn seal_in_place_append_tag<A, InOut>(
        &mut self,
        nonce: Nonce,
        aad: Aad<A>,
        in_out: &mut InOut,
    ) -> Result<(), Unspecified>
    where
        A: AsRef<[u8]>,
        InOut: AsMut<[u8]> + for<'in_out> Extend<&'in_out u8>,
    {
        self.key
            .seal_in_place_append_tag(Some(nonce), aad.as_ref(), in_out)
            .map(|_| ())
    }

    /// Encrypts and signs (“seals”) data in place.
    ///
    /// `aad` is the additional authenticated data (AAD), if any. This is
    /// authenticated but not encrypted. The type `A` could be a byte slice
    /// `&[u8]`, a byte array `[u8; N]` for some constant `N`, `Vec<u8>`, etc.
    /// If there is no AAD then use `Aad::empty()`.
    ///
    /// The plaintext is given as the input value of `in_out`. `seal_in_place()`
    /// will overwrite the plaintext with the ciphertext and return the tag.
    /// For most protocols, the caller must append the tag to the ciphertext.
    /// The tag will be `self.algorithm.tag_len()` bytes long.
    ///
    /// The Nonce used for the operation is randomly generated, and returned to the caller.
    ///
    /// # Errors
    /// `error::Unspecified` if encryption operation fails.
    #[inline]
    #[allow(clippy::needless_pass_by_value)]
    pub fn seal_in_place_separate_tag<A>(
        &mut self,
        nonce: Nonce,
        aad: Aad<A>,
        in_out: &mut [u8],
    ) -> Result<Tag, Unspecified>
    where
        A: AsRef<[u8]>,
    {
        self.key
            .seal_in_place_separate_tag(Some(nonce), aad.as_ref(), in_out)
            .map(|(_, tag)| tag)
    }

    /// Encrypts and signs (“seals”) `in_plaintext` into a separate `out_ciphertext`
    /// buffer, leaving `in_plaintext` untouched.
    ///
    /// This is the out-of-place counterpart to [`Self::seal_in_place_separate_tag`].
    ///
    /// `out_ciphertext` must be exactly `in_plaintext.len()` bytes. `extra_in` is
    /// additional plaintext, such as TLS 1.3's inner content-type byte, that is
    /// encrypted into `extra_out_and_tag` ahead of the tag, so `extra_out_and_tag` must
    /// be `extra_in.len() + self.algorithm().tag_len()` bytes. A caller with no extra
    /// plaintext passes an empty `extra_in` and an `extra_out_and_tag` of
    /// `self.algorithm().tag_len()` bytes.
    ///
    /// `nonce` must be unique and incremented per sealing operation, as for the in-place
    /// methods: both advance the same counter.
    ///
    /// # Errors
    /// `error::Unspecified` if the buffer lengths are wrong or the encryption operation
    /// fails. A length mismatch is rejected before the AEAD runs, leaving both output
    /// buffers untouched.
    #[inline]
    #[allow(clippy::needless_pass_by_value)]
    pub fn seal_out_of_place_scatter<A>(
        &mut self,
        nonce: Nonce,
        aad: Aad<A>,
        in_plaintext: &[u8],
        out_ciphertext: &mut [u8],
        extra_in: &[u8],
        extra_out_and_tag: &mut [u8],
    ) -> Result<(), Unspecified>
    where
        A: AsRef<[u8]>,
    {
        self.key.seal_out_of_place_scatter(
            nonce,
            aad.as_ref(),
            in_plaintext,
            out_ciphertext,
            extra_in,
            extra_out_and_tag,
        )
    }

    /// The key's AEAD algorithm.
    #[inline]
    #[must_use]
    pub fn algorithm(&self) -> &'static Algorithm {
        self.key.algorithm()
    }

    /// The key's associated `TlsProtocolId`.
    #[must_use]
    pub fn tls_protocol_id(&self) -> TlsProtocolId {
        self.protocol
    }
}

#[allow(clippy::missing_fields_in_debug)]
impl Debug for TlsRecordSealingKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TlsRecordSealingKey")
            .field("key", &self.key)
            .field("protocol", &self.protocol)
            .finish()
    }
}

/// AEAD Encryption key used for TLS protocol record encryption.
///
/// This type encapsulates decryption operations for TLS AEAD algorithms.
///
/// The following algorithms are supported:
/// * `AES_128_GCM`
/// * `AES_256_GCM`
///
/// Prefer this type in place of `LessSafeKey`, `OpeningKey`, `SealingKey` for TLS protocol implementations.
#[allow(clippy::module_name_repetitions)]
pub struct TlsRecordOpeningKey {
    // Unlike the seal path, the TLS-specific AEAD open operations in AWS-LC are stateless
    // and safe for concurrent use. The open methods take `&self`.
    key: UnboundKey,
    protocol: TlsProtocolId,
}

impl TlsRecordOpeningKey {
    /// New TLS record opening key. Only supports `AES_128_GCM` and `AES_256_GCM` Algorithms.
    ///
    /// # Errors
    /// * `Unspecified`: Returned if the length of `key_bytes` does not match the chosen algorithm,
    ///   or if an unsupported algorithm is provided.
    pub fn new(
        algorithm: &'static Algorithm,
        protocol: TlsProtocolId,
        key_bytes: &[u8],
    ) -> Result<Self, Unspecified> {
        let ctx = match (algorithm.id, protocol) {
            (AlgorithmID::AES_128_GCM, TlsProtocolId::TLS12) => AeadCtx::aes_128_gcm_tls12(
                key_bytes,
                algorithm.tag_len(),
                aead_ctx::AeadDirection::Open,
            ),
            (AlgorithmID::AES_128_GCM, TlsProtocolId::TLS13) => AeadCtx::aes_128_gcm_tls13(
                key_bytes,
                algorithm.tag_len(),
                aead_ctx::AeadDirection::Open,
            ),
            (AlgorithmID::AES_256_GCM, TlsProtocolId::TLS12) => AeadCtx::aes_256_gcm_tls12(
                key_bytes,
                algorithm.tag_len(),
                aead_ctx::AeadDirection::Open,
            ),
            (AlgorithmID::AES_256_GCM, TlsProtocolId::TLS13) => AeadCtx::aes_256_gcm_tls13(
                key_bytes,
                algorithm.tag_len(),
                aead_ctx::AeadDirection::Open,
            ),
            (
                AlgorithmID::AES_128_GCM_SIV
                | AlgorithmID::AES_192_GCM
                | AlgorithmID::AES_256_GCM_SIV
                | AlgorithmID::CHACHA20_POLY1305,
                _,
            ) => Err(Unspecified),
        }?;
        Ok(Self {
            key: UnboundKey::from(ctx),
            protocol,
        })
    }

    /// See [`super::OpeningKey::open_in_place()`] for details.
    ///
    /// # Errors
    /// `error::Unspecified` when ciphertext is invalid.
    #[inline]
    #[allow(clippy::needless_pass_by_value)]
    pub fn open_in_place<'in_out, A>(
        &self,
        nonce: Nonce,
        aad: Aad<A>,
        in_out: &'in_out mut [u8],
    ) -> Result<&'in_out mut [u8], Unspecified>
    where
        A: AsRef<[u8]>,
    {
        self.key.open_within(nonce, aad.as_ref(), in_out, 0..)
    }

    /// See [`super::OpeningKey::open_within()`] for details.
    ///
    /// # Errors
    /// `error::Unspecified` when ciphertext is invalid.
    #[inline]
    #[allow(clippy::needless_pass_by_value)]
    pub fn open_within<'in_out, A>(
        &self,
        nonce: Nonce,
        aad: Aad<A>,
        in_out: &'in_out mut [u8],
        ciphertext_and_tag: RangeFrom<usize>,
    ) -> Result<&'in_out mut [u8], Unspecified>
    where
        A: AsRef<[u8]>,
    {
        self.key
            .open_within(nonce, aad.as_ref(), in_out, ciphertext_and_tag)
    }

    /// The key's AEAD algorithm.
    #[inline]
    #[must_use]
    pub fn algorithm(&self) -> &'static Algorithm {
        self.key.algorithm()
    }

    /// The key's associated `TlsProtocolId`.
    #[must_use]
    pub fn tls_protocol_id(&self) -> TlsProtocolId {
        self.protocol
    }
}

#[allow(clippy::missing_fields_in_debug)]
impl Debug for TlsRecordOpeningKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TlsRecordOpeningKey")
            .field("key", &self.key)
            .field("protocol", &self.protocol)
            .finish()
    }
}

// mantle: TLS 1.3 sealing from several borrowed plaintext slices (aws-lc-rs#1241;
// vendor/UPSTREAM.md).
#[cfg(not(feature = "fips"))]
pub use vectored::Tls13VectoredSealingKey;

#[cfg(not(feature = "fips"))]
mod vectored {
    use super::super::{Aad, Algorithm, AlgorithmID, NONCE_LEN};
    use crate::aws_lc::{
        EVP_CIPHER_CTX_ctrl, EVP_CIPHER_CTX_new, EVP_EncryptFinal_ex, EVP_EncryptInit_ex,
        EVP_EncryptUpdate, EVP_aes_128_gcm, EVP_aes_256_gcm, EVP_CIPHER_CTX, EVP_CTRL_GCM_GET_TAG,
    };
    use crate::error::Unspecified;
    use crate::ptr::LcPtr;
    use core::ffi::c_int;
    use core::fmt::Debug;
    use core::ptr::{null, null_mut};
    use zeroize::Zeroize;

    /// AES-GCM sealing key for TLS 1.3 records whose plaintext lies in several borrowed slices.
    ///
    /// A TLS 1.3 record's plaintext often arrives in pieces: a payload in several buffers,
    /// then the inner content type and any padding (RFC 8446 §5.2). This key seals their
    /// concatenation as one record, in one AES-GCM invocation with one nonce and one tag,
    /// reading each piece where it lies. Its ciphertext and tag are byte for byte those
    /// [`super::TlsRecordSealingKey`] produces for the same record.
    ///
    /// The key makes each record's nonce from the record's sequence number and the traffic
    /// IV (RFC 8446 §5.3). TLS numbers records from 0 and one apart; the key refuses any
    /// number not above every number it has sealed, so no nonce repeats, and refuses
    /// `u64::MAX`, after which no number could follow, as AWS-LC's own TLS 1.3 AEAD does. A
    /// seal consumes its sequence number before it encrypts anything, and a seal that fails
    /// after that leaves the key refusing every later seal, so no nonce is used twice even
    /// when encryption stops partway. The caller must then replace the key.
    ///
    /// The caller builds the additional data (the record header), supplies the inner content
    /// type and padding as the last plaintext slices, and keeps to the cipher suite's limit
    /// on records per key (RFC 8446 §5.5).
    ///
    /// Supports `AES_128_GCM` and `AES_256_GCM`. Not available with the `fips` feature: it
    /// runs AES-GCM through the incremental `EVP_CIPHER` interface, with the nonce chosen
    /// here, and makes no claim of FIPS approval.
    pub struct Tls13VectoredSealingKey {
        ctx: LcPtr<EVP_CIPHER_CTX>,
        algorithm: &'static Algorithm,
        iv: [u8; NONCE_LEN],
        next_sequence: u64,
        /// Set when a seal has consumed its sequence number and not yet completed; a seal
        /// that fails or unwinds leaves it set.
        failed: bool,
    }

    // SAFETY: the key owns its `EVP_CIPHER_CTX` alone and uses it only through `&mut self`.
    unsafe impl Send for Tls13VectoredSealingKey {}

    impl Tls13VectoredSealingKey {
        /// A key for `algorithm` from a TLS 1.3 traffic key and IV (RFC 8446 §7.3).
        ///
        /// # Errors
        /// `error::Unspecified` if the algorithm is not AES-GCM, `key_bytes` is not the
        /// algorithm's key length, `iv` is not `NONCE_LEN` bytes, or AWS-LC cannot set the key.
        pub fn new(
            algorithm: &'static Algorithm,
            key_bytes: &[u8],
            iv: &[u8],
        ) -> Result<Self, Unspecified> {
            let cipher = match algorithm.id {
                AlgorithmID::AES_128_GCM => unsafe { EVP_aes_128_gcm() },
                AlgorithmID::AES_256_GCM => unsafe { EVP_aes_256_gcm() },
                AlgorithmID::AES_192_GCM
                | AlgorithmID::AES_128_GCM_SIV
                | AlgorithmID::AES_256_GCM_SIV
                | AlgorithmID::CHACHA20_POLY1305 => return Err(Unspecified),
            };
            if key_bytes.len() != algorithm.key_len() {
                return Err(Unspecified);
            }
            let iv: [u8; NONCE_LEN] = iv.try_into().map_err(|_| Unspecified)?;
            let mut ctx = LcPtr::new(unsafe { EVP_CIPHER_CTX_new() })?;
            // AWS-LC copies the key into the context.
            if 1 != unsafe {
                EVP_EncryptInit_ex(
                    ctx.as_mut_ptr(),
                    cipher,
                    null_mut(),
                    key_bytes.as_ptr(),
                    null(),
                )
            } {
                return Err(Unspecified);
            }
            Ok(Self {
                ctx,
                algorithm,
                iv,
                next_sequence: 0,
                failed: false,
            })
        }

        /// Seals the record with sequence number `sequence`, writing its ciphertext and then
        /// its tag to the start of `out`.
        ///
        /// `aad` is the record's additional data, its header (RFC 8446 §5.2). The plaintext
        /// is the concatenation of the slices `plaintext` yields, `plaintext_len` bytes in
        /// all; empty slices are allowed and slices need not align to blocks. `out` must hold
        /// at least `plaintext_len + self.algorithm().tag_len()` bytes, and nothing past them
        /// is written.
        ///
        /// # Errors
        /// `error::Unspecified` if the key has failed before, `sequence` is not above every
        /// sequence number this key has sealed or is `u64::MAX`, `out` is too short, or the
        /// lengths exceed what AWS-LC takes: all refused before anything is encrypted, with
        /// `out` untouched and the key still usable. Also if the slices do not add up to
        /// `plaintext_len`, which is found without writing past `plaintext_len`, or
        /// AWS-LC fails: then `out` may hold part of the ciphertext, and the key refuses
        /// every later seal.
        pub fn seal_vectored<'p, A, P>(
            &mut self,
            sequence: u64,
            aad: Aad<A>,
            plaintext: P,
            plaintext_len: usize,
            out: &mut [u8],
        ) -> Result<(), Unspecified>
        where
            A: AsRef<[u8]>,
            P: IntoIterator<Item = &'p [u8]>,
        {
            let sealed_len = plaintext_len
                .checked_add(self.algorithm.tag_len())
                .ok_or(Unspecified)?;
            let out = out.get_mut(..sealed_len).ok_or(Unspecified)?;
            self.begin(sequence, aad.as_ref(), plaintext_len)?;
            // SAFETY: `out` is valid for `sealed_len` bytes.
            unsafe { self.encrypt(plaintext, plaintext_len, out.as_mut_ptr()) }
        }

        /// Seals the record as [`Self::seal_vectored`] does, appending its ciphertext and tag
        /// to `out`.
        ///
        /// The sealed bytes are written into `out`'s spare capacity, reserved first, and
        /// `out`'s length covers them only once the seal has succeeded: the bytes `out`
        /// held are kept, and no byte is exposed before it is written.
        ///
        /// # Errors
        /// As for [`Self::seal_vectored`], and if the capacity cannot be reserved, which is
        /// refused before anything is encrypted. On any error `out` keeps its length and
        /// contents.
        pub fn seal_vectored_append<'p, A, P>(
            &mut self,
            sequence: u64,
            aad: Aad<A>,
            plaintext: P,
            plaintext_len: usize,
            out: &mut Vec<u8>,
        ) -> Result<(), Unspecified>
        where
            A: AsRef<[u8]>,
            P: IntoIterator<Item = &'p [u8]>,
        {
            let sealed_len = plaintext_len
                .checked_add(self.algorithm.tag_len())
                .ok_or(Unspecified)?;
            out.try_reserve(sealed_len).map_err(|_| Unspecified)?;
            self.begin(sequence, aad.as_ref(), plaintext_len)?;
            let start = out.len();
            let spare = out.spare_capacity_mut();
            if spare.len() < sealed_len {
                return Err(Unspecified);
            }
            // SAFETY: the spare capacity is valid for writes of `sealed_len` bytes; AWS-LC
            // writes bytes through the pointer and never reads them first.
            unsafe { self.encrypt(plaintext, plaintext_len, spare.as_mut_ptr().cast::<u8>())? };
            // SAFETY: `encrypt` succeeded, so it wrote all `sealed_len` bytes after `start`.
            unsafe { out.set_len(start + sealed_len) };
            Ok(())
        }

        /// The key's AEAD algorithm.
        #[must_use]
        pub fn algorithm(&self) -> &'static Algorithm {
            self.algorithm
        }

        /// Checks `sequence` and the lengths, consumes the sequence number, and starts the
        /// record: its nonce, then its additional data.
        fn begin(
            &mut self,
            sequence: u64,
            aad: &[u8],
            plaintext_len: usize,
        ) -> Result<(), Unspecified> {
            if self.failed || sequence < self.next_sequence || sequence == u64::MAX {
                return Err(Unspecified);
            }
            // `EVP_EncryptUpdate` takes `int` lengths; each slice is at most the whole.
            c_int::try_from(plaintext_len).map_err(|_| Unspecified)?;
            let aad_len = c_int::try_from(aad.len()).map_err(|_| Unspecified)?;
            // From here the sequence number is spent, and the key stays failed unless the
            // seal completes.
            self.next_sequence = sequence + 1;
            self.failed = true;

            // RFC 8446 §5.3: the sequence number, big-endian and left-padded to the IV's
            // length, XORed with the IV.
            let mut nonce = self.iv;
            for (n, s) in nonce[NONCE_LEN - 8..]
                .iter_mut()
                .zip(sequence.to_be_bytes())
            {
                *n ^= s;
            }
            // With no cipher and no key, AWS-LC keeps both and starts a new message under
            // this nonce.
            let started = unsafe {
                EVP_EncryptInit_ex(
                    self.ctx.as_mut_ptr(),
                    null(),
                    null_mut(),
                    null(),
                    nonce.as_ptr(),
                )
            };
            nonce.zeroize();
            if started != 1 {
                return Err(Unspecified);
            }
            if !aad.is_empty() {
                let mut written: c_int = 0;
                // A null output makes the input additional data.
                if 1 != unsafe {
                    EVP_EncryptUpdate(
                        self.ctx.as_mut_ptr(),
                        null_mut(),
                        &mut written,
                        aad.as_ptr(),
                        aad_len,
                    )
                } {
                    return Err(Unspecified);
                }
            }
            Ok(())
        }

        /// Encrypts the plaintext into `out`, then writes the tag after it.
        ///
        /// # Safety
        /// `out` must be valid for writes of `plaintext_len + self.algorithm.tag_len()` bytes,
        /// and `begin` must have succeeded for this record.
        unsafe fn encrypt<'p, P>(
            &mut self,
            plaintext: P,
            plaintext_len: usize,
            out: *mut u8,
        ) -> Result<(), Unspecified>
        where
            P: IntoIterator<Item = &'p [u8]>,
        {
            let mut done = 0usize;
            for piece in plaintext {
                if piece.is_empty() {
                    continue;
                }
                // A slice past the declared length is refused before it is written.
                let end = done
                    .checked_add(piece.len())
                    .filter(|&end| end <= plaintext_len)
                    .ok_or(Unspecified)?;
                let len = c_int::try_from(piece.len()).map_err(|_| Unspecified)?;
                let mut written: c_int = 0;
                if 1 != EVP_EncryptUpdate(
                    self.ctx.as_mut_ptr(),
                    out.add(done),
                    &mut written,
                    piece.as_ptr(),
                    len,
                ) || written != len
                {
                    return Err(Unspecified);
                }
                done = end;
            }
            if done != plaintext_len {
                return Err(Unspecified);
            }
            let mut written: c_int = 0;
            if 1 != EVP_EncryptFinal_ex(self.ctx.as_mut_ptr(), out.add(done), &mut written)
                || written != 0
            {
                return Err(Unspecified);
            }
            let tag_len = c_int::try_from(self.algorithm.tag_len()).map_err(|_| Unspecified)?;
            if 1 != EVP_CIPHER_CTX_ctrl(
                self.ctx.as_mut_ptr(),
                EVP_CTRL_GCM_GET_TAG,
                tag_len,
                out.add(done).cast(),
            ) {
                return Err(Unspecified);
            }
            self.failed = false;
            Ok(())
        }
    }

    impl Drop for Tls13VectoredSealingKey {
        fn drop(&mut self) {
            self.iv.zeroize();
        }
    }

    #[allow(clippy::missing_fields_in_debug)]
    impl Debug for Tls13VectoredSealingKey {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.debug_struct("Tls13VectoredSealingKey")
                .field("algorithm", &self.algorithm)
                .field("next_sequence", &self.next_sequence)
                .field("failed", &self.failed)
                .finish()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{TlsProtocolId, TlsRecordOpeningKey, TlsRecordSealingKey};
    use crate::aead::{Aad, Nonce, AES_128_GCM, AES_256_GCM, CHACHA20_POLY1305};
    use crate::test::from_hex;
    use paste::paste;

    const TEST_128_BIT_KEY: &[u8] = &[
        0xb0, 0x37, 0x9f, 0xf8, 0xfb, 0x8e, 0xa6, 0x31, 0xf4, 0x1c, 0xe6, 0x3e, 0xb5, 0xc5, 0x20,
        0x7c,
    ];

    const TEST_256_BIT_KEY: &[u8] = &[
        0x56, 0xd8, 0x96, 0x68, 0xbd, 0x96, 0xeb, 0xff, 0x5e, 0xa2, 0x0b, 0x34, 0xf2, 0x79, 0x84,
        0x6e, 0x2b, 0x13, 0x01, 0x3d, 0xab, 0x1d, 0xa4, 0x07, 0x5a, 0x16, 0xd5, 0x0b, 0x53, 0xb0,
        0xcc, 0x88,
    ];

    struct TlsNonceTestCase {
        nonce: &'static str,
        expect_err: bool,
    }

    const TLS_NONCE_TEST_CASES: &[TlsNonceTestCase] = &[
        TlsNonceTestCase {
            nonce: "9fab40177c900aad9fc28cc3",
            expect_err: false,
        },
        TlsNonceTestCase {
            nonce: "9fab40177c900aad9fc28cc4",
            expect_err: false,
        },
        TlsNonceTestCase {
            nonce: "9fab40177c900aad9fc28cc2",
            expect_err: true,
        },
    ];

    macro_rules! test_tls_aead {
        ($name:ident, $alg:expr, $proto:expr, $key:expr) => {
            paste! {
                #[test]
                fn [<test_ $name _tls_aead_unsupported>]() {
                    assert!(TlsRecordSealingKey::new($alg, $proto, $key).is_err());
                    assert!(TlsRecordOpeningKey::new($alg, $proto, $key).is_err());
                }
            }
        };
        ($name:ident, $alg:expr, $proto:expr, $key:expr, $expect_tag_len:expr, $expect_nonce_len:expr) => {
            paste! {
                #[test]
                fn [<test_ $name>]() {
                    let mut sealing_key =
                        TlsRecordSealingKey::new($alg, $proto, $key).unwrap();

                    let opening_key =
                        TlsRecordOpeningKey::new($alg, $proto, $key).unwrap();

                    for case in TLS_NONCE_TEST_CASES {
                        let plaintext = from_hex("00112233445566778899aabbccddeeff").unwrap();

                        assert_eq!($alg, sealing_key.algorithm());
                        assert_eq!(*$expect_tag_len, $alg.tag_len());
                        assert_eq!(*$expect_nonce_len, $alg.nonce_len());

                        let mut in_out = Vec::from(plaintext.as_slice());

                        let nonce = from_hex(case.nonce).unwrap();

                        let nonce_bytes = nonce.as_slice();

                        let result = sealing_key.seal_in_place_append_tag(
                            Nonce::try_assume_unique_for_key(nonce_bytes).unwrap(),
                            Aad::empty(),
                            &mut in_out,
                        );

                        match (result, case.expect_err) {
                            (Ok(()), true) => panic!("expected error for seal_in_place_append_tag"),
                            (Ok(()), false) => {}
                            (Err(_), true) => return,
                            (Err(e), false) => panic!("{e}"),
                        }

                        assert_ne!(plaintext, in_out[..plaintext.len()]);

                        // copy ciphertext with prefix, to exercise `open_within`
                        let mut offset_cipher_text = vec![ 1, 2, 3, 4 ];
                        offset_cipher_text.extend_from_slice(&in_out);

                        opening_key
                            .open_in_place(
                                Nonce::try_assume_unique_for_key(nonce_bytes).unwrap(),
                                Aad::empty(),
                                &mut in_out,
                            )
                            .unwrap();

                        assert_eq!(plaintext, in_out[..plaintext.len()]);

                        opening_key
                            .open_within(
                                         Nonce::try_assume_unique_for_key(nonce_bytes).unwrap(),
                                         Aad::empty(),
                                         &mut offset_cipher_text,
                                         4..)
                            .unwrap();
                        assert_eq!(plaintext, offset_cipher_text[..plaintext.len()]);
                    }
                }
            }
        };
    }

    test_tls_aead!(
        aes_128_gcm_tls12,
        &AES_128_GCM,
        TlsProtocolId::TLS12,
        TEST_128_BIT_KEY,
        &16,
        &12
    );
    test_tls_aead!(
        aes_128_gcm_tls13,
        &AES_128_GCM,
        TlsProtocolId::TLS13,
        TEST_128_BIT_KEY,
        &16,
        &12
    );
    test_tls_aead!(
        aes_256_gcm_tls12,
        &AES_256_GCM,
        TlsProtocolId::TLS12,
        TEST_256_BIT_KEY,
        &16,
        &12
    );
    test_tls_aead!(
        aes_256_gcm_tls13,
        &AES_256_GCM,
        TlsProtocolId::TLS13,
        TEST_256_BIT_KEY,
        &16,
        &12
    );
    test_tls_aead!(
        chacha20_poly1305_tls12,
        &CHACHA20_POLY1305,
        TlsProtocolId::TLS12,
        TEST_256_BIT_KEY
    );
    test_tls_aead!(
        chacha20_poly1305_tls13,
        &CHACHA20_POLY1305,
        TlsProtocolId::TLS13,
        TEST_256_BIT_KEY
    );
}
