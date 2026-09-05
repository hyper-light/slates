//! The fixed facts of the machine, queried from the OS rather than measured: page sizes, cache
//! line, cores with their classes and NUMA nodes and L2 sizes, memory totals, address space, and
//! the power state. Each fact names its source per platform; a fact the OS will not give is
//! recorded as unknown with a note, never guessed silently (D-11).
//!
//! Sources:
//! - macOS: `sysctlbyname` (`hw.pagesize`, `hw.cachelinesize`, `hw.nperflevels`,
//!   `hw.perflevelN.*`, `hw.memsize`, `machdep.cpu.brand_string`, `kern.osversion`),
//!   `host_statistics64` for free memory, IOKit's power-source snapshot for the power state
//!   [B: Apple sysctl(3), IOPSCopyPowerSourcesInfo].
//! - Linux: `sysconf`, `sysinfo(2)`, `uname(2)`, and the kernel's pseudo-files under `/sys` and
//!   `/proc` (read as queries; never written) [B: man sysconf(3), man 5 proc, Documentation/ABI/].
//! - Windows: `GetSystemInfo`, `GetLargePageMinimum`, `GlobalMemoryStatusEx`,
//!   `GetLogicalProcessorInformation`, `GetSystemPowerStatus` [B: Microsoft Learn].

use serde::{Deserialize, Serialize};

/// Which class a core belongs to, when the OS says.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CoreClass {
  /// The tier above performance where the OS has one (Apple's "Super" level).
  Super,
  /// A performance core.
  Performance,
  /// An efficiency core.
  Efficiency,
  /// The OS did not say; the profile treats it as performance and notes the gap.
  Unknown,
}

/// One logical core.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoreFacts {
  /// The OS's id for the core (the id affinity calls take).
  pub id: u32,
  /// Its class.
  pub class: CoreClass,
  /// The OS's performance level, 0 the fastest (Apple's perflevel index; capacity rank on Linux).
  pub level: u32,
  /// Its NUMA node (0 on machines with one).
  pub numa: u32,
  /// The L2 cache it sees, in bytes (0 when unknown).
  pub l2_bytes: u64,
}

/// Page facts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageFacts {
  /// The base page size in bytes.
  pub base: u64,
  /// Huge page sizes the OS offers explicitly, ascending (empty when none).
  pub huge: Vec<u64>,
  /// Whether transparent huge pages can be requested (Linux `madvise`; false elsewhere).
  pub transparent_huge: bool,
  /// The mapping granularity (equals the base page except on Windows, where it is coarser).
  pub allocation_granularity: u64,
}

/// Memory facts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryFacts {
  /// Physical memory in bytes.
  pub total: u64,
  /// Memory available to a new allocation without reclaim, in bytes, as the OS estimates it.
  pub available: u64,
  /// The width of a user address, in bits.
  pub address_bits: u32,
}

/// The power state that gates re-measurement.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PowerState {
  /// Mains or equivalent.
  Mains,
  /// Running on a battery.
  Battery,
  /// The OS did not say (a desktop without a power supply entry, or a refused query).
  Unknown,
}

/// The hardware and OS identity the cache is keyed by.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
  /// The CPU model string as the OS reports it.
  pub cpu: String,
  /// The OS name and build.
  pub os: String,
  /// The target architecture.
  pub arch: String,
  /// Number of logical cores.
  pub cores: u32,
  /// Physical memory in bytes.
  pub memory: u64,
  /// The base page size.
  pub page: u64,
}

impl Identity {
  /// A stable single-line rendering, hashed for the cache key and shown in `ProfileStale`.
  pub fn line(&self) -> String {
    format!(
      "{} | {} | {} | {} cores | {} bytes | {} page",
      self.cpu, self.os, self.arch, self.cores, self.memory, self.page
    )
  }

  /// The BLAKE3 hash of the identity line.
  pub fn hash(&self) -> [u8; 32] {
    *blake3::hash(self.line().as_bytes()).as_bytes()
  }
}

/// Every fixed fact together.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Facts {
  /// The identity.
  pub identity: Identity,
  /// Pages.
  pub page: PageFacts,
  /// The cache line in bytes.
  pub cache_line: u64,
  /// The cores.
  pub cores: Vec<CoreFacts>,
  /// Memory.
  pub memory: MemoryFacts,
  /// The power state at the time of the query.
  pub power: PowerState,
  /// Facts the OS would not give, each with what was recorded instead.
  pub notes: Vec<String>,
}

/// The macOS `sysctlbyname` reader, shared with the probes.
#[cfg(target_os = "macos")]
pub(crate) use platform::sysctl_u64;

impl Facts {
  /// Queries every fact from the OS.
  pub fn query() -> Facts {
    let mut notes = Vec::new();
    let page = platform::page(&mut notes);
    let cache_line = platform::cache_line(&mut notes);
    let cores = platform::cores(&mut notes);
    let memory = platform::memory(&mut notes);
    let power = platform::power(&mut notes);
    let (cpu, os) = platform::identity(&mut notes);
    let identity = Identity {
      cpu,
      os,
      arch: std::env::consts::ARCH.to_owned(),
      cores: u32::try_from(cores.len()).unwrap_or(u32::MAX),
      memory: memory.total,
      page: page.base,
    };
    Facts {
      identity,
      page,
      cache_line,
      cores,
      memory,
      power,
      notes,
    }
  }

  /// The largest L2 any core reports (0 when unknown).
  pub fn largest_l2(&self) -> u64 {
    self.cores.iter().map(|c| c.l2_bytes).max().unwrap_or(0)
  }
}

/// What the OS reports as locked (wired) for this process, for cross-checking a locking
/// sequence against the OS (AC-0.5): Linux `VmLck` from `/proc/self/status`, macOS `ri_wired_size`
/// from `proc_pid_rusage`; `None` where the OS has no such report.
pub fn locked_bytes() -> Option<u64> {
  platform::locked_bytes()
}

/// Shape: the largest cache line on any target we build for (Apple silicon's 128 bytes), used
/// only when the OS refuses to say; crossbeam's `CachePadded` uses the same fallback reasoning.
pub const CACHE_LINE_FALLBACK: u64 = 128;

/// The number of logical cores the OS lets this process use.
fn parallelism() -> u32 {
  std::thread::available_parallelism()
    .map(|n| u32::try_from(n.get()).unwrap_or(u32::MAX))
    .unwrap_or(1)
}

/// Parses a decimal integer from text that may carry a unit suffix (`K`, `M`, `kB`).
// Only the Linux sysfs queries and the tests call it; the other platforms keep it compiled.
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
fn parse_size(text: &str) -> Option<u64> {
  let text = text.trim();
  let digits: String = text.chars().take_while(char::is_ascii_digit).collect();
  let value: u64 = digits.parse().ok()?;
  let suffix = text[digits.len()..].trim().to_ascii_lowercase();
  /// Format: binary prefixes as the kernel's pseudo-files print them.
  const KIB: u64 = 1024;
  let multiplier = match suffix.as_str() {
    "" | "b" => 1,
    "k" | "kb" | "kib" => KIB,
    "m" | "mb" | "mib" => KIB * KIB,
    "g" | "gb" | "gib" => KIB * KIB * KIB,
    _ => return None,
  };
  value.checked_mul(multiplier)
}

#[cfg(target_os = "macos")]
mod platform {
  use super::{
    CACHE_LINE_FALLBACK, CoreClass, CoreFacts, MemoryFacts, PageFacts, PowerState, parallelism,
  };
  use std::ffi::{CStr, c_void};

  pub(crate) fn sysctl_u64(name: &CStr) -> Option<u64> {
    let mut value: u64 = 0;
    let mut len = std::mem::size_of::<u64>();
    // SAFETY: `name` is NUL-terminated; `value` is a writable u64 and `len` its size; sysctl
    // writes at most `len` bytes and updates `len` to the number written.
    let rc = unsafe {
      libc::sysctlbyname(
        name.as_ptr(),
        (&raw mut value).cast::<c_void>(),
        &raw mut len,
        std::ptr::null_mut(),
        0,
      )
    };
    if rc != 0 {
      return None;
    }
    // A four-byte answer sits in the low bytes on this little-endian target.
    Some(if len == std::mem::size_of::<u32>() {
      value & u64::from(u32::MAX)
    } else {
      value
    })
  }

  fn sysctl_string(name: &CStr) -> Option<String> {
    let mut len: usize = 0;
    // SAFETY: a null buffer with a zero length asks sysctl for the size only.
    let rc = unsafe {
      libc::sysctlbyname(
        name.as_ptr(),
        std::ptr::null_mut(),
        &raw mut len,
        std::ptr::null_mut(),
        0,
      )
    };
    if rc != 0 || len == 0 {
      return None;
    }
    let mut buf = vec![0u8; len];
    // SAFETY: `buf` has exactly `len` writable bytes.
    let rc = unsafe {
      libc::sysctlbyname(
        name.as_ptr(),
        buf.as_mut_ptr().cast::<c_void>(),
        &raw mut len,
        std::ptr::null_mut(),
        0,
      )
    };
    if rc != 0 {
      return None;
    }
    buf.truncate(len);
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    Some(String::from_utf8_lossy(&buf[..end]).into_owned())
  }

  pub(super) fn page(notes: &mut Vec<String>) -> PageFacts {
    let base = sysctl_u64(c"hw.pagesize").unwrap_or_else(|| {
      notes.push("hw.pagesize refused; recorded the C library's page size".to_owned());
      // SAFETY: sysconf has no preconditions.
      u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).unwrap_or(0)
    });
    // macOS offers no superpages to user programs (Appendix C), so `huge` stays empty.
    PageFacts {
      base,
      huge: Vec::new(),
      transparent_huge: false,
      allocation_granularity: base,
    }
  }

  pub(super) fn cache_line(notes: &mut Vec<String>) -> u64 {
    sysctl_u64(c"hw.cachelinesize")
      .filter(|v| *v > 0)
      .unwrap_or_else(|| {
        notes.push(format!(
          "hw.cachelinesize refused; recorded the {CACHE_LINE_FALLBACK}-byte fallback"
        ));
        CACHE_LINE_FALLBACK
      })
  }

  pub(super) fn cores(notes: &mut Vec<String>) -> Vec<CoreFacts> {
    let levels = sysctl_u64(c"hw.nperflevels").unwrap_or(0);
    let mut cores = Vec::new();
    let mut next_id: u32 = 0;
    for level in 0..levels {
      let count = sysctl_u64(&level_key(level, "logicalcpu")).unwrap_or(0);
      let name = sysctl_string(&level_key(level, "name")).unwrap_or_default();
      let l2 = sysctl_u64(&level_key(level, "l2cachesize")).unwrap_or(0);
      let class = match name.as_str() {
        "Super" => CoreClass::Super,
        "Performance" => CoreClass::Performance,
        "Efficiency" => CoreClass::Efficiency,
        _ => CoreClass::Unknown,
      };
      if class == CoreClass::Unknown {
        notes.push(format!(
          "hw.perflevel{level}.name is {name:?}; recorded unknown class"
        ));
      }
      let level = u32::try_from(level).unwrap_or(u32::MAX);
      for _ in 0..count {
        cores.push(CoreFacts {
          id: next_id,
          class,
          level,
          numa: 0,
          l2_bytes: l2,
        });
        next_id = next_id.saturating_add(1);
      }
    }
    if cores.is_empty() {
      notes.push("hw.nperflevels refused; recorded every core as unknown class".to_owned());
      cores = (0..parallelism())
        .map(|id| CoreFacts {
          id,
          class: CoreClass::Unknown,
          level: 0,
          numa: 0,
          l2_bytes: 0,
        })
        .collect();
    }
    cores
  }

  fn level_key(level: u64, leaf: &str) -> std::ffi::CString {
    std::ffi::CString::new(format!("hw.perflevel{level}.{leaf}")).unwrap_or_default()
  }

  pub(super) fn memory(notes: &mut Vec<String>) -> MemoryFacts {
    let total = sysctl_u64(c"hw.memsize").unwrap_or_else(|| {
      notes.push("hw.memsize refused; recorded 0".to_owned());
      0
    });
    let available = available_bytes().unwrap_or_else(|| {
      notes.push("host_statistics64 refused; recorded total as available".to_owned());
      total
    });
    MemoryFacts {
      total,
      available,
      address_bits: usize::BITS,
    }
  }

  // libc marks mach_host_self deprecated in favour of the mach2 crate; one declaration here
  // keeps the dependency set small (the symbol is in libSystem on every macOS we build for).
  unsafe extern "C" {
    fn mach_host_self() -> libc::mach_port_t;
  }

  fn available_bytes() -> Option<u64> {
    let page = sysctl_u64(c"hw.pagesize")?;
    // SAFETY: an all-zero libc::vm_statistics64 is a valid, if empty, value for the call below to fill.
    let mut stats: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
    let mut count = libc::HOST_VM_INFO64_COUNT;
    // SAFETY: `stats` is a writable vm_statistics64 and `count` says how many integers it
    // holds; the host port is the caller's own.
    let rc = unsafe {
      libc::host_statistics64(
        mach_host_self(),
        libc::HOST_VM_INFO64,
        (&raw mut stats).cast::<libc::integer_t>(),
        &raw mut count,
      )
    };
    if rc != libc::KERN_SUCCESS {
      return None;
    }
    // Free, inactive, speculative and purgeable pages are what a new allocation can take
    // without swapping (Apple's own "memory available" arithmetic).
    let pages = u64::from(stats.free_count)
      .saturating_add(u64::from(stats.inactive_count))
      .saturating_add(u64::from(stats.speculative_count))
      .saturating_add(u64::from(stats.purgeable_count));
    Some(pages.saturating_mul(page))
  }

  #[link(name = "IOKit", kind = "framework")]
  unsafe extern "C" {
    fn IOPSCopyPowerSourcesInfo() -> *const c_void;
    fn IOPSGetProvidingPowerSourceType(snapshot: *const c_void) -> *const c_void;
  }

  #[link(name = "CoreFoundation", kind = "framework")]
  unsafe extern "C" {
    fn CFStringGetCString(
      string: *const c_void,
      buffer: *mut libc::c_char,
      size: isize,
      encoding: u32,
    ) -> u8;
    fn CFRelease(cf: *const c_void);
  }

  /// Format: kCFStringEncodingUTF8.
  const CF_UTF8: u32 = 0x0800_0100;

  pub(super) fn power(notes: &mut Vec<String>) -> PowerState {
    // SAFETY: the snapshot is a CF object we own and release below; the providing-type string is
    // owned by the snapshot (a "Get" call) and is read before the release.
    let state = unsafe {
      let snapshot = IOPSCopyPowerSourcesInfo();
      if snapshot.is_null() {
        None
      } else {
        let kind = IOPSGetProvidingPowerSourceType(snapshot);
        // Shape: room for the longest power-source name IOKit reports ("Battery Power").
        let mut buf = [0 as libc::c_char; 64];
        let ok = !kind.is_null()
          && CFStringGetCString(
            kind,
            buf.as_mut_ptr(),
            isize::try_from(buf.len()).unwrap_or(0),
            CF_UTF8,
          ) != 0;
        let text = if ok {
          CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
        } else {
          String::new()
        };
        CFRelease(snapshot);
        Some(text)
      }
    };
    match state.as_deref() {
      Some("AC Power") => PowerState::Mains,
      Some("Battery Power") | Some("UPS Power") => PowerState::Battery,
      other => {
        notes.push(format!(
          "power source query gave {other:?}; recorded unknown"
        ));
        PowerState::Unknown
      }
    }
  }

  #[repr(C)]
  struct RusageInfoV0 {
    // Format: the layout of rusage_info_v0 (sys/resource.h): a 16-byte uuid, then ten u64s.
    uuid: [u8; 16],
    user_time: u64,
    system_time: u64,
    pkg_idle_wkups: u64,
    interrupt_wkups: u64,
    pageins: u64,
    wired_size: u64,
    resident_size: u64,
    phys_footprint: u64,
    proc_start_abstime: u64,
    proc_exit_abstime: u64,
  }

  unsafe extern "C" {
    fn proc_pid_rusage(
      pid: libc::c_int,
      flavor: libc::c_int,
      buffer: *mut RusageInfoV0,
    ) -> libc::c_int;
  }

  pub(super) fn locked_bytes() -> Option<u64> {
    /// Format: RUSAGE_INFO_V0 (sys/resource.h).
    const RUSAGE_INFO_V0: libc::c_int = 0;
    // SAFETY: an all-zero rusage_info_v0 is a valid buffer for the call to fill.
    let mut info: RusageInfoV0 = unsafe { std::mem::zeroed() };
    // SAFETY: our own pid, the V0 flavor, and a writable buffer of the V0 layout.
    let rc = unsafe { proc_pid_rusage(libc::getpid(), RUSAGE_INFO_V0, &raw mut info) };
    if rc == 0 { Some(info.wired_size) } else { None }
  }

  pub(super) fn identity(notes: &mut Vec<String>) -> (String, String) {
    let cpu = sysctl_string(c"machdep.cpu.brand_string").unwrap_or_else(|| {
      notes.push("machdep.cpu.brand_string refused; recorded hw.model".to_owned());
      sysctl_string(c"hw.model").unwrap_or_else(|| "unknown cpu".to_owned())
    });
    let build = sysctl_string(c"kern.osversion").unwrap_or_else(|| "unknown build".to_owned());
    let release = sysctl_string(c"kern.osproductversion").unwrap_or_else(|| "macOS".to_owned());
    (cpu, format!("macOS {release} ({build})"))
  }
}

#[cfg(target_os = "linux")]
mod platform {
  use super::{
    CACHE_LINE_FALLBACK, CoreClass, CoreFacts, MemoryFacts, PageFacts, PowerState, parallelism,
    parse_size,
  };
  use std::path::Path;

  /// Reads a pseudo-file as text (a query of the kernel, never a disk read; R1's allowed site).
  fn read(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
      .ok()
      .map(|s| s.trim().to_owned())
  }

  fn list(path: &str) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(path)
      .map(|d| {
        d.filter_map(Result::ok)
          .map(|e| e.file_name().to_string_lossy().into_owned())
          .collect()
      })
      .unwrap_or_default();
    names.sort();
    names
  }

  pub(super) fn page(_notes: &mut Vec<String>) -> PageFacts {
    let base = u64::try_from(rustix::param::page_size()).unwrap_or(0);
    let mut huge: Vec<u64> = list("/sys/kernel/mm/hugepages")
      .iter()
      .filter_map(|name| name.strip_prefix("hugepages-").and_then(parse_size))
      .collect();
    huge.sort_unstable();
    let transparent_huge = read("/sys/kernel/mm/transparent_hugepage/enabled")
      .is_some_and(|s| s.contains("[always]") || s.contains("[madvise]"));
    PageFacts {
      base,
      huge,
      transparent_huge,
      allocation_granularity: base,
    }
  }

  pub(super) fn cache_line(notes: &mut Vec<String>) -> u64 {
    read("/sys/devices/system/cpu/cpu0/cache/index0/coherency_line_size")
      .and_then(|s| s.parse::<u64>().ok())
      .filter(|v| *v > 0)
      .unwrap_or_else(|| {
        notes.push(format!("no cache line size from sysconf or sysfs; recorded the {CACHE_LINE_FALLBACK}-byte fallback"));
        CACHE_LINE_FALLBACK
      })
  }

  pub(super) fn cores(notes: &mut Vec<String>) -> Vec<CoreFacts> {
    let count = parallelism();
    let atom = cpu_list("/sys/devices/cpu_atom/cpus");
    let core = cpu_list("/sys/devices/cpu_core/cpus");
    let mut unknown = 0u32;
    let cores = (0..count)
      .map(|id| {
        let base = format!("/sys/devices/system/cpu/cpu{id}");
        let class = class_of(id, &base, &atom, &core);
        if class == CoreClass::Unknown {
          unknown = unknown.saturating_add(1);
        }
        CoreFacts {
          id,
          class,
          level: u32::from(class == CoreClass::Efficiency),
          numa: numa_of(&base),
          l2_bytes: l2_of(&base),
        }
      })
      .collect();
    if unknown > 0 {
      notes.push(format!(
        "{unknown} core(s) have no class in sysfs; recorded unknown"
      ));
    }
    cores
  }

  fn class_of(id: u32, base: &str, atom: &[u32], core: &[u32]) -> CoreClass {
    if let Some(capacity) =
      read(&format!("{base}/cpu_capacity")).and_then(|s| s.parse::<u64>().ok())
    {
      /// Format: the kernel normalizes the largest core's capacity to 1024.
      const FULL_CAPACITY: u64 = 1024;
      return if capacity >= FULL_CAPACITY {
        CoreClass::Performance
      } else {
        CoreClass::Efficiency
      };
    }
    if atom.contains(&id) {
      return CoreClass::Efficiency;
    }
    if core.contains(&id) {
      return CoreClass::Performance;
    }
    if atom.is_empty() && core.is_empty() && Path::new("/sys/devices/system/cpu/cpu0").exists() {
      // A homogeneous machine: every core is the same class, which is performance.
      return CoreClass::Performance;
    }
    CoreClass::Unknown
  }

  /// Parses a kernel cpu list such as `0-3,8,10-11`.
  fn cpu_list(path: &str) -> Vec<u32> {
    let Some(text) = read(path) else {
      return Vec::new();
    };
    let mut out = Vec::new();
    for part in text.split(',') {
      let part = part.trim();
      if let Some((a, b)) = part.split_once('-') {
        if let (Ok(a), Ok(b)) = (a.parse::<u32>(), b.parse::<u32>()) {
          out.extend(a..=b);
        }
      } else if let Ok(v) = part.parse::<u32>() {
        out.push(v);
      }
    }
    out
  }

  fn numa_of(base: &str) -> u32 {
    list(base)
      .iter()
      .filter_map(|n| n.strip_prefix("node").and_then(|d| d.parse::<u32>().ok()))
      .next()
      .unwrap_or(0)
  }

  fn l2_of(base: &str) -> u64 {
    for index in list(&format!("{base}/cache")) {
      let dir = format!("{base}/cache/{index}");
      if read(&format!("{dir}/level")).as_deref() == Some("2") {
        return read(&format!("{dir}/size"))
          .and_then(|s| parse_size(&s))
          .unwrap_or(0);
      }
    }
    0
  }

  pub(super) fn memory(_notes: &mut Vec<String>) -> MemoryFacts {
    let info = rustix::system::sysinfo();
    let unit = u64::from(info.mem_unit).max(1);
    // c_ulong is u32 on i686 and u64 elsewhere; the conversion is for the former.
    #[allow(clippy::useless_conversion)]
    let (total, free) = (
      u64::from(info.totalram).saturating_mul(unit),
      u64::from(info.freeram).saturating_mul(unit),
    );
    let available = read("/proc/meminfo")
      .and_then(|text| {
        text
          .lines()
          .find_map(|l| l.strip_prefix("MemAvailable:"))
          .and_then(parse_size)
      })
      .unwrap_or(free);
    MemoryFacts {
      total,
      available,
      address_bits: usize::BITS,
    }
  }

  pub(super) fn power(notes: &mut Vec<String>) -> PowerState {
    let supplies = list("/sys/class/power_supply");
    let mut mains_online = false;
    let mut battery = false;
    for name in &supplies {
      let base = format!("/sys/class/power_supply/{name}");
      match read(&format!("{base}/type")).as_deref() {
        Some("Mains") | Some("USB") => {
          if read(&format!("{base}/online")).as_deref() == Some("1") {
            mains_online = true;
          }
        }
        Some("Battery") => battery = true,
        _ => {}
      }
    }
    match (mains_online, battery) {
      (true, _) => PowerState::Mains,
      (false, true) => PowerState::Battery,
      (false, false) => {
        notes.push("no power supply entry answered; recorded unknown".to_owned());
        PowerState::Unknown
      }
    }
  }

  pub(super) fn locked_bytes() -> Option<u64> {
    let status = read("/proc/self/status")?;
    let line = status.lines().find(|l| l.starts_with("VmLck:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    /// Format: VmLck is printed in kibibytes.
    const KIB: u64 = 1024;
    Some(kib.saturating_mul(KIB))
  }

  pub(super) fn identity(notes: &mut Vec<String>) -> (String, String) {
    let cpu = read("/proc/cpuinfo")
      .and_then(|text| {
        text
          .lines()
          .find(|l| {
            l.starts_with("model name") || l.starts_with("Model") || l.starts_with("cpu model")
          })
          .and_then(|l| l.split_once(':').map(|(_, v)| v.trim().to_owned()))
      })
      .unwrap_or_else(|| {
        notes.push("/proc/cpuinfo has no model line; recorded unknown cpu".to_owned());
        "unknown cpu".to_owned()
      });
    let uts = rustix::system::uname();
    let os = format!(
      "{} {}",
      uts.sysname().to_string_lossy(),
      uts.release().to_string_lossy()
    );
    (cpu, os)
  }
}

#[cfg(windows)]
mod platform {
  use super::{
    CACHE_LINE_FALLBACK, CoreClass, CoreFacts, MemoryFacts, PageFacts, PowerState, parallelism,
  };
  use windows_sys::Win32::System::Memory::GetLargePageMinimum;
  use windows_sys::Win32::System::Power::GetSystemPowerStatus;
  use windows_sys::Win32::System::Power::SYSTEM_POWER_STATUS;
  use windows_sys::Win32::System::SystemInformation::{
    GetLogicalProcessorInformation, GetSystemInfo, GetVersion, GlobalMemoryStatusEx,
    MEMORYSTATUSEX, RelationCache, RelationNumaNode, SYSTEM_INFO,
    SYSTEM_LOGICAL_PROCESSOR_INFORMATION,
  };

  fn system_info() -> SYSTEM_INFO {
    // SAFETY: an all-zero SYSTEM_INFO is a valid, if empty, value for the call below to fill.
    let mut info: SYSTEM_INFO = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is a writable SYSTEM_INFO; GetSystemInfo cannot fail.
    unsafe { GetSystemInfo(&raw mut info) };
    info
  }

  pub(super) fn page(_notes: &mut Vec<String>) -> PageFacts {
    let info = system_info();
    // SAFETY: no preconditions; 0 means large pages are unavailable to this process.
    let large = unsafe { GetLargePageMinimum() };
    let huge = if large > 0 {
      vec![u64::try_from(large).unwrap_or(0)]
    } else {
      Vec::new()
    };
    PageFacts {
      base: u64::from(info.dwPageSize),
      huge,
      transparent_huge: false,
      allocation_granularity: u64::from(info.dwAllocationGranularity),
    }
  }

  fn processor_information() -> Vec<SYSTEM_LOGICAL_PROCESSOR_INFORMATION> {
    let mut len: u32 = 0;
    // SAFETY: a null buffer asks for the required length.
    unsafe { GetLogicalProcessorInformation(std::ptr::null_mut(), &raw mut len) };
    let entry = u32::try_from(std::mem::size_of::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION>())
      .unwrap_or(u32::MAX);
    if len == 0 || entry == 0 {
      return Vec::new();
    }
    let count = usize::try_from(len / entry).unwrap_or(0);
    let mut out: Vec<SYSTEM_LOGICAL_PROCESSOR_INFORMATION> = Vec::with_capacity(count);
    // SAFETY: the buffer holds `count` entries of `len` bytes total; on success the call filled
    // `len` bytes, so `count` entries are initialized.
    let ok = unsafe { GetLogicalProcessorInformation(out.as_mut_ptr(), &raw mut len) };
    if ok == 0 {
      return Vec::new();
    }
    // SAFETY: see above; `len / entry` entries were written.
    unsafe { out.set_len(usize::try_from(len / entry).unwrap_or(0)) };
    out
  }

  pub(super) fn cache_line(notes: &mut Vec<String>) -> u64 {
    for entry in processor_information() {
      if entry.Relationship == RelationCache {
        // SAFETY: the relationship says the union holds the cache descriptor.
        let cache = unsafe { entry.Anonymous.Cache };
        if cache.Level == 1 && cache.LineSize > 0 {
          return u64::from(cache.LineSize);
        }
      }
    }
    notes.push(format!("GetLogicalProcessorInformation gave no L1 line size; recorded the {CACHE_LINE_FALLBACK}-byte fallback"));
    CACHE_LINE_FALLBACK
  }

  pub(super) fn cores(notes: &mut Vec<String>) -> Vec<CoreFacts> {
    let info = processor_information();
    let mut l2: u64 = 0;
    for entry in &info {
      if entry.Relationship == RelationCache {
        // SAFETY: the relationship says the union holds the cache descriptor.
        let cache = unsafe { entry.Anonymous.Cache };
        if cache.Level == 2 {
          l2 = l2.max(u64::from(cache.Size));
        }
      }
    }
    let numa_of = |id: u32| -> u32 {
      for entry in &info {
        if entry.Relationship == RelationNumaNode && (entry.ProcessorMask >> id) & 1 == 1 {
          // SAFETY: the relationship says the union holds the NUMA descriptor.
          return unsafe { entry.Anonymous.NumaNode.NodeNumber };
        }
      }
      0
    };
    notes
      .push("core classes are not queried on Windows in this phase; recorded unknown".to_owned());
    (0..parallelism())
      .map(|id| CoreFacts {
        id,
        class: CoreClass::Unknown,
        level: 0,
        numa: numa_of(id),
        l2_bytes: l2,
      })
      .collect()
  }

  pub(super) fn memory(notes: &mut Vec<String>) -> MemoryFacts {
    // SAFETY: an all-zero MEMORYSTATUSEX is a valid, if empty, value for the call below to fill.
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = u32::try_from(std::mem::size_of::<MEMORYSTATUSEX>()).unwrap_or(0);
    // SAFETY: `status` is a writable MEMORYSTATUSEX with its length set.
    let ok = unsafe { GlobalMemoryStatusEx(&raw mut status) };
    if ok == 0 {
      notes.push("GlobalMemoryStatusEx refused; recorded 0".to_owned());
      return MemoryFacts {
        total: 0,
        available: 0,
        address_bits: usize::BITS,
      };
    }
    MemoryFacts {
      total: status.ullTotalPhys,
      available: status.ullAvailPhys,
      address_bits: usize::BITS,
    }
  }

  pub(super) fn power(notes: &mut Vec<String>) -> PowerState {
    // SAFETY: an all-zero SYSTEM_POWER_STATUS is a valid, if empty, value for the call below to fill.
    let mut status: SYSTEM_POWER_STATUS = unsafe { std::mem::zeroed() };
    // SAFETY: `status` is a writable SYSTEM_POWER_STATUS.
    let ok = unsafe { GetSystemPowerStatus(&raw mut status) };
    if ok == 0 {
      notes.push("GetSystemPowerStatus refused; recorded unknown".to_owned());
      return PowerState::Unknown;
    }
    match status.ACLineStatus {
      1 => PowerState::Mains,
      0 => PowerState::Battery,
      _ => PowerState::Unknown,
    }
  }

  pub(super) fn locked_bytes() -> Option<u64> {
    None
  }

  pub(super) fn identity(_notes: &mut Vec<String>) -> (String, String) {
    let info = system_info();
    // SAFETY: no preconditions.
    let version = unsafe { GetVersion() };
    let cpu = format!(
      "windows processor architecture {} level {} revision {}",
      // SAFETY: the union's named struct is the documented layout on every current Windows.
      unsafe { info.Anonymous.Anonymous.wProcessorArchitecture },
      info.wProcessorLevel,
      info.wProcessorRevision
    );
    (cpu, format!("windows {version:#x}"))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_facts_are_queried_without_refusal_on_this_machine() {
    let facts = Facts::query();
    assert!(facts.page.base >= 4096, "{facts:?}");
    assert!(facts.page.base.is_power_of_two());
    assert!(
      facts.cache_line >= 32 && facts.cache_line.is_power_of_two(),
      "{}",
      facts.cache_line
    );
    assert!(!facts.cores.is_empty());
    assert!(facts.memory.total > 0);
    assert!(facts.memory.available <= facts.memory.total || facts.memory.total == 0);
    assert!(!facts.identity.cpu.is_empty());
    let line = facts.identity.line();
    assert!(line.contains("cores"), "{line}");
  }

  #[test]
  fn identity_hash_is_stable_and_changes_with_any_field() {
    let a = Identity {
      cpu: "cpu".into(),
      os: "os".into(),
      arch: "arch".into(),
      cores: 8,
      memory: 1,
      page: 4096,
    };
    let mut b = a.clone();
    assert_eq!(a.hash(), b.hash());
    b.cores = 9;
    assert_ne!(a.hash(), b.hash());
  }

  #[test]
  fn sizes_with_unit_suffixes_parse() {
    assert_eq!(parse_size("2048kB"), Some(2048 * 1024));
    assert_eq!(parse_size("1024K"), Some(1024 * 1024));
    assert_eq!(parse_size("16 MB"), Some(16 * 1024 * 1024));
    assert_eq!(parse_size("12345"), Some(12345));
    assert_eq!(parse_size("x"), None);
    assert_eq!(parse_size("3 parsecs"), None);
  }

  #[test]
  fn facts_round_trip_through_json() {
    let facts = Facts::query();
    let json = serde_json::to_string(&facts).unwrap();
    let back: Facts = serde_json::from_str(&json).unwrap();
    assert_eq!(facts, back);
  }
}
