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

use std::collections::HashMap;

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::PyDict;
use slates_client::{
  Client as RustClient, ClientError, CreateSpec, Deadlines, Filter, Landing, NamePolicy, Rebased,
  SizeClass, SnapshotId, StatusReport, Submitted, VolumeId, WorkOp,
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

/// Builds the uniform merge-outcome dict for `submit`/`rebase` (§4.16): `ok` — whether the increment
/// landed cleanly (a new green version) rather than conflicting; `version` — the green version produced,
/// or `None` on conflict; `conflicts` — the windows to resolve, each a dict of `path`, `at`, `len` (base
/// coordinates) and `class` (the conflict-class discriminant). Windows arrive as plain tuples so this
/// needs no protocol type — `ok` is exactly "a version was produced".
fn merge_outcome_dict(
  py: Python<'_>,
  version: Option<u64>,
  windows: Vec<(String, u64, u64, u8)>,
) -> PyResult<Py<PyDict>> {
  let dict = PyDict::new_bound(py);
  dict.set_item("ok", version.is_some())?;
  dict.set_item("version", version)?;
  let mut conflicts: Vec<Py<PyDict>> = Vec::with_capacity(windows.len());
  for (path, at, len, class) in windows {
    let window = PyDict::new_bound(py);
    window.set_item("path", path)?;
    window.set_item("at", at)?;
    window.set_item("len", len)?;
    window.set_item("class", class)?;
    conflicts.push(window.into());
  }
  dict.set_item("conflicts", conflicts)?;
  Ok(dict.into())
}

/// Format: a 32-byte manifest hash rendered as lowercase hex (64 characters), two digits per byte —
/// the plain value Python holds, with no digit arithmetic to get wrong.
fn hex32(bytes: &[u8; 32]) -> String {
  let mut out = String::with_capacity(bytes.len() * 2);
  for byte in bytes {
    out.push_str(&format!("{byte:02x}"));
  }
  out
}

/// A landing's result as a dict (§4.15): `grant_required` False with the finished `outcome` (its
/// terminal `state` and counts), or True with the `landing` id, the `manifest` hash (hex), the
/// `summary` (entries per action, bytes, filtered-out), the `conflicts` paths, and `grant_with` — the
/// exact `slates grant` command a human runs. The SDK never issues the grant itself (R10): a landing
/// without one comes back `grant_required` for a human to authorize on the CLI.
fn landing_dict(py: Python<'_>, landing: Landing) -> PyResult<Py<PyDict>> {
  let dict = PyDict::new_bound(py);
  match landing {
    Landing::Landed(outcome) => {
      dict.set_item("grant_required", false)?;
      let o = PyDict::new_bound(py);
      o.set_item("landing", outcome.landing)?;
      o.set_item("state", outcome.state)?;
      o.set_item("written", outcome.written)?;
      o.set_item("skipped", outcome.skipped)?;
      o.set_item("conflicts", outcome.conflicts)?;
      o.set_item("failed", outcome.failed)?;
      o.set_item("bytes_written", outcome.bytes_written)?;
      dict.set_item("outcome", o)?;
    }
    Landing::GrantRequired {
      landing,
      manifest,
      summary,
      conflicts,
    } => {
      dict.set_item("grant_required", true)?;
      dict.set_item("landing", landing)?;
      dict.set_item("manifest", hex32(&manifest))?;
      let s = PyDict::new_bound(py);
      let mut by_action: Vec<Py<PyDict>> = Vec::with_capacity(summary.by_action.len());
      for action in summary.by_action {
        let a = PyDict::new_bound(py);
        a.set_item("action", action.action)?;
        a.set_item("count", action.count)?;
        by_action.push(a.into());
      }
      s.set_item("by_action", by_action)?;
      s.set_item("bytes", summary.bytes)?;
      s.set_item("filtered_out", summary.filtered_out)?;
      dict.set_item("summary", s)?;
      dict.set_item("conflicts", conflicts)?;
      dict.set_item("grant_with", format!("slates grant {landing}"))?;
    }
  }
  Ok(dict.into())
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

  /// Lists the daemon's volumes (§4.4) as a list of dicts — each with its `id` (hex), `name`, the byte
  /// accounting (`referenced_bytes`, `unique_bytes`), and whether it is an `overlay` of a host directory.
  /// The plain shape `slates list` prints, one dict per volume.
  fn list(&mut self, py: Python<'_>) -> PyResult<Vec<Py<PyDict>>> {
    let volumes = self.inner.list().map_err(refusal)?;
    let mut out = Vec::with_capacity(volumes.len());
    for volume in volumes {
      let dict = PyDict::new_bound(py);
      dict.set_item("id", volume_hex(&volume.id))?;
      dict.set_item("name", volume.name)?;
      dict.set_item("referenced_bytes", volume.referenced_bytes)?;
      dict.set_item("unique_bytes", volume.unique_bytes)?;
      dict.set_item("overlay", volume.overlay)?;
      out.push(dict.into());
    }
    Ok(out)
  }

  /// Resizes the volume's quota (§4.4): `size_bytes` is the new `Bounded` reserve, or the new maximum of
  /// a `Dynamic` volume when `dynamic` is set. A refusal (a shrink below what is referenced, a class the
  /// volume does not hold) crosses as [`SlatesError`].
  #[pyo3(signature = (volume, size_bytes, dynamic=false))]
  fn resize(&mut self, volume: &str, size_bytes: u64, dynamic: bool) -> PyResult<()> {
    let id = parse_volume(volume)?;
    let size = if dynamic {
      SizeClass::Dynamic { max: size_bytes }
    } else {
      SizeClass::Bounded { limit: size_bytes }
    };
    self.inner.resize(id, size).map_err(refusal)?;
    Ok(())
  }

  /// Destroys the volume (§4.4): its RAM is reclaimed and its id retired. Idempotent verbs do not exist
  /// here — destroying a missing volume is a typed [`SlatesError`], not a silent success.
  fn destroy(&mut self, volume: &str) -> PyResult<()> {
    let id = parse_volume(volume)?;
    self.inner.destroy(id).map_err(refusal)?;
    Ok(())
  }

  /// Plans, and with a grant executes, a landing of the volume's diverged entries onto the host
  /// directory `target` (§4.15). Returns a dict ([`landing_dict`]): without a grant, `grant_required`
  /// True with the manifest and the `slates grant` command a human runs; with one, `grant_required`
  /// False with the finished `outcome`. `snapshot` lands a snapshot (else the head); `include`/`exclude`
  /// are path filters; `grant` is a grant id a human already issued on the CLI (the SDK never issues one,
  /// R10). This is the whole landing surface an agent has: it can plan and, once a human authorizes,
  /// execute — but it cannot authorize.
  #[pyo3(signature = (volume, target, snapshot=None, include=None, exclude=None, grant=None))]
  fn land(
    &mut self,
    volume: &str,
    target: &str,
    snapshot: Option<u64>,
    include: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
    grant: Option<u64>,
  ) -> PyResult<Py<PyDict>> {
    let id = parse_volume(volume)?;
    let snap = snapshot.map(|value| SnapshotId { value });
    let filter = Filter {
      include: include.unwrap_or_default(),
      exclude: exclude.unwrap_or_default(),
    };
    let landing = self
      .inner
      .land(id, snap, target, filter, grant)
      .map_err(refusal)?;
    // The GIL is already held (Python called this method); `with_gil` re-borrows it to build the dict,
    // so `land` stays under the argument-count lint without taking an explicit `py` token.
    Python::with_gil(|py| landing_dict(py, landing))
  }

  /// Creates a green volume — a shared merge target (§4.16) — returning its hex id. `require_evidence`
  /// makes the green refuse an increment that carries no evidence of the base it was derived from.
  #[pyo3(signature = (name, require_evidence=false))]
  fn create_green(&mut self, name: &str, require_evidence: bool) -> PyResult<String> {
    let id = self
      .inner
      .create_green(name, require_evidence)
      .map_err(refusal)?;
    Ok(volume_hex(&id))
  }

  /// Creates a work volume over a green (§4.16), returning a dict of the work's `id` (hex) and the green
  /// `base` version it is based on — the version its edits will be submitted against.
  fn create_work(&mut self, py: Python<'_>, green: &str, name: &str) -> PyResult<Py<PyDict>> {
    let green = parse_volume(green)?;
    let (id, base) = self.inner.create_work(green, name).map_err(refusal)?;
    let dict = PyDict::new_bound(py);
    dict.set_item("id", volume_hex(&id))?;
    dict.set_item("base", base)?;
    Ok(dict.into())
  }

  /// A green's head version (§4.16 merge chain) — the number that advances with each accepted submit.
  fn versions(&mut self, green: &str) -> PyResult<u64> {
    let green = parse_volume(green)?;
    self.inner.versions(green).map_err(refusal)
  }

  /// The files a green changed strictly after `version` (§4.16) — the paths a caller based at `version`
  /// must reconcile before it can submit cleanly.
  fn changed_since(&mut self, green: &str, version: u64) -> PyResult<Vec<String>> {
    let green = parse_volume(green)?;
    self.inner.changed_since(green, version).map_err(refusal)
  }

  /// Declares a content edit on a work volume (§4.16): a splice at `path` — remove `delete_len` bytes at
  /// offset `at`, then insert `data`. The bytes cross from Python as `bytes`. An insert is `delete_len`
  /// zero; an overwrite is a delete and an insert in one call.
  fn edit(
    &mut self,
    work: &str,
    path: &str,
    at: u64,
    delete_len: u64,
    data: &[u8],
  ) -> PyResult<()> {
    let work = parse_volume(work)?;
    self
      .inner
      .edit(work, path, at, delete_len, data)
      .map_err(refusal)?;
    Ok(())
  }

  /// Submits a work volume's declared edits to its green (§4.16), returning the merge outcome (see
  /// [`merge_outcome_dict`]): `ok` with the new green `version` when accepted, or `ok` false with the
  /// `conflicts` windows to rebase against when an intervening change met the same range.
  fn submit(&mut self, py: Python<'_>, work: &str) -> PyResult<Py<PyDict>> {
    let work = parse_volume(work)?;
    match self.inner.submit(work).map_err(refusal)? {
      Submitted::Accepted(version) => merge_outcome_dict(py, Some(version), Vec::new()),
      Submitted::Conflict(windows) => merge_outcome_dict(
        py,
        None,
        windows
          .into_iter()
          .map(|w| (w.path, w.at, w.len, w.class))
          .collect(),
      ),
    }
  }

  /// Rebases a work volume onto its green's head (§4.16), mapping its pending edits forward without
  /// committing to the green — the same outcome shape as [`Client::submit`]: `ok` with the head
  /// `version` it now sits on, or the `conflicts` to resolve first.
  fn rebase(&mut self, py: Python<'_>, work: &str) -> PyResult<Py<PyDict>> {
    let work = parse_volume(work)?;
    match self.inner.rebase(work).map_err(refusal)? {
      Rebased::Rebased(version) => merge_outcome_dict(py, Some(version), Vec::new()),
      Rebased::Conflict(windows) => merge_outcome_dict(
        py,
        None,
        windows
          .into_iter()
          .map(|w| (w.path, w.at, w.len, w.class))
          .collect(),
      ),
    }
  }

  /// Removes the name at `path` on a work volume (§4.16) — a file, symlink or hard link.
  fn unlink(&mut self, work: &str, path: &str) -> PyResult<()> {
    self.declare(
      work,
      WorkOp::Unlink {
        path: path.to_owned(),
      },
    )
  }

  /// Renames `from` to `to` on a work volume (§4.16).
  fn rename(&mut self, work: &str, from: &str, to: &str) -> PyResult<()> {
    self.declare(
      work,
      WorkOp::Rename {
        from: from.to_owned(),
        to: to.to_owned(),
      },
    )
  }

  /// Creates an empty directory at `path` on a work volume (§4.16).
  fn mkdir(&mut self, work: &str, path: &str) -> PyResult<()> {
    self.declare(
      work,
      WorkOp::Mkdir {
        path: path.to_owned(),
      },
    )
  }

  /// Removes the empty directory at `path` on a work volume (§4.16).
  fn rmdir(&mut self, work: &str, path: &str) -> PyResult<()> {
    self.declare(
      work,
      WorkOp::Rmdir {
        path: path.to_owned(),
      },
    )
  }

  /// Sets the mode of the file or directory at `path` on a work volume (§4.16).
  fn chmod(&mut self, work: &str, path: &str, mode: u32) -> PyResult<()> {
    self.declare(
      work,
      WorkOp::SetMode {
        path: path.to_owned(),
        mode,
      },
    )
  }

  /// Creates or retargets a symbolic link at `path` pointing at `target` on a work volume (§4.16).
  fn symlink(&mut self, work: &str, path: &str, target: &str) -> PyResult<()> {
    self.declare(
      work,
      WorkOp::Symlink {
        path: path.to_owned(),
        target: target.to_owned(),
      },
    )
  }

  /// Creates a hard link at `path` to the existing file `target` on a work volume (§4.16).
  fn link(&mut self, work: &str, path: &str, target: &str) -> PyResult<()> {
    self.declare(
      work,
      WorkOp::Link {
        path: path.to_owned(),
        target: target.to_owned(),
      },
    )
  }

  /// Sets the extended attribute `name` on `path` to `value` (bytes) on a work volume (§4.16).
  fn set_xattr(&mut self, work: &str, path: &str, name: &str, value: &[u8]) -> PyResult<()> {
    self.declare(
      work,
      WorkOp::SetXattr {
        path: path.to_owned(),
        name: name.to_owned(),
        value: value.to_vec(),
      },
    )
  }

  /// Removes the extended attribute `name` from `path` on a work volume (§4.16).
  fn remove_xattr(&mut self, work: &str, path: &str, name: &str) -> PyResult<()> {
    self.declare(
      work,
      WorkOp::RemoveXattr {
        path: path.to_owned(),
        name: name.to_owned(),
      },
    )
  }

  /// How many times this client has reconnected across daemon restarts (an observability counter, so a
  /// test can assert a session survived a restart — §4.9).
  fn reconnects(&self) -> u64 {
    self.inner.reconnects()
  }
}

impl Client {
  /// Declares one namespace operation on a work volume (§4.16) — the shared body of the namespace verbs
  /// (`unlink`/`rename`/`mkdir`/…). Private, not a `#[pymethods]` verb: `WorkOp` is a Rust enum that does
  /// not cross into Python, so the SDK exposes the ergonomic verbs above rather than a raw `declare`.
  fn declare(&mut self, work: &str, op: WorkOp) -> PyResult<()> {
    let work = parse_volume(work)?;
    self.inner.declare(work, op).map_err(refusal)?;
    Ok(())
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
