//! Whether Windows Error Reporting is set to write a full-memory dump of this daemon when it crashes (A-239; GAPS
//! `8a-win-dumps`; §4.2 "RAM-only guarantees", R1), read once, logged once, and reported as `dumps_local_full`.
//!
//! **Why.** On Unix the anchor and the daemon keep themselves out of core dumps (`slates-cli`'s `dumps`). Windows has no
//! unprivileged equivalent: an administrator's `LocalDumps` setting has WER write a dump of a crashed process, and at
//! `DumpType` 2 that dump is the process's whole memory, tenant content and keys included (measured: every private
//! mapping's pattern in it, runs 42355310061 and 42362092866). WER's own region exclusion does not reach that dump and
//! could not be measured for its own report (`WerDisabled` on CI's runner), so the daemon registers nothing (the
//! supervisor's decision, R4). What it can do is say so: it reads the setting, unprivileged, and reports it. It never
//! refuses to start over it, since turning the setting off is an administrator's step (R10).
//!
//! **The setting** (Microsoft Learn, "Collecting User-Mode Dumps"): values under
//! `HKLM\SOFTWARE\Microsoft\Windows\Windows Error Reporting\LocalDumps`, overridden by the same values under its subkey
//! named for the program's image (`slates.exe`). `DumpType` 0 is a custom dump whose `CustomDumpFlags` choose what it
//! holds, 1 a minidump (also the default), 2 a full dump.

/// Format: `DumpType`'s custom dump, whose `CustomDumpFlags` say what it carries.
const CUSTOM_DUMP: u32 = 0;
/// Format: `DumpType`'s full dump: the process's whole memory.
const FULL_DUMP: u32 = 2;
/// Format: the `MINIDUMP_TYPE` flags under which a custom dump carries the process's private memory
/// (`MiniDumpWithFullMemory` 0x2, `MiniDumpWithPrivateReadWriteMemory` 0x200; `minidumpapiset.h`).
const PRIVATE_MEMORY_FLAGS: u32 = 0x2 | 0x200;

/// One key's `LocalDumps` values: each absent when the key or the value is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DumpSettings {
  /// `DumpType`.
  pub dump_type: Option<u32>,
  /// `CustomDumpFlags`.
  pub custom_flags: Option<u32>,
}

/// Whether the settings in force, the program's own (`app`) over the global ones, make a dump that carries the process's
/// private memory: a full dump, or a custom one whose flags take that memory. A minidump, the default, does not.
pub fn carries_private_memory(global: DumpSettings, app: DumpSettings) -> bool {
  let dump_type = app.dump_type.or(global.dump_type);
  let custom_flags = app.custom_flags.or(global.custom_flags).unwrap_or(0);
  match dump_type {
    Some(FULL_DUMP) => true,
    Some(CUSTOM_DUMP) => custom_flags & PRIVATE_MEMORY_FLAGS != 0,
    _ => false,
  }
}

/// Whether WER is set to write this process a dump of its private memory when it crashes: read once a process, and
/// logged the first time it is read true. Always `false` off Windows.
pub fn full() -> bool {
  static FULL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
  *FULL.get_or_init(|| {
    let full = read_full();
    if full {
      slates_machine::log_line!(
        "slates-server: Windows Error Reporting is set to write a full dump of this process if it crashes \
         (LocalDumps): such a dump holds every byte the daemon serves, keys included. An administrator turns it off; \
         the daemon serves either way and reports dumps_local_full in status"
      );
    }
    full
  })
}

#[cfg(windows)]
fn read_full() -> bool {
  /// Format: the global `LocalDumps` key, under `HKEY_LOCAL_MACHINE`.
  const KEY: &str = r"SOFTWARE\Microsoft\Windows\Windows Error Reporting\LocalDumps";
  let image = std::env::current_exe().ok().and_then(|exe| {
    exe
      .file_name()
      .map(|name| name.to_string_lossy().into_owned())
  });
  let global = settings_of(KEY);
  let app = image.map_or_else(DumpSettings::default, |image| {
    settings_of(&format!(r"{KEY}\{image}"))
  });
  carries_private_memory(global, app)
}

#[cfg(not(windows))]
fn read_full() -> bool {
  false
}

/// The `LocalDumps` values under `HKEY_LOCAL_MACHINE\key`.
#[cfg(windows)]
fn settings_of(key: &str) -> DumpSettings {
  DumpSettings {
    dump_type: dword(key, "DumpType"),
    custom_flags: dword(key, "CustomDumpFlags"),
  }
}

/// The DWORD `value` under `HKEY_LOCAL_MACHINE\key`, read with the caller's rights (no privilege); `None` when the key or
/// the value is absent or not a DWORD.
#[cfg(windows)]
fn dword(key: &str, value: &str) -> Option<u32> {
  use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RegGetValueW};
  let wide = |text: &str| -> Vec<u16> { text.encode_utf16().chain(std::iter::once(0)).collect() };
  let (key, value) = (wide(key), wide(value));
  let mut data = 0u32;
  let mut bytes = u32::try_from(size_of::<u32>()).ok()?;
  // SAFETY: `key` and `value` are NUL-terminated; `data` is a live local of `bytes` bytes, which the call reads as its
  // capacity and writes no more than.
  let status = unsafe {
    RegGetValueW(
      HKEY_LOCAL_MACHINE,
      key.as_ptr(),
      value.as_ptr(),
      RRF_RT_REG_DWORD,
      std::ptr::null_mut(),
      (&raw mut data).cast(),
      &mut bytes,
    )
  };
  (status == 0).then_some(data)
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A-239. Do: judge the `LocalDumps` settings a machine can hold. Expect: a full dump, globally or for the program
  /// alone, carries private memory; a minidump or no setting does not; a custom dump does exactly when its flags take
  /// private memory; and the program's own setting overrides the global one either way.
  #[test]
  fn local_dumps_settings_are_judged_as_wer_applies_them() {
    let none = DumpSettings::default();
    let of = |dump_type, custom_flags| DumpSettings {
      dump_type: Some(dump_type),
      custom_flags,
    };
    assert!(
      !carries_private_memory(none, none),
      "no setting: no local dump"
    );
    assert!(
      carries_private_memory(of(2, None), none),
      "a global full dump"
    );
    assert!(
      carries_private_memory(none, of(2, None)),
      "the program's own full dump"
    );
    assert!(!carries_private_memory(of(1, None), none), "a minidump");
    assert!(
      carries_private_memory(of(0, Some(0x2)), none),
      "a custom dump with full memory"
    );
    assert!(
      !carries_private_memory(of(0, Some(0x4)), none),
      "a custom dump with handles only"
    );
    assert!(
      !carries_private_memory(of(2, None), of(1, None)),
      "the program's minidump over a global full dump"
    );
    assert!(
      carries_private_memory(of(1, None), of(2, None)),
      "the program's full dump over a global minidump"
    );
  }
}
