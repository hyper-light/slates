//! `slates`: the command (§4.12 "CLI", §2.5 "the anchor process", §2.6 "Boot order"; Phase 2
//! task 5). Three roles in one binary, so a machine runs one thing:
//!
//! - `slates anchor` is the anchor process of §2.5: it measures the machine profile, creates
//!   the shared segment, publishes the profile into it, and supervises `slates daemon` as a
//!   child with the segment handed over in its environment, restarting it under the derived
//!   bound and killing it when its heartbeat lapses (§4.14 `daemon.alive`).
//! - `slates daemon` is the daemon: it attaches the segment from its environment (or, run
//!   alone, measures and creates one), reads the published profile, derives its
//!   configuration and serves; it leaves when its anchor does.
//!
//!   Both roles are the library's (`slates_api::role`, A-278), which an embedding host serves from
//!   its own executable too; this binary serves them with no prefix.
//! - Every other verb is a client: the rendezvous, one request, one reply, printed in a
//!   stable plain form (one `key: value` per line, or one record per line), with the refusal
//!   taxonomy mapped to exit codes.
//!
//! Arguments are parsed by hand (`args.rs`): a dozen verbs do not justify a dependency, and
//! every flag is listed in one place with what it takes. No grant verb exists here yet: the
//! grant surface of §4.13 arrives with Phase 2 task 8, and the ring channel refuses the kind.

// The no-panic law (CLAUDE.md, banned item 6): shipped code never overflows or divides by zero. Test builds
// are exempt. Once a crate is clean this holds it there; out-of-bounds indexing and slicing are denied
// workspace-wide.
// The panic rule: a print macro panics on a refused write; lines go through `slates_machine::log_line!` (A-213).
#![deny(clippy::print_stdout, clippy::print_stderr)]
#![cfg_attr(not(test), deny(clippy::arithmetic_side_effects))]

use std::process::ExitCode;

mod apparmor;
mod args;
mod format;
mod log_view;
// The handshake is the library's (`slates_api::container_engine`, A-520), which asks an engine only on Unix, where
// its poll loop runs; this module prints its answer there, and Windows keeps its refusal (`run_oci_runtime`).
#[cfg(unix)]
mod oci_runtime;
// The self-pipe `slates pmem` reads its signals from, Linux's alone.
#[cfg(target_os = "linux")]
mod signal;
mod verbs;

use args::{Command, ParseError};

/// Format: the exit code for a request the daemon refused (the refusal is printed).
const EXIT_REFUSED: u8 = 1;
/// Format: the exit code for a usage error (the usage is printed).
const EXIT_USAGE: u8 = 2;
/// Format: the exit code when no daemon answers at the instance.
const EXIT_UNAVAILABLE: u8 = 3;
/// Format: the exit code when the anchor or the daemon itself failed; a daemon its own watchdog ends exits with it too
/// (A-232), so it is the server's one constant.
const EXIT_FAILED: u8 = slates_server::watchdog::FAILED_EXIT_CODE;

/// Every allocation of the anchor, the daemon and the client: a large one mapped directly and unmapped when freed, so
/// a freed large buffer returns to the OS at once (`slates_mem::large`, A-742); the rest the system's. An
/// embedding host that serves the roles from its own executable installs it the same way.
#[global_allocator]
static ALLOCATOR: sites::Sited = sites::Sited;

/// DIAG ONLY (w-hg's alloc-sites branch, never main; main 01a20182's order of 2026-11-23): `LargeMapped` with a byte
/// sampler behind `SLATES_ALLOC_SITES`. Every [`sites::SAMPLE`] bytes allocated, a backtrace names the allocation's
/// innermost frames outside the allocator and the standard library; the sampled bytes are charged to it. The cumulative
/// top sites are dumped to stderr (the anchor's log), stamped with the wall clock: first once the run has allocated what
/// its first second allocated again, then each time its allocation grows by a [`sites::FRACTION`]th, so the arena's
/// rows (`ROW-START` stamps) split them.
#[allow(
  clippy::disallowed_types,
  clippy::print_stderr,
  clippy::unwrap_used,
  clippy::arithmetic_side_effects,
  clippy::indexing_slicing
)]
mod sites {
  use std::alloc::{GlobalAlloc, Layout};
  use std::collections::HashMap;
  use std::sync::Mutex;
  use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

  use slates_mem::large::LargeMapped;

  /// Diag shape: the bytes between samples.
  pub const SAMPLE: u64 = 4 << 20;
  /// Diag shape: a dump each time the run's allocation grows by this fraction of itself.
  pub const FRACTION: u64 = 16;
  /// Diag shape: the sites a dump lists.
  const LISTED: usize = 25;

  /// Whether sampling is on (`SLATES_ALLOC_SITES` at start).
  static ENABLED: AtomicBool = AtomicBool::new(false);
  /// Bytes allocated since sampling began, and since the last sample.
  static TOTAL: AtomicU64 = AtomicU64::new(0);
  static SINCE: AtomicU64 = AtomicU64::new(0);
  /// The run's first second's allocation, once known, and the allocation the next dump waits for.
  static FIRST: AtomicU64 = AtomicU64::new(0);
  static NEXT: AtomicU64 = AtomicU64::new(u64::MAX);
  /// When sampling began, milliseconds of the wall clock.
  static START_MS: AtomicU64 = AtomicU64::new(0);
  /// The sampled bytes by site.
  static TABLE: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);

  std::thread_local! {
    /// Set while this thread samples or dumps: its own allocations are not sampled.
    static GUARD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
  }

  /// The wall clock, milliseconds.
  fn now_ms() -> u64 {
    std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .map_or(0, |since| {
        u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
      })
  }

  /// Turns sampling on where `SLATES_ALLOC_SITES` is set.
  pub fn enable_from_env() {
    if std::env::var_os("SLATES_ALLOC_SITES").is_some() {
      START_MS.store(now_ms(), Ordering::Relaxed);
      ENABLED.store(true, Ordering::Relaxed);
      eprintln!(
        "slates: DIAG alloc-sites on: a sample every {SAMPLE} B, a dump each 1/{FRACTION} of the run's allocation"
      );
    }
  }

  /// A backtrace's site: its innermost two frames outside the allocator, the standard library and this module.
  fn site(trace: &str) -> String {
    let skip = |name: &str| {
      name.starts_with("std::")
        || name.starts_with("core::")
        || name.starts_with("alloc::")
        || name.starts_with("<alloc::")
        || name.starts_with("<core::")
        || name.starts_with("<std::")
        || name.starts_with("__rust")
        || name.starts_with("__rdl")
        || name.contains("slates_mem::large")
        || name.contains("slates::sites")
        || name.contains("sites::")
        || name.contains("backtrace")
        || name.contains("GlobalAlloc")
    };
    let frames: Vec<&str> = trace
      .lines()
      .map(str::trim)
      .filter_map(|line| {
        let (number, name) = line.split_once(": ")?;
        number.chars().all(|c| c.is_ascii_digit()).then_some(name)
      })
      .filter(|name| !skip(name))
      .take(2)
      .collect();
    let short = |name: &str| {
      name
        .rsplit_once("::h")
        .map_or(name, |(head, _)| head)
        .to_owned()
    };
    match frames.as_slice() {
      [inner, caller, ..] => format!("{} <- {}", short(inner), short(caller)),
      [inner] => short(inner),
      [] => "(no frame)".to_owned(),
    }
  }

  /// Dumps the cumulative top sites, stamped.
  fn dump(total: u64) {
    let Ok(table) = TABLE.lock() else {
      return;
    };
    let Some(table) = table.as_ref() else {
      return;
    };
    let mut sites: Vec<(&String, &u64)> = table.iter().collect();
    sites.sort_by(|left, right| right.1.cmp(left.1));
    eprintln!(
      "slates-server: DIAG alloc-sites at_ms={} since_ms={} total={total} sites={}",
      now_ms(),
      now_ms().saturating_sub(START_MS.load(Ordering::Relaxed)),
      table.len()
    );
    for (name, bytes) in sites.into_iter().take(LISTED) {
      eprintln!("slates-server: DIAG alloc-site {bytes} {name}");
    }
  }

  /// Notes `size` bytes allocated: sampled every [`SAMPLE`], dumped as the run's allocation grows.
  fn note(size: usize) {
    if !ENABLED.load(Ordering::Relaxed) {
      return;
    }
    let size = size as u64;
    let total = TOTAL.fetch_add(size, Ordering::Relaxed) + size;
    let since = SINCE.fetch_add(size, Ordering::Relaxed) + size;
    if since < SAMPLE {
      return;
    }
    if GUARD.with(|guard| guard.replace(true)) {
      return;
    }
    SINCE.store(0, Ordering::Relaxed);
    let key = site(&std::backtrace::Backtrace::force_capture().to_string());
    if let Ok(mut table) = TABLE.lock() {
      *table
        .get_or_insert_with(HashMap::new)
        .entry(key)
        .or_insert(0) += since;
    }
    // The first interval: what the run's first second allocated.
    if FIRST.load(Ordering::Relaxed) == 0
      && now_ms().saturating_sub(START_MS.load(Ordering::Relaxed)) >= 1_000
    {
      FIRST.store(total.max(1), Ordering::Relaxed);
      NEXT.store(total.saturating_mul(2), Ordering::Relaxed);
    }
    if total >= NEXT.load(Ordering::Relaxed) {
      let step = FIRST.load(Ordering::Relaxed).max(total / FRACTION);
      NEXT.store(total.saturating_add(step), Ordering::Relaxed);
      dump(total);
    }
    GUARD.with(|guard| guard.set(false));
  }

  /// `LargeMapped`, sampled.
  pub struct Sited;

  // SAFETY: every call is `LargeMapped`'s, unchanged; the sampler only reads the layout's size before it.
  unsafe impl GlobalAlloc for Sited {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
      note(layout.size());
      // SAFETY: as the caller's contract with `alloc`.
      unsafe { LargeMapped.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
      // SAFETY: as the caller's contract with `dealloc`.
      unsafe { LargeMapped.dealloc(ptr, layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
      note(layout.size());
      // SAFETY: as the caller's contract with `alloc_zeroed`.
      unsafe { LargeMapped.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
      note(new_size.saturating_sub(layout.size()));
      // SAFETY: as the caller's contract with `realloc`.
      unsafe { LargeMapped.realloc(ptr, layout, new_size) }
    }
  }
}

fn main() -> ExitCode {
  sites::enable_from_env();
  let arguments: Vec<String> = std::env::args().skip(1).collect();
  // The daemon's WinFsp purge client, a role only a daemon starts (no daemon thread opens its own volume, §4.6), served
  // by the library's role entry as a host's executable serves it.
  #[cfg(windows)]
  if arguments.first().map(String::as_str) == Some(slates_bridge_winfsp::purge_client::ROLE_ARG) {
    return slates_api::role::main(&[], &arguments);
  }
  let command = match args::parse(&arguments) {
    Ok(command) => command,
    Err(ParseError::Help) => {
      slates_machine::out_line!("{}\n{}", args::USAGE, args::USAGE_NOTES);
      return output_delivered();
    }
    Err(e) => {
      slates_machine::log_line!("slates: {e}\n\n{}\n{}", args::USAGE, args::USAGE_NOTES);
      return ExitCode::from(EXIT_USAGE);
    }
  };
  // Whether a failure should be reported as JSON: the `--json` switch, for the verbs that carry it.
  let json = match &command {
    Command::Client(request) => request.json,
    Command::Profile(options) => options.json,
    Command::Run(request) => request.json,
    Command::Shards(request) => request.json,
    Command::VhostUser(request) => request.json,
    Command::Pmem(request) => request.json,
    Command::AppArmor(args::AppArmorRequest::Check { json }) => *json,
    _ => false,
  };
  let outcome = match command {
    Command::Anchor(options) => {
      slates_api::role::serve(&[], &slates_api::role::Role::Anchor(options)).map_err(Failure::from)
    }
    Command::Daemon(options) => {
      slates_api::role::serve(&[], &slates_api::role::Role::Daemon(options)).map_err(Failure::from)
    }
    Command::Profile(options) => verbs::profile(&options),
    Command::Version => {
      slates_machine::out_line!("slates {}", env!("CARGO_PKG_VERSION"));
      Ok(())
    }
    Command::Mcp(options) => verbs::mcp(&options),
    Command::Client(request) => verbs::run(&request),
    Command::Exec(spec) => run_exec(&spec),
    Command::AppArmor(args::AppArmorRequest::Profile { executable }) => {
      apparmor::profile(executable.as_deref())
    }
    Command::AppArmor(args::AppArmorRequest::Check { json }) => apparmor::check(json),
    Command::Run(request) => verbs::run_consumer(&request),
    Command::Shards(request) => verbs::run_shards(&request),
    Command::VhostUser(request) => verbs::run_vhost_user(&request),
    Command::Pmem(request) => verbs::run_pmem(&request),
    Command::OciCheck(check) => verbs::oci_check(&check),
    Command::OciRuntime(runtime) => run_oci_runtime(&runtime),
  };
  match outcome {
    Ok(()) => output_delivered(),
    Err(failure) => report_failure(&failure, json),
  }
}

/// The exit of a command that succeeded: success, unless some of its output could not be written (its standard output
/// closed, or a pipe whose reader went away), which is the command's failure, since its caller did not get what it
/// asked for (A-213). Before, the first such line panicked the command (exit 134 in a release build).
fn output_delivered() -> ExitCode {
  match slates_machine::log::out_lines_refused() {
    0 => ExitCode::SUCCESS,
    refused => {
      slates_machine::log_line!("slates: {refused} lines of output could not be written");
      ExitCode::from(EXIT_FAILED)
    }
  }
}

/// Prints a failure and returns its exit code. Under `--json` it is a JSON object
/// `{"error": {"kind", "message"}}` (so a harness parsing `--json` gets a structured failure, not
/// prose on stderr — GAP-A9-10 "consistent JSON errors"); otherwise the plain `slates: …` text. The
/// exit code — 1 refused, 3 no daemon, 4 failed — is the same either way, so a script can key on it.
fn report_failure(failure: &Failure, json: bool) -> ExitCode {
  // The workload's own exit is passed through as it was, and nothing is printed: its output was
  // its own. A code no process exit carries on this platform is reported as a failure.
  if let Failure::ChildExited { code } = failure {
    return match u8::try_from(*code) {
      Ok(code) => ExitCode::from(code),
      Err(_) => {
        slates_machine::log_line!("slates: the command exited with code {code}");
        ExitCode::from(EXIT_FAILED)
      }
    };
  }
  let (code, kind) = match failure {
    Failure::Refused(_) => (EXIT_REFUSED, "refused"),
    Failure::Unavailable { .. } => (EXIT_UNAVAILABLE, "unavailable"),
    Failure::Failed(_) | Failure::ChildExited { .. } => (EXIT_FAILED, "failed"),
    Failure::NeverServed => (slates_api::role::EXIT_NEVER_SERVED, "never_served"),
  };
  if json {
    let message = match failure {
      Failure::Refused(text) | Failure::Failed(text) => text.clone(),
      Failure::Unavailable { instance, cause } => {
        format!("no daemon at instance {instance}: {cause}")
      }
      Failure::ChildExited { code } => format!("the command exited with code {code}"),
      Failure::NeverServed => slates_api::role::Failure::NeverServed.to_string(),
    };
    slates_machine::log_line!(
      "{}",
      serde_json::json!({ "error": { "kind": kind, "message": message } })
    );
  } else {
    match failure {
      Failure::Refused(text) => slates_machine::log_line!("slates: refused: {text}"),
      Failure::Unavailable { instance, cause } => {
        slates_machine::log_line!(
          "slates: no daemon at instance {instance}: {cause} (is `slates anchor` running?)"
        );
      }
      Failure::Failed(text) => slates_machine::log_line!("slates: {text}"),
      Failure::ChildExited { code } => {
        slates_machine::log_line!("slates: the command exited with code {code}")
      }
      Failure::NeverServed => {
        slates_machine::log_line!("slates anchor: {}", slates_api::role::Failure::NeverServed)
      }
    }
  }
  ExitCode::from(code)
}

/// Runs the launcher, the library's (`slates_api::exec`, A-370), which an embedding host's executable serves too. On
/// Linux it never returns on success: it becomes the command. Every refusal is a failure (exit 4) printing its sentence;
/// off Linux it is refused there, since the namespaces it needs do not exist.
fn run_exec(spec: &slates_client::exec::ExecSpec) -> Result<(), Failure> {
  match slates_api::exec::run(spec) {
    Ok(never) => match never {},
    Err(fault) => Err(Failure::Failed(fault.to_string())),
  }
}

/// Runs the consuming runtime's handshake on Unix, where the engine's CLI can be polled and judged.
#[cfg(unix)]
fn run_oci_runtime(runtime: &str) -> Result<(), Failure> {
  oci_runtime::oci_runtime(runtime)
}

/// The handshake is Unix-only: the library polls the engine's CLI there alone (`slates_api::container_engine`, A-520,
/// which answers `EngineFault::Unsupported` elsewhere). Windows refuses it rather than pretending to check, because a
/// bind source this verb has not judged is exactly what it exists to catch.
#[cfg(not(unix))]
fn run_oci_runtime(_runtime: &str) -> Result<(), Failure> {
  Err(Failure::Failed(
    "slates oci-runtime polls a container engine's CLI through facilities this platform does not have; \
     it is not available here"
      .to_owned(),
  ))
}

/// A role's failure as the command's: the same sentence, and the same exit code (1 refused, 4 failed, 5 never served).
impl From<slates_api::role::Failure> for Failure {
  fn from(failure: slates_api::role::Failure) -> Failure {
    match failure {
      slates_api::role::Failure::Refused(text) => Failure::Refused(text),
      slates_api::role::Failure::Failed(text) => Failure::Failed(text),
      slates_api::role::Failure::NeverServed => Failure::NeverServed,
    }
  }
}

/// How a command ends other than well; each maps to one exit code.
#[derive(Debug)]
pub(crate) enum Failure {
  /// The daemon refused; the typed refusal, printed.
  Refused(String),
  /// No daemon at the instance, and why the client concluded so (the rendezvous absent, a claim
  /// unanswered, a daemon that stopped answering) — the operator's first clue.
  Unavailable {
    /// The instance asked for.
    instance: String,
    /// The client's reason.
    cause: String,
  },
  /// The command itself failed (the anchor or the daemon).
  Failed(String),
  /// The workload `slates run` spawned ended with this code, which `run` exits with as it was.
  ChildExited {
    /// The workload's exit code.
    code: i32,
  },
  /// The anchor, whose starter asked it to (`slates_api::StartSpec::until_served`), stopped: its daemons crash-looped
  /// and none of them ever served (A-366); exits `slates_api::role::EXIT_NEVER_SERVED`, as the role does in a host.
  NeverServed,
}
