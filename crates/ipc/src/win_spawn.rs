//! Windows: starting a child that inherits exactly the handles it is meant to, and nothing else of this process (the
//! consumer delivery's read end, `crate::delivery`, §4.13; a Shards stream's end, `crate::stream::spawn_with`, A-179).
//!
//! `CreateProcessW` with inheritance on hands a child every inheritable handle of the process, so a handle that is
//! inheritable for one spawn reaches every child another thread spawns meanwhile. The
//! `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` attribute narrows a spawn's inheritance to the handles it names (Raymond Chen,
//! "Programmatically controlling which handles are inherited by new processes in Win32" \[D\]). The standard library's
//! `Command` cannot set it — it passes `bInheritHandles` and the attribute is unstable on the pinned toolchain
//! (`windows_process_extensions_raw_attribute`, rust-lang/rust#114854) — so the spawn is made here, the way
//! `crates/rt`'s AFD reactor and `bridge-winfsp` make their Win32 calls: hand-transcribed from the headers windows-sys
//! binds, each call with its invariant stated.
//!
//! A handed handle is made inheritable only as a duplicate that exists for the length of one spawn, and the child's
//! standard handles are inheritable duplicates of this process's, so this process's own handles are never made
//! inheritable at all. The window Win32 leaves every handle list remains: a `CreateProcess` from another thread of this
//! process that inherits handles without a list of its own, between the duplicate and the create, would carry the
//! duplicate along (Chen \[D\]). The callers are single-threaded at the spawn (the `slates` command, a harness).

use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::OsStrExt;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
  CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
  WAIT_OBJECT_0,
};
use windows_sys::Win32::System::Console::{
  GetStdHandle, STD_ERROR_HANDLE, STD_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
  SetConsoleCtrlHandler,
};
use windows_sys::Win32::System::JobObjects::{
  CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
  JobObjectExtendedLimitInformation, SetInformationJobObject,
};
use windows_sys::Win32::System::Threading::{
  CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
  EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess, GetExitCodeProcess, INFINITE,
  InitializeProcThreadAttributeList, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
  PROC_THREAD_ATTRIBUTE_JOB_LIST, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW,
  TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
};

use crate::error::IpcError;

/// The calling thread's last Win32 error, as the refusal's code.
pub(crate) fn last_error() -> Option<i32> {
  // SAFETY: a thread-local read with no preconditions.
  let code = unsafe { GetLastError() };
  Some(i32::try_from(code).unwrap_or(i32::MAX))
}

/// `call` refused, with the calling thread's last Win32 error.
pub(crate) fn refused(call: &'static str) -> IpcError {
  IpcError::OsRefused {
    call,
    code: last_error(),
  }
}

/// A handle this crate created or duplicated, closed when it goes out of scope.
pub(crate) struct ClosedOnDrop(pub(crate) HANDLE);

impl Drop for ClosedOnDrop {
  fn drop(&mut self) {
    if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
      // SAFETY: a live handle this crate owns and nothing else closes.
      unsafe { CloseHandle(self.0) };
    }
  }
}

/// Whether a duplicate is inheritable (the `bInheritHandle` of `DuplicateHandle`).
#[derive(Clone, Copy)]
pub(crate) enum Inheritance {
  /// Only this process holds it.
  Kept,
  /// A child created with it in its handle list inherits it.
  Inheritable,
}

/// A duplicate of `handle` in this process — the same object, a new handle — which the caller closes.
pub(crate) fn duplicate(
  handle: HANDLE,
  inheritance: Inheritance,
) -> Result<ClosedOnDrop, IpcError> {
  let mut duplicate: HANDLE = null_mut();
  let inherit = match inheritance {
    Inheritance::Kept => 0,
    Inheritance::Inheritable => 1,
  };
  // SAFETY: the current process's pseudo-handle needs no closing; `handle` is live; the out-pointer is a local the call
  // fills.
  let ok = unsafe {
    DuplicateHandle(
      GetCurrentProcess(),
      handle,
      GetCurrentProcess(),
      &mut duplicate,
      0,
      inherit,
      DUPLICATE_SAME_ACCESS,
    )
  };
  if ok == 0 {
    return Err(refused("DuplicateHandle"));
  }
  Ok(ClosedOnDrop(duplicate))
}

/// An inheritable duplicate of `handle` in this process, which the caller closes after the spawn.
pub(crate) fn duplicate_inheritable(handle: HANDLE) -> Result<ClosedOnDrop, IpcError> {
  duplicate(handle, Inheritance::Inheritable)
}

/// An inheritable duplicate of one of this process's standard handles, or none when the process has no such handle (no
/// console, a closed stream).
fn standard_handle_inheritable(which: STD_HANDLE) -> Result<Option<ClosedOnDrop>, IpcError> {
  // SAFETY: a query with no preconditions; the answer may be null or invalid, checked below.
  let handle = unsafe { GetStdHandle(which) };
  if handle.is_null() || handle == INVALID_HANDLE_VALUE {
    return Ok(None);
  }
  duplicate_inheritable(handle).map(Some)
}

/// The attributes a spawn is created with: the `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` naming exactly the handles the child
/// inherits (Chen \[D\]), and, when a job is given, the `PROC_THREAD_ATTRIBUTE_JOB_LIST` that makes the child a member
/// of it from its creation, so no instant passes in which it runs outside the job (Microsoft, "UpdateProcThreadAttribute"
/// \[B\]). The list's storage is pointer-aligned and the two arrays outlive the list, as each attribute keeps a pointer
/// to its array until the list is deleted (on drop).
struct AttributeList {
  storage: Vec<usize>,
  handles: Vec<HANDLE>,
  jobs: Vec<HANDLE>,
}

impl AttributeList {
  fn new(handles: Vec<HANDLE>, job: Option<HANDLE>) -> Result<AttributeList, IpcError> {
    let jobs: Vec<HANDLE> = job.into_iter().collect();
    let count = if jobs.is_empty() { 1 } else { 2 };
    let mut size: usize = 0;
    // SAFETY: the documented size query — a null list fills `size` and fails with ERROR_INSUFFICIENT_BUFFER, which is
    // the expected answer here.
    unsafe { InitializeProcThreadAttributeList(null_mut(), count, 0, &mut size) };
    if size == 0 {
      return Err(refused("InitializeProcThreadAttributeList"));
    }
    let mut list = AttributeList {
      storage: vec![0usize; size.div_ceil(size_of::<usize>())],
      handles,
      jobs,
    };
    // SAFETY: `storage` holds `size` bytes, pointer-aligned, and lives as long as the list.
    let ok = unsafe {
      InitializeProcThreadAttributeList(list.storage.as_mut_ptr().cast(), count, 0, &mut size)
    };
    if ok == 0 {
      list.storage.clear();
      return Err(refused("InitializeProcThreadAttributeList"));
    }
    let handles = (list.handles.as_ptr(), list.handles.len());
    list.update(PROC_THREAD_ATTRIBUTE_HANDLE_LIST, handles)?;
    if !list.jobs.is_empty() {
      let jobs = (list.jobs.as_ptr(), list.jobs.len());
      list.update(PROC_THREAD_ATTRIBUTE_JOB_LIST, jobs)?;
    }
    Ok(list)
  }

  /// Sets `attribute` to the array of `count` handles at `array`, which is one of the list's own arrays.
  fn update(
    &mut self,
    attribute: u32,
    (array, count): (*const HANDLE, usize),
  ) -> Result<(), IpcError> {
    // SAFETY: the list is initialized; `array` is one of the arrays owned beside it, live for the list's whole life
    // (dropped after the delete) and never resized; the attribute id and byte length are the documented ones.
    let ok = unsafe {
      UpdateProcThreadAttribute(
        self.storage.as_mut_ptr().cast(),
        0,
        usize::try_from(attribute).unwrap_or(0),
        array.cast(),
        count.saturating_mul(size_of::<HANDLE>()),
        null_mut(),
        null(),
      )
    };
    if ok == 0 {
      return Err(refused("UpdateProcThreadAttribute"));
    }
    Ok(())
  }

  fn pointer(&mut self) -> *mut core::ffi::c_void {
    self.storage.as_mut_ptr().cast()
  }
}

impl Drop for AttributeList {
  fn drop(&mut self) {
    if !self.storage.is_empty() {
      // SAFETY: an initialized list this struct owns; deleted once, here.
      unsafe { DeleteProcThreadAttributeList(self.storage.as_mut_ptr().cast()) };
    }
  }
}

/// A job object that ends every process in it when its last handle closes (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`), held
/// by this process alone: when this process ends, by any means, the kernel closes the handle and the job's processes
/// end with it. The anchor holds its daemon the same way (`crates/anchor/src/supervise.rs`). A process in this job may
/// make jobs of its own beneath it (nested jobs, Windows 8 and later).
pub(crate) fn kill_on_close_job() -> Result<ClosedOnDrop, IpcError> {
  // SAFETY: no security attributes (not inheritable) and no name; the answer is checked below.
  let job = unsafe { CreateJobObjectW(null(), null()) };
  if job.is_null() {
    return Err(refused("CreateJobObjectW"));
  }
  let job = ClosedOnDrop(job);
  // SAFETY: a plain-data record every zero of which is a valid field value.
  let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
  limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
  let size = u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()).unwrap_or(0);
  // SAFETY: the job just made; the record is a live local of the size passed, of the class named.
  let ok = unsafe {
    SetInformationJobObject(
      job.0,
      JobObjectExtendedLimitInformation,
      (&raw const limits).cast(),
      size,
    )
  };
  if ok == 0 {
    return Err(refused("SetInformationJobObject"));
  }
  Ok(job)
}

/// Appends `argument` to a command line under the C runtime's parsing rules (Microsoft, "Parsing C++ command-line
/// arguments" \[B\]): an argument with a space, a tab, a quote — or nothing at all — is quoted; a quote inside is escaped
/// with a backslash; backslashes are doubled only where they precede a quote or the closing quote.
fn append_quoted(line: &mut Vec<u16>, argument: &OsStr) {
  let quote = u16::from(b'"');
  let backslash = u16::from(b'\\');
  let units: Vec<u16> = argument.encode_wide().collect();
  let needs_quotes = units.is_empty()
    || units
      .iter()
      .any(|unit| *unit == u16::from(b' ') || *unit == u16::from(b'\t') || *unit == quote);
  if needs_quotes {
    line.push(quote);
  }
  let mut backslashes = 0usize;
  for unit in units {
    if unit == backslash {
      backslashes = backslashes.saturating_add(1);
      line.push(unit);
      continue;
    }
    if unit == quote {
      line.extend(std::iter::repeat_n(
        backslash,
        backslashes.saturating_add(1),
      ));
    }
    backslashes = 0;
    line.push(unit);
  }
  if needs_quotes {
    line.extend(std::iter::repeat_n(backslash, backslashes));
    line.push(quote);
  }
}

/// The command line `CreateProcessW` parses back into the child's `argv`, null-terminated; the program is its first
/// token, which the call resolves as a shell would (the application's directory, the working directory, the system
/// directories, then `PATH`).
fn command_line_of(program: &OsStr, args: &[OsString]) -> Vec<u16> {
  let mut line: Vec<u16> = Vec::new();
  append_quoted(&mut line, program);
  for argument in args {
    line.push(u16::from(b' '));
    append_quoted(&mut line, argument);
  }
  line.push(0);
  line
}

/// What a child's standard output is: this process's, or the write end of a pipe the caller reads.
pub(crate) enum StandardOutput<'a> {
  /// This process's standard output, when it has one.
  Inherited,
  /// The write end of a pipe this process reads back; the child inherits a duplicate of it.
  Into(&'a ClosedOnDrop),
}

/// What a spawn hands its child.
pub(crate) struct Spawn<'a> {
  /// The program, resolved as a shell would.
  pub(crate) program: &'a OsStr,
  /// Its arguments.
  pub(crate) args: &'a [OsString],
  /// The child's environment block (`NAME=VALUE\0` per entry, sorted, and a final `\0`), or `None` for this process's.
  pub(crate) environment: Option<&'a [u16]>,
  /// The handles the child inherits beyond its standard three, each already inheritable, at the same values.
  pub(crate) inherited: &'a [HANDLE],
  /// Where its standard output goes.
  pub(crate) output: StandardOutput<'a>,
  /// The job the child is a member of from its creation, if any.
  pub(crate) job: Option<HANDLE>,
}

/// Starts the child `spawn` describes: it inherits `spawn.inherited` and inheritable duplicates of this process's
/// standard handles, and nothing else of this process. The duplicates of the standard handles are closed before this
/// returns, whether or not the child started; the caller's inherited handles are the caller's to close.
pub(crate) fn spawn(spawn: &Spawn<'_>) -> Result<Process, IpcError> {
  let stdin = standard_handle_inheritable(STD_INPUT_HANDLE)?;
  let stdout = match spawn.output {
    StandardOutput::Into(write_end) => Some(duplicate_inheritable(write_end.0)?),
    StandardOutput::Inherited => standard_handle_inheritable(STD_OUTPUT_HANDLE)?,
  };
  let stderr = standard_handle_inheritable(STD_ERROR_HANDLE)?;
  // SAFETY: a plain-data record every zero of which is a valid field value.
  let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
  startup.StartupInfo.cb = u32::try_from(size_of::<STARTUPINFOEXW>()).unwrap_or(0);
  startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
  let mut handles: Vec<HANDLE> = spawn.inherited.to_vec();
  for (slot, duplicate) in [
    (&mut startup.StartupInfo.hStdInput, &stdin),
    (&mut startup.StartupInfo.hStdOutput, &stdout),
    (&mut startup.StartupInfo.hStdError, &stderr),
  ] {
    if let Some(duplicate) = duplicate {
      *slot = duplicate.0;
      handles.push(duplicate.0);
    }
  }
  let mut list = AttributeList::new(handles, spawn.job)?;
  startup.lpAttributeList = list.pointer();
  let mut command_line = command_line_of(spawn.program, spawn.args);
  let (environment, unicode) = match spawn.environment {
    Some(block) => (block.as_ptr().cast(), CREATE_UNICODE_ENVIRONMENT),
    None => (null(), 0),
  };
  // SAFETY: a plain-data record the call fills.
  let mut information: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
  // SAFETY: the command line is a mutable null-terminated buffer (the call may edit it); the environment is null (this
  // process's) or a well-formed double-null-terminated wide block, and the flag says which; the startup record is the
  // extended form with its attribute list live, and the flag says so; inheritance is on, narrowed to the list by the
  // attribute.
  let ok = unsafe {
    CreateProcessW(
      null(),
      command_line.as_mut_ptr(),
      null(),
      null(),
      1,
      EXTENDED_STARTUPINFO_PRESENT | unicode,
      environment,
      null(),
      &startup.StartupInfo,
      &mut information,
    )
  };
  let error = if ok == 0 {
    Some(refused("CreateProcessW"))
  } else {
    None
  };
  drop(list);
  drop(stdin);
  drop(stdout);
  drop(stderr);
  if let Some(error) = error {
    return Err(error);
  }
  drop(ClosedOnDrop(information.hThread));
  Ok(Process {
    handle: ClosedOnDrop(information.hProcess),
    id: information.dwProcessId,
  })
}

/// A child [`spawn`] started, held by its process handle.
pub(crate) struct Process {
  handle: ClosedOnDrop,
  id: u32,
}

impl Process {
  /// The child's process id.
  pub(crate) fn id(&self) -> u32 {
    self.id
  }

  /// Waits for the child to end: its exit code.
  pub(crate) fn wait(&mut self) -> Result<Option<i32>, IpcError> {
    // SAFETY: a live process handle this struct owns; the wait returns when the process ends.
    let waited = unsafe { WaitForSingleObject(self.handle.0, INFINITE) };
    if waited != WAIT_OBJECT_0 {
      return Err(refused("WaitForSingleObject"));
    }
    let mut code = 0u32;
    // SAFETY: the out-pointer is a live local; the handle is the process's.
    let ok = unsafe { GetExitCodeProcess(self.handle.0, &mut code) };
    if ok == 0 {
      return Err(refused("GetExitCodeProcess"));
    }
    Ok(Some(i32::from_ne_bytes(code.to_ne_bytes())))
  }

  /// Ends the child now.
  pub(crate) fn kill(&mut self) -> Result<(), IpcError> {
    // SAFETY: the process handle this struct owns.
    let ok = unsafe { TerminateProcess(self.handle.0, 1) };
    if ok == 0 {
      return Err(refused("TerminateProcess"));
    }
    Ok(())
  }
}

/// Answers every console control event (Ctrl-C, Ctrl-Break, the console's close) as handled, so this process is not
/// ended by it: the event reaches every process attached to the console, the child too, and the child decides.
unsafe extern "system" fn the_event_is_the_childs(_event: u32) -> windows_sys::core::BOOL {
  1
}

/// Keeps this process alive through console control events while a child it waits for runs: Ctrl-C reaches every
/// process on the console, and a waiting parent that died of it first would close its kill-on-close job and end the
/// child before the child could shut down as it chooses. Installed after the child exists, as a handler, never as
/// `SetConsoleCtrlHandler(NULL, TRUE)`: that form's ignore flag is inherited by children created afterwards (Microsoft,
/// "SetConsoleCtrlHandler" \[B\]), so the child would ignore Ctrl-C too. cargo's `exec_replace` on Windows does the same.
pub(crate) fn outlive_console_events() -> Result<(), IpcError> {
  // SAFETY: registers a handler that touches no state and returns at once; it stays valid for the process's life, as
  // it is a function, and adding it twice only calls it twice.
  let ok = unsafe { SetConsoleCtrlHandler(Some(the_event_is_the_childs), 1) };
  if ok == 0 {
    return Err(refused("SetConsoleCtrlHandler"));
  }
  Ok(())
}
