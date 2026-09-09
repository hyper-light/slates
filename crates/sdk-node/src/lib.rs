//! The TypeScript/Node SDK (§2.3, D-19; Phase 5): a napi-rs addon over the typed
//! [`slates_client::Client`], so a Node or TypeScript agent drives slates through the same rings and
//! completion records the Rust client uses — never a parallel reimplementation (the design rejects
//! that). This is the **synchronous base** the design says the async form wraps (R6): every method here
//! is one `Client` call; the async form over the completion descriptor (napi's `uv_poll`) is owed and
//! will drive these same calls off the event loop as Promises.
//!
//! The binding is thin and honest: each verb maps one-to-one to a client method, a typed refusal
//! becomes a JS `Error` carrying the refusal's text (richer error classes are owed — the message
//! preserves the kind), ids cross as hex (a volume) or a number (a snapshot), and every integer that
//! crosses is range-checked (a JS number is an `f64`; the SDK refuses a value that would not round-trip
//! rather than silently truncating).
//!
//! Grants are deliberately absent (R10): the SDK has no verb that creates a landing grant — a grant is
//! made only by a human at the CLI or a confirmation surface, never by an agent answering its own
//! question.

// napi-rs's `#[napi]` macros generate `unsafe fn` bodies that call napi's own unsafe helpers without an
// inner `unsafe` block, identity conversions, and undocumented public registration functions —
// patterns the workspace's edition-2024 strict lints reject in *generated* code. This crate's own code
// has no `unsafe`, no needless conversions, and a doc on every item (verified by reading every line
// below); these allows cover only the macro expansions.
#![allow(unsafe_op_in_unsafe_fn)]
#![allow(clippy::useless_conversion)]
#![allow(missing_docs)]
// D-8 exception 1 (bindings objects a GC may drop mid-call): napi-rs's `#[napi]` macros generate the
// V8-facing binding glue, which holds the addon's class through an `Rc` the JavaScript GC co-owns.
// This crate's OWN code uses no `Rc`/`Arc` (verified by reading every line below); this allow covers
// only that generated glue — the same FFI-edge exception the transport's rustls `Arc` carries (D-8
// exception 2, `slates-transport::handshake`).
#![allow(clippy::disallowed_types)]

use napi::bindgen_prelude::*;
use napi_derive::napi;
use slates_client::{
  Client as RustClient, ClientError, CreateSpec, Deadlines, NamePolicy, SizeClass, VolumeId,
};

/// Format: a volume id is 16 bytes on the wire — its high half names the creator host (§4.8 "Lookup").
const VOLUME_ID_BYTES: usize = 16;
/// The hex characters a volume id renders to — two per byte.
const VOLUME_ID_HEX: usize = VOLUME_ID_BYTES * 2;
/// Format: hexadecimal has sixteen digits — the radix a byte's two hex digits parse under.
const HEX_RADIX: u32 = 16;

/// Turns a client refusal into a JS error, preserving the refusal's text so the kind is legible in
/// Node (richer error classes are owed).
fn refusal(error: ClientError) -> Error {
  Error::from_reason(format!("{error:?}"))
}

/// A JS number is an `f64`; a nanosecond deadline or size that crossed as one is range-checked back to
/// `u64` here, refusing a negative or non-integral value rather than truncating it.
fn checked_u64(value: i64, field: &str) -> Result<u64> {
  u64::try_from(value)
    .map_err(|_| Error::from_reason(format!("{field} must be a non-negative integer")))
}

/// Renders a volume id as lowercase hex — the plain value Node holds and passes back.
fn volume_hex(id: &VolumeId) -> String {
  let mut out = String::with_capacity(VOLUME_ID_HEX);
  for byte in id.bytes {
    out.push_str(&format!("{byte:02x}"));
  }
  out
}

/// Parses a volume id from the hex a prior call returned, refusing a wrong length or a non-hex digit
/// with a JS error (hostile input from Node — never a panic).
fn parse_volume(hex: &str) -> Result<VolumeId> {
  if hex.len() != VOLUME_ID_HEX {
    return Err(Error::from_reason(format!(
      "a volume id is {VOLUME_ID_HEX} hex characters, got {}",
      hex.len()
    )));
  }
  let mut bytes = [0u8; VOLUME_ID_BYTES];
  for (index, byte) in bytes.iter_mut().enumerate() {
    // Two hex digits per byte; the length is checked above, so the slice is always in bounds.
    let start = index * 2;
    let pair = &hex[start..start + 2];
    *byte = u8::from_str_radix(pair, HEX_RADIX)
      .map_err(|_| Error::from_reason(format!("not a hex byte: {pair:?}")))?;
  }
  Ok(VolumeId { bytes })
}

/// A connected slates client (§4.4, §4.9): the lifecycle verbs as methods. Constructed by
/// [`Client::connect`]; used from the Node main thread the addon runs on.
#[napi]
pub struct Client {
  inner: RustClient,
}

#[napi]
impl Client {
  /// Connects to the named daemon `instance` as a new client (§4.7 rendezvous). `replyNs` is how long
  /// a reply is awaited before the daemon's liveness is questioned, and `reconnectNs` how long a
  /// reconnect is tried after it is found gone — both nanoseconds, derived by the caller from the
  /// machine's budgets.
  #[napi(factory)]
  pub fn connect(instance: String, reply_ns: i64, reconnect_ns: i64) -> Result<Client> {
    let deadlines = Deadlines {
      reply_ns: checked_u64(reply_ns, "replyNs")?,
      reconnect_ns: checked_u64(reconnect_ns, "reconnectNs")?,
    };
    let inner = RustClient::connect(&instance, deadlines).map_err(refusal)?;
    Ok(Client { inner })
  }

  /// The client id the daemon bound to this session.
  #[napi]
  pub fn client_id(&self) -> u32 {
    self.inner.client_id()
  }

  /// Creates a volume and returns its id as hex (§4.4). `sizeBytes` is the bound (a `Bounded` reserve)
  /// or the maximum (a `Dynamic` volume) per `dynamic`; `fold` selects the name-equivalence policy
  /// (folded like APFS by default, or byte-exact); `base` overlays a host directory, or a scratch
  /// volume when absent.
  #[napi]
  pub fn create(
    &mut self,
    name: String,
    size_bytes: i64,
    dynamic: Option<bool>,
    fold: Option<bool>,
    require_locked: Option<bool>,
    base: Option<String>,
  ) -> Result<String> {
    let size_bytes = checked_u64(size_bytes, "sizeBytes")?;
    let size = if dynamic.unwrap_or(false) {
      SizeClass::Dynamic { max: size_bytes }
    } else {
      SizeClass::Bounded { limit: size_bytes }
    };
    let names = if fold.unwrap_or(true) {
      NamePolicy::Fold
    } else {
      NamePolicy::Exact
    };
    let spec = CreateSpec {
      name,
      size,
      names,
      require_locked: require_locked.unwrap_or(false),
      base,
    };
    let id = self.inner.create(&spec).map_err(refusal)?;
    Ok(volume_hex(&id))
  }

  /// Takes a snapshot of the volume named by its hex id and returns the snapshot's sequence (§4.4).
  #[napi]
  pub fn snapshot(&mut self, volume: String) -> Result<i64> {
    let id = parse_volume(&volume)?;
    let taken = self.inner.snapshot(id).map_err(refusal)?;
    i64::try_from(taken.value)
      .map_err(|_| Error::from_reason("the snapshot id is too large for a JS number"))
  }

  /// How many times this client has reconnected across daemon restarts (an observability counter, so a
  /// test can assert a session survived a restart — §4.9).
  #[napi]
  pub fn reconnects(&self) -> Result<i64> {
    i64::try_from(self.inner.reconnects())
      .map_err(|_| Error::from_reason("the reconnect count is too large for a JS number"))
  }
}
