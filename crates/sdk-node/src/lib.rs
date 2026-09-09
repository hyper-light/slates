//! The TypeScript/Node SDK (§2.3, D-19; Phase 5): a napi-rs addon over the typed
//! [`slates_client::Client`], so a Node or TypeScript agent drives slates through the same rings and
//! completion records the Rust client uses — never a parallel reimplementation (the design rejects
//! that). Two client surfaces over the one daemon (R6, D-19): the **async-primary** `AsyncClient` (the
//! `async.mjs` JS wrapper over this addon's low-level primitives) returns a Promise per verb resolved
//! by the completion fd's readiness — the fd wrapped in a libuv-polled `net.Socket` (`uv_poll`), never
//! blocking the loop, over the `slates-client` async core; the [`Client`] here is the **thin blocking
//! facade**, each method one `Client` call. No external runtime — no `tokio`.
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
  Client as RustClient, ClientError, CreateSpec, Deadlines, Filter, Landing, NamePolicy, Rebased,
  SizeClass, SnapshotId, StatusReport, Submitted, VolumeId, VolumeSummary, WorkOp,
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

/// A `u64` count crossing to Node as an object field, range-checked to a JS-safe `i64` — a value that
/// would not round-trip is a typed error, never a silent truncation (the SDK's integer rule).
fn status_i64(value: u64, field: &str) -> Result<i64> {
  i64::try_from(value)
    .map_err(|_| Error::from_reason(format!("{field} is too large for a JS number")))
}

/// An optional `u64` status field: `null` in JS when absent, range-checked when present.
fn status_opt_i64(value: Option<u64>, field: &str) -> Result<Option<i64>> {
  value.map(|v| status_i64(v, field)).transpose()
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

/// A volume's status as a plain JS object (§4.4) — the fields `slates status` prints, with napi's
/// camelCase keys (`referencedBytes`, `nfsPort`, `mirrorAgeNs`, `hostEpoch`, …). Ids cross as hex; every
/// `u64` count is range-checked to a JS-safe integer before the object is built; the placement (§4.8,
/// D-18) is flattened to `placed`/`mirrorAgeNs`/`hostEpoch`; `nfsPort` is the daemon's `mount_nfs` port
/// or `null`; `drifted` is the full list of drifted overlay paths (the CLI shows only the count).
#[napi(object)]
pub struct VolumeStatus {
  pub id: String,
  pub name: String,
  pub referenced_bytes: i64,
  pub unique_bytes: i64,
  pub lease_epoch: Option<i64>,
  pub attachments: u32,
  pub head: i64,
  pub snapshots: u32,
  pub watcher: String,
  pub drifted: Vec<String>,
  pub nfs_port: Option<u32>,
  pub placed: bool,
  pub mirror_age_ns: Option<i64>,
  pub host_epoch: i64,
}

/// A volume in a listing (§4.4) as a plain JS object — its id (hex), name, byte accounting, and whether
/// it overlays a host directory. napi's camelCase keys; the u64 counts range-checked to JS-safe integers.
#[napi(object)]
pub struct VolumeEntry {
  pub id: String,
  pub name: String,
  pub referenced_bytes: i64,
  pub unique_bytes: i64,
  pub overlay: bool,
}

/// A work volume created over a green (§4.16): its id (hex) and the green base version its edits submit
/// against. napi camelCase keys.
#[napi(object)]
pub struct WorkVolume {
  pub id: String,
  pub base: i64,
}

/// A merge conflict window (§4.16): the file and the base-coordinate range (`at`, `len`) that met an
/// intervening change, with the conflict-class discriminant. napi camelCase keys.
#[napi(object)]
pub struct ConflictWindow {
  pub path: String,
  pub at: i64,
  pub len: i64,
  pub class: u32,
}

/// The outcome of a `submit` or `rebase` (§4.16): `ok` whether the increment landed cleanly, the green
/// `version` produced (or null on conflict), and the `conflicts` windows to resolve.
#[napi(object)]
pub struct MergeOutcome {
  pub ok: bool,
  pub version: Option<i64>,
  pub conflicts: Vec<ConflictWindow>,
}

/// Builds the uniform merge outcome for `submit`/`rebase`: `ok` is exactly "a version was produced";
/// windows arrive as plain tuples so this needs no protocol type. Every u64 range-checked to a JS int.
fn merge_outcome(
  version: Option<u64>,
  windows: Vec<(String, u64, u64, u8)>,
) -> Result<MergeOutcome> {
  let mut conflicts = Vec::with_capacity(windows.len());
  for (path, at, len, class) in windows {
    conflicts.push(ConflictWindow {
      path,
      at: status_i64(at, "at")?,
      len: status_i64(len, "len")?,
      class: u32::from(class),
    });
  }
  let version = match version {
    Some(value) => Some(status_i64(value, "version")?),
    None => None,
  };
  Ok(MergeOutcome {
    ok: version.is_some(),
    version,
    conflicts,
  })
}

/// One action's entry count in a landing plan (§4.15): the action name and how many entries take it.
#[napi(object)]
pub struct LandingAction {
  pub action: String,
  pub count: i64,
}

/// A landing plan's summary (§4.15): entries per action, bytes to write, and entries the filter left out.
#[napi(object)]
pub struct LandingSummaryJs {
  pub by_action: Vec<LandingAction>,
  pub bytes: i64,
  pub filtered_out: i64,
}

/// A finished landing's outcome (§4.15): its id, terminal `state`, and the per-entry and byte counts.
#[napi(object)]
pub struct LandingOutcomeJs {
  pub landing: i64,
  pub state: String,
  pub written: i64,
  pub skipped: i64,
  pub conflicts: i64,
  pub failed: i64,
  pub bytes_written: i64,
}

/// The result of `land` (§4.15): `grantRequired` false with the finished `outcome`, or true with the
/// `landing` id, the `manifest` hash (hex), the `summary`, the `conflicts` paths, and `grantWith` — the
/// `slates grant` command a human runs. The optional fields are set for exactly one of the two cases.
#[napi(object)]
pub struct LandingResult {
  pub grant_required: bool,
  pub outcome: Option<LandingOutcomeJs>,
  pub landing: Option<i64>,
  pub manifest: Option<String>,
  pub summary: Option<LandingSummaryJs>,
  pub conflicts: Option<Vec<String>>,
  pub grant_with: Option<String>,
}

/// Format: a 32-byte manifest hash rendered as lowercase hex (64 characters), two digits per byte.
fn hex32(bytes: &[u8; 32]) -> String {
  let mut out = String::with_capacity(bytes.len() * 2);
  for byte in bytes {
    out.push_str(&format!("{byte:02x}"));
  }
  out
}

/// Builds the landing result object (§4.15), every `u64` range-checked to a JS-safe integer. A landing
/// without a grant comes back `grantRequired` with the manifest and the `slates grant` command; with a
/// grant, `grantRequired` false with the finished counts. The SDK never issues a grant (R10).
fn landing_result(landing: Landing) -> Result<LandingResult> {
  match landing {
    Landing::Landed(o) => Ok(LandingResult {
      grant_required: false,
      outcome: Some(LandingOutcomeJs {
        landing: status_i64(o.landing, "landing")?,
        state: o.state,
        written: status_i64(o.written, "written")?,
        skipped: status_i64(o.skipped, "skipped")?,
        conflicts: status_i64(o.conflicts, "conflicts")?,
        failed: status_i64(o.failed, "failed")?,
        bytes_written: status_i64(o.bytes_written, "bytesWritten")?,
      }),
      landing: None,
      manifest: None,
      summary: None,
      conflicts: None,
      grant_with: None,
    }),
    Landing::GrantRequired {
      landing,
      manifest,
      summary,
      conflicts,
    } => {
      let mut by_action = Vec::with_capacity(summary.by_action.len());
      for action in summary.by_action {
        by_action.push(LandingAction {
          action: action.action,
          count: status_i64(action.count, "count")?,
        });
      }
      Ok(LandingResult {
        grant_required: true,
        outcome: None,
        landing: Some(status_i64(landing, "landing")?),
        manifest: Some(hex32(&manifest)),
        summary: Some(LandingSummaryJs {
          by_action,
          bytes: status_i64(summary.bytes, "bytes")?,
          filtered_out: status_i64(summary.filtered_out, "filteredOut")?,
        }),
        conflicts: Some(conflicts),
        grant_with: Some(format!("slates grant {landing}")),
      })
    }
  }
}

/// A connected slates client (§4.4, §4.9): the lifecycle verbs as methods. Constructed by
/// [`Client::connect`]; used from the Node main thread the addon runs on.
/// Builds a [`VolumeStatus`] from a report — the same fields `slates status` prints. Shared by the
/// sync `status` verb and the async one, so both surfaces return the identical shape (§4.4).
fn volume_status(report: StatusReport) -> Result<VolumeStatus> {
  Ok(VolumeStatus {
    id: volume_hex(&report.id),
    name: report.name,
    referenced_bytes: status_i64(report.referenced_bytes, "referencedBytes")?,
    unique_bytes: status_i64(report.unique_bytes, "uniqueBytes")?,
    lease_epoch: status_opt_i64(report.lease_epoch, "leaseEpoch")?,
    attachments: report.attachments,
    head: status_i64(report.head.value, "head")?,
    snapshots: report.snapshots,
    watcher: report.watcher,
    drifted: report.drifted,
    nfs_port: report.nfs_port.map(u32::from),
    placed: report.placed.region,
    mirror_age_ns: status_opt_i64(report.placed.mirror_age_ns, "mirrorAgeNs")?,
    host_epoch: status_i64(report.placed.host_epoch, "hostEpoch")?,
  })
}

/// Parses a request-id word from its decimal string (the async client holds it as a string across the
/// yield, since a `u64` word can exceed a JS-safe integer).
fn parse_word(word: &str) -> Result<u64> {
  word
    .parse::<u64>()
    .map_err(|_| Error::from_reason("a request id word is a decimal u64 string"))
}

/// The result of an async verb's begin-and-spin: the request id word to await on, and the decoded
/// reply if it already landed within the spin window (the fast path, no event loop).
#[napi(object)]
pub struct CreateBegin {
  /// The request id word, held by the async client to match the reply.
  pub word: String,
  /// The volume id hex, present when the reply came within the spin window.
  pub fast: Option<String>,
}

/// A snapshot begin-and-spin result (see [`CreateBegin`]).
#[napi(object)]
pub struct SnapshotBegin {
  /// The request id word.
  pub word: String,
  /// The snapshot sequence, present when the reply came within the spin window.
  pub fast: Option<i64>,
}

/// A status begin-and-spin result (see [`CreateBegin`]).
#[napi(object)]
pub struct StatusBegin {
  /// The request id word.
  pub word: String,
  /// The status, present when the reply came within the spin window.
  pub fast: Option<VolumeStatus>,
}

/// A list begin-and-spin result (see [`CreateBegin`]).
#[napi(object)]
pub struct ListBegin {
  /// The request id word.
  pub word: String,
  /// The volume entries, present when the reply came within the spin window.
  pub fast: Option<Vec<VolumeEntry>>,
}

/// A unit verb's begin-and-spin result (resize/destroy): `fast` is `true` when the verb completed
/// within the spin window, distinguishing "done" from "not yet" (the JS side resolves to undefined).
#[napi(object)]
pub struct UnitBegin {
  /// The request id word.
  pub word: String,
  /// `true` when the verb completed within the spin window.
  pub fast: Option<bool>,
}

/// Builds a [`VolumeEntry`] from a summary — the fields `slates list` prints. Shared by the sync
/// `list` verb and the async one, so both surfaces return the identical shape (§4.4).
fn volume_entry(volume: VolumeSummary) -> Result<VolumeEntry> {
  Ok(VolumeEntry {
    id: volume_hex(&volume.id),
    name: volume.name,
    referenced_bytes: status_i64(volume.referenced_bytes, "referencedBytes")?,
    unique_bytes: status_i64(volume.unique_bytes, "uniqueBytes")?,
    overlay: volume.overlay,
  })
}

/// Builds a [`WorkVolume`] from a create-work's id and base. Shared by the sync and async verbs (§4.16).
fn work_volume(id: VolumeId, base: u64) -> Result<WorkVolume> {
  Ok(WorkVolume {
    id: volume_hex(&id),
    base: status_i64(base, "base")?,
  })
}

/// Builds a [`MergeOutcome`] from a submit's typed outcome. Shared by the sync and async `submit`
/// verbs, so both render one shape (§4.16).
fn submit_outcome(outcome: Submitted) -> Result<MergeOutcome> {
  match outcome {
    Submitted::Accepted(version) => merge_outcome(Some(version), Vec::new()),
    Submitted::Conflict(windows) => merge_outcome(
      None,
      windows
        .into_iter()
        .map(|window| (window.path, window.at, window.len, window.class))
        .collect(),
    ),
  }
}

/// Builds a [`MergeOutcome`] from a rebase's typed outcome — the same shape as a submit (§4.16).
/// Shared by the sync and async `rebase` verbs.
fn rebase_outcome(outcome: Rebased) -> Result<MergeOutcome> {
  match outcome {
    Rebased::Rebased(version) => merge_outcome(Some(version), Vec::new()),
    Rebased::Conflict(windows) => merge_outcome(
      None,
      windows
        .into_iter()
        .map(|window| (window.path, window.at, window.len, window.class))
        .collect(),
    ),
  }
}

/// A create-work begin-and-spin result (see [`CreateBegin`]).
#[napi(object)]
pub struct WorkBegin {
  /// The request id word.
  pub word: String,
  /// The work volume, present when the reply came within the spin window.
  pub fast: Option<WorkVolume>,
}

/// A submit or rebase begin-and-spin result (see [`CreateBegin`]).
#[napi(object)]
pub struct SubmitBegin {
  /// The request id word.
  pub word: String,
  /// The merge outcome, present when the reply came within the spin window.
  pub fast: Option<MergeOutcome>,
}

/// A changed-since begin-and-spin result (see [`CreateBegin`]).
#[napi(object)]
pub struct ChangedBegin {
  /// The request id word.
  pub word: String,
  /// The changed paths, present when the reply came within the spin window.
  pub fast: Option<Vec<String>>,
}

/// A landing begin-and-spin result (see [`CreateBegin`]).
#[napi(object)]
pub struct LandBegin {
  /// The request id word.
  pub word: String,
  /// The landing result, present when the reply came within the spin window.
  pub fast: Option<LandingResult>,
}

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

  /// Reads the volume's status (§4.4) as a [`VolumeStatus`] object — its placement, byte accounting,
  /// attachments, overlay drift and the daemon's NFS port, the same fields `slates status` prints.
  #[napi]
  pub fn status(&mut self, volume: String) -> Result<VolumeStatus> {
    let id = parse_volume(&volume)?;
    let report = self.inner.status(id).map_err(refusal)?;
    volume_status(report)
  }

  // The async surface's low-level primitives (R6, D-19): begin-and-spin returns the request id word
  // and the reply if it landed within the spin (the fast path); poll takes it by word once the
  // completion fd signals; completionFd / arm / disarm / takeReady drive the JS `AsyncClient`'s libuv
  // `uv_poll` loop. The verbs bound async today (create, snapshot, status); the rest follow the shape.

  /// Begins a create and spins for its reply within the daemon's window; the word to await and the id
  /// if it already landed (the async fast path).
  #[napi]
  pub fn begin_spin_create(
    &mut self,
    name: String,
    size_bytes: i64,
    dynamic: Option<bool>,
    fold: Option<bool>,
    require_locked: Option<bool>,
    base: Option<String>,
  ) -> Result<CreateBegin> {
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
    let id = self.inner.create_begin(&spec).map_err(refusal)?;
    self.inner.begin_ack_if_due().map_err(refusal)?;
    let spin = self.inner.published_spin_ns();
    let fast = self
      .inner
      .create_spin(id, spin)
      .map_err(refusal)?
      .map(|volume| volume_hex(&volume));
    Ok(CreateBegin {
      word: id.word().to_string(),
      fast,
    })
  }

  /// Takes a create's reply by its word once the completion fd signals; `null` until it is on the ring.
  #[napi]
  pub fn poll_create(&mut self, word: String) -> Result<Option<String>> {
    let word = parse_word(&word)?;
    Ok(
      self
        .inner
        .create_poll(word)
        .map_err(refusal)?
        .map(|volume| volume_hex(&volume)),
    )
  }

  /// Begins a snapshot and spins for its reply; the word and the sequence if it landed in the spin.
  #[napi]
  pub fn begin_spin_snapshot(&mut self, volume: String) -> Result<SnapshotBegin> {
    let id = parse_volume(&volume)?;
    let request = self.inner.snapshot_begin(id).map_err(refusal)?;
    self.inner.begin_ack_if_due().map_err(refusal)?;
    let spin = self.inner.published_spin_ns();
    let fast = match self.inner.snapshot_spin(request, spin).map_err(refusal)? {
      Some(snapshot) => Some(status_i64(snapshot.value, "snapshot")?),
      None => None,
    };
    Ok(SnapshotBegin {
      word: request.word().to_string(),
      fast,
    })
  }

  /// Takes a snapshot's reply by its word once the completion fd signals.
  #[napi]
  pub fn poll_snapshot(&mut self, word: String) -> Result<Option<i64>> {
    let word = parse_word(&word)?;
    match self.inner.snapshot_poll(word).map_err(refusal)? {
      Some(snapshot) => Ok(Some(status_i64(snapshot.value, "snapshot")?)),
      None => Ok(None),
    }
  }

  /// Begins a status read and spins for its reply; the word and the status if it landed in the spin.
  #[napi]
  pub fn begin_spin_status(&mut self, volume: String) -> Result<StatusBegin> {
    let id = parse_volume(&volume)?;
    let request = self.inner.status_begin(id).map_err(refusal)?;
    self.inner.begin_ack_if_due().map_err(refusal)?;
    let spin = self.inner.published_spin_ns();
    let fast = match self.inner.status_spin(request, spin).map_err(refusal)? {
      Some(report) => Some(volume_status(report)?),
      None => None,
    };
    Ok(StatusBegin {
      word: request.word().to_string(),
      fast,
    })
  }

  /// Takes a status reply by its word once the completion fd signals.
  #[napi]
  pub fn poll_status(&mut self, word: String) -> Result<Option<VolumeStatus>> {
    let word = parse_word(&word)?;
    match self.inner.status_poll(word).map_err(refusal)? {
      Some(report) => Ok(Some(volume_status(report)?)),
      None => Ok(None),
    }
  }

  /// The completion fd an async event loop polls (`uv_poll`), starting the completion channel on first
  /// call (§4.7, D-19). A **dup** the JS side owns: Node's `net.Socket` adopts and closes the fd it
  /// wraps, so it must be given its own descriptor — closing it leaves the client's intact.
  #[napi]
  pub fn completion_fd(&mut self) -> Result<i32> {
    self.inner.enable_async_completion_dup().map_err(refusal)
  }

  /// Arms the completion signal before the async client yields to its loop (the daemon wakes a parked
  /// client on a reply).
  #[napi]
  pub fn arm(&mut self) -> Result<()> {
    self.inner.arm_async().map_err(refusal)
  }

  /// Clears the arm once no request is in flight.
  #[napi]
  pub fn disarm(&mut self) -> Result<()> {
    self.inner.disarm_async().map_err(refusal)
  }

  /// Drains every ready reply into the client's buffer and returns their words, so the async pump
  /// resolves each waiting request in one pass.
  #[napi]
  pub fn take_ready(&mut self) -> Result<Vec<String>> {
    Ok(
      self
        .inner
        .take_ready()
        .map_err(refusal)?
        .into_iter()
        .map(|word| word.to_string())
        .collect(),
    )
  }

  /// Begins a list and spins for its reply; the word and the entries if they landed in the spin.
  #[napi]
  pub fn begin_spin_list(&mut self) -> Result<ListBegin> {
    let request = self.inner.list_begin().map_err(refusal)?;
    self.inner.begin_ack_if_due().map_err(refusal)?;
    let spin = self.inner.published_spin_ns();
    let fast = match self.inner.list_spin(request, spin).map_err(refusal)? {
      Some(volumes) => Some(
        volumes
          .into_iter()
          .map(volume_entry)
          .collect::<Result<Vec<_>>>()?,
      ),
      None => None,
    };
    Ok(ListBegin {
      word: request.word().to_string(),
      fast,
    })
  }

  /// Takes a list reply by its word once the completion fd signals.
  #[napi]
  pub fn poll_list(&mut self, word: String) -> Result<Option<Vec<VolumeEntry>>> {
    let word = parse_word(&word)?;
    match self.inner.list_poll(word).map_err(refusal)? {
      Some(volumes) => Ok(Some(
        volumes
          .into_iter()
          .map(volume_entry)
          .collect::<Result<Vec<_>>>()?,
      )),
      None => Ok(None),
    }
  }

  /// Begins a resize and spins for its reply; the word and `true` if it completed within the spin.
  #[napi]
  pub fn begin_spin_resize(
    &mut self,
    volume: String,
    size_bytes: i64,
    dynamic: Option<bool>,
  ) -> Result<UnitBegin> {
    let id = parse_volume(&volume)?;
    let size_bytes = checked_u64(size_bytes, "sizeBytes")?;
    let size = if dynamic.unwrap_or(false) {
      SizeClass::Dynamic { max: size_bytes }
    } else {
      SizeClass::Bounded { limit: size_bytes }
    };
    let request = self.inner.resize_begin(id, size).map_err(refusal)?;
    self.inner.begin_ack_if_due().map_err(refusal)?;
    let spin = self.inner.published_spin_ns();
    let fast = self.inner.resize_spin(request, spin).map_err(refusal)?;
    Ok(UnitBegin {
      word: request.word().to_string(),
      fast,
    })
  }

  /// Takes a resize's reply by its word once the completion fd signals (`true` when done).
  #[napi]
  pub fn poll_resize(&mut self, word: String) -> Result<Option<bool>> {
    let word = parse_word(&word)?;
    self.inner.resize_poll(word).map_err(refusal)
  }

  /// Begins a destroy and spins for its reply; the word and `true` if it completed within the spin.
  #[napi]
  pub fn begin_spin_destroy(&mut self, volume: String) -> Result<UnitBegin> {
    let id = parse_volume(&volume)?;
    let request = self.inner.destroy_begin(id).map_err(refusal)?;
    self.inner.begin_ack_if_due().map_err(refusal)?;
    let spin = self.inner.published_spin_ns();
    let fast = self.inner.destroy_spin(request, spin).map_err(refusal)?;
    Ok(UnitBegin {
      word: request.word().to_string(),
      fast,
    })
  }

  /// Takes a destroy's reply by its word once the completion fd signals (`true` when done).
  #[napi]
  pub fn poll_destroy(&mut self, word: String) -> Result<Option<bool>> {
    let word = parse_word(&word)?;
    self.inner.destroy_poll(word).map_err(refusal)
  }

  /// Begins a create-green and spins; the word and the hex id if it landed in the spin.
  #[napi]
  pub fn begin_spin_create_green(
    &mut self,
    name: String,
    require_evidence: Option<bool>,
  ) -> Result<CreateBegin> {
    let request = self
      .inner
      .create_green_begin(&name, require_evidence.unwrap_or(false))
      .map_err(refusal)?;
    self.inner.begin_ack_if_due().map_err(refusal)?;
    let spin = self.inner.published_spin_ns();
    let fast = self
      .inner
      .create_green_spin(request, spin)
      .map_err(refusal)?
      .map(|volume| volume_hex(&volume));
    Ok(CreateBegin {
      word: request.word().to_string(),
      fast,
    })
  }

  /// Takes a create-green's reply by its word once the completion fd signals.
  #[napi]
  pub fn poll_create_green(&mut self, word: String) -> Result<Option<String>> {
    let word = parse_word(&word)?;
    Ok(
      self
        .inner
        .create_green_poll(word)
        .map_err(refusal)?
        .map(|volume| volume_hex(&volume)),
    )
  }

  /// Begins a create-work and spins; the word and the work volume if it landed in the spin.
  #[napi]
  pub fn begin_spin_create_work(&mut self, green: String, name: String) -> Result<WorkBegin> {
    let green = parse_volume(&green)?;
    let request = self
      .inner
      .create_work_begin(green, &name)
      .map_err(refusal)?;
    self.inner.begin_ack_if_due().map_err(refusal)?;
    let spin = self.inner.published_spin_ns();
    let fast = match self
      .inner
      .create_work_spin(request, spin)
      .map_err(refusal)?
    {
      Some((id, base)) => Some(work_volume(id, base)?),
      None => None,
    };
    Ok(WorkBegin {
      word: request.word().to_string(),
      fast,
    })
  }

  /// Takes a create-work's reply by its word once the completion fd signals.
  #[napi]
  pub fn poll_create_work(&mut self, word: String) -> Result<Option<WorkVolume>> {
    let word = parse_word(&word)?;
    match self.inner.create_work_poll(word).map_err(refusal)? {
      Some((id, base)) => Ok(Some(work_volume(id, base)?)),
      None => Ok(None),
    }
  }

  /// Begins an edit and spins; the word and `true` if it completed within the spin.
  #[napi]
  pub fn begin_spin_edit(
    &mut self,
    work: String,
    path: String,
    at: i64,
    delete_len: i64,
    data: Buffer,
  ) -> Result<UnitBegin> {
    let work = parse_volume(&work)?;
    let at = checked_u64(at, "at")?;
    let delete_len = checked_u64(delete_len, "deleteLen")?;
    let request = self
      .inner
      .edit_begin(work, &path, at, delete_len, data.as_ref())
      .map_err(refusal)?;
    self.inner.begin_ack_if_due().map_err(refusal)?;
    let spin = self.inner.published_spin_ns();
    let fast = self.inner.edit_spin(request, spin).map_err(refusal)?;
    Ok(UnitBegin {
      word: request.word().to_string(),
      fast,
    })
  }

  /// Takes an edit's reply by its word once the completion fd signals (`true` when done).
  #[napi]
  pub fn poll_edit(&mut self, word: String) -> Result<Option<bool>> {
    let word = parse_word(&word)?;
    self.inner.edit_poll(word).map_err(refusal)
  }

  /// Begins a submit and spins; the word and the merge outcome if it landed in the spin.
  #[napi]
  pub fn begin_spin_submit(&mut self, work: String) -> Result<SubmitBegin> {
    let work = parse_volume(&work)?;
    let request = self.inner.submit_begin(work).map_err(refusal)?;
    self.inner.begin_ack_if_due().map_err(refusal)?;
    let spin = self.inner.published_spin_ns();
    let fast = match self.inner.submit_spin(request, spin).map_err(refusal)? {
      Some(outcome) => Some(submit_outcome(outcome)?),
      None => None,
    };
    Ok(SubmitBegin {
      word: request.word().to_string(),
      fast,
    })
  }

  /// Takes a submit's outcome by its word once the completion fd signals.
  #[napi]
  pub fn poll_submit(&mut self, word: String) -> Result<Option<MergeOutcome>> {
    let word = parse_word(&word)?;
    match self.inner.submit_poll(word).map_err(refusal)? {
      Some(outcome) => Ok(Some(submit_outcome(outcome)?)),
      None => Ok(None),
    }
  }

  /// Begins a versions query and spins; the word and the head version if it landed in the spin.
  #[napi]
  pub fn begin_spin_versions(&mut self, green: String) -> Result<SnapshotBegin> {
    let green = parse_volume(&green)?;
    let request = self.inner.versions_begin(green).map_err(refusal)?;
    self.inner.begin_ack_if_due().map_err(refusal)?;
    let spin = self.inner.published_spin_ns();
    let fast = match self.inner.versions_spin(request, spin).map_err(refusal)? {
      Some(head) => Some(status_i64(head, "version")?),
      None => None,
    };
    Ok(SnapshotBegin {
      word: request.word().to_string(),
      fast,
    })
  }

  /// Takes a versions reply by its word once the completion fd signals.
  #[napi]
  pub fn poll_versions(&mut self, word: String) -> Result<Option<i64>> {
    let word = parse_word(&word)?;
    match self.inner.versions_poll(word).map_err(refusal)? {
      Some(head) => Ok(Some(status_i64(head, "version")?)),
      None => Ok(None),
    }
  }

  /// Begins a changed-since query and spins; the word and the paths if they landed in the spin.
  #[napi]
  pub fn begin_spin_changed_since(&mut self, green: String, version: i64) -> Result<ChangedBegin> {
    let green = parse_volume(&green)?;
    let version = checked_u64(version, "version")?;
    let request = self
      .inner
      .changed_since_begin(green, version)
      .map_err(refusal)?;
    self.inner.begin_ack_if_due().map_err(refusal)?;
    let spin = self.inner.published_spin_ns();
    let fast = self
      .inner
      .changed_since_spin(request, spin)
      .map_err(refusal)?;
    Ok(ChangedBegin {
      word: request.word().to_string(),
      fast,
    })
  }

  /// Takes a changed-since reply by its word once the completion fd signals.
  #[napi]
  pub fn poll_changed_since(&mut self, word: String) -> Result<Option<Vec<String>>> {
    let word = parse_word(&word)?;
    self.inner.changed_since_poll(word).map_err(refusal)
  }

  /// Begins a rebase and spins; the word and the merge outcome if it landed in the spin.
  #[napi]
  pub fn begin_spin_rebase(&mut self, work: String) -> Result<SubmitBegin> {
    let work = parse_volume(&work)?;
    let request = self.inner.rebase_begin(work).map_err(refusal)?;
    self.inner.begin_ack_if_due().map_err(refusal)?;
    let spin = self.inner.published_spin_ns();
    let fast = match self.inner.rebase_spin(request, spin).map_err(refusal)? {
      Some(outcome) => Some(rebase_outcome(outcome)?),
      None => None,
    };
    Ok(SubmitBegin {
      word: request.word().to_string(),
      fast,
    })
  }

  /// Takes a rebase's outcome by its word once the completion fd signals.
  #[napi]
  pub fn poll_rebase(&mut self, word: String) -> Result<Option<MergeOutcome>> {
    let word = parse_word(&word)?;
    match self.inner.rebase_poll(word).map_err(refusal)? {
      Some(outcome) => Ok(Some(rebase_outcome(outcome)?)),
      None => Ok(None),
    }
  }

  // The namespace operations' async begin-and-spin (§4.16): each builds one `WorkOp` and defers to the
  // private `declare_begin_spin`; all share `poll_declare`. The `WorkOp` enum never crosses into JS.

  /// Begins an unlink and spins; the word and `true` if it completed within the spin.
  #[napi]
  pub fn begin_spin_unlink(&mut self, work: String, path: String) -> Result<UnitBegin> {
    self.declare_begin_spin(&work, WorkOp::Unlink { path })
  }

  /// Begins a rename and spins.
  #[napi]
  pub fn begin_spin_rename(&mut self, work: String, from: String, to: String) -> Result<UnitBegin> {
    self.declare_begin_spin(&work, WorkOp::Rename { from, to })
  }

  /// Begins a mkdir and spins.
  #[napi]
  pub fn begin_spin_mkdir(&mut self, work: String, path: String) -> Result<UnitBegin> {
    self.declare_begin_spin(&work, WorkOp::Mkdir { path })
  }

  /// Begins a rmdir and spins.
  #[napi]
  pub fn begin_spin_rmdir(&mut self, work: String, path: String) -> Result<UnitBegin> {
    self.declare_begin_spin(&work, WorkOp::Rmdir { path })
  }

  /// Begins a chmod and spins.
  #[napi]
  pub fn begin_spin_chmod(&mut self, work: String, path: String, mode: u32) -> Result<UnitBegin> {
    self.declare_begin_spin(&work, WorkOp::SetMode { path, mode })
  }

  /// Begins a symlink and spins.
  #[napi]
  pub fn begin_spin_symlink(
    &mut self,
    work: String,
    path: String,
    target: String,
  ) -> Result<UnitBegin> {
    self.declare_begin_spin(&work, WorkOp::Symlink { path, target })
  }

  /// Begins a hard link and spins.
  #[napi]
  pub fn begin_spin_link(
    &mut self,
    work: String,
    path: String,
    target: String,
  ) -> Result<UnitBegin> {
    self.declare_begin_spin(&work, WorkOp::Link { path, target })
  }

  /// Begins a set-xattr and spins.
  #[napi]
  pub fn begin_spin_set_xattr(
    &mut self,
    work: String,
    path: String,
    name: String,
    value: Buffer,
  ) -> Result<UnitBegin> {
    self.declare_begin_spin(
      &work,
      WorkOp::SetXattr {
        path,
        name,
        value: value.to_vec(),
      },
    )
  }

  /// Begins a remove-xattr and spins.
  #[napi]
  pub fn begin_spin_remove_xattr(
    &mut self,
    work: String,
    path: String,
    name: String,
  ) -> Result<UnitBegin> {
    self.declare_begin_spin(&work, WorkOp::RemoveXattr { path, name })
  }

  /// Takes a namespace declaration's reply by its word once the completion fd signals (`true` when
  /// done); shared by every namespace op.
  #[napi]
  pub fn poll_declare(&mut self, word: String) -> Result<Option<bool>> {
    let word = parse_word(&word)?;
    self.inner.declare_poll(word).map_err(refusal)
  }

  /// Begins a landing (§4.15) and spins; the word and the result if it landed in the spin. The SDK
  /// creates no grant itself (R10): a landing without a satisfying grant comes back grant-required.
  #[napi]
  pub fn begin_spin_land(
    &mut self,
    volume: String,
    target: String,
    snapshot: Option<i64>,
    include: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
    grant: Option<i64>,
  ) -> Result<LandBegin> {
    let id = parse_volume(&volume)?;
    let snap = match snapshot {
      Some(value) => Some(SnapshotId {
        value: checked_u64(value, "snapshot")?,
      }),
      None => None,
    };
    let filter = Filter {
      include: include.unwrap_or_default(),
      exclude: exclude.unwrap_or_default(),
    };
    let grant = match grant {
      Some(value) => Some(checked_u64(value, "grant")?),
      None => None,
    };
    let request = self
      .inner
      .land_begin(id, snap, &target, filter, grant)
      .map_err(refusal)?;
    self.inner.begin_ack_if_due().map_err(refusal)?;
    let spin = self.inner.published_spin_ns();
    let fast = match self.inner.land_spin(request, spin).map_err(refusal)? {
      Some(landing) => Some(landing_result(landing)?),
      None => None,
    };
    Ok(LandBegin {
      word: request.word().to_string(),
      fast,
    })
  }

  /// Takes a landing's result by its word once the completion fd signals.
  #[napi]
  pub fn poll_land(&mut self, word: String) -> Result<Option<LandingResult>> {
    let word = parse_word(&word)?;
    match self.inner.land_poll(word).map_err(refusal)? {
      Some(landing) => Ok(Some(landing_result(landing)?)),
      None => Ok(None),
    }
  }

  /// Lists the daemon's volumes (§4.4) as an array of [`VolumeEntry`] objects — id (hex), name, byte
  /// accounting, and overlay flag, the plain shape `slates list` prints.
  #[napi]
  pub fn list(&mut self) -> Result<Vec<VolumeEntry>> {
    let volumes = self.inner.list().map_err(refusal)?;
    volumes.into_iter().map(volume_entry).collect()
  }

  /// Resizes the volume's quota (§4.4): `sizeBytes` is the new `Bounded` reserve, or the new maximum of a
  /// `Dynamic` volume when `dynamic` is set. A refusal crosses as a JS `Error`.
  #[napi]
  pub fn resize(&mut self, volume: String, size_bytes: i64, dynamic: Option<bool>) -> Result<()> {
    let id = parse_volume(&volume)?;
    let size_bytes = checked_u64(size_bytes, "sizeBytes")?;
    let size = if dynamic.unwrap_or(false) {
      SizeClass::Dynamic { max: size_bytes }
    } else {
      SizeClass::Bounded { limit: size_bytes }
    };
    self.inner.resize(id, size).map_err(refusal)?;
    Ok(())
  }

  /// Destroys the volume (§4.4): its RAM is reclaimed and its id retired. Destroying a missing volume is
  /// a typed JS `Error`, not a silent success.
  #[napi]
  pub fn destroy(&mut self, volume: String) -> Result<()> {
    let id = parse_volume(&volume)?;
    self.inner.destroy(id).map_err(refusal)?;
    Ok(())
  }

  /// Plans, and with a grant executes, a landing of the volume's diverged entries onto the host
  /// directory `target` (§4.15). Returns the landing result ([`LandingResult`]): without a grant,
  /// `grantRequired` with the manifest and the `slates grant` command a human runs; with one, the
  /// finished `outcome`. `snapshot` lands a snapshot (else the head); `include`/`exclude` are path
  /// filters; `grant` is a grant id a human already issued on the CLI. The SDK never issues a grant
  /// itself (R10) — an agent can plan and, once a human authorizes, execute, but cannot authorize.
  #[napi]
  pub fn land(
    &mut self,
    volume: String,
    target: String,
    snapshot: Option<i64>,
    include: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
    grant: Option<i64>,
  ) -> Result<LandingResult> {
    let id = parse_volume(&volume)?;
    let snap = match snapshot {
      Some(value) => Some(SnapshotId {
        value: checked_u64(value, "snapshot")?,
      }),
      None => None,
    };
    let filter = Filter {
      include: include.unwrap_or_default(),
      exclude: exclude.unwrap_or_default(),
    };
    let grant = match grant {
      Some(value) => Some(checked_u64(value, "grant")?),
      None => None,
    };
    let landing = self
      .inner
      .land(id, snap, &target, filter, grant)
      .map_err(refusal)?;
    landing_result(landing)
  }

  /// Creates a green volume — a shared merge target (§4.16) — returning its hex id. `requireEvidence`
  /// makes the green refuse an increment that carries no evidence of the base it was derived from.
  #[napi]
  pub fn create_green(&mut self, name: String, require_evidence: Option<bool>) -> Result<String> {
    let id = self
      .inner
      .create_green(&name, require_evidence.unwrap_or(false))
      .map_err(refusal)?;
    Ok(volume_hex(&id))
  }

  /// Creates a work volume over a green (§4.16), returning its id (hex) and the green base version its
  /// edits will be submitted against.
  #[napi]
  pub fn create_work(&mut self, green: String, name: String) -> Result<WorkVolume> {
    let green = parse_volume(&green)?;
    let (id, base) = self.inner.create_work(green, &name).map_err(refusal)?;
    work_volume(id, base)
  }

  /// A green's head version (§4.16) — the number that advances with each accepted submit.
  #[napi]
  pub fn versions(&mut self, green: String) -> Result<i64> {
    let green = parse_volume(&green)?;
    status_i64(self.inner.versions(green).map_err(refusal)?, "version")
  }

  /// The files a green changed strictly after `version` (§4.16).
  #[napi]
  pub fn changed_since(&mut self, green: String, version: i64) -> Result<Vec<String>> {
    let green = parse_volume(&green)?;
    let version = checked_u64(version, "version")?;
    self.inner.changed_since(green, version).map_err(refusal)
  }

  /// Declares a content edit on a work volume (§4.16): a splice at `path` — remove `deleteLen` bytes at
  /// offset `at`, then insert `data` (a Buffer). An insert is `deleteLen` zero; an overwrite is a delete
  /// and an insert in one call.
  #[napi]
  pub fn edit(
    &mut self,
    work: String,
    path: String,
    at: i64,
    delete_len: i64,
    data: Buffer,
  ) -> Result<()> {
    let work = parse_volume(&work)?;
    let at = checked_u64(at, "at")?;
    let delete_len = checked_u64(delete_len, "deleteLen")?;
    self
      .inner
      .edit(work, &path, at, delete_len, data.as_ref())
      .map_err(refusal)?;
    Ok(())
  }

  /// Submits a work volume's declared edits to its green (§4.16): the merge outcome — `ok` with the new
  /// green `version` when accepted, or the `conflicts` windows to rebase against when it conflicts.
  #[napi]
  pub fn submit(&mut self, work: String) -> Result<MergeOutcome> {
    let work = parse_volume(&work)?;
    submit_outcome(self.inner.submit(work).map_err(refusal)?)
  }

  /// Rebases a work volume onto its green's head (§4.16), mapping its pending edits forward without
  /// committing to the green — the same outcome shape as [`Client::submit`].
  #[napi]
  pub fn rebase(&mut self, work: String) -> Result<MergeOutcome> {
    let work = parse_volume(&work)?;
    rebase_outcome(self.inner.rebase(work).map_err(refusal)?)
  }

  /// How many times this client has reconnected across daemon restarts (an observability counter, so a
  /// test can assert a session survived a restart — §4.9).
  #[napi]
  pub fn reconnects(&self) -> Result<i64> {
    i64::try_from(self.inner.reconnects())
      .map_err(|_| Error::from_reason("the reconnect count is too large for a JS number"))
  }

  /// Removes the name at `path` on a work volume (§4.16) — a file, symlink or hard link.
  #[napi]
  pub fn unlink(&mut self, work: String, path: String) -> Result<()> {
    self.declare(&work, WorkOp::Unlink { path })
  }

  /// Renames `from` to `to` on a work volume (§4.16).
  #[napi]
  pub fn rename(&mut self, work: String, from: String, to: String) -> Result<()> {
    self.declare(&work, WorkOp::Rename { from, to })
  }

  /// Creates an empty directory at `path` on a work volume (§4.16).
  #[napi]
  pub fn mkdir(&mut self, work: String, path: String) -> Result<()> {
    self.declare(&work, WorkOp::Mkdir { path })
  }

  /// Removes the empty directory at `path` on a work volume (§4.16).
  #[napi]
  pub fn rmdir(&mut self, work: String, path: String) -> Result<()> {
    self.declare(&work, WorkOp::Rmdir { path })
  }

  /// Sets the mode of the file or directory at `path` on a work volume (§4.16).
  #[napi]
  pub fn chmod(&mut self, work: String, path: String, mode: u32) -> Result<()> {
    self.declare(&work, WorkOp::SetMode { path, mode })
  }

  /// Creates or retargets a symbolic link at `path` pointing at `target` on a work volume (§4.16).
  #[napi]
  pub fn symlink(&mut self, work: String, path: String, target: String) -> Result<()> {
    self.declare(&work, WorkOp::Symlink { path, target })
  }

  /// Creates a hard link at `path` to the existing file `target` on a work volume (§4.16).
  #[napi]
  pub fn link(&mut self, work: String, path: String, target: String) -> Result<()> {
    self.declare(&work, WorkOp::Link { path, target })
  }

  /// Sets the extended attribute `name` on `path` to `value` (a Buffer) on a work volume (§4.16).
  #[napi]
  pub fn set_xattr(
    &mut self,
    work: String,
    path: String,
    name: String,
    value: Buffer,
  ) -> Result<()> {
    self.declare(
      &work,
      WorkOp::SetXattr {
        path,
        name,
        value: value.to_vec(),
      },
    )
  }

  /// Removes the extended attribute `name` from `path` on a work volume (§4.16).
  #[napi]
  pub fn remove_xattr(&mut self, work: String, path: String, name: String) -> Result<()> {
    self.declare(&work, WorkOp::RemoveXattr { path, name })
  }
}

impl Client {
  /// Declares one namespace operation on a work volume (§4.16) — the shared body of the namespace verbs
  /// (`unlink`/`rename`/`mkdir`/…). Not a `#[napi]` verb: `WorkOp` is a Rust enum that does not cross into
  /// JS, so the SDK exposes the ergonomic verbs above rather than a raw `declare`.
  fn declare(&mut self, work: &str, op: WorkOp) -> Result<()> {
    let work = parse_volume(work)?;
    self.inner.declare(work, op).map_err(refusal)?;
    Ok(())
  }

  /// Begins one namespace declaration and spins — the async begin-and-spin body the namespace verbs
  /// share (`true` when done in the spin). Not a `#[napi]` verb: `WorkOp` does not cross into JS.
  fn declare_begin_spin(&mut self, work: &str, op: WorkOp) -> Result<UnitBegin> {
    let work = parse_volume(work)?;
    let request = self.inner.declare_begin(work, op).map_err(refusal)?;
    self.inner.begin_ack_if_due().map_err(refusal)?;
    let spin = self.inner.published_spin_ns();
    let fast = self.inner.declare_spin(request, spin).map_err(refusal)?;
    Ok(UnitBegin {
      word: request.word().to_string(),
      fast,
    })
  }
}
