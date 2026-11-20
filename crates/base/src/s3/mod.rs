//! An overlay's base read from S3 objects (§4.15 "An S3 base", A-464; Mantle's CP-T10.5): the pure core, with no I/O
//! and no platform code, so every host tests it alike.
//!
//! - [`sigv4`]: AWS Signature Version 4 for S3 requests, the canonical request, the string to sign, the signing key
//!   and the signature, held to the four worked examples of AWS's S3 signing page (golden vectors).
//! - [`list`]: one `ListObjectsV2` response page read into its objects, its common prefixes and its continuation, its
//!   length checked against the page's cap before a byte is parsed, every hostile shape refused typed.
//! - [`keys`]: the keys under a base prefix mapped to a directory tree, or one directory's entries from a delimited
//!   page, each key that names no POSIX path (an empty or a dot component, a NUL, a name or key too long, a file a
//!   directory shadows) refused by its kind, so no key leaves the base or aliases another.
//!
//! - [`http`]: HTTP/1.1 framing, a request's head written and a response read as its bytes arrive, every length checked
//!   against its cap before it is held, chunked and length bodies alike, every hostile shape refused by its kind.
//! - [`wait`]: how long a request waits and how long it pauses before trying again, learned from the path's round trips
//!   within the bounds the caller states, as A-442's learned wait.
//! - [`tls`]: rustls' client over aws-lc-rs, driven sans-io beneath the framing.
//! - [`client`]: `HeadObject`, ranged `GetObject` bound by `If-Match` or a version id, and `ListObjectsV2` pages,
//!   signed, sent over the runtime's TCP, every answer typed (A-471).
//! - [`time`]: the calendar the signing timestamp is written in and S3's `LastModified` read in.
//!
//! The base itself (A-524, part 3):
//! - [`name`]: an S3 base as the library names it (endpoint, region, bucket, prefix, addressing), refused by its kind
//!   when no request could read it, and its identity, the device of its fingerprints.
//! - [`host`]: the read-only host seam answered from the base's objects, sans-io: a miss parked as a pending answer
//!   naming its fetch, a directory its delimited listing, a file bound to its object by its ETag and version, ranges in
//!   the volume's chunks within a budgeted cache, every window learned from the path's round trips.
//! - [`fetch`]: what the host asks of its store and the store's answer, run through the network client or the simulated
//!   store alike ([`fetch::Store`]).
//! - [`driver`]: the fetch driver, the volume's owner shard's task that runs the host's fetches against the store and
//!   hands each answer back, woken by the host when a fetch is wanted and ended when the host is (A-555).
//! - [`sim`]: `SimS3`, the simulated store that is the host's oracle: versioned objects, an outsider's puts and
//!   deletes, truncated listings and the typed refusals, as S3 answers them.

pub mod client;
pub mod driver;
pub mod fetch;
pub mod host;
pub mod http;
pub mod keys;
pub mod list;
pub mod name;
pub mod sigv4;
pub mod sim;
pub mod time;
pub mod tls;
pub mod wait;
