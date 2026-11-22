//! A child handed only the descriptors its spawn names (main 01a20182's ruling of 2026-11-22 02:06 PST on the hook
//! pipe race; A-622's driver and every hook of git's, Mercurial's and Subversion's, and the daemon's own helpers).
//!
//! **Why.** A process that spawns from several threads at once leaks descriptors between its spawns where a pipe is
//! made in two steps. macOS has no `pipe2`: the standard library makes a pipe with `pipe` and marks its ends
//! close-on-exec a call later, so a child another thread spawns in between inherits the pipe's ends and holds them for
//! its life. A hook's words stream then stays open past the hook's own exit, held by a stranger, and its run waits on it
//! (main's daemon_edge flake: a first run, with no learned bound, waited for a refusing hook's words until its client's
//! 10 s read gave up, 2 of 12 runs). The standard library's `Command` cannot say "inherit only these".
//!
//! **What.** [`spawn`] starts the child through `posix_spawn` with file actions naming its standard input, output and
//! error alone (a descriptor of the caller's, duplicated to 0, 1 or 2, or the null device), its working directory, and
//! where asked a process group of its own. On macOS the spawn carries `POSIX_SPAWN_CLOEXEC_DEFAULT`: every descriptor
//! the file actions do not name is closed in the child whatever its close-on-exec flag, so no window between a pipe's
//! making and its marking reaches a child spawned here. On Linux the standard library makes pipes with `pipe2` and
//! `O_CLOEXEC` (and sockets with `SOCK_CLOEXEC`), atomic, so no such window exists; the same spawn serves there, and the
//! same test holds it. The child's signal mask is emptied and `SIGPIPE` set back to its default, as the standard
//! library's spawn does (a Rust process ignores `SIGPIPE`, and an ignored signal survives `exec`).
//!
//! **The child** ([`SpawnedChild`]): its pid, waited for and reaped by its owner (the hook driver's exit watch and
//! reaper); never reaped behind its owner's back, so the pid names it until its owner lets it go.

use std::ffi::{CString, OsStr, OsString};
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::ExitStatus;

/// Where one of the child's standard streams goes.
#[derive(Clone, Copy, Debug)]
pub enum ChildStream<'a> {
  /// The null device.
  Null,
  /// A descriptor of the caller's, duplicated to the stream's number in the child; the caller's own stays open.
  Fd(BorrowedFd<'a>),
}

/// What the child's environment starts from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Environment {
  /// Nothing but the overrides (a Subversion hook's hooks-env section, A-623).
  Clear,
  /// This process's environment, then the overrides.
  Inherited,
}

/// A child to spawn (module docs).
#[derive(Debug)]
pub struct Spawn<'a> {
  /// The program, an absolute path (no search of `PATH`).
  pub program: &'a Path,
  /// Its arguments after its own name, which is `program`.
  pub args: &'a [&'a OsStr],
  /// What its environment starts from.
  pub environment: Environment,
  /// Variables set over it, in order (a later one wins).
  pub overrides: &'a [(&'a OsStr, &'a OsStr)],
  /// Its working directory.
  pub directory: &'a Path,
  /// Whether it leads a process group of its own (so it and what it starts can be ended together).
  pub own_group: bool,
  /// Its standard input.
  pub stdin: ChildStream<'a>,
  /// Its standard output.
  pub stdout: ChildStream<'a>,
  /// Its standard error.
  pub stderr: ChildStream<'a>,
}

/// A child [`spawn`] started: its pid, and its status once its owner reaped it.
#[derive(Debug)]
pub struct SpawnedChild {
  /// Its pid.
  pid: u32,
  /// Its status, once reaped.
  status: Option<ExitStatus>,
}

/// Format: the null device a stream with no other end reads and writes.
const NULL_DEVICE: &[u8] = b"/dev/null";

/// The C string of `bytes`, refused where they hold a NUL.
fn c_string(bytes: &[u8]) -> io::Result<CString> {
  CString::new(bytes).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

/// The child's environment as `NAME=VALUE` strings (module docs): this process's or none, then the overrides, a name
/// set once.
fn environment_of(spawn: &Spawn<'_>) -> io::Result<Vec<CString>> {
  let mut by_name: Vec<(OsString, OsString)> = match spawn.environment {
    Environment::Inherited => std::env::vars_os().collect(),
    Environment::Clear => Vec::new(),
  };
  for (name, value) in spawn.overrides {
    by_name.retain(|(held, _)| held.as_os_str() != *name);
    by_name.push(((*name).to_owned(), (*value).to_owned()));
  }
  by_name
    .iter()
    .map(|(name, value)| {
      let mut entry = name.as_bytes().to_vec();
      entry.push(b'=');
      entry.extend_from_slice(value.as_bytes());
      c_string(&entry)
    })
    .collect()
}

/// `errno`-style answer of a `posix_spawn` call, as an I/O error.
fn checked(answer: libc::c_int) -> io::Result<()> {
  if answer == 0 {
    Ok(())
  } else {
    Err(io::Error::from_raw_os_error(answer))
  }
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
  /// `posix_spawn_file_actions_addchdir_np` (macOS 10.15 and later, `<spawn.h>`), which the pinned libc does not bind
  /// on Apple platforms.
  fn posix_spawn_file_actions_addchdir_np(
    actions: *mut libc::posix_spawn_file_actions_t,
    path: *const libc::c_char,
  ) -> libc::c_int;
}

#[cfg(not(target_os = "macos"))]
use libc::posix_spawn_file_actions_addchdir_np;

/// The flags every spawn carries (module docs): the signal mask and `SIGPIPE`'s default set, a group where asked, and on
/// macOS close-on-exec by default for every descriptor the file actions do not name.
fn flags(own_group: bool) -> io::Result<libc::c_short> {
  let mut flags = libc::POSIX_SPAWN_SETSIGMASK | libc::POSIX_SPAWN_SETSIGDEF;
  if own_group {
    flags |= libc::POSIX_SPAWN_SETPGROUP;
  }
  #[cfg(target_os = "macos")]
  {
    flags |= libc::POSIX_SPAWN_CLOEXEC_DEFAULT;
  }
  // Every flag is a small bit of `<spawn.h>`'s, so the sum fits `short`, which the call takes.
  libc::c_short::try_from(flags).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

/// The spawn's file actions and attributes, destroyed when it ends.
struct Prepared {
  actions: libc::posix_spawn_file_actions_t,
  attributes: libc::posix_spawnattr_t,
}

impl Drop for Prepared {
  fn drop(&mut self) {
    // SAFETY: both were initialised by `prepare` (it builds this value only once both `init` calls succeeded) and are
    // destroyed once, here.
    unsafe {
      libc::posix_spawn_file_actions_destroy(&raw mut self.actions);
      libc::posix_spawnattr_destroy(&raw mut self.attributes);
    }
  }
}

/// The file actions and attributes of `spawn` (module docs), `directory` and the null device's path the C strings they
/// copy.
fn prepare(spawn: &Spawn<'_>, directory: &CString, null: &CString) -> io::Result<Prepared> {
  // SAFETY: zeroed storage for the two opaque records, each initialised by its own `init` call below before any other
  // use; on Apple both are pointers, on Linux plain structs, and either is valid zeroed until `init` writes it.
  let mut prepared: Prepared = unsafe { std::mem::zeroed() };
  // SAFETY: `init` writes the record it is given, this frame's own. Were it to fail, nothing would need destroying, so
  // the value is forgotten rather than dropped.
  let actions = unsafe { libc::posix_spawn_file_actions_init(&raw mut prepared.actions) };
  if actions != 0 {
    std::mem::forget(prepared);
    return Err(io::Error::from_raw_os_error(actions));
  }
  // SAFETY: as above, for the attributes; on failure the actions, made just above, are destroyed and the value
  // forgotten.
  let attributes = unsafe { libc::posix_spawnattr_init(&raw mut prepared.attributes) };
  if attributes != 0 {
    // SAFETY: the actions were initialised above and are destroyed once.
    unsafe { libc::posix_spawn_file_actions_destroy(&raw mut prepared.actions) };
    std::mem::forget(prepared);
    return Err(io::Error::from_raw_os_error(attributes));
  }
  let streams = [
    (libc::STDIN_FILENO, spawn.stdin, libc::O_RDONLY),
    (libc::STDOUT_FILENO, spawn.stdout, libc::O_WRONLY),
    (libc::STDERR_FILENO, spawn.stderr, libc::O_WRONLY),
  ];
  for (number, stream, mode) in streams {
    let answer = match stream {
      // SAFETY: the actions are initialised; `fd` is the caller's open descriptor, borrowed for this call and the spawn
      // after it (the caller's `Spawn` outlives both), duplicated to `number` in the child.
      ChildStream::Fd(fd) => unsafe {
        libc::posix_spawn_file_actions_adddup2(&raw mut prepared.actions, fd.as_raw_fd(), number)
      },
      // SAFETY: the actions are initialised; `null` is a NUL-terminated path the action copies.
      ChildStream::Null => unsafe {
        libc::posix_spawn_file_actions_addopen(
          &raw mut prepared.actions,
          number,
          null.as_ptr(),
          mode,
          0,
        )
      },
    };
    checked(answer)?;
  }
  // SAFETY: the actions are initialised; `directory` is a NUL-terminated path the action copies.
  checked(unsafe {
    posix_spawn_file_actions_addchdir_np(&raw mut prepared.actions, directory.as_ptr())
  })?;
  // SAFETY: the attributes are initialised; the flags are `<spawn.h>`'s.
  let flags = flags(spawn.own_group)?;
  // SAFETY: the attributes are initialised; the flags are `<spawn.h>`'s.
  checked(unsafe { libc::posix_spawnattr_setflags(&raw mut prepared.attributes, flags) })?;
  // SAFETY: two signal sets in this frame, zeroed and then made valid by `sigemptyset` before any other use (`sigaddset`
  // adds `SIGPIPE` to one); the attribute calls take the initialised attributes and copy the sets. The calls' answers
  // are each checked after.
  let (masked, defaulted) = unsafe {
    let (mut mask, mut defaults): (libc::sigset_t, libc::sigset_t) = std::mem::zeroed();
    libc::sigemptyset(&raw mut mask);
    libc::sigemptyset(&raw mut defaults);
    libc::sigaddset(&raw mut defaults, libc::SIGPIPE);
    (
      libc::posix_spawnattr_setsigmask(&raw mut prepared.attributes, &raw const mask),
      libc::posix_spawnattr_setsigdefault(&raw mut prepared.attributes, &raw const defaults),
    )
  };
  checked(masked)?;
  checked(defaulted)?;
  if spawn.own_group {
    // SAFETY: the attributes are initialised; group 0 makes the child the leader of a group of its own pid.
    checked(unsafe { libc::posix_spawnattr_setpgroup(&raw mut prepared.attributes, 0) })?;
  }
  Ok(prepared)
}

/// Starts `spawn`'s child (module docs).
///
/// # Errors
/// `InvalidInput` for a program that is no absolute path, or a path, an argument or a variable that holds a NUL; the
/// operating system's refusal of the spawn (`ENOENT` for a program that is not there, `EACCES` for one that may not be
/// run, `EAGAIN`, `ENOMEM`), as its error.
pub fn spawn(spawn: &Spawn<'_>) -> io::Result<SpawnedChild> {
  if !spawn.program.is_absolute() {
    return Err(io::Error::from(io::ErrorKind::InvalidInput));
  }
  let program = c_string(spawn.program.as_os_str().as_bytes())?;
  let directory = c_string(spawn.directory.as_os_str().as_bytes())?;
  let null = c_string(NULL_DEVICE)?;
  let mut words = vec![program.clone()];
  for arg in spawn.args {
    words.push(c_string(arg.as_bytes())?);
  }
  let environment = environment_of(spawn)?;
  let argv: Vec<*mut libc::c_char> = words
    .iter()
    .map(|word| word.as_ptr().cast_mut())
    .chain(std::iter::once(std::ptr::null_mut()))
    .collect();
  let envp: Vec<*mut libc::c_char> = environment
    .iter()
    .map(|entry| entry.as_ptr().cast_mut())
    .chain(std::iter::once(std::ptr::null_mut()))
    .collect();
  let prepared = prepare(spawn, &directory, &null)?;
  let mut pid: libc::pid_t = 0;
  // SAFETY: `program`, every word and entry are NUL-terminated strings this frame owns, `argv` and `envp` arrays of
  // pointers to them ending in a null pointer, alive across the call; the actions and attributes are initialised
  // (`prepare`); `posix_spawn` writes the child's pid to `pid`, this frame's own, and reads nothing it is given after
  // it returns.
  let answer = unsafe {
    libc::posix_spawn(
      &raw mut pid,
      program.as_ptr(),
      &raw const prepared.actions,
      &raw const prepared.attributes,
      argv.as_ptr(),
      envp.as_ptr(),
    )
  };
  drop(prepared);
  checked(answer)?;
  let pid = u32::try_from(pid).map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
  Ok(SpawnedChild { pid, status: None })
}

impl SpawnedChild {
  /// Its pid.
  pub fn id(&self) -> u32 {
    self.pid
  }

  /// Its status if it has exited (reaped now), none while it runs.
  ///
  /// # Errors
  /// The operating system's refusal of the wait.
  pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
    if self.status.is_some() {
      return Ok(self.status);
    }
    self.wait_with(rustix::process::WaitOptions::NOHANG)
  }

  /// Its status, waited for (reaped).
  ///
  /// # Errors
  /// The operating system's refusal of the wait.
  pub fn wait(&mut self) -> io::Result<ExitStatus> {
    if let Some(status) = self.status {
      return Ok(status);
    }
    loop {
      match self.wait_with(rustix::process::WaitOptions::empty()) {
        Ok(Some(status)) => return Ok(status),
        Ok(None) => {}
        Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
        Err(error) => return Err(error),
      }
    }
  }

  /// One `waitpid` with `options`: its status, kept once reaped.
  fn wait_with(&mut self, options: rustix::process::WaitOptions) -> io::Result<Option<ExitStatus>> {
    let pid = i32::try_from(self.pid)
      .ok()
      .and_then(rustix::process::Pid::from_raw)
      .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
    let waited = rustix::process::waitpid(Some(pid), options).map_err(io::Error::from)?;
    let status = waited
      .map(|(_, status)| ExitStatus::from_raw(i32::try_from(status.as_raw()).unwrap_or(i32::MAX)));
    if status.is_some() {
      self.status = status;
    }
    Ok(status)
  }

  /// Sends `SIGKILL` to the process group it leads (spawned with [`Spawn::own_group`]): it and what it started that
  /// stayed in its group; refused only where the group has gone, which is what the kill was for.
  pub fn kill_group(&self) {
    if let Some(pid) = i32::try_from(self.pid)
      .ok()
      .and_then(rustix::process::Pid::from_raw)
    {
      let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
  }
}

#[cfg(test)]
mod tests {
  use std::io::Read;
  use std::os::fd::AsFd;

  use super::{ChildStream, Environment, Spawn, spawn};

  /// main 01a20182's ruling of 2026-11-22 02:06 (the hook pipe race, (a)): a descriptor another thread made and had not
  /// marked close-on-exec yet leaked into a child spawned meanwhile. Do: hold a pipe whose write end's close-on-exec is
  /// cleared, as one is between `pipe` and `fcntl` on macOS; spawn `/bin/sh` through [`spawn`], its standard output a
  /// pipe, to list the descriptors it holds. Expect: none but its standard streams (and the one its listing opens),
  /// never the held pipe's; and it exits 0.
  /// Shape: the descriptor number the held pipe's end is moved to, far past what the test process and `ls` open.
  #[cfg(target_os = "macos")]
  const HELD_AT: i32 = 77;

  #[cfg(target_os = "macos")]
  #[test]
  fn a_child_holds_only_the_streams_its_spawn_names() {
    let (_held_read, pipe_write) = std::io::pipe().unwrap();
    // The held end at a number nothing else here takes, its close-on-exec clear.
    let held_write = rustix::io::fcntl_dupfd_cloexec(pipe_write.as_fd(), HELD_AT).unwrap();
    rustix::io::fcntl_setfd(held_write.as_fd(), rustix::io::FdFlags::empty()).unwrap();
    let (mut out_read, out_write) = std::io::pipe().unwrap();
    let held = std::os::fd::AsRawFd::as_raw_fd(&held_write);
    let script = std::ffi::OsString::from("ls /dev/fd");
    let mut child = spawn(&Spawn {
      program: std::path::Path::new("/bin/sh"),
      args: &[std::ffi::OsStr::new("-c"), &script],
      environment: Environment::Inherited,
      overrides: &[],
      directory: std::path::Path::new("/"),
      own_group: true,
      stdin: ChildStream::Null,
      stdout: ChildStream::Fd(out_write.as_fd()),
      stderr: ChildStream::Null,
    })
    .unwrap();
    drop(out_write);
    let mut listed = String::new();
    out_read.read_to_string(&mut listed).unwrap();
    let status = child.wait().unwrap();
    let numbers: Vec<i32> = listed
      .split_whitespace()
      .filter_map(|word| word.parse().ok())
      .collect();
    assert!(status.success(), "{status:?}");
    assert!(
      !numbers.contains(&held),
      "the child held {numbers:?}; the held pipe's write end is {held}"
    );
  }
}
