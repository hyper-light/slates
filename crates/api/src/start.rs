//! Starting an instance from a host's own executable (§2.6 "Boot order", §4.12 "Embedding", A-278): no `slates`
//! binary is installed or run. The host names the executable that serves the roles — its own, whose `main` hands the
//! arguments after its chosen prefix to [`crate::role::main`] — and the anchor runs as that executable, then starts its
//! daemon from it with the same prefix (`crate::role`).
//!
//! **Two ways to start.** [`Instance::start`] spawns the anchor as a child of this process and returns once the
//! instance answers, with the group of a laptop instance made ([`Instance::ensure_group`]). A host that launches the
//! anchor its own way — under its service manager (`systemd-run --user`), so the instance outlives the host's process —
//! takes [`StartSpec::anchor_command`], launches it, and waits with [`Instance::wait_for`].
//!
//! **Waiting for the first answer.** The anchor measures the machine and creates the segment before it holds the
//! instance's rendezvous; until then a connect is refused at once, so the wait asks again at the anchor's own heartbeat
//! cadence (`slates_server::daemon::HEARTBEAT_NS`, the cadence the anchor looks at its daemon). Once the rendezvous is
//! held, a connect itself waits for the daemon (A-114) up to the client's reconnect deadline ([`crate::deadlines`]). A
//! started anchor's exit ends the wait at once with its exit code ([`StartFault::AnchorExited`]); a wait without a
//! child to watch ends at the host's deadline, as `Unavailable`.
//!
//! **Waiting while the anchor starts (A-366).** The anchor judges its starting daemon by the daemon's own CPU progress,
//! never by a wall-clock second (A-232), and never gives an instance up once a daemon has served (A-119): a client may
//! hold a mount on it. So a host that watches the anchor it started needs no deadline of its own: the wait lasts while
//! the anchor lives. A host that has attached nothing yet asks [`StartSpec::until_served`]: an anchor none of whose
//! daemons has reached its serving mark stops at their crash loop rather than looping on an instance no client could
//! have held, and the wait ends [`StartFault::NeverServed`].
//!
//! **The launcher's authority (A-299).** Under Ada's rule of 2026-11-18 the launcher — whoever starts the instance —
//! sets agents' write modes and approves what an ask mode holds, so the starter is to be handed the instance's issuer
//! on every platform (owed, the A-299 row of GAPS): today it holds the issuer only where the anchor's handoff reaches it
//! (the anchor's own children on Linux, the exported pairs elsewhere, [`crate::Issuer`]).

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::error::{Error, StartFault};
use crate::instance::Instance;
use crate::launcher::{ENV_LAUNCHER, LAUNCHER_ON_STDIN, LauncherSecret, record_of};
use crate::role::{FleetSelection, ProcessOptions, Role};

/// What starts an instance: its name, the executable that serves the roles and the prefix before a role's arguments,
/// and the anchor's options.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartSpec {
  /// The instance (its rendezvous name, §4.7).
  pub instance: String,
  /// The executable that serves the roles: the host's own (`std::env::current_exe()`), whose `main` calls
  /// [`crate::role::main`] when its arguments begin with [`StartSpec::prefix`]; or a `slates` binary, with no prefix.
  pub program: PathBuf,
  /// The arguments before a role's own, which the host's `main` recognises (`["slates-role"]`, say). Every process of
  /// the instance is started with them.
  pub prefix: Vec<String>,
  /// Shards, when the host overrides the derivation (tests); otherwise derived from the machine (§4.1).
  pub shards: Option<u16>,
  /// Measure the quick profile (tests): a full profile takes seconds.
  pub quick: bool,
  /// The fleet this node joins (§2.6 boot step 6); `None` for an instance run alone, whose group [`Instance::start`]
  /// makes.
  pub fleet: Option<FleetSelection>,
  /// Where the version-control servers' shared HTTP edge listens, when the host starts one (`--vcs-http`, A-625 part 3;
  /// [`crate::role::vcs_address`]'s rule: cleartext on a loopback address alone without a fleet): the anchor binds it
  /// once and holds it across its daemons' restarts. `None` starts no edge.
  pub vcs_http: Option<slates_rt::tcp::SocketAddrV4>,
  /// Where the version-control servers' SSH edge listens, when the host starts one (`--vcs-ssh`, A-684;
  /// [`crate::role::vcs_ssh_address`]: any IPv4 address and port): the anchor binds it once and makes the edge's host key
  /// at its start (a fleet node's edge presents the node's own key), and holds both across its daemons' restarts. `None`
  /// starts no edge.
  pub vcs_ssh: Option<slates_rt::tcp::SocketAddrV4>,
  /// Where the svn:// listener listens, when the host starts one (`--vcs-svn`, A-689, A-656 part 2;
  /// [`crate::role::vcs_address`]'s rule: ra_svn is cleartext, on a loopback address alone without a fleet): the anchor
  /// binds it once and holds it across its daemons' restarts, as it holds the HTTP edge's. `None` starts no listener.
  pub vcs_svn: Option<slates_rt::tcp::SocketAddrV4>,
  /// The launcher's secret (A-299, [`crate::launcher`]), handed to the anchor at the start so the host's authority
  /// outlives its own restarts and the daemon's; `None` for an instance whose authority is the anchor segment's alone.
  pub launcher: Option<LauncherSecret>,
  /// Stop the anchor when its daemons crash-loop before any of them has served (A-366), rather than loop on them with
  /// a backoff for good as an anchor does (A-119): the instance never answered, so no client can be waiting on it. For
  /// a host that waits on the anchor it started ([`Instance::wait_for`] with the anchor's child), which then ends
  /// [`StartFault::NeverServed`]. Once a daemon has served, the anchor never gives the instance up, opted in or not.
  pub until_served: bool,
}

/// Format: the environment word that hands the anchor [`StartSpec::until_served`] (A-366), as [`ENV_LAUNCHER`] hands it
/// the launcher's record: set on the anchor's command, read once by the anchor.
pub(crate) const ENV_UNTIL_SERVED: &str = "SLATES_UNTIL_SERVED";
/// Format: [`ENV_UNTIL_SERVED`]'s value when the starter asked for it.
const UNTIL_SERVED_ASKED: &str = "1";

/// Whether this process, an anchor, was started [`StartSpec::until_served`] (A-366).
pub(crate) fn until_served_asked() -> bool {
  std::env::var_os(ENV_UNTIL_SERVED).is_some_and(|value| value == UNTIL_SERVED_ASKED)
}

impl StartSpec {
  /// An instance run alone from `program` with `prefix`: derived shards, the full profile, no fleet.
  pub fn new(instance: &str, program: PathBuf, prefix: &[&str]) -> StartSpec {
    StartSpec {
      instance: instance.to_owned(),
      program,
      prefix: prefix.iter().map(|word| (*word).to_owned()).collect(),
      shards: None,
      quick: false,
      fleet: None,
      vcs_http: None,
      vcs_ssh: None,
      vcs_svn: None,
      launcher: None,
      until_served: false,
    }
  }

  /// This spec with [`StartSpec::until_served`]: an anchor whose daemons crash-loop before any of them has served stops,
  /// and a wait on it ends [`StartFault::NeverServed`] (A-366).
  pub fn until_served(mut self) -> StartSpec {
    self.until_served = true;
    self
  }

  /// This spec with the launcher's secret `secret`, which the host keeps and provides again at every start.
  pub fn with_launcher(mut self, secret: LauncherSecret) -> StartSpec {
    self.launcher = Some(secret);
    self
  }

  /// The anchor role this spec starts.
  pub fn role(&self) -> Role {
    Role::Anchor(ProcessOptions {
      instance: self.instance.clone(),
      quick: self.quick,
      shards: self.shards,
      fleet: self.fleet.clone(),
      vcs_http: self.vcs_http,
      vcs_ssh: self.vcs_ssh,
      vcs_svn: self.vcs_svn,
    })
  }

  /// The anchor's command: `PROGRAM PREFIX... --instance NAME anchor [--quick] [--shards N] [--fleet PATH --node NAME]
  /// [--vcs-http ADDRESS] [--vcs-ssh ADDRESS] [--vcs-svn ADDRESS]`. Its standard input is closed, or, with a
  /// launcher's secret, a pipe holding the secret's record and then its end ([`crate::launcher`]), so the anchor's read always ends; a host that
  /// wraps the command (`systemd-run --scope`) keeps the command's standard input, which an exec passes unchanged.
  /// Standard output and error are the host's to direct: the roles write their log lines to standard error. On Unix the
  /// anchor leads a process group of its own, so a signal meant for the host's group (a terminal's interrupt) does not
  /// stop the instance; a host stops it through [`Anchor::stop`], or by sending the anchor `SIGTERM` itself. Refused
  /// [`StartFault::Launcher`] when the pipe cannot be made.
  pub fn anchor_command(&self) -> Result<Command, Error> {
    self.anchor_command_under(&[])
  }

  /// [`StartSpec::anchor_command`] run under `wrapper`, a command that execs what follows it in the same process
  /// (`systemd-run --user --scope --unit=NAME --`, so the instance outlives the host's process): `WRAPPER... PROGRAM
  /// PREFIX... --instance NAME anchor ...`, with the same standard input — the launcher's record, which an exec keeps —
  /// the same environment and the same process group. An empty `wrapper` is [`StartSpec::anchor_command`] itself.
  pub fn anchor_command_under(&self, wrapper: &[&OsStr]) -> Result<Command, Error> {
    let mut command = match wrapper.split_first() {
      Some((first, rest)) => {
        let mut command = Command::new(first);
        command.args(rest).arg(&self.program);
        command
      }
      None => Command::new(&self.program),
    };
    command
      .args(self.prefix.iter().map(OsString::from))
      .args(self.role().args());
    match &self.launcher {
      None => {
        command.stdin(Stdio::null()).env_remove(ENV_LAUNCHER);
      }
      Some(secret) => {
        command
          .stdin(launcher_input(secret)?)
          .env(ENV_LAUNCHER, LAUNCHER_ON_STDIN);
      }
    }
    if self.until_served {
      command.env(ENV_UNTIL_SERVED, UNTIL_SERVED_ASKED);
    } else {
      command.env_remove(ENV_UNTIL_SERVED);
    }
    in_its_own_group(&mut command);
    Ok(command)
  }
}

/// A pipe holding `secret`'s record, its write end closed: the anchor's standard input (A-299).
fn launcher_input(secret: &LauncherSecret) -> Result<Stdio, Error> {
  use std::io::Write;
  let refused = |error: std::io::Error| {
    Error::Start(StartFault::Launcher {
      errno: error.raw_os_error(),
    })
  };
  let (reader, mut writer) = std::io::pipe().map_err(refused)?;
  let mut record = record_of(secret);
  let written = writer.write_all(&record);
  record.fill(0);
  written.map_err(refused)?;
  drop(writer);
  Ok(Stdio::from(reader))
}

/// See [`StartSpec::anchor_command`]: a process group of the anchor's own (`setpgid(0, 0)` in the child).
#[cfg(unix)]
fn in_its_own_group(command: &mut Command) {
  use std::os::unix::process::CommandExt;
  /// Format: `setpgid`'s "the child's own process id" for a new group.
  const OWN_GROUP: i32 = 0;
  command.process_group(OWN_GROUP);
}

/// See the Unix arm: the Windows anchor stays in the host's console group, so its console control handler hears the
/// host's interrupt; [`Anchor::stop`] ends it.
#[cfg(not(unix))]
fn in_its_own_group(_command: &mut Command) {}

/// A started instance: the attached instance, and its anchor.
#[derive(Debug)]
pub struct Started {
  /// The instance, attached, its group made when it runs alone.
  pub instance: Instance,
  /// The anchor, a child of this process.
  pub anchor: Anchor,
}

/// An anchor this process started: a child process. Dropping it leaves the anchor running (an instance outlives the
/// host's handle on it); [`Anchor::stop`] stops it.
#[derive(Debug)]
pub struct Anchor {
  /// The anchor's process.
  child: Child,
}

impl Anchor {
  /// The anchor's process id: every process of the instance carries `--instance NAME` in its arguments, the anchor
  /// first among them.
  pub fn id(&self) -> u32 {
    self.child.id()
  }

  /// The anchor's process, for a host that waits on it or keeps it otherwise.
  pub fn into_child(self) -> Child {
    self.child
  }

  /// Stops the instance and waits for its anchor to exit; the anchor's exit code (`None` when a signal ended it). On
  /// Unix the anchor is asked to stop (`SIGTERM`): its daemon hands off what it leads and exits, killed if it does not
  /// within the anchor's own bounds (§2.6), and then the anchor exits 0. On Windows the anchor is ended outright, and
  /// its daemon with it (the anchor's job object). Either way the instance's volumes are gone: they lived in its memory
  /// (R1).
  pub fn stop(mut self) -> Result<Option<i32>, Error> {
    ask_to_stop(&mut self.child)?;
    self
      .child
      .wait()
      .map(|status| status.code())
      .map_err(|error| {
        Error::Start(StartFault::Wait {
          errno: error.raw_os_error(),
        })
      })
  }
}

/// See [`Anchor::stop`]: `SIGTERM`, which the anchor's stop flag turns into its graceful stop.
#[cfg(unix)]
fn ask_to_stop(child: &mut Child) -> Result<(), Error> {
  let refused = |errno: Option<i32>| Error::Start(StartFault::Stop { errno });
  let pid = i32::try_from(child.id())
    .ok()
    .and_then(rustix::process::Pid::from_raw)
    .ok_or(refused(None))?;
  match rustix::process::kill_process(pid, rustix::process::Signal::TERM) {
    Ok(()) => Ok(()),
    // Already exited, so already stopped: the wait reaps it.
    Err(rustix::io::Errno::SRCH) => Ok(()),
    Err(errno) => Err(refused(Some(errno.raw_os_error()))),
  }
}

/// See [`Anchor::stop`]: the process ended outright.
#[cfg(not(unix))]
fn ask_to_stop(child: &mut Child) -> Result<(), Error> {
  child.kill().map_err(|error| {
    Error::Start(StartFault::Stop {
      errno: error.raw_os_error(),
    })
  })
}

/// Derived: how long a wait for an instance's first answer pauses between connects refused at once: the anchor's
/// heartbeat, the cadence at which the anchor itself looks at its daemon (§4.14), so the wait is at most a beat behind
/// the instance.
fn pause() -> Duration {
  Duration::from_nanos(slates_server::daemon::HEARTBEAT_NS)
}

impl Instance {
  /// Starts the instance `spec` names (A-278): spawns its anchor ([`StartSpec::anchor_command`], standard output and
  /// error inherited from this process) as a child, waits until the instance answers ([`Instance::wait_for`] while the
  /// anchor lives), and makes the group of an instance run alone ([`Instance::ensure_group`]). Refused
  /// [`StartFault::AlreadyRunning`] when the instance already answers (attach to it instead), [`StartFault::Spawn`]
  /// when the program cannot run, and [`StartFault::AnchorExited`] when the anchor ends before the instance answered
  /// (its log lines, on standard error, say why).
  pub fn start(spec: &StartSpec) -> Result<Started, Error> {
    match Instance::attach(&spec.instance) {
      Ok(_running) => {
        return Err(Error::Start(StartFault::AlreadyRunning {
          instance: spec.instance.clone(),
        }));
      }
      Err(error) if error.is_unavailable() => {}
      Err(error) => return Err(error),
    }
    let mut child = spec.anchor_command()?.spawn().map_err(|error| {
      Error::Start(StartFault::Spawn {
        program: spec.program.to_string_lossy().into_owned(),
        errno: error.raw_os_error(),
      })
    })?;
    let mut instance = match Instance::wait_for(&spec.instance, Some(&mut child), None) {
      Ok(instance) => instance,
      Err(error) => return Err(stop_after(child, error)),
    };
    if spec.fleet.is_none()
      && let Err(error) = instance.ensure_group()
    {
      return Err(stop_after(child, error));
    }
    Ok(Started {
      instance,
      anchor: Anchor { child },
    })
  }

  /// Waits until the instance `name` answers, and attaches to it: a connect refused at once (no rendezvous yet: the
  /// anchor is still measuring the machine) is asked again a heartbeat later; a held rendezvous is waited on by the
  /// connect itself. With `anchor`, the anchor's exit ends the wait ([`StartFault::AnchorExited`], or
  /// [`StartFault::NeverServed`] for one started [`StartSpec::until_served`] whose daemons never served), and a host
  /// needs no deadline of its own: the anchor judges its daemon's start by its progress (A-366, the module doc); with
  /// `until`, the deadline does, refused `Unavailable` as the last connect was. With neither, it waits as long as no
  /// daemon answers, so a host that launched the anchor through a service manager passes a deadline.
  pub fn wait_for(
    name: &str,
    mut anchor: Option<&mut Child>,
    until: Option<Instant>,
  ) -> Result<Instance, Error> {
    loop {
      if let Some(child) = anchor.as_deref_mut() {
        anchor_still_runs(child)?;
      }
      match Instance::attach(name) {
        Ok(instance) => return Ok(instance),
        Err(error) if error.is_unavailable() => {
          if until.is_some_and(|until| Instant::now() >= until) {
            return Err(error);
          }
        }
        Err(error) => return Err(error),
      }
      std::thread::park_timeout(pause());
    }
  }
}

/// Refused [`StartFault::AnchorExited`] once `child` has exited, or [`StartFault::NeverServed`] when it stopped
/// because none of its daemons served (A-366: the role's [`crate::role::EXIT_NEVER_SERVED`]).
fn anchor_still_runs(child: &mut Child) -> Result<(), Error> {
  match child.try_wait() {
    Ok(None) => Ok(()),
    Ok(Some(status)) if status.code() == Some(i32::from(crate::role::EXIT_NEVER_SERVED)) => {
      Err(Error::Start(StartFault::NeverServed))
    }
    Ok(Some(status)) => Err(Error::Start(StartFault::AnchorExited {
      code: status.code(),
    })),
    Err(error) => Err(Error::Start(StartFault::Wait {
      errno: error.raw_os_error(),
    })),
  }
}

/// `error`, after stopping the anchor a start spawned and could not finish with, so a refused start leaves no instance
/// behind. A stop that fails too is said on standard error, beside the roles' own lines: the start's failure is still
/// `error`, and the anchor's process id is in the line.
fn stop_after(child: Child, error: Error) -> Error {
  let anchor = Anchor { child };
  let pid = anchor.id();
  if let Err(stop) = anchor.stop() {
    slates_machine::log_line!(
      "slates: a start that failed ({error}) could not stop its anchor, pid {pid}: {stop}"
    );
  }
  error
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A-625 part 3 (§4.17.2 "Transports" item 1). Do: make a host's spec that asks for the version-control edge on a
  /// loopback address, then its anchor's role and command. Expect: the role carries the address, and the command names
  /// it after `--vcs-http`, so the anchor the host starts holds the edge's listener and hands it to every daemon.
  #[test]
  fn a_hosts_anchor_is_started_with_the_vcs_http_edge_it_asks_for() {
    let address = slates_rt::tcp::SocketAddrV4::new(slates_rt::tcp::Ipv4Addr::LOCALHOST, 8080);
    let mut spec = StartSpec::new("edge", PathBuf::from("slates"), &[]);
    spec.vcs_http = Some(address);
    assert_eq!(spec.role().options().vcs_http, Some(address));
    let command = spec.anchor_command().expect("the anchor's command");
    let words: Vec<String> = command
      .get_args()
      .map(|word| word.to_string_lossy().into_owned())
      .collect();
    assert!(
      words
        .windows(2)
        .any(|pair| pair[0] == "--vcs-http" && pair[1] == "127.0.0.1:8080"),
      "{words:?}"
    );
  }

  /// A-684 (§4.17.2 "Transports" item 2). Do: make a host's spec that asks for the version-control SSH edge on a loopback
  /// address, then its anchor's role and command. Expect: the role carries the address, and the command names it after
  /// `--vcs-ssh`, so the anchor the host starts holds the edge's listener and host key for every daemon.
  #[test]
  fn a_hosts_anchor_is_started_with_the_vcs_ssh_edge_it_asks_for() {
    let address = slates_rt::tcp::SocketAddrV4::new(slates_rt::tcp::Ipv4Addr::LOCALHOST, 2222);
    let mut spec = StartSpec::new("ssh-edge", PathBuf::from("slates"), &[]);
    spec.vcs_ssh = Some(address);
    assert_eq!(spec.role().options().vcs_ssh, Some(address));
    let command = spec.anchor_command().expect("the anchor's command");
    let words: Vec<String> = command
      .get_args()
      .map(|word| word.to_string_lossy().into_owned())
      .collect();
    assert!(
      words
        .windows(2)
        .any(|pair| pair[0] == "--vcs-ssh" && pair[1] == "127.0.0.1:2222"),
      "{words:?}"
    );
  }

  /// A-689 (§4.17.4 piece 7). Do: make a host's spec that asks for the svn:// listener on a loopback address, then its
  /// anchor's role and command. Expect: the role carries the address, and the command names it after `--vcs-svn`, so
  /// the anchor the host starts holds the listener and hands it to every daemon.
  #[test]
  fn a_hosts_anchor_is_started_with_the_svn_listener_it_asks_for() {
    let address = slates_rt::tcp::SocketAddrV4::new(slates_rt::tcp::Ipv4Addr::LOCALHOST, 3690);
    let mut spec = StartSpec::new("svn", PathBuf::from("slates"), &[]);
    spec.vcs_svn = Some(address);
    assert_eq!(spec.role().options().vcs_svn, Some(address));
    let command = spec.anchor_command().expect("the anchor's command");
    let words: Vec<String> = command
      .get_args()
      .map(|word| word.to_string_lossy().into_owned())
      .collect();
    assert!(
      words
        .windows(2)
        .any(|pair| pair[0] == "--vcs-svn" && pair[1] == "127.0.0.1:3690"),
      "{words:?}"
    );
  }
}
