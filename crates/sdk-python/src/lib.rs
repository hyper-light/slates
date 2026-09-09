//! The Python SDK (§2.3, D-19; Phase 5): a PyO3 extension over the typed [`slates_client::Client`],
//! so an agent drives slates from Python through the same rings and completion records the Rust client
//! uses — never a parallel reimplementation (the design rejects that: "Lost: parallel pure-Python/TS
//! compatibility implementations"). This is the **synchronous base** the design says the async form
//! wraps (R6): every method here is one `Client` call; the async form over the completion descriptor
//! (fd-readiness on Linux, the parked-reply wake elsewhere) is owed and will drive these same calls off
//! the event loop.
//!
//! The binding is thin and honest: each verb maps one-to-one to a client method, a typed refusal
//! becomes a [`SlatesError`] carrying the refusal's text (richer per-refusal exception subclasses are
//! owed — the message preserves the kind today), and ids cross as hex (a volume) or an integer (a
//! snapshot) so Python holds a plain value. The class is `unsendable`: the underlying client is pinned
//! to the thread that connected it (its rings are single-consumer), so Python must use it from that
//! thread — PyO3 enforces this rather than allowing an unsound cross-thread move.
//!
//! Grants are deliberately absent (R10): the SDK has no verb that creates a landing grant, exactly as
//! the design requires — a grant is made only by a human at the CLI or a confirmation surface, never by
//! an agent answering its own question.

// PyO3 0.22's `#[pymethods]`/`#[pymodule]`/`#[pyclass]` macros generate `unsafe fn` bodies that call
// PyO3's own unsafe helpers without an inner `unsafe` block, and identity conversions on `PyErr` —
// patterns the workspace's edition-2024 strict lints reject in *generated* code. This crate's own code
// contains no `unsafe` and no needless conversions (verified by reading every line below); these two
// allows cover only the macro expansions, and nothing else in the crate relaxes the safety lints.
#![allow(unsafe_op_in_unsafe_fn)]
#![allow(clippy::useless_conversion)]

// The FFI boundary is unsafe by PyO3's signature; every block PyO3 generates carries its own safety.
// Our code adds none. A ClientError cannot cross into Python as a Rust type, so it crosses as a
// message string on `SlatesError` — the one place a typed error becomes text, and the reason is here.

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::PyDict;
use slates_client::{
  Client as RustClient, ClientError, CreateSpec, Deadlines, NamePolicy, SizeClass, VolumeId,
};

/// Format: a volume id is 16 bytes on the wire — its high half names the creator host (§4.8 "Lookup").
const VOLUME_ID_BYTES: usize = 16;
/// The hex characters a volume id renders to — two per byte.
const VOLUME_ID_HEX: usize = VOLUME_ID_BYTES * 2;
/// Format: hexadecimal has sixteen digits — the radix a byte's two hex digits parse under.
const HEX_RADIX: u32 = 16;

pyo3::create_exception!(
  slates,
  SlatesError,
  PyRuntimeError,
  "A refused or failed slates operation (the client's typed refusal, as text)."
);

/// Turns a client refusal into the SDK's Python exception, preserving the refusal's text so the kind
/// is legible to Python (per-refusal exception subclasses are owed).
fn refusal(error: ClientError) -> PyErr {
  SlatesError::new_err(format!("{error:?}"))
}

/// Renders a volume id as lowercase hex — the plain value Python holds and passes back. Uses the
/// standard two-hex-digit-per-byte formatter, so there is no digit arithmetic to get wrong.
fn volume_hex(id: &VolumeId) -> String {
  let mut out = String::with_capacity(VOLUME_ID_HEX);
  for byte in id.bytes {
    out.push_str(&format!("{byte:02x}"));
  }
  out
}

/// Parses a volume id from the hex a prior call returned, refusing a wrong length or a non-hex digit
/// with a typed error (hostile input crossing from Python — never a panic). Each byte is two hex
/// digits, decoded by the standard radix parser.
fn parse_volume(hex: &str) -> PyResult<VolumeId> {
  if hex.len() != VOLUME_ID_HEX {
    return Err(SlatesError::new_err(format!(
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
      .map_err(|_| SlatesError::new_err(format!("not a hex byte: {pair:?}")))?;
  }
  Ok(VolumeId { bytes })
}

/// A connected slates client (§4.4, §4.9): the lifecycle verbs as methods. Constructed by
/// [`Client::connect`]; pinned to the connecting thread (`unsendable`).
#[pyclass(unsendable)]
struct Client {
  inner: RustClient,
}

#[pymethods]
impl Client {
  /// Connects to the named daemon `instance` as a new client (§4.7 rendezvous). `reply_ns` is how
  /// long a reply is awaited before the daemon's liveness is questioned, and `reconnect_ns` how long a
  /// reconnect is tried after it is found gone — both derived by the caller from the machine's budgets
  /// (`slates_client.Deadlines.derive` in Rust; exposed here as explicit nanoseconds).
  #[staticmethod]
  #[pyo3(signature = (instance, reply_ns, reconnect_ns))]
  fn connect(instance: &str, reply_ns: u64, reconnect_ns: u64) -> PyResult<Client> {
    let deadlines = Deadlines {
      reply_ns,
      reconnect_ns,
    };
    let inner = RustClient::connect(instance, deadlines).map_err(refusal)?;
    Ok(Client { inner })
  }

  /// The client id the daemon bound to this session.
  fn client_id(&self) -> u32 {
    self.inner.client_id()
  }

  /// Creates a volume and returns its id as hex (§4.4). `size_bytes` is the bound (a `Bounded` reserve)
  /// or the maximum (a `Dynamic` volume grown from measured rates) per `dynamic`; `fold` selects the
  /// name-equivalence policy (folded like APFS, or byte-exact); `base` overlays a host directory, or a
  /// scratch volume when absent.
  #[pyo3(signature = (name, size_bytes, dynamic=false, fold=true, require_locked=false, base=None))]
  fn create(
    &mut self,
    name: String,
    size_bytes: u64,
    dynamic: bool,
    fold: bool,
    require_locked: bool,
    base: Option<String>,
  ) -> PyResult<String> {
    let size = if dynamic {
      SizeClass::Dynamic { max: size_bytes }
    } else {
      SizeClass::Bounded { limit: size_bytes }
    };
    let names = if fold {
      NamePolicy::Fold
    } else {
      NamePolicy::Exact
    };
    let spec = CreateSpec {
      name,
      size,
      names,
      require_locked,
      base,
    };
    let id = self.inner.create(&spec).map_err(refusal)?;
    Ok(volume_hex(&id))
  }

  /// Takes a snapshot of the volume named by its hex id and returns the snapshot's sequence (§4.4).
  fn snapshot(&mut self, volume: &str) -> PyResult<u64> {
    let id = parse_volume(volume)?;
    let taken = self.inner.snapshot(id).map_err(refusal)?;
    Ok(taken.value)
  }

  /// Reads the volume's status (§4.4) as a dict of plain Python values — the same fields `slates status`
  /// prints: its id and name, the byte accounting (`referenced_bytes`, `unique_bytes`), the held lease
  /// epoch (`None` when unheld), live `attachments`, the `head` snapshot sequence, `snapshots` held, the
  /// overlay `watcher` state, and the `drifted` overlay paths (the full list; the CLI shows only the
  /// count). The placement (§4.8, D-18) is flattened: `placed` is whether the head is placed in the
  /// region, `mirror_age_ns` the mirror's lag (`None` where no mirror exists), `host_epoch` the owner's
  /// authority. `nfs_port` is the daemon's loopback port for `mount_nfs`, or `None` when it serves no NFS.
  fn status(&mut self, py: Python<'_>, volume: &str) -> PyResult<Py<PyDict>> {
    let id = parse_volume(volume)?;
    let report = self.inner.status(id).map_err(refusal)?;
    let dict = PyDict::new_bound(py);
    dict.set_item("id", volume_hex(&report.id))?;
    dict.set_item("name", report.name)?;
    dict.set_item("referenced_bytes", report.referenced_bytes)?;
    dict.set_item("unique_bytes", report.unique_bytes)?;
    dict.set_item("lease_epoch", report.lease_epoch)?;
    dict.set_item("attachments", report.attachments)?;
    dict.set_item("head", report.head.value)?;
    dict.set_item("snapshots", report.snapshots)?;
    dict.set_item("watcher", report.watcher)?;
    dict.set_item("drifted", report.drifted)?;
    dict.set_item("nfs_port", report.nfs_port)?;
    dict.set_item("placed", report.placed.region)?;
    dict.set_item("mirror_age_ns", report.placed.mirror_age_ns)?;
    dict.set_item("host_epoch", report.placed.host_epoch)?;
    Ok(dict.into())
  }

  /// How many times this client has reconnected across daemon restarts (an observability counter, so a
  /// test can assert a session survived a restart — §4.9).
  fn reconnects(&self) -> u64 {
    self.inner.reconnects()
  }
}

/// The `slates` extension module: the [`Client`] class and the SDK version.
#[pymodule]
fn slates(module: &Bound<'_, PyModule>) -> PyResult<()> {
  module.add_class::<Client>()?;
  module.add("SlatesError", module.py().get_type_bound::<SlatesError>())?;
  module.add("__version__", env!("CARGO_PKG_VERSION"))?;
  Ok(())
}
