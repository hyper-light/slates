//! The chosen-path launcher, `slates exec` (§4.6 "The launcher", §4.12; Phase 3 task 4; A-370). It makes a volume
//! visible at a path its caller names, for one command, without changing what any other process sees and without
//! writing disk: it enters a new user and mount namespace, makes the mount tree private so nothing propagates back,
//! bind-mounts the volume's directory (under the daemon's root mount) onto the chosen path, and execs the command. The
//! parent's view of the chosen path is unchanged, and the launcher creates no filesystem entry: a path that does not
//! exist is refused naming it, never created (AC-3.5).
//!
//! **Where it runs.** Linux alone (user and mount namespaces are Linux's); everywhere else [`run`] answers
//! [`ExecFault::Unsupported`]. It is served by whichever executable runs it: the `slates` binary's `exec` verb, or an
//! embedding host's own executable through [`crate::role::main`], so a host such as kiln runs no slates binary (A-278).
//! Moved here from the `slates` binary on 2026-11-18 (A-370), unchanged in what it does without the AppArmor option.
//!
//! **No privilege (R10, D-2).** An unprivileged user namespace gives the mount capability. Ubuntu's AppArmor (24.04 and
//! 26.04) confines every unprivileged user namespace instead (`kernel.apparmor_restrict_unprivileged_userns=1`): the
//! namespace gets no capability, so the launcher's maps and bind are refused (diagnostics run 42169647315). An
//! administrator lifts that for the launcher's executable alone by loading slates' AppArmor profile, which grants it
//! `userns,` (`slates_client::exec::profile`). Slates never loads it, since that needs root.
//!
//! **The flag (A-370).** Whether the launcher may use a loaded profile is its caller's [`AppArmor`] option, off unless
//! the user opts in (Ada, 2026-11-18: "Ship the profile but feature flag it so whether via SDK (Node/Python), API, or
//! CLI it is user enabled."). Each of these was measured on Ubuntu 24.04 and 26.04 runners, diagnostics run
//! 44344947954:
//! - **Off:** a launcher whose executable runs under its slates profile leaves it before anything else
//!   (`changeprofile unconfined`, its own label's to make). Its namespace then meets the host's restriction exactly as
//!   with no profile loaded, and is refused with the same remedy.
//! - **On:** the launcher makes its namespace under the profile, then marks its exec to leave it (`exec unconfined`).
//!   So the command runs unconfined, and a namespace it makes is restricted as any other process's: the measured
//!   command's own `unshare -Ur` was refused, where with the profile inherited it succeeded.
//! - **Who uses the grant.** A loaded profile labels every process of its executable. Slates' own code uses the grant
//!   in the launcher alone, and there only when asked.
//!
//! **Refusals are typed** ([`ExecFault`]): each names its remedy. Without the option they read as they did before
//! A-370: the AppArmor one, a host that refuses namespaces outright, a missing path.

use std::fmt;
use std::path::Path;
use std::process::Command;

pub use slates_client::exec::{AppArmor, ExecArgsError, ExecSpec, ProfileRefusal};

/// Format: the environment variable naming the daemon's root mount, where a volume appears as `<root>/<volume>`. The
/// caller sets it; the daemon publishing its root mount is owed (GAPS). No default: without a known root the launcher
/// cannot find the volume.
pub const ROOT_ENV: &str = "SLATES_ROOT";

/// What the launcher learned of AppArmor before it made its namespace: whether the host restricts unprivileged user
/// namespaces and whether this thread is exempt, the label it runs under, and the executable it runs from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Confinement {
  /// Whether the host restricts unprivileged user namespaces: `kernel.apparmor_restrict_unprivileged_userns` reads 1
  /// (`Some(true)`) or 0 (`Some(false)`); `None` where the kernel has no such setting.
  pub restricted: Option<bool>,
  /// Whether this thread holds `CAP_SYS_ADMIN` in its user namespace, which the restriction exempts (root, or a process
  /// granted the capability): its namespaces get their capabilities whatever the setting. `None` where it cannot be
  /// read.
  pub privileged: Option<bool>,
  /// The profile this thread runs under, as its AppArmor label names it (`unconfined` when it runs under none); `None`
  /// where AppArmor is not the kernel's security module or the label cannot be read.
  pub label: Option<String>,
  /// This process's executable, as the kernel records it; `None` where it cannot be read.
  pub executable: Option<String>,
}

impl Confinement {
  /// Whether the host's restriction applies to this thread: the host restricts unprivileged user namespaces, and the
  /// thread holds no `CAP_SYS_ADMIN` that would exempt it.
  pub fn restricted_here(&self) -> bool {
    self.restricted == Some(true) && self.privileged != Some(true)
  }

  /// Whether this thread runs under slates' profile for its own executable: the profile named by the executable's path
  /// (`slates_client::exec::profile`).
  pub fn under_own_profile(&self) -> bool {
    match (&self.label, &self.executable) {
      (Some(label), Some(executable)) => label == executable,
      _ => false,
    }
  }
}

/// Whether `exec` can make its namespace on this host, and what it needs if not: what `slates apparmor check` reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Readiness {
  /// The launcher runs on Linux alone.
  Unsupported,
  /// The host does not restrict unprivileged user namespaces, or this process is exempt (it holds `CAP_SYS_ADMIN`): the
  /// launcher needs no profile, flag or not.
  Unrestricted,
  /// The host restricts them and this executable runs under its slates profile: with the [`AppArmor`] option on, the
  /// launcher makes its namespace.
  ProfileLoaded,
  /// The host restricts them and this executable runs under no slates profile: an administrator loads it, as root.
  ProfileMissing,
}

/// The readiness `confinement` shows on Linux.
pub fn readiness(confinement: &Confinement) -> Readiness {
  if !confinement.restricted_here() {
    Readiness::Unrestricted
  } else if confinement.under_own_profile() {
    Readiness::ProfileLoaded
  } else {
    Readiness::ProfileMissing
  }
}

/// Which directory the launcher found missing or not a directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
  /// The volume's directory under the root mount.
  Volume,
  /// The chosen path.
  ChosenPath,
}

impl fmt::Display for Place {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(match self {
      Self::Volume => "the volume's mount",
      Self::ChosenPath => "the chosen path",
    })
  }
}

/// Which of the namespace's map files a refusal names, and the call refused on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapStep {
  /// Opening `/proc/self/setgroups`.
  OpenSetgroups,
  /// Writing `deny` to it.
  WriteSetgroups,
  /// Opening `/proc/self/gid_map`.
  OpenGidMap,
  /// Writing the caller's gid to itself.
  WriteGidMap,
  /// Opening `/proc/self/uid_map`.
  OpenUidMap,
  /// Writing the caller's uid to itself.
  WriteUidMap,
}

impl fmt::Display for MapStep {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(match self {
      Self::OpenSetgroups => "open /proc/self/setgroups",
      Self::WriteSetgroups => "write /proc/self/setgroups",
      Self::OpenGidMap => "open /proc/self/gid_map",
      Self::WriteGidMap => "write /proc/self/gid_map",
      Self::OpenUidMap => "open /proc/self/uid_map",
      Self::WriteUidMap => "write /proc/self/uid_map",
    })
  }
}

/// Which AppArmor change the launcher could not make.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transition {
  /// Leaving the profile before the namespace, the option being off.
  Leave,
  /// Marking the command's exec to run outside the profile, the option being on.
  CommandOutside,
}

/// Why the launcher could not run its command: a closed taxonomy, each naming its remedy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecFault {
  /// The launcher needs Linux's user and mount namespaces.
  Unsupported,
  /// [`ROOT_ENV`] is not set, so the volume cannot be found.
  NoRoot,
  /// A directory the launcher needs does not exist; it never creates one (AC-3.5).
  Missing {
    /// Which directory.
    place: Place,
    /// Its path.
    path: String,
  },
  /// A path the launcher needs as a directory is something else.
  NotDirectory {
    /// Which path.
    place: Place,
    /// The path.
    path: String,
  },
  /// No command to run.
  NoCommand,
  /// The kernel refused a user and mount namespace outright.
  NamespacesRefused {
    /// The error code.
    code: i32,
  },
  /// The host's AppArmor confines unprivileged user namespaces, and the launcher ran under no profile of its own: the
  /// refusal as it read before A-370.
  ConfinedByAppArmor,
  /// The host's AppArmor confines unprivileged user namespaces, and slates' profile for this executable is loaded, but
  /// the caller did not ask the launcher to use it, so the launcher left it.
  ProfileNotChosen {
    /// The executable.
    executable: String,
  },
  /// The caller asked the launcher to use slates' AppArmor profile, the host's AppArmor confines unprivileged user
  /// namespaces, and no slates profile is loaded for this executable.
  ProfileNotLoaded {
    /// The executable, when the kernel names it.
    executable: Option<String>,
  },
  /// An AppArmor change the launcher needs was refused.
  ProfileTransition {
    /// Which change.
    transition: Transition,
    /// The executable whose profile it is.
    executable: String,
    /// The error code.
    code: i32,
  },
  /// Writing the namespace's maps was refused for a reason AppArmor's restriction does not explain.
  Map {
    /// The call refused.
    step: MapStep,
    /// The error code.
    code: i32,
  },
  /// Making the mount tree private was refused.
  Private {
    /// The error code.
    code: i32,
  },
  /// The bind was refused.
  Bind {
    /// The volume's directory.
    source: String,
    /// The chosen path.
    at: String,
    /// The error code.
    code: i32,
  },
  /// The command could not be executed.
  Exec {
    /// The program.
    program: String,
    /// The error code, when the system gave one.
    code: Option<i32>,
    /// The error's kind.
    kind: std::io::ErrorKind,
  },
}

/// Format: the refusal for a host whose AppArmor confines unprivileged user namespaces, as it read before A-370 and
/// still reads where the launcher ran under no profile of its own.
const CONFINED_TEXT: &str = "this host's AppArmor confines unprivileged user namespaces \
  (kernel.apparmor_restrict_unprivileged_userns=1), so the launcher's namespace has no capability to bind the volume; \
  an administrator can allow it with an AppArmor profile for the slates binary that grants `userns,`, or set the \
  sysctl to 0";

/// Format: the restriction, as the AppArmor refusals of A-370 open.
const RESTRICTED_TEXT: &str = "this host's AppArmor confines unprivileged user namespaces (kernel.apparmor_restrict_unprivileged_userns=1)";

/// Format: the option, as each surface spells it.
const OPTION_TEXT: &str = "`slates exec --apparmor-profile`, slates-api's `AppArmor::Profile`, or the SDKs' \
  `apparmor_profile=True` / `{ apparmorProfile: true }`";

impl fmt::Display for ExecFault {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Unsupported => f.write_str(
        "slates exec needs Linux user and mount namespaces; it is not available on this platform",
      ),
      Self::NoRoot => write!(
        f,
        "{ROOT_ENV} is not set; it names the daemon's root mount (where the volume appears)"
      ),
      Self::Missing { place, path } => write!(
        f,
        "{place} {path} does not exist (the launcher does not create it)"
      ),
      Self::NotDirectory { place, path } => write!(f, "{place} {path} is not a directory"),
      Self::NoCommand => f.write_str("exec needs a command"),
      Self::NamespacesRefused { code } => write!(
        f,
        "cannot create a user+mount namespace (code {code}); enable unprivileged user namespaces (sysctl \
         kernel.unprivileged_userns_clone=1, or an AppArmor userns profile)"
      ),
      Self::ConfinedByAppArmor => f.write_str(CONFINED_TEXT),
      Self::ProfileNotChosen { executable } => write!(
        f,
        "{RESTRICTED_TEXT}; slates' AppArmor profile for {executable} is loaded, and the launcher makes its namespace \
         under it only when asked: {OPTION_TEXT}"
      ),
      Self::ProfileNotLoaded { executable } => fmt_not_loaded(f, executable.as_deref()),
      Self::ProfileTransition {
        transition,
        executable,
        code,
      } => fmt_transition(f, *transition, executable, *code),
      Self::Map { step, code } => write!(f, "{step}: code {code}"),
      Self::Private { code } => write!(f, "making the mount tree private: code {code}"),
      Self::Bind { source, at, code } => {
        write!(f, "cannot bind {source} onto {at} (code {code})")
      }
      Self::Exec {
        program,
        code,
        kind,
      } => match code {
        Some(code) => write!(
          f,
          "exec {program}: {}",
          std::io::Error::from_raw_os_error(*code)
        ),
        None => write!(f, "exec {program}: {kind}"),
      },
    }
  }
}

/// [`ExecFault::ProfileNotLoaded`]'s sentence: the restriction, and how an administrator loads the profile for this
/// executable, or why its path cannot name one.
fn fmt_not_loaded(f: &mut fmt::Formatter<'_>, executable: Option<&str>) -> fmt::Result {
  let Some(executable) = executable else {
    return write!(
      f,
      "{RESTRICTED_TEXT}, and the kernel does not name this executable, so no slates AppArmor profile can attach to it"
    );
  };
  match (
    slates_client::exec::profile_file(executable),
    slates_client::exec::load_command(executable),
  ) {
    (Ok(file), Ok(load)) => write!(
      f,
      "{RESTRICTED_TEXT}, and no slates AppArmor profile is loaded for {executable}; an administrator loads it as \
       root: save the profile (`slates apparmor profile --executable {executable}` prints it) as {file}, then `{load}`"
    ),
    (Err(refusal), _) | (_, Err(refusal)) => write!(
      f,
      "{RESTRICTED_TEXT}, and no slates AppArmor profile can be loaded for this executable: {refusal}"
    ),
  }
}

/// [`ExecFault::ProfileTransition`]'s sentence.
fn fmt_transition(
  f: &mut fmt::Formatter<'_>,
  transition: Transition,
  executable: &str,
  code: i32,
) -> fmt::Result {
  match transition {
    Transition::Leave => write!(
      f,
      "the launcher runs under slates' AppArmor profile for {executable} and could not leave it (code {code}), so it \
       cannot make its namespace as the host would without the profile; to make it under the profile: {OPTION_TEXT}"
    ),
    Transition::CommandOutside => write!(
      f,
      "the launcher could not mark its command to run outside slates' AppArmor profile for {executable} (code \
       {code}); refused rather than run the command holding the profile's user-namespace grant"
    ),
  }
}

impl std::error::Error for ExecFault {}

/// Judges a refused map write (A-370): by the host's restriction, the caller's option, and whether the launcher left
/// its own profile first. The same refusal reads as it did before A-370 wherever the launcher ran under no profile of
/// its own and was not asked to use one.
pub fn judge_map_refusal(
  step: MapStep,
  code: i32,
  apparmor: AppArmor,
  confinement: &Confinement,
  left_own_profile: bool,
) -> ExecFault {
  if !confinement.restricted_here() {
    return ExecFault::Map { step, code };
  }
  match (apparmor, confinement.executable.clone()) {
    (AppArmor::Off, Some(executable)) if left_own_profile => {
      ExecFault::ProfileNotChosen { executable }
    }
    (AppArmor::Off, _) => ExecFault::ConfinedByAppArmor,
    // Under its own profile and still refused: not the restriction, which the profile lifts, so the call is named.
    (AppArmor::Profile, _) if confinement.under_own_profile() => ExecFault::Map { step, code },
    (AppArmor::Profile, executable) => ExecFault::ProfileNotLoaded { executable },
  }
}

/// The command that runs the launcher for `spec` from `program`, an executable that serves it: the `slates` binary
/// with no `prefix`, or an embedding host's own executable with the prefix its `main` hands to [`crate::role::main`].
/// `SLATES_ROOT` is the caller's to set on it. Where the [`AppArmor`] option is on, slates' profile must be loaded for
/// `program`'s real path (`slates_client::exec::profile`).
pub fn command(spec: &ExecSpec, program: &Path, prefix: &[String]) -> Command {
  let mut command = Command::new(program);
  command.args(prefix).args(spec.arguments());
  command
}

/// The role entry's launcher (A-370): `args` begin with the `exec` word; the request runs and never returns on
/// success. A usage error answers [`crate::role::EXIT_USAGE`]; a refusal prints its sentence and answers
/// [`crate::role::EXIT_FAILED`], as the `slates` command's `exec` does.
pub(crate) fn serve(args: &[String]) -> std::process::ExitCode {
  let spec = match ExecSpec::parse(args) {
    Ok(spec) => spec,
    Err(error) => {
      slates_machine::log_line!(
        "slates: {error}\nusage: exec --volume V --at PATH [--apparmor-profile] -- CMD [ARG ...]"
      );
      return std::process::ExitCode::from(crate::role::EXIT_USAGE);
    }
  };
  match run(&spec) {
    Ok(never) => match never {},
    Err(fault) => {
      slates_machine::log_line!("slates: {fault}");
      std::process::ExitCode::from(crate::role::EXIT_FAILED)
    }
  }
}

#[cfg(target_os = "linux")]
pub use linux::{confinement, run};

/// The launcher on a platform without Linux's namespaces: refused, typed.
#[cfg(not(target_os = "linux"))]
pub fn run(_spec: &ExecSpec) -> Result<std::convert::Infallible, ExecFault> {
  Err(ExecFault::Unsupported)
}

/// What the launcher would learn of AppArmor: nothing, off Linux.
#[cfg(not(target_os = "linux"))]
pub fn confinement() -> Confinement {
  Confinement::default()
}

/// What `slates apparmor check` reports: the confinement, and whether `exec` can make its namespace here.
pub fn check() -> (Confinement, Readiness) {
  let confinement = confinement();
  let readiness = if cfg!(target_os = "linux") {
    readiness(&confinement)
  } else {
    Readiness::Unsupported
  };
  (confinement, readiness)
}

#[cfg(target_os = "linux")]
mod linux {
  use std::convert::Infallible;
  use std::os::unix::process::CommandExt;
  use std::process::Command;

  use rustix::fs::{Mode, OFlags};
  use rustix::mount::{MountPropagationFlags, mount_bind, mount_change};
  use rustix::thread::UnshareFlags;

  use super::{
    AppArmor, Confinement, ExecFault, ExecSpec, MapStep, Place, ROOT_ENV, Transition,
    judge_map_refusal,
  };

  /// Format: the sysctl through which Ubuntu's AppArmor (24.04 and later) confines every unprivileged user namespace.
  const APPARMOR_USERNS_SYSCTL: &str = "/proc/sys/kernel/apparmor_restrict_unprivileged_userns";
  /// Format: the bytes of that sysctl's value, a flag and its newline (`0\n` or `1\n`).
  const SYSCTL_FLAG_BYTES: usize = 2;
  /// Format: this thread's AppArmor label, which a write of `changeprofile NAME` changes at once (Linux 5.8's
  /// per-module attribute directory; `/proc/thread-self` so a host's other threads keep theirs).
  const LABEL_FILE: &str = "/proc/thread-self/attr/apparmor/current";
  /// Format: the label this thread's next exec takes, which a write of `exec NAME` sets.
  const EXEC_LABEL_FILE: &str = "/proc/thread-self/attr/apparmor/exec";
  /// Format: the request that leaves the profile at once for the namespace's unconfined label.
  const LEAVE_REQUEST: &str = "changeprofile unconfined";
  /// Format: the request that runs this thread's next exec unconfined.
  const COMMAND_OUTSIDE_REQUEST: &str = "exec unconfined";
  /// Format: the longest label read: a profile named by an executable's path (`PATH_MAX`, 4,096 bytes on Linux,
  /// `linux/limits.h`), then its mode in parentheses, the longest `" (unconfined)"`, and the newline. A longer label is a
  /// stack or another module's and names no profile of slates', so the read stops there.
  const LABEL_BYTES: usize = 4096 + " (unconfined)\n".len();

  /// Runs the launcher. On success it never returns (it execs the command); any error before the exec is an
  /// [`ExecFault`], and the exec's own failure (the command not found) is one too.
  pub fn run(spec: &ExecSpec) -> Result<Infallible, ExecFault> {
    let root = std::env::var(ROOT_ENV).map_err(|_| ExecFault::NoRoot)?;
    let source = format!("{}/{}", root.trim_end_matches('/'), spec.volume);
    // The source must exist (the daemon mounted the volume); the chosen path must exist and be a directory (the
    // launcher never creates it, AC-3.5).
    ensure_directory(&source, Place::Volume)?;
    ensure_directory(&spec.at, Place::ChosenPath)?;
    let (program, args) = spec.command.split_first().ok_or(ExecFault::NoCommand)?;
    let confinement = confinement();
    let own_profile = confinement.under_own_profile();
    let executable = confinement.executable.clone().unwrap_or_default();
    // Off: a loaded profile changes nothing for a caller who did not ask (A-370), so the launcher leaves it first.
    let left_own_profile = own_profile && spec.apparmor == AppArmor::Off;
    if left_own_profile {
      write_attribute(LABEL_FILE, LEAVE_REQUEST).map_err(|code| ExecFault::ProfileTransition {
        transition: Transition::Leave,
        executable: executable.clone(),
        code,
      })?;
    }
    enter_namespace().map_err(|refused| match refused {
      Refused::Unshare(code) => ExecFault::NamespacesRefused { code },
      Refused::Map(step, code) => {
        judge_map_refusal(step, code, spec.apparmor, &confinement, left_own_profile)
      }
    })?;
    make_root_private()?;
    mount_bind(&source, &spec.at).map_err(|e| ExecFault::Bind {
      source: source.clone(),
      at: spec.at.clone(),
      code: e.raw_os_error(),
    })?;
    // On: the namespace is made; the command runs outside the profile, so it holds nothing the host withholds from it.
    if own_profile && spec.apparmor == AppArmor::Profile {
      write_attribute(EXEC_LABEL_FILE, COMMAND_OUTSIDE_REQUEST).map_err(|code| {
        ExecFault::ProfileTransition {
          transition: Transition::CommandOutside,
          executable,
          code,
        }
      })?;
    }
    // Exec replaces this process; the bind mount lives only in this namespace, so the parent's view is untouched and
    // nothing is left mounted when the command exits.
    let error = Command::new(program).args(args).exec();
    Err(ExecFault::Exec {
      program: program.clone(),
      code: error.raw_os_error(),
      kind: error.kind(),
    })
  }

  /// What the launcher learns of AppArmor: the host's restriction, this thread's label, its executable.
  pub fn confinement() -> Confinement {
    Confinement {
      restricted: restricted(),
      privileged: privileged(),
      label: label(),
      executable: executable(),
    }
  }

  /// Whether this thread holds `CAP_SYS_ADMIN` in its user namespace, which the restriction exempts, or `None` where
  /// the kernel does not say.
  fn privileged() -> Option<bool> {
    let sets = rustix::thread::capabilities(None).ok()?;
    Some(
      sets
        .effective
        .contains(rustix::thread::CapabilitySet::SYS_ADMIN),
    )
  }

  /// Whether the host restricts unprivileged user namespaces, or `None` where it has no such setting.
  fn restricted() -> Option<bool> {
    // structural: allow — a read of a /proc/sys value (a kernel pseudo-file, not disk, R1).
    let fd = rustix::fs::open(APPARMOR_USERNS_SYSCTL, OFlags::RDONLY, Mode::empty()).ok()?;
    let mut value = [0u8; SYSCTL_FLAG_BYTES];
    let read = rustix::io::read(&fd, &mut value).ok()?;
    if read == 0 {
      return None;
    }
    Some(value.first() == Some(&b'1'))
  }

  /// The profile this thread runs under, by its label's name, or `None` where the label cannot be read.
  fn label() -> Option<String> {
    // structural: allow — a read of this thread's AppArmor label (a kernel pseudo-file, not disk, R1).
    let fd = rustix::fs::open(LABEL_FILE, OFlags::RDONLY, Mode::empty()).ok()?;
    let mut buffer = vec![0u8; LABEL_BYTES];
    let mut filled = 0usize;
    while let Some(rest) = buffer.get_mut(filled..) {
      if rest.is_empty() {
        // Full: a label this long names no profile of slates'.
        return None;
      }
      match rustix::io::read(&fd, rest) {
        Ok(0) => break,
        Ok(read) => filled = filled.checked_add(read)?,
        Err(_) => return None,
      }
    }
    let text = std::str::from_utf8(buffer.get(..filled)?).ok()?;
    Some(slates_client::exec::label_name(text).to_owned())
  }

  /// This process's executable as the kernel records it, or `None` where it cannot be read.
  fn executable() -> Option<String> {
    // structural: allow — a read of the /proc link naming this process's executable (a kernel pseudo-file, R1).
    let path = rustix::fs::readlink("/proc/self/exe", Vec::new()).ok()?;
    path.into_string().ok()
  }

  /// Writes `request` to one of this thread's AppArmor attributes, answering the error code on refusal. A kernel
  /// pseudo-file, not disk: it changes the thread's label and creates nothing (R1).
  fn write_attribute(file: &str, request: &str) -> Result<(), i32> {
    // structural: allow — a write to this thread's AppArmor attribute, a /proc control file, not a disk file.
    let fd = rustix::fs::open(file, OFlags::WRONLY, Mode::empty()).map_err(|e| e.raw_os_error())?;
    rustix::io::write(&fd, request.as_bytes()).map_err(|e| e.raw_os_error())?;
    Ok(())
  }

  /// Refuses when `path` is not an existing directory, naming it exactly; never creates it.
  fn ensure_directory(path: &str, place: Place) -> Result<(), ExecFault> {
    // structural: allow — a read-only stat of the path to refuse a missing one; creates nothing.
    match rustix::fs::stat(path) {
      Ok(st)
        if rustix::fs::FileType::from_raw_mode(st.st_mode) == rustix::fs::FileType::Directory =>
      {
        Ok(())
      }
      Ok(_) => Err(ExecFault::NotDirectory {
        place,
        path: path.to_owned(),
      }),
      Err(_) => Err(ExecFault::Missing {
        place,
        path: path.to_owned(),
      }),
    }
  }

  /// Which step of the namespace the kernel refused, and its code.
  enum Refused {
    /// The namespace itself.
    Unshare(i32),
    /// A map write.
    Map(MapStep, i32),
  }

  /// Enters a new user and mount namespace, mapping the caller's own uid and gid so the mount capability is available
  /// without privilege.
  fn enter_namespace() -> Result<(), Refused> {
    let uid = rustix::process::getuid().as_raw();
    let gid = rustix::process::getgid().as_raw();
    // SAFETY: `unshare` changes this process's namespace membership, which rustix marks unsafe because it affects
    // every thread; the launcher is single-threaded here (the kernel refuses CLONE_NEWUSER to a threaded process) and
    // execs immediately after, so no other thread observes the change. It creates the private namespace the bind
    // mount and exec then use.
    let unshared =
      unsafe { rustix::thread::unshare_unsafe(UnshareFlags::NEWUSER | UnshareFlags::NEWNS) };
    unshared.map_err(|e| Refused::Unshare(e.raw_os_error()))?;
    // The mapping: deny setgroups, then map this uid and gid to themselves inside the namespace.
    write_map(
      "/proc/self/setgroups",
      "deny",
      (MapStep::OpenSetgroups, MapStep::WriteSetgroups),
    )?;
    write_map(
      "/proc/self/gid_map",
      &format!("{gid} {gid} 1"),
      (MapStep::OpenGidMap, MapStep::WriteGidMap),
    )?;
    write_map(
      "/proc/self/uid_map",
      &format!("{uid} {uid} 1"),
      (MapStep::OpenUidMap, MapStep::WriteUidMap),
    )
  }

  /// Writes `contents` to a `/proc/self` map file (a kernel pseudo-file, not disk, so this creates nothing on any
  /// filesystem, R1), naming the refused call by `steps` (the open's, the write's).
  fn write_map(path: &str, contents: &str, steps: (MapStep, MapStep)) -> Result<(), Refused> {
    // /proc/self is a kernel pseudo-filesystem; the file already exists and writing it configures the namespace, it
    // never touches disk (R1).
    // structural: allow — a write to a /proc control file, not a disk file.
    let fd = rustix::fs::open(path, OFlags::WRONLY, Mode::empty())
      .map_err(|e| Refused::Map(steps.0, e.raw_os_error()))?;
    rustix::io::write(&fd, contents.as_bytes())
      .map_err(|e| Refused::Map(steps.1, e.raw_os_error()))?;
    Ok(())
  }

  /// Makes the mount tree recursively private so the bind mount does not propagate to the parent namespace (the
  /// parent's view stays unchanged).
  fn make_root_private() -> Result<(), ExecFault> {
    mount_change(
      "/",
      MountPropagationFlags::REC | MountPropagationFlags::PRIVATE,
    )
    .map_err(|e| ExecFault::Private {
      code: e.raw_os_error(),
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn host(restricted: Option<bool>, label: &str, executable: &str) -> Confinement {
    Confinement {
      restricted,
      privileged: Some(false),
      label: Some(label.to_owned()),
      executable: Some(executable.to_owned()),
    }
  }

  const SLATES: &str = "/usr/local/bin/slates";

  /// A-370, the flag's judgement. Do: judge a refused map write on every host the launcher meets, with the option off
  /// and on. Expect: off and under no profile, the AppArmor refusal word for word as before A-370; off after leaving a
  /// loaded profile, a refusal naming the option; on with no profile, the commands that load it for this executable;
  /// on under the profile, the call itself (the restriction cannot explain it); and on an unrestricted host the call,
  /// whatever the option.
  #[test]
  fn a_refused_map_write_is_judged_by_the_restriction_the_option_and_the_profile() {
    let step = MapStep::OpenSetgroups;
    let restricted_bare = host(Some(true), "unconfined", SLATES);
    let restricted_loaded = host(Some(true), SLATES, SLATES);
    let unrestricted = host(Some(false), "unconfined", SLATES);

    let before = judge_map_refusal(step, 13, AppArmor::Off, &restricted_bare, false);
    assert_eq!(before, ExecFault::ConfinedByAppArmor);
    assert_eq!(
      before.to_string(),
      "this host's AppArmor confines unprivileged user namespaces (kernel.apparmor_restrict_unprivileged_userns=1), \
       so the launcher's namespace has no capability to bind the volume; an administrator can allow it with an \
       AppArmor profile for the slates binary that grants `userns,`, or set the sysctl to 0",
      "the refusal reads as before A-370"
    );

    let not_chosen = judge_map_refusal(step, 13, AppArmor::Off, &restricted_loaded, true);
    assert_eq!(
      not_chosen,
      ExecFault::ProfileNotChosen {
        executable: SLATES.into()
      }
    );
    assert!(
      not_chosen.to_string().contains("--apparmor-profile"),
      "{not_chosen}"
    );

    let not_loaded = judge_map_refusal(step, 13, AppArmor::Profile, &restricted_bare, false);
    assert_eq!(
      not_loaded,
      ExecFault::ProfileNotLoaded {
        executable: Some(SLATES.into())
      }
    );
    let text = not_loaded.to_string();
    assert!(
      text.contains("/etc/apparmor.d/usr.local.bin.slates")
        && text.contains("sudo apparmor_parser -r /etc/apparmor.d/usr.local.bin.slates"),
      "{text}"
    );

    assert_eq!(
      judge_map_refusal(step, 1, AppArmor::Profile, &restricted_loaded, false),
      ExecFault::Map { step, code: 1 }
    );
    for apparmor in [AppArmor::Off, AppArmor::Profile] {
      let fault = judge_map_refusal(step, 13, apparmor, &unrestricted, false);
      assert_eq!(fault, ExecFault::Map { step, code: 13 });
      assert_eq!(fault.to_string(), "open /proc/self/setgroups: code 13");
    }
  }

  /// A-370. Do: ask the readiness of every host shape. Expect: unrestricted (or no such setting) needs no profile;
  /// restricted needs this executable's own profile, and a profile of another name or another executable's is not it.
  #[test]
  fn readiness_follows_the_restriction_and_the_executables_own_profile() {
    assert_eq!(
      readiness(&host(None, "unconfined", SLATES)),
      Readiness::Unrestricted
    );
    assert_eq!(
      readiness(&host(Some(false), SLATES, SLATES)),
      Readiness::Unrestricted
    );
    assert_eq!(
      readiness(&host(Some(true), SLATES, SLATES)),
      Readiness::ProfileLoaded
    );
    assert_eq!(
      readiness(&host(Some(true), "slates", SLATES)),
      Readiness::ProfileMissing
    );
    assert_eq!(
      readiness(&host(Some(true), "/usr/bin/slates", SLATES)),
      Readiness::ProfileMissing
    );
    assert_eq!(
      readiness(&Confinement {
        restricted: Some(true),
        privileged: None,
        label: None,
        executable: Some(SLATES.into()),
      }),
      Readiness::ProfileMissing
    );
    let root = Confinement {
      privileged: Some(true),
      ..host(Some(true), "unconfined", SLATES)
    };
    assert_eq!(
      readiness(&root),
      Readiness::Unrestricted,
      "CAP_SYS_ADMIN exempts a process from the restriction"
    );
    assert_eq!(
      judge_map_refusal(MapStep::WriteUidMap, 1, AppArmor::Off, &root, false),
      ExecFault::Map {
        step: MapStep::WriteUidMap,
        code: 1
      },
      "an exempt process's refusal is not the restriction's"
    );
  }

  /// A-370, the flag's plumbing through the API. Do: build the launcher's command for a host executable and prefix, with
  /// the option on and off. Expect: the host's program, its prefix, then the arguments the role entry reads back to the
  /// same request, the switch present exactly when the option is on.
  #[test]
  fn the_api_builds_the_launchers_command_with_the_option_exactly_when_it_is_on() {
    for apparmor in [AppArmor::Off, AppArmor::Profile] {
      let spec = ExecSpec {
        volume: "scratch".into(),
        at: "/home/u/build".into(),
        command: vec!["cargo".into(), "build".into()],
        apparmor,
      };
      let prefix = vec!["slates-role".to_owned()];
      let built = command(&spec, Path::new("/usr/bin/kiln"), &prefix);
      assert_eq!(built.get_program(), "/usr/bin/kiln");
      let args: Vec<String> = built
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
      assert_eq!(args[0], "slates-role");
      assert_eq!(
        args.contains(&"--apparmor-profile".to_owned()),
        apparmor == AppArmor::Profile
      );
      assert_eq!(ExecSpec::parse(&args[1..]), Ok(spec));
    }
  }

  /// §4.6 "The launcher". Do: render every refusal that existed before A-370. Expect: the sentences the CLI printed
  /// then, so a script that read them reads them still.
  #[test]
  fn the_refusals_before_a370_read_as_they_did() {
    let cases = [
      (
        ExecFault::Unsupported,
        "slates exec needs Linux user and mount namespaces; it is not available on this platform",
      ),
      (
        ExecFault::NoRoot,
        "SLATES_ROOT is not set; it names the daemon's root mount (where the volume appears)",
      ),
      (
        ExecFault::Missing {
          place: Place::ChosenPath,
          path: "/p/no".into(),
        },
        "the chosen path /p/no does not exist (the launcher does not create it)",
      ),
      (
        ExecFault::NotDirectory {
          place: Place::Volume,
          path: "/r/v".into(),
        },
        "the volume's mount /r/v is not a directory",
      ),
      (ExecFault::NoCommand, "exec needs a command"),
      (
        ExecFault::NamespacesRefused { code: 1 },
        "cannot create a user+mount namespace (code 1); enable unprivileged user namespaces (sysctl \
         kernel.unprivileged_userns_clone=1, or an AppArmor userns profile)",
      ),
      (
        ExecFault::Private { code: 13 },
        "making the mount tree private: code 13",
      ),
      (
        ExecFault::Bind {
          source: "/r/v".into(),
          at: "/p".into(),
          code: 13,
        },
        "cannot bind /r/v onto /p (code 13)",
      ),
    ];
    for (fault, text) in cases {
      assert_eq!(fault.to_string(), text);
    }
  }
}
