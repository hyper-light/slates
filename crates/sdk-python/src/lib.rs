//! The Python SDK (§2.3, D-19; Phase 5): a PyO3 extension over the typed [`slates_client::Client`],
//! so an agent drives slates from Python through the same rings and completion records the Rust client
//! uses — never a parallel reimplementation (the design rejects that: "Lost: parallel pure-Python/TS
//! compatibility implementations"). Two client classes over the one daemon (R6, D-19): [`AsyncClient`]
//! is the **async-primary** form — each verb an `async` method a real `asyncio` loop drives to
//! completion by the completion fd's readiness (`loop.add_reader`), never blocking the loop, over the
//! `slates-client` async core; [`Client`] is the **thin blocking facade**, each method one `Client`
//! call. No external runtime — no `tokio`, no `pyo3-asyncio`.
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

// The no-panic law (CLAUDE.md, banned item 6): shipped code never indexes or slices out of bounds, never
// slices a string off a character boundary, and never overflows. Test builds are exempt. Once a crate is
// clean this holds it there.
#![cfg_attr(
  not(test),
  deny(
    clippy::indexing_slicing,
    clippy::string_slice,
    clippy::arithmetic_side_effects
  )
)]
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
use std::time::Duration;

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};
use slates_client::driver::{Begin, Driver, Event, Ticket};
use slates_client::{
  Client as RustClient, ClientError, Connecting, CreateSpec, Deadlines, Filter, Landing,
  NamePolicy, Rebased, RequestId, SizeClass, SnapshotId, StatusReport, Submitted, VolumeId,
  VolumeSummary, WorkOp,
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
  // Two hex digits per byte, taken as bytes: the length is checked above, and a pair that is not two ASCII
  // hex digits (a multi-byte character among them) is refused, never sliced through.
  for (byte, pair) in bytes.iter_mut().zip(hex.as_bytes().chunks(2)) {
    let digits = std::str::from_utf8(pair).unwrap_or_default();
    *byte = u8::from_str_radix(digits, HEX_RADIX)
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
  let mut out = String::with_capacity(bytes.len().saturating_mul(2));
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
/// Builds the status dict — the fields `slates status` prints — from a report. Shared by the sync
/// `status` verb and the async one, so both surfaces render the identical shape (§4.4).
fn status_dict(py: Python<'_>, report: StatusReport) -> PyResult<Py<PyDict>> {
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

/// Builds a volume-list entry dict from a summary — the fields `slates list` prints. Shared by the
/// sync `list` verb and the async one, so both surfaces render the identical shape (§4.4).
fn summary_dict(py: Python<'_>, volume: VolumeSummary) -> PyResult<Py<PyDict>> {
  let dict = PyDict::new_bound(py);
  dict.set_item("id", volume_hex(&volume.id))?;
  dict.set_item("name", volume.name)?;
  dict.set_item("referenced_bytes", volume.referenced_bytes)?;
  dict.set_item("unique_bytes", volume.unique_bytes)?;
  dict.set_item("overlay", volume.overlay)?;
  Ok(dict.into())
}

/// A Python list of entry dicts from the volume summaries — the async `list`'s value (the sync verb
/// returns a `Vec<Py<PyDict>>`, which crosses as the same list shape).
fn summaries_to_py(py: Python<'_>, volumes: Vec<VolumeSummary>) -> PyResult<PyObject> {
  let list = PyList::empty_bound(py);
  for volume in volumes {
    list.append(summary_dict(py, volume)?)?;
  }
  Ok(list.into_any().unbind())
}

/// Builds a work dict from its id and base version — the shape `create_work` returns. Shared by the
/// sync and async verbs (§4.16).
fn work_dict(py: Python<'_>, id: VolumeId, base: u64) -> PyResult<Py<PyDict>> {
  let dict = PyDict::new_bound(py);
  dict.set_item("id", volume_hex(&id))?;
  dict.set_item("base", base)?;
  Ok(dict.into())
}

/// Builds a submit-outcome dict from the typed outcome — `ok`, the new `version`, and any `conflicts`
/// windows (§4.16). Shared by the sync and async `submit` verbs, so both render one shape.
fn submitted_to_py(py: Python<'_>, outcome: Submitted) -> PyResult<PyObject> {
  let dict = match outcome {
    Submitted::Accepted(version) => merge_outcome_dict(py, Some(version), Vec::new())?,
    Submitted::Conflict(windows) => merge_outcome_dict(
      py,
      None,
      windows
        .into_iter()
        .map(|window| (window.path, window.at, window.len, window.class))
        .collect(),
    )?,
  };
  Ok(dict.into_any())
}

/// Builds a rebase-outcome dict from the typed outcome — the same `{ok, version, conflicts}` shape as
/// a submit (§4.16). Shared by the sync and async `rebase` verbs.
fn rebased_to_py(py: Python<'_>, outcome: Rebased) -> PyResult<PyObject> {
  let dict = match outcome {
    Rebased::Rebased(version) => merge_outcome_dict(py, Some(version), Vec::new())?,
    Rebased::Conflict(windows) => merge_outcome_dict(
      py,
      None,
      windows
        .into_iter()
        .map(|window| (window.path, window.at, window.len, window.class))
        .collect(),
    )?,
  };
  Ok(dict.into_any())
}

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
    status_dict(py, report)
  }

  /// Lists the daemon's volumes (§4.4) as a list of dicts — each with its `id` (hex), `name`, the byte
  /// accounting (`referenced_bytes`, `unique_bytes`), and whether it is an `overlay` of a host directory.
  /// The plain shape `slates list` prints, one dict per volume.
  fn list(&mut self, py: Python<'_>) -> PyResult<Vec<Py<PyDict>>> {
    let volumes = self.inner.list().map_err(refusal)?;
    volumes
      .into_iter()
      .map(|volume| summary_dict(py, volume))
      .collect()
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
    work_dict(py, id, base)
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
  fn submit(&mut self, py: Python<'_>, work: &str) -> PyResult<Py<PyAny>> {
    let work = parse_volume(work)?;
    let outcome = self.inner.submit(work).map_err(refusal)?;
    submitted_to_py(py, outcome)
  }

  /// Rebases a work volume onto its green's head (§4.16), mapping its pending edits forward without
  /// committing to the green — the same outcome shape as [`Client::submit`]: `ok` with the head
  /// `version` it now sits on, or the `conflicts` to resolve first.
  fn rebase(&mut self, py: Python<'_>, work: &str) -> PyResult<Py<PyAny>> {
    let work = parse_volume(work)?;
    let outcome = self.inner.rebase(work).map_err(refusal)?;
    rebased_to_py(py, outcome)
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

/// The reply shape a pending async request decodes to when the completion fd signals — the async
/// pump dispatches on it to turn the typed reply into the same Python value the sync verb returns.
#[derive(Clone, Copy)]
enum Decode {
  /// A created volume's id, as hex.
  Created,
  /// A snapshot's sequence.
  Snapshotted,
  /// A status dict.
  Status,
  /// A list of volume-entry dicts.
  Listed,
  /// A resize's confirmation (resolves to `None`).
  Resized,
  /// A destroy's confirmation (resolves to `None`).
  Destroyed,
  /// A created green's id, as hex.
  GreenCreated,
  /// A created work's `{id, base}` dict.
  WorkCreated,
  /// An edit's confirmation (resolves to `None`).
  Edited,
  /// A submit's outcome dict.
  Submitted,
  /// A green's head version.
  Versions,
  /// A list of changed paths.
  Changed,
  /// A rebase's outcome dict.
  Rebased,
  /// A namespace declaration's confirmation (resolves to `None`).
  Declared,
  /// A landing's result dict.
  Landed,
}

/// A call in flight on the async client: the future its `await` suspends on, and how to decode its reply
/// once the driver reports it landed.
struct Pending {
  future: Py<PyAny>,
  decode: Decode,
}

/// An already-resolved awaitable for the async fast path: `await` returns the value without touching
/// the event loop (§4.7 worked example — the reply came within the spin, so no `add_reader`, no
/// suspend). It is its own iterator, raising `StopIteration(value)` at once, the coroutine protocol's
/// return channel.
#[pyclass]
struct Ready {
  value: Option<PyObject>,
}

#[pymethods]
impl Ready {
  fn __await__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
    slf
  }

  fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
    slf
  }

  fn __next__(&mut self, py: Python<'_>) -> PyResult<PyObject> {
    let value = self.value.take().unwrap_or_else(|| py.None());
    Err(pyo3::exceptions::PyStopIteration::new_err(value))
  }
}

/// The async client (§2.3, R6, D-19): the same typed verbs as [`Client`], each an `async` method a
/// Python event loop drives to completion by the completion fd's readiness (`loop.add_reader`). The
/// fast path returns within the daemon's spin window without touching the loop; the slow path arms
/// the completion signal, registers the fd, and resolves the awaiting future when the reply lands.
/// One reader per client serves every call in flight. The sync [`Client`] is the thin blocking facade
/// over the same daemon; this is the primary async form.
///
/// Every call goes through the client's [`Driver`] (AUD-29-19, AUD-29-20): sent when the client admits
/// it and queued otherwise, never waiting; past the bound refused `TooManyOutstanding` at once. Every
/// call ends: answered; refused typed; `Stalled` at its reply deadline; recovered across a daemon
/// restart (resent under its own id) or failed `DaemonGone` when no daemon answers within the reconnect
/// budget; `CompletionLost` when the completion reader fails. The loop's timer (`call_later`) drives the
/// deadlines and recovery; a cancelled await releases its call.
#[pyclass(unsendable)]
struct AsyncClient {
  inner: RustClient,
  /// The calls' admission, queue, deadlines and recovery.
  driver: Driver,
  /// Calls in flight, by driver ticket.
  pending: HashMap<Ticket, Pending>,
  /// The completion handle registered with the loop's reader set, and the channel (the client's
  /// reconnect count) it belongs to; `None` while no reader is registered. A reconnect gives the client
  /// a new channel with its own handle, so the reader moves with it. An `i64` holds both a Unix fd and a
  /// Windows completion `SOCKET` (D-10): on Windows the loop must be a `SelectorEventLoop`, whose
  /// `add_reader` accepts a socket — the default Proactor loop has no `add_reader` (a caller sets
  /// `WindowsSelectorEventLoopPolicy`).
  reader: Option<(i64, u64)>,
  /// The running loop, held to move or remove the reader and to set the timer.
  event_loop: Option<Py<PyAny>>,
  /// The loop's timer handle for the driver's next wake.
  timer: Option<Py<PyAny>>,
  /// Why the completion reader was lost, once it was: every call then failed with it, and every later
  /// call is refused with it.
  broken: Option<String>,
}

impl AsyncClient {
  /// The async client over a connected `inner`.
  fn over(inner: RustClient) -> AsyncClient {
    let driver = Driver::new(&inner);
    AsyncClient {
      inner,
      driver,
      pending: HashMap::new(),
      reader: None,
      event_loop: None,
      timer: None,
      broken: None,
    }
  }

  /// Submits one call through the driver: sent now when the client admits it, else queued — never
  /// waiting — then, only when it was sent, spun for its reply within the daemon's window (the fast
  /// path). The ticket the pending call is keyed by, and the decoded reply when it landed in the spin;
  /// a reply taken or refused in the spin ends the call here.
  fn begin_spin(
    &mut self,
    begin: Begin,
    spin: impl FnOnce(&mut RustClient, RequestId, u64) -> PyResult<Option<PyObject>>,
  ) -> PyResult<(Ticket, Option<PyObject>)> {
    self.begin_spin_with(begin, spin, false)
  }

  /// [`Self::begin_spin`], waiting for a deferred reply while the daemon lives when `patient` (a granted
  /// landing; `slates_client::defers_reply`).
  fn begin_spin_with(
    &mut self,
    begin: Begin,
    spin: impl FnOnce(&mut RustClient, RequestId, u64) -> PyResult<Option<PyObject>>,
    patient: bool,
  ) -> PyResult<(Ticket, Option<PyObject>)> {
    if let Some(reason) = &self.broken {
      return Err(refusal(ClientError::CompletionLost {
        reason: reason.clone(),
      }));
    }
    let ticket = if patient {
      self.driver.submit_patient(&mut self.inner, begin)
    } else {
      self.driver.submit(&mut self.inner, begin)
    }
    .map_err(refusal)?;
    self.inner.begin_ack_if_due().map_err(refusal)?;
    let Some(word) = self.driver.word_of(ticket) else {
      return Ok((ticket, None));
    };
    let spin_ns = self.inner.published_spin_ns();
    let fast = spin(&mut self.inner, RequestId::from_word(word), spin_ns);
    if !matches!(fast, Ok(None)) {
      self.driver.finish(ticket);
    }
    Ok((ticket, fast?))
  }
}

#[pymethods]
impl AsyncClient {
  /// Connects to the named daemon `instance` as a new async client, without holding the loop
  /// (AUD-29-19): the rendezvous claim is made at once and its answer read on the loop's timers at the
  /// pacing the client asks for. Awaitable; raises `SlatesError` when the daemon refuses the claim or
  /// leaves it unanswered past the claim wait. `reply_ns`/`reconnect_ns` are the same derived budgets
  /// [`Client.connect`] takes.
  #[staticmethod]
  #[pyo3(signature = (instance, reply_ns, reconnect_ns))]
  fn connect(
    py: Python<'_>,
    instance: &str,
    reply_ns: u64,
    reconnect_ns: u64,
  ) -> PyResult<PyObject> {
    let deadlines = Deadlines {
      reply_ns,
      reconnect_ns,
    };
    let connecting = RustClient::begin_connect(instance, deadlines).map_err(refusal)?;
    let event_loop = py
      .import_bound("asyncio")?
      .call_method0("get_running_loop")?;
    let future = event_loop.call_method0("create_future")?;
    let step = Bound::new(
      py,
      ConnectStep {
        connecting: Some(connecting),
        future: future.clone().unbind(),
        event_loop: event_loop.unbind(),
      },
    )?;
    step.call0()?;
    Ok(future.unbind())
  }

  /// The client id the daemon bound to this session.
  fn client_id(&self) -> u32 {
    self.inner.client_id()
  }

  /// How many times the client has reconnected across daemon restarts (§4.9) — each a new channel, which
  /// the loop's reader moves to.
  fn reconnects(&self) -> u64 {
    self.inner.reconnects()
  }

  /// The calls the client admits outstanding at once; the driver queues as many more behind them, and
  /// refuses `TooManyOutstanding` past that.
  fn outstanding_limit(&self) -> usize {
    self.inner.outstanding_limit()
  }

  /// Creates a volume and returns its id as hex (§4.4) — the async form of [`Client.create`].
  #[pyo3(signature = (name, size_bytes, dynamic=false, fold=true, require_locked=false, base=None))]
  fn create<'py>(
    slf: Bound<'py, Self>,
    name: String,
    size_bytes: u64,
    dynamic: bool,
    fold: bool,
    require_locked: bool,
    base: Option<String>,
  ) -> PyResult<Bound<'py, PyAny>> {
    let py = slf.py();
    let spec = build_spec(name, size_bytes, dynamic, fold, require_locked, base);
    let (ticket, fast) = slf.borrow_mut().begin_spin(
      Box::new(move |c: &mut RustClient| c.create_begin(&spec)),
      |c: &mut RustClient, id: RequestId, spin: u64| {
        let fast = c
          .create_spin(id, spin)
          .map_err(refusal)?
          .map(|volume| volume_hex(&volume).into_py(py));
        Ok(fast)
      },
    )?;
    finish(&slf, ticket, fast, Decode::Created)
  }

  /// Takes a snapshot of the volume named by its hex id and returns the snapshot's sequence (§4.4) —
  /// the async form of [`Client.snapshot`].
  fn snapshot<'py>(slf: Bound<'py, Self>, volume: &str) -> PyResult<Bound<'py, PyAny>> {
    let py = slf.py();
    let id = parse_volume(volume)?;
    let (ticket, fast) = slf.borrow_mut().begin_spin(
      Box::new(move |c: &mut RustClient| c.snapshot_begin(id)),
      |c: &mut RustClient, request: RequestId, spin: u64| {
        let fast = c
          .snapshot_spin(request, spin)
          .map_err(refusal)?
          .map(|snapshot| snapshot.value.into_py(py));
        Ok(fast)
      },
    )?;
    finish(&slf, ticket, fast, Decode::Snapshotted)
  }

  /// Reads the volume's status as a dict (§4.4) — the async form of [`Client.status`], the identical
  /// shape.
  fn status<'py>(slf: Bound<'py, Self>, volume: &str) -> PyResult<Bound<'py, PyAny>> {
    let py = slf.py();
    let id = parse_volume(volume)?;
    let (ticket, fast) = slf.borrow_mut().begin_spin(
      Box::new(move |c: &mut RustClient| c.status_begin(id)),
      |c: &mut RustClient, request: RequestId, spin: u64| {
        let fast = match c.status_spin(request, spin).map_err(refusal)? {
          Some(report) => Some(status_dict(py, report)?.into_any()),
          None => None,
        };
        Ok(fast)
      },
    )?;
    finish(&slf, ticket, fast, Decode::Status)
  }

  /// Lists the daemon's volumes as a list of dicts (§4.4) — the async form of [`Client.list`].
  fn list<'py>(slf: Bound<'py, Self>) -> PyResult<Bound<'py, PyAny>> {
    let py = slf.py();
    let (ticket, fast) = slf.borrow_mut().begin_spin(
      Box::new(move |c: &mut RustClient| c.list_begin()),
      |c: &mut RustClient, request: RequestId, spin: u64| {
        let fast = match c.list_spin(request, spin).map_err(refusal)? {
          Some(volumes) => Some(summaries_to_py(py, volumes)?),
          None => None,
        };
        Ok(fast)
      },
    )?;
    finish(&slf, ticket, fast, Decode::Listed)
  }

  /// Resizes the volume's quota (§4.4) — the async form of [`Client.resize`]; resolves to `None`.
  #[pyo3(signature = (volume, size_bytes, dynamic=false))]
  fn resize<'py>(
    slf: Bound<'py, Self>,
    volume: &str,
    size_bytes: u64,
    dynamic: bool,
  ) -> PyResult<Bound<'py, PyAny>> {
    let py = slf.py();
    let id = parse_volume(volume)?;
    let size = if dynamic {
      SizeClass::Dynamic { max: size_bytes }
    } else {
      SizeClass::Bounded { limit: size_bytes }
    };
    let (ticket, fast) = slf.borrow_mut().begin_spin(
      Box::new(move |c: &mut RustClient| c.resize_begin(id, size)),
      |c: &mut RustClient, request: RequestId, spin: u64| {
        let fast = c
          .resize_spin(request, spin)
          .map_err(refusal)?
          .map(|_done| py.None());
        Ok(fast)
      },
    )?;
    finish(&slf, ticket, fast, Decode::Resized)
  }

  /// Destroys the volume and reclaims its memory (§4.4) — the async form of [`Client.destroy`];
  /// resolves to `None`.
  fn destroy<'py>(slf: Bound<'py, Self>, volume: &str) -> PyResult<Bound<'py, PyAny>> {
    let py = slf.py();
    let id = parse_volume(volume)?;
    let (ticket, fast) = slf.borrow_mut().begin_spin(
      Box::new(move |c: &mut RustClient| c.destroy_begin(id)),
      |c: &mut RustClient, request: RequestId, spin: u64| {
        let fast = c
          .destroy_spin(request, spin)
          .map_err(refusal)?
          .map(|_done| py.None());
        Ok(fast)
      },
    )?;
    finish(&slf, ticket, fast, Decode::Destroyed)
  }

  /// Creates a green merge target (§4.16) — the async form of [`Client.create_green`].
  #[pyo3(signature = (name, require_evidence=false))]
  fn create_green<'py>(
    slf: Bound<'py, Self>,
    name: &str,
    require_evidence: bool,
  ) -> PyResult<Bound<'py, PyAny>> {
    let py = slf.py();
    let (ticket, fast) = slf.borrow_mut().begin_spin(
      {
        let name = name.to_owned();
        Box::new(move |c: &mut RustClient| c.create_green_begin(&name, require_evidence))
      },
      |c: &mut RustClient, request: RequestId, spin: u64| {
        let fast = c
          .create_green_spin(request, spin)
          .map_err(refusal)?
          .map(|volume| volume_hex(&volume).into_py(py));
        Ok(fast)
      },
    )?;
    finish(&slf, ticket, fast, Decode::GreenCreated)
  }

  /// Creates a work volume over a green (§4.16) as a `{id, base}` dict — the async form of
  /// [`Client.create_work`].
  fn create_work<'py>(
    slf: Bound<'py, Self>,
    green: &str,
    name: &str,
  ) -> PyResult<Bound<'py, PyAny>> {
    let py = slf.py();
    let green = parse_volume(green)?;
    let (ticket, fast) = slf.borrow_mut().begin_spin(
      {
        let name = name.to_owned();
        Box::new(move |c: &mut RustClient| c.create_work_begin(green, &name))
      },
      |c: &mut RustClient, request: RequestId, spin: u64| {
        let fast = match c.create_work_spin(request, spin).map_err(refusal)? {
          Some((id, base)) => Some(work_dict(py, id, base)?.into_any()),
          None => None,
        };
        Ok(fast)
      },
    )?;
    finish(&slf, ticket, fast, Decode::WorkCreated)
  }

  /// Declares a content edit on a work volume (§4.16) — the async form of [`Client.edit`]; resolves
  /// to `None`.
  fn edit<'py>(
    slf: Bound<'py, Self>,
    work: &str,
    path: &str,
    at: u64,
    delete_len: u64,
    data: &[u8],
  ) -> PyResult<Bound<'py, PyAny>> {
    let py = slf.py();
    let work = parse_volume(work)?;
    let (ticket, fast) = slf.borrow_mut().begin_spin(
      {
        let (path, data) = (path.to_owned(), data.to_vec());
        Box::new(move |c: &mut RustClient| c.edit_begin(work, &path, at, delete_len, &data))
      },
      |c: &mut RustClient, request: RequestId, spin: u64| {
        let fast = c
          .edit_spin(request, spin)
          .map_err(refusal)?
          .map(|_done| py.None());
        Ok(fast)
      },
    )?;
    finish(&slf, ticket, fast, Decode::Edited)
  }

  /// Submits a work volume's operations to its green (§4.16) as an outcome dict — the async form of
  /// [`Client.submit`].
  fn submit<'py>(slf: Bound<'py, Self>, work: &str) -> PyResult<Bound<'py, PyAny>> {
    let py = slf.py();
    let work = parse_volume(work)?;
    let (ticket, fast) = slf.borrow_mut().begin_spin(
      Box::new(move |c: &mut RustClient| c.submit_begin(work)),
      |c: &mut RustClient, request: RequestId, spin: u64| {
        let fast = match c.submit_spin(request, spin).map_err(refusal)? {
          Some(outcome) => Some(submitted_to_py(py, outcome)?),
          None => None,
        };
        Ok(fast)
      },
    )?;
    finish(&slf, ticket, fast, Decode::Submitted)
  }

  /// A green's head version (§4.16) — the async form of [`Client.versions`].
  fn versions<'py>(slf: Bound<'py, Self>, green: &str) -> PyResult<Bound<'py, PyAny>> {
    let py = slf.py();
    let green = parse_volume(green)?;
    let (ticket, fast) = slf.borrow_mut().begin_spin(
      Box::new(move |c: &mut RustClient| c.versions_begin(green)),
      |c: &mut RustClient, request: RequestId, spin: u64| {
        let fast = c
          .versions_spin(request, spin)
          .map_err(refusal)?
          .map(|head| head.into_py(py));
        Ok(fast)
      },
    )?;
    finish(&slf, ticket, fast, Decode::Versions)
  }

  /// The files a green changed strictly after `version` (§4.16) — the async form of
  /// [`Client.changed_since`].
  fn changed_since<'py>(
    slf: Bound<'py, Self>,
    green: &str,
    version: u64,
  ) -> PyResult<Bound<'py, PyAny>> {
    let py = slf.py();
    let green = parse_volume(green)?;
    let (ticket, fast) = slf.borrow_mut().begin_spin(
      Box::new(move |c: &mut RustClient| c.changed_since_begin(green, version)),
      |c: &mut RustClient, request: RequestId, spin: u64| {
        let fast = c
          .changed_since_spin(request, spin)
          .map_err(refusal)?
          .map(|paths| paths.into_py(py));
        Ok(fast)
      },
    )?;
    finish(&slf, ticket, fast, Decode::Changed)
  }

  /// Rebases a work volume onto its green's head (§4.16) as an outcome dict — the async form of
  /// [`Client.rebase`].
  fn rebase<'py>(slf: Bound<'py, Self>, work: &str) -> PyResult<Bound<'py, PyAny>> {
    let py = slf.py();
    let work = parse_volume(work)?;
    let (ticket, fast) = slf.borrow_mut().begin_spin(
      Box::new(move |c: &mut RustClient| c.rebase_begin(work)),
      |c: &mut RustClient, request: RequestId, spin: u64| {
        let fast = match c.rebase_spin(request, spin).map_err(refusal)? {
          Some(outcome) => Some(rebased_to_py(py, outcome)?),
          None => None,
        };
        Ok(fast)
      },
    )?;
    finish(&slf, ticket, fast, Decode::Rebased)
  }

  // The namespace operations (§4.16) — the async form of [`Client`]'s, each declaring one `WorkOp` on
  // a work volume and resolving to `None`. Each builds its op and defers to `declare_async`; the
  // `WorkOp` enum stays inside the SDK, never crossing the FFI.

  /// Removes the name at `path` on a work volume (§4.16).
  fn unlink<'py>(slf: Bound<'py, Self>, work: &str, path: &str) -> PyResult<Bound<'py, PyAny>> {
    let work = parse_volume(work)?;
    Self::declare_async(
      slf,
      work,
      WorkOp::Unlink {
        path: path.to_owned(),
      },
    )
  }

  /// Renames `from` to `to` on a work volume (§4.16).
  fn rename<'py>(
    slf: Bound<'py, Self>,
    work: &str,
    from: &str,
    to: &str,
  ) -> PyResult<Bound<'py, PyAny>> {
    let work = parse_volume(work)?;
    Self::declare_async(
      slf,
      work,
      WorkOp::Rename {
        from: from.to_owned(),
        to: to.to_owned(),
      },
    )
  }

  /// Creates an empty directory at `path` on a work volume (§4.16).
  fn mkdir<'py>(slf: Bound<'py, Self>, work: &str, path: &str) -> PyResult<Bound<'py, PyAny>> {
    let work = parse_volume(work)?;
    Self::declare_async(
      slf,
      work,
      WorkOp::Mkdir {
        path: path.to_owned(),
      },
    )
  }

  /// Removes the empty directory at `path` on a work volume (§4.16).
  fn rmdir<'py>(slf: Bound<'py, Self>, work: &str, path: &str) -> PyResult<Bound<'py, PyAny>> {
    let work = parse_volume(work)?;
    Self::declare_async(
      slf,
      work,
      WorkOp::Rmdir {
        path: path.to_owned(),
      },
    )
  }

  /// Sets the mode bits at `path` on a work volume (§4.16).
  fn chmod<'py>(
    slf: Bound<'py, Self>,
    work: &str,
    path: &str,
    mode: u32,
  ) -> PyResult<Bound<'py, PyAny>> {
    let work = parse_volume(work)?;
    Self::declare_async(
      slf,
      work,
      WorkOp::SetMode {
        path: path.to_owned(),
        mode,
      },
    )
  }

  /// Creates a symbolic link at `path` pointing at `target` on a work volume (§4.16).
  fn symlink<'py>(
    slf: Bound<'py, Self>,
    work: &str,
    path: &str,
    target: &str,
  ) -> PyResult<Bound<'py, PyAny>> {
    let work = parse_volume(work)?;
    Self::declare_async(
      slf,
      work,
      WorkOp::Symlink {
        path: path.to_owned(),
        target: target.to_owned(),
      },
    )
  }

  /// Creates a hard link at `path` to `target` on a work volume (§4.16).
  fn link<'py>(
    slf: Bound<'py, Self>,
    work: &str,
    path: &str,
    target: &str,
  ) -> PyResult<Bound<'py, PyAny>> {
    let work = parse_volume(work)?;
    Self::declare_async(
      slf,
      work,
      WorkOp::Link {
        path: path.to_owned(),
        target: target.to_owned(),
      },
    )
  }

  /// Sets the extended attribute `name` to `value` at `path` on a work volume (§4.16).
  fn set_xattr<'py>(
    slf: Bound<'py, Self>,
    work: &str,
    path: &str,
    name: &str,
    value: &[u8],
  ) -> PyResult<Bound<'py, PyAny>> {
    let work = parse_volume(work)?;
    Self::declare_async(
      slf,
      work,
      WorkOp::SetXattr {
        path: path.to_owned(),
        name: name.to_owned(),
        value: value.to_vec(),
      },
    )
  }

  /// Removes the extended attribute `name` at `path` on a work volume (§4.16).
  fn remove_xattr<'py>(
    slf: Bound<'py, Self>,
    work: &str,
    path: &str,
    name: &str,
  ) -> PyResult<Bound<'py, PyAny>> {
    let work = parse_volume(work)?;
    Self::declare_async(
      slf,
      work,
      WorkOp::RemoveXattr {
        path: path.to_owned(),
        name: name.to_owned(),
      },
    )
  }

  /// Executes a landing (§4.15) — the async form of [`Client.land`]. Resolves to a dict: the finished
  /// `outcome`, or `grant_required` with the exact `slates grant` command a human runs. The SDK creates
  /// no grant itself (R10) — a landing without a satisfying `grant` comes back for a human to authorize.
  #[pyo3(signature = (volume, target, snapshot=None, include=None, exclude=None, grant=None))]
  fn land<'py>(
    slf: Bound<'py, Self>,
    volume: &str,
    target: &str,
    snapshot: Option<u64>,
    include: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
    grant: Option<u64>,
  ) -> PyResult<Bound<'py, PyAny>> {
    let py = slf.py();
    let id = parse_volume(volume)?;
    let snap = snapshot.map(|value| SnapshotId { value });
    let filter = Filter {
      include: include.unwrap_or_default(),
      exclude: exclude.unwrap_or_default(),
    };
    let (ticket, fast) = slf.borrow_mut().begin_spin_with(
      {
        let target = target.to_owned();
        Box::new(move |c: &mut RustClient| c.land_begin(id, snap, &target, filter.clone(), grant))
      },
      |c: &mut RustClient, request: RequestId, spin: u64| {
        let fast = match c.land_spin(request, spin).map_err(refusal)? {
          Some(landing) => Some(landing_dict(py, landing)?.into_any()),
          None => None,
        };
        Ok(fast)
      },
      grant.is_some(),
    )?;
    finish(&slf, ticket, fast, Decode::Landed)
  }

  /// The event loop's reader callback: drain the completion fd, take every landed reply and admit
  /// waiting calls. Never raises (an exception in a reader callback is only logged by the loop): a
  /// failure loses the reader and fails every call with it.
  fn _pump(slf: Bound<'_, Self>) {
    let outcome = {
      let mut this = slf.borrow_mut();
      this.inner.drain_completion();
      let this = &mut *this;
      Ok(this.driver.pump(&mut this.inner))
    };
    settle_or_lose(&slf, outcome);
  }

  /// The loop's timer callback: the driver's deadlines and recovery. Never raises, as [`Self::_pump`].
  fn _tick(slf: Bound<'_, Self>) {
    let outcome = {
      let mut this = slf.borrow_mut();
      this.timer = None;
      let this = &mut *this;
      Ok(this.driver.tick(&mut this.inner))
    };
    settle_or_lose(&slf, outcome);
  }
}

impl AsyncClient {
  /// Declares one `WorkOp` on a work volume and resolves to `None` — the async namespace verbs' shared
  /// core. Not a `#[pymethods]` verb: `WorkOp` is a Rust enum that does not cross the FFI, so this stays
  /// a private helper the ergonomic verbs above defer to (the sync `Client`'s `declare` shape).
  fn declare_async<'py>(
    slf: Bound<'py, Self>,
    work: VolumeId,
    op: WorkOp,
  ) -> PyResult<Bound<'py, PyAny>> {
    let py = slf.py();
    let (ticket, fast) = slf.borrow_mut().begin_spin(
      Box::new(move |c: &mut RustClient| c.declare_begin(work, op.clone())),
      |c: &mut RustClient, request: RequestId, spin: u64| {
        let fast = c
          .declare_spin(request, spin)
          .map_err(refusal)?
          .map(|_done| py.None());
        Ok(fast)
      },
    )?;
    finish(&slf, ticket, fast, Decode::Declared)
  }
}

/// Builds a create spec from the Python arguments.
fn build_spec(
  name: String,
  size_bytes: u64,
  dynamic: bool,
  fold: bool,
  require_locked: bool,
  base: Option<String>,
) -> CreateSpec {
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
  CreateSpec {
    name,
    size,
    names,
    require_locked,
    base,
  }
}

/// Resolves an async call: the fast path returns a ready awaitable (no event loop); otherwise the call
/// is recorded under its ticket with a future to await, released if that future is cancelled, and the
/// reader, the arm and the timer are set for it.
fn finish<'py>(
  slf: &Bound<'py, AsyncClient>,
  ticket: Ticket,
  fast: Option<PyObject>,
  decode: Decode,
) -> PyResult<Bound<'py, PyAny>> {
  let py = slf.py();
  if let Some(value) = fast {
    return Ok(Bound::new(py, Ready { value: Some(value) })?.into_any());
  }
  let event_loop = py
    .import_bound("asyncio")?
    .call_method0("get_running_loop")?;
  let future = event_loop.call_method0("create_future")?;
  {
    let mut this = slf.borrow_mut();
    this.pending.insert(
      ticket,
      Pending {
        future: future.clone().unbind(),
        decode,
      },
    );
    if this.event_loop.is_none() {
      this.event_loop = Some(event_loop.unbind());
    }
  }
  let hook = Bound::new(
    py,
    CancelHook {
      client: slf.clone().unbind(),
      ticket,
    },
  )?;
  future.call_method1("add_done_callback", (hook,))?;
  let outcome = attach_and_pump(slf);
  settle_or_lose(slf, outcome);
  Ok(future)
}

/// Releases a call whose future was cancelled (AUD-29-20): queued, it is never sent; in flight, its
/// reply is dropped when it comes. A done callback on every pending future; a future the client
/// settled itself is not cancelled, so the hook leaves it.
#[pyclass(unsendable)]
struct CancelHook {
  client: Py<AsyncClient>,
  ticket: Ticket,
}

#[pymethods]
impl CancelHook {
  fn __call__(&self, py: Python<'_>, future: Bound<'_, PyAny>) -> PyResult<()> {
    if !future.call_method0("cancelled")?.extract::<bool>()? {
      return Ok(());
    }
    let client = self.client.bind(py);
    {
      let mut this = client.borrow_mut();
      if this.pending.remove(&self.ticket).is_some() {
        let this = &mut *this;
        this.driver.cancel(&mut this.inner, self.ticket);
      }
    }
    // Never raises into the loop (a done callback's exception is only logged).
    settle_or_lose(client, Ok(Vec::new()));
    Ok(())
  }
}

/// One step of an async connect on the loop: polls the claim's answer, resolves the future with the
/// connected client or the refusal, else sets the loop's timer for the next look. A cancelled connect
/// drops its claim, which gives it back.
#[pyclass(unsendable)]
struct ConnectStep {
  connecting: Option<Connecting>,
  future: Py<PyAny>,
  event_loop: Py<PyAny>,
}

#[pymethods]
impl ConnectStep {
  fn __call__(slf: Bound<'_, Self>) -> PyResult<()> {
    let py = slf.py();
    let future = slf.borrow().future.clone_ref(py).into_bound(py);
    if future.call_method0("done")?.extract::<bool>()? {
      slf.borrow_mut().connecting = None;
      return Ok(());
    }
    let polled = match slf.borrow_mut().connecting.as_mut() {
      Some(connecting) => connecting.poll(),
      None => return Ok(()),
    };
    match polled {
      Ok(Some(inner)) => {
        slf.borrow_mut().connecting = None;
        let client = Bound::new(py, AsyncClient::over(inner))?;
        future.call_method1("set_result", (client,))?;
      }
      Ok(None) => {
        let wait_ns = match slf.borrow_mut().connecting.as_mut() {
          Some(connecting) => connecting.next_poll_ns(),
          None => return Ok(()),
        };
        let event_loop = slf.borrow().event_loop.clone_ref(py).into_bound(py);
        event_loop.call_method1(
          "call_later",
          (Duration::from_nanos(wait_ns).as_secs_f64(), slf.clone()),
        )?;
      }
      Err(error) => {
        slf.borrow_mut().connecting = None;
        future.call_method1("set_exception", (refusal(error).into_value(py),))?;
      }
    }
    Ok(())
  }
}

/// Arms the completion signal, moves the loop's reader to the client's current channel when a
/// reconnect gave it a new one, and takes what has landed — in that order, so a reply that landed
/// before the arm is taken by this pump rather than lost.
fn attach_and_pump(slf: &Bound<'_, AsyncClient>) -> PyResult<Vec<Event>> {
  let py = slf.py();
  let channel = slf.borrow().inner.reconnects();
  let current = slf.borrow().reader;
  if current.map(|(_, on)| on) != Some(channel) {
    let event_loop = slf
      .borrow()
      .event_loop
      .as_ref()
      .map(|event_loop| event_loop.clone_ref(py));
    let Some(event_loop) = event_loop else {
      return Ok(Vec::new());
    };
    let event_loop = event_loop.bind(py);
    if let Some((old, _)) = current {
      // The old channel's handle closed with it; the selector forgets a closed descriptor's key.
      event_loop.call_method1("remove_reader", (old,))?;
      slf.borrow_mut().reader = None;
    }
    let fd = {
      let mut this = slf.borrow_mut();
      let handle = this.inner.enable_async_completion().map_err(refusal)?;
      // A Unix fd is an `i32`; a Windows completion `SOCKET` is a kernel handle-table value that fits
      // in an `i64`. Both cross to the loop's `add_reader` as one integer.
      #[cfg(unix)]
      let fd = i64::from(handle);
      #[cfg(windows)]
      let fd = i64::try_from(handle).unwrap_or(i64::MAX);
      fd
    };
    event_loop.call_method1("add_reader", (fd, slf.getattr("_pump")?))?;
    slf.borrow_mut().reader = Some((fd, channel));
  }
  let mut this = slf.borrow_mut();
  this.inner.arm_async().map_err(refusal)?;
  let this = &mut *this;
  Ok(this.driver.pump(&mut this.inner))
}

/// Settles `outcome`'s events, or — when taking them failed — loses the reader: every call fails
/// `CompletionLost` and later calls are refused with it. The reader and timer callbacks end here, so
/// nothing raises into the loop.
fn settle_or_lose(slf: &Bound<'_, AsyncClient>, outcome: PyResult<Vec<Event>>) {
  let failure = match outcome {
    Ok(events) => match settle(slf, events) {
      Ok(()) => return,
      Err(error) => error,
    },
    Err(error) => error,
  };
  lose(slf, &failure.to_string());
}

/// Ends every call with `CompletionLost { reason }` and refuses later ones with it. Best effort by
/// construction: the client is already failing, and every call learns that through its own future.
fn lose(slf: &Bound<'_, AsyncClient>, reason: &str) {
  let py = slf.py();
  let lost = ClientError::CompletionLost {
    reason: reason.to_owned(),
  };
  let pending: Vec<Pending> = {
    let mut this = slf.borrow_mut();
    if this.broken.is_none() {
      this.broken = Some(reason.to_owned());
    }
    let this = &mut *this;
    let _ = this.driver.fail_all(&mut this.inner, &lost);
    this.pending.drain().map(|(_, pending)| pending).collect()
  };
  for entry in pending {
    let future = entry.future.bind(py);
    // A future already done (cancelled) needs nothing; any other refusal here leaves nothing to tell.
    if !future
      .call_method0("done")
      .and_then(|done| done.extract::<bool>())
      .unwrap_or(true)
    {
      let _ = future.call_method1("set_exception", (refusal(lost.clone()).into_value(py),));
    }
  }
  let _ = idle(slf);
}

/// Settles the driver's events — a landed reply is decoded by its verb's poll and resolves its call, a
/// failure raises its typed error into it — then moves the reader if the client reconnected, and sets
/// the timer for the driver's next wake (or goes idle when no call waits).
fn settle(slf: &Bound<'_, AsyncClient>, events: Vec<Event>) -> PyResult<()> {
  let py = slf.py();
  let mut events = events;
  loop {
    for event in events {
      settle_one(slf, event)?;
    }
    if slf.borrow().pending.is_empty() {
      return idle(slf);
    }
    let moved = {
      let this = slf.borrow();
      this.broken.is_none() && this.reader.map(|(_, on)| on) != Some(this.inner.reconnects())
    };
    if !moved {
      break;
    }
    events = attach_and_pump(slf)?;
  }
  schedule(slf, py)
}

/// Settles one driver event into its pending future.
fn settle_one(slf: &Bound<'_, AsyncClient>, event: Event) -> PyResult<()> {
  let py = slf.py();
  let (ticket, outcome) = match event {
    Event::Failed { ticket, error } => (ticket, Err(refusal(error))),
    Event::Ready { ticket, word } => {
      let Some(decode) = slf.borrow().pending.get(&ticket).map(|entry| entry.decode) else {
        return Ok(());
      };
      let decoded = {
        let mut this = slf.borrow_mut();
        decode_pending(py, &mut this.inner, word, decode)
      };
      match decoded {
        Ok(None) => return Ok(()),
        Ok(Some(value)) => (ticket, Ok(value)),
        Err(error) => (ticket, Err(error)),
      }
    }
  };
  let entry = {
    let mut this = slf.borrow_mut();
    this.driver.finish(ticket);
    this.pending.remove(&ticket)
  };
  let Some(entry) = entry else {
    return Ok(());
  };
  let future = entry.future.bind(py);
  if future.call_method0("done")?.extract::<bool>()? {
    return Ok(());
  }
  match outcome {
    Ok(value) => future.call_method1("set_result", (value,))?,
    Err(error) => future.call_method1("set_exception", (error.into_value(py),))?,
  };
  Ok(())
}

/// Sets the one timer for the driver's next wake (`call_later`); none when the driver has no call.
fn schedule(slf: &Bound<'_, AsyncClient>, py: Python<'_>) -> PyResult<()> {
  cancel_timer(slf, py)?;
  let wake = {
    let this = slf.borrow();
    this.driver.next_wake_ns(&this.inner)
  };
  let event_loop = slf
    .borrow()
    .event_loop
    .as_ref()
    .map(|event_loop| event_loop.clone_ref(py));
  let (Some(wake), Some(event_loop)) = (wake, event_loop) else {
    return Ok(());
  };
  let handle = event_loop.bind(py).call_method1(
    "call_later",
    (
      Duration::from_nanos(wake).as_secs_f64(),
      slf.getattr("_tick")?,
    ),
  )?;
  slf.borrow_mut().timer = Some(handle.unbind());
  Ok(())
}

/// Cancels the pending timer, if any.
fn cancel_timer(slf: &Bound<'_, AsyncClient>, py: Python<'_>) -> PyResult<()> {
  let timer = slf.borrow_mut().timer.take();
  if let Some(timer) = timer {
    timer.bind(py).call_method0("cancel")?;
  }
  Ok(())
}

/// No call waits: the timer, the arm and the reader go.
fn idle(slf: &Bound<'_, AsyncClient>) -> PyResult<()> {
  let py = slf.py();
  cancel_timer(slf, py)?;
  if slf.borrow().broken.is_none() {
    slf.borrow_mut().inner.disarm_async().map_err(refusal)?;
  }
  let remove = {
    let this = slf.borrow();
    this
      .event_loop
      .as_ref()
      .map(|event_loop| event_loop.clone_ref(py))
      .zip(this.reader)
  };
  if let Some((event_loop, (fd, _))) = remove {
    event_loop.bind(py).call_method1("remove_reader", (fd,))?;
    slf.borrow_mut().reader = None;
  }
  Ok(())
}

/// Decodes a landed reply for `word` into the Python value its verb returns (the same shapes the sync
/// verbs render).
fn decode_pending(
  py: Python<'_>,
  inner: &mut RustClient,
  word: u64,
  decode: Decode,
) -> PyResult<Option<PyObject>> {
  Ok(match decode {
    Decode::Created => inner
      .create_poll(word)
      .map_err(refusal)?
      .map(|volume| volume_hex(&volume).into_py(py)),
    Decode::Snapshotted => inner
      .snapshot_poll(word)
      .map_err(refusal)?
      .map(|snapshot| snapshot.value.into_py(py)),
    Decode::Status => match inner.status_poll(word).map_err(refusal)? {
      Some(report) => Some(status_dict(py, report)?.into_any()),
      None => None,
    },
    Decode::Listed => match inner.list_poll(word).map_err(refusal)? {
      Some(volumes) => Some(summaries_to_py(py, volumes)?),
      None => None,
    },
    Decode::Resized => inner
      .resize_poll(word)
      .map_err(refusal)?
      .map(|_done| py.None()),
    Decode::Destroyed => inner
      .destroy_poll(word)
      .map_err(refusal)?
      .map(|_done| py.None()),
    Decode::GreenCreated => inner
      .create_green_poll(word)
      .map_err(refusal)?
      .map(|volume| volume_hex(&volume).into_py(py)),
    Decode::WorkCreated => match inner.create_work_poll(word).map_err(refusal)? {
      Some((id, base)) => Some(work_dict(py, id, base)?.into_any()),
      None => None,
    },
    Decode::Edited => inner
      .edit_poll(word)
      .map_err(refusal)?
      .map(|_done| py.None()),
    Decode::Submitted => match inner.submit_poll(word).map_err(refusal)? {
      Some(outcome) => Some(submitted_to_py(py, outcome)?),
      None => None,
    },
    Decode::Versions => inner
      .versions_poll(word)
      .map_err(refusal)?
      .map(|head| head.into_py(py)),
    Decode::Changed => inner
      .changed_since_poll(word)
      .map_err(refusal)?
      .map(|paths| paths.into_py(py)),
    Decode::Rebased => match inner.rebase_poll(word).map_err(refusal)? {
      Some(outcome) => Some(rebased_to_py(py, outcome)?),
      None => None,
    },
    Decode::Declared => inner
      .declare_poll(word)
      .map_err(refusal)?
      .map(|_done| py.None()),
    Decode::Landed => match inner.land_poll(word).map_err(refusal)? {
      Some(landing) => Some(landing_dict(py, landing)?.into_any()),
      None => None,
    },
  })
}

/// The `slates` extension module: the [`Client`] and [`AsyncClient`] classes and the SDK version.
#[pymodule]
fn slates(module: &Bound<'_, PyModule>) -> PyResult<()> {
  module.add_class::<Client>()?;
  module.add_class::<AsyncClient>()?;
  module.add("SlatesError", module.py().get_type_bound::<SlatesError>())?;
  module.add("__version__", env!("CARGO_PKG_VERSION"))?;
  Ok(())
}
