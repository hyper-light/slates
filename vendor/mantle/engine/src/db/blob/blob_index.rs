//! `BlobIndex`: RocksDB's `db/blob/blob_index.h` (docs/research/24 §1.16).
//!
//! The value of a `kTypeBlobIndex` entry, and of a blob column of a version-2 wide-column
//! entity, is one type byte and then:
//!
//! | Type | Layout |
//! |---|---|
//! | 0 inlined with TTL | varint64 expiration, the value's bytes |
//! | 1 blob | varint64 file number, varint64 offset, varint64 size, u8 compression |
//! | 2 blob with TTL | varint64 expiration, then as type 1 |
//!
//! RocksDB's accessors assert the type they need (`file_number()` of an inlined index, say); the
//! port's type is an enum whose accessors return `None` for a field the type does not have.
//! RocksDB's decoder dereferences the first byte of an empty slice; the port refuses it.

use crate::error::{Error, Malformed};
use crate::util::coding::{get_varint64, put_varint64};

/// The type byte [R db/blob/blob_index.h:47-52].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum BlobIndexType {
    /// `kInlinedTTL`: the value itself, with an expiration.
    InlinedTtl = 0,
    /// `kBlob`: a reference into a blob file.
    Blob = 1,
    /// `kBlobTTL`: a reference with an expiration.
    BlobTtl = 2,
}

/// Where a blob lives: `file_number`, `offset` and `size` of its value in a blob file, and the
/// file's compression type byte (`CompressionType`, [R include/rocksdb/compression_type.h]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlobReference {
    pub file_number: u64,
    pub offset: u64,
    pub size: u64,
    pub compression: u8,
}

/// A decoded blob index. An inlined index borrows its value from the bytes it was decoded from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobIndex<'a> {
    InlinedTtl {
        expiration: u64,
        value: &'a [u8],
    },
    Blob(BlobReference),
    BlobTtl {
        expiration: u64,
        reference: BlobReference,
    },
}

/// Replaces the `what` of a decode error, as RocksDB replaces the message of a failed
/// `GetVarint64` with its own.
fn relabel(e: Error, what: &'static str) -> Error {
    match e {
        Error::Corruption { why, .. } => Error::corruption(what, why),
        other => other,
    }
}

impl<'a> BlobIndex<'a> {
    /// `DecodeFrom` [R db/blob/blob_index.h:103-129]. A reference must end with exactly its
    /// compression byte.
    pub fn decode_from(slice: &'a [u8]) -> Result<Self, Error> {
        let (&type_byte, mut rest) = slice.split_first().ok_or(Error::truncated("blob index"))?;
        let expiration = |rest: &mut &'a [u8]| {
            get_varint64(rest).map_err(|e| relabel(e, "blob index expiration"))
        };
        match type_byte {
            t if t == BlobIndexType::InlinedTtl as u8 => {
                let expiration = expiration(&mut rest)?;
                Ok(Self::InlinedTtl {
                    expiration,
                    value: rest,
                })
            }
            t if t == BlobIndexType::Blob as u8 => Ok(Self::Blob(decode_reference(rest)?)),
            t if t == BlobIndexType::BlobTtl as u8 => {
                let expiration = expiration(&mut rest)?;
                Ok(Self::BlobTtl {
                    expiration,
                    reference: decode_reference(rest)?,
                })
            }
            t => Err(Error::corruption(
                "blob index type",
                Malformed::UnknownTag(t),
            )),
        }
    }

    /// The type byte's meaning.
    pub const fn index_type(&self) -> BlobIndexType {
        match self {
            Self::InlinedTtl { .. } => BlobIndexType::InlinedTtl,
            Self::Blob(_) => BlobIndexType::Blob,
            Self::BlobTtl { .. } => BlobIndexType::BlobTtl,
        }
    }

    /// `IsInlined`.
    pub const fn is_inlined(&self) -> bool {
        matches!(self, Self::InlinedTtl { .. })
    }

    /// `HasTTL`.
    pub const fn has_ttl(&self) -> bool {
        matches!(self, Self::InlinedTtl { .. } | Self::BlobTtl { .. })
    }

    /// `expiration()`, for the types that carry one.
    pub const fn expiration(&self) -> Option<u64> {
        match self {
            Self::InlinedTtl { expiration, .. } | Self::BlobTtl { expiration, .. } => {
                Some(*expiration)
            }
            Self::Blob(_) => None,
        }
    }

    /// `value()` of an inlined index.
    pub const fn value(&self) -> Option<&'a [u8]> {
        match self {
            Self::InlinedTtl { value, .. } => Some(value),
            _ => None,
        }
    }

    /// The blob-file reference of a non-inlined index (`file_number()`, `offset()`, `size()`,
    /// `compression()`).
    pub const fn reference(&self) -> Option<BlobReference> {
        match self {
            Self::Blob(reference) | Self::BlobTtl { reference, .. } => Some(*reference),
            Self::InlinedTtl { .. } => None,
        }
    }

    /// `EncodeTo` [R db/blob/blob_index.h:155-164]: replaces `dst` with this index's encoding.
    pub fn encode_to(&self, dst: &mut Vec<u8>) {
        match *self {
            Self::InlinedTtl { expiration, value } => encode_inlined_ttl(dst, expiration, value),
            Self::Blob(r) => encode_blob(dst, r.file_number, r.offset, r.size, r.compression),
            Self::BlobTtl {
                expiration,
                reference: r,
            } => encode_blob_ttl(
                dst,
                expiration,
                r.file_number,
                r.offset,
                r.size,
                r.compression,
            ),
        }
    }
}

/// The three varints and the compression byte of a reference, which must be the last byte.
fn decode_reference(mut rest: &[u8]) -> Result<BlobReference, Error> {
    let what = "blob index offset";
    let file_number = get_varint64(&mut rest).map_err(|e| relabel(e, what))?;
    let offset = get_varint64(&mut rest).map_err(|e| relabel(e, what))?;
    let size = get_varint64(&mut rest).map_err(|e| relabel(e, what))?;
    match rest {
        [compression] => Ok(BlobReference {
            file_number,
            offset,
            size,
            compression: *compression,
        }),
        [] => Err(Error::truncated(what)),
        _ => Err(Error::corruption(what, Malformed::TrailingBytes)),
    }
}

/// `EncodeInlinedTTL` [R db/blob/blob_index.h:166-174]: replaces `dst`.
pub fn encode_inlined_ttl(dst: &mut Vec<u8>, expiration: u64, value: &[u8]) {
    dst.clear();
    dst.push(BlobIndexType::InlinedTtl as u8);
    put_varint64(dst, expiration);
    dst.extend_from_slice(value);
}

/// `EncodeBlob` [R db/blob/blob_index.h:176-187]: replaces `dst`.
pub fn encode_blob(dst: &mut Vec<u8>, file_number: u64, offset: u64, size: u64, compression: u8) {
    dst.clear();
    dst.push(BlobIndexType::Blob as u8);
    put_varint64(dst, file_number);
    put_varint64(dst, offset);
    put_varint64(dst, size);
    dst.push(compression);
}

/// `EncodeBlobTTL` [R db/blob/blob_index.h:189-201]: replaces `dst`.
pub fn encode_blob_ttl(
    dst: &mut Vec<u8>,
    expiration: u64,
    file_number: u64,
    offset: u64,
    size: u64,
    compression: u8,
) {
    dst.clear();
    dst.push(BlobIndexType::BlobTtl as u8);
    put_varint64(dst, expiration);
    put_varint64(dst, file_number);
    put_varint64(dst, offset);
    put_varint64(dst, size);
    dst.push(compression);
}
