//! Diagnostics only, never on main (A-223's open item (C), goal g234): how long a thread parked as a slates shard
//! parks on Windows takes to run again after a kick, alone and beside spinners, and which of the wait's primitive, the
//! wake's priority boost, the thread's base priority and its pinning decides it. Prints the runner's scheduler facts,
//! the length of a turn on a CPU two threads share (the quantum), then one line per arm.
//!
//! Usage: `wake_load <condition>`; the condition is only a label (the workflow starts the spinners).
#![allow(clippy::all, clippy::pedantic, clippy::restriction, clippy::nursery, unreachable_pub)]

#[cfg(not(windows))]
fn main() {
  eprintln!("wake_load: Windows only (A-223 item (C))");
}

#[cfg(windows)]
fn main() {
  let condition = std::env::args().nth(1).unwrap_or_else(|| "quiet".to_string());
  let samples = std::env::var("WAKE_SAMPLES")
    .ok()
    .and_then(|text| text.parse().ok())
    .unwrap_or(200);
  probe::run(&condition, samples);
}

#[cfg(windows)]
mod probe {
  use std::collections::BTreeMap;
  use std::ffi::c_void;
  use std::future::Future;
  use std::pin::Pin;
  use std::sync::OnceLock;
  use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
  use std::sync::mpsc;
  use std::task::{Context, Poll, Waker};
  use std::time::{Duration, Instant};

  use slates_rt::runtime::{Runtime, RuntimeConfig};
  use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
  use windows_sys::Win32::System::IO::{
    CreateIoCompletionPort, GetQueuedCompletionStatusEx, OVERLAPPED_ENTRY, PostQueuedCompletionStatus,
  };
  use windows_sys::Win32::System::Registry::{
    HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW,
  };
  use windows_sys::Win32::System::SystemInformation::GetSystemTimeAdjustment;
  use windows_sys::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, GetCurrentProcessorNumber, GetCurrentThread, GetPriorityClass, INFINITE,
    SetEvent, SetThreadAffinityMask, SetThreadPriority, SetThreadPriorityBoost, THREAD_PRIORITY_ABOVE_NORMAL,
    THREAD_PRIORITY_NORMAL, THREAD_PRIORITY_TIME_CRITICAL, WaitForSingleObject, WaitOnAddress,
    WakeByAddressSingle,
  };

  #[link(name = "ntdll", kind = "raw-dylib")]
  unsafe extern "system" {
    fn NtQueryInformationThread(
      thread: HANDLE,
      class: u32,
      information: *mut c_void,
      length: u32,
      returned: *mut u32,
    ) -> i32;
    fn NtQueryTimerResolution(coarsest: *mut u32, finest: *mut u32, current: *mut u32) -> i32;
  }

  /// The CPU the kicker is fixed to (the runtime's control core).
  const KICKER_CPU: usize = 0;
  /// The CPU the waiter is fixed to when pinned (the runtime's first shard core).
  const WAITER_CPU: usize = 1;
  /// How long the kicker waits after the waiter announced its wait, so the waiter is asleep when the kick lands.
  const SETTLE: Duration = Duration::from_millis(2);
  /// The longest an arm runs before it stops taking samples.
  const ARM_BUDGET: Duration = Duration::from_secs(60);
  /// How long the kicker waits for the waiter's announcement before it gives the arm up, milliseconds.
  const READY_WAIT_MS: u32 = 120_000;
  /// A gap in a spinner's clock readings longer than this is a turn another thread ran.
  const GAP: Duration = Duration::from_micros(100);
  /// How long the quantum probe spins.
  const QUANTUM_SPAN: Duration = Duration::from_secs(3);
  /// Bucket edges for the latency histogram, microseconds.
  const EDGES_US: [u64; 13] = [10, 30, 100, 1_000, 5_000, 10_000, 20_000, 40_000, 60_000, 80_000, 100_000, 150_000, 200_000];

  static EPOCH: OnceLock<Instant> = OnceLock::new();

  fn now_ns() -> u64 {
    u64::try_from(EPOCH.get_or_init(Instant::now).elapsed().as_nanos()).unwrap_or(u64::MAX)
  }

  #[repr(C)]
  struct ThreadBasicInformation {
    exit_status: i32,
    teb: *mut c_void,
    unique_process: *mut c_void,
    unique_thread: *mut c_void,
    affinity: usize,
    priority: i32,
    base_priority: i32,
  }

  /// The calling thread's current (dynamic) priority and its base priority, as the kernel keeps them.
  fn priorities() -> (i32, i32) {
    let mut information = ThreadBasicInformation {
      exit_status: 0,
      teb: std::ptr::null_mut(),
      unique_process: std::ptr::null_mut(),
      unique_thread: std::ptr::null_mut(),
      affinity: 0,
      priority: -1,
      base_priority: -1,
    };
    let mut returned = 0u32;
    let length = u32::try_from(size_of::<ThreadBasicInformation>()).unwrap_or(0);
    // SAFETY: the calling thread's pseudo-handle; the record is this frame's own and its size is stated.
    let status = unsafe {
      NtQueryInformationThread(
        GetCurrentThread(),
        0,
        (&raw mut information).cast(),
        length,
        &raw mut returned,
      )
    };
    if status < 0 {
      (-1, -1)
    } else {
      (information.priority, information.base_priority)
    }
  }

  fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
  }

  fn registry_dword(key: &str, value: &str) -> Option<u32> {
    let (key, value) = (wide(key), wide(value));
    let mut data = 0u32;
    let mut size = 4u32;
    // SAFETY: both names are NUL-terminated; the data is a u32 whose size is stated.
    let status = unsafe {
      RegGetValueW(
        HKEY_LOCAL_MACHINE,
        key.as_ptr(),
        value.as_ptr(),
        RRF_RT_REG_DWORD,
        std::ptr::null_mut(),
        (&raw mut data).cast(),
        &raw mut size,
      )
    };
    (status == 0).then_some(data)
  }

  fn registry_string(key: &str, value: &str) -> Option<String> {
    let (key, value) = (wide(key), wide(value));
    let mut data = vec![0u16; 512];
    let mut size = u32::try_from(data.len() * 2).unwrap_or(0);
    // SAFETY: both names are NUL-terminated; the buffer holds `size` bytes.
    let status = unsafe {
      RegGetValueW(
        HKEY_LOCAL_MACHINE,
        key.as_ptr(),
        value.as_ptr(),
        RRF_RT_REG_SZ,
        std::ptr::null_mut(),
        data.as_mut_ptr().cast(),
        &raw mut size,
      )
    };
    if status != 0 {
      return None;
    }
    let end = data.iter().position(|unit| *unit == 0).unwrap_or(data.len());
    Some(String::from_utf16_lossy(&data[..end]))
  }

  fn facts() {
    let version = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
    let (mut coarsest, mut finest, mut current) = (0u32, 0u32, 0u32);
    // SAFETY: three writable words of this frame.
    let resolution = unsafe { NtQueryTimerResolution(&raw mut coarsest, &raw mut finest, &raw mut current) };
    let (mut adjustment, mut increment, mut disabled) = (0u32, 0u32, 0i32);
    // SAFETY: three writable words of this frame.
    unsafe { GetSystemTimeAdjustment(&raw mut adjustment, &raw mut increment, &raw mut disabled) };
    // SAFETY: the current process's pseudo-handle.
    let class = unsafe { GetPriorityClass(GetCurrentProcess()) };
    let (priority, base) = priorities();
    println!(
      "facts: cpus={} product={:?} installation={:?} build={:?} ubr={:?} win32_priority_separation={:?} \
       timer_resolution_100ns(status {resolution}) coarsest={coarsest} finest={finest} current={current} \
       clock_increment_100ns={increment} priority_class={class:#x} main_thread_priority={priority} base={base}",
      std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
      registry_string(version, "ProductName"),
      registry_string(version, "InstallationType"),
      registry_string(version, "CurrentBuild"),
      registry_dword(version, "UBR"),
      registry_dword(r"SYSTEM\CurrentControlSet\Control\PriorityControl", "Win32PrioritySeparation"),
    );
  }

  fn pin(cpu: usize) -> bool {
    // SAFETY: the calling thread's pseudo-handle and a mask of one CPU the machine has.
    unsafe { SetThreadAffinityMask(GetCurrentThread(), 1usize << cpu) != 0 }
  }

  fn set_priority(level: i32) -> bool {
    // SAFETY: the calling thread's pseudo-handle.
    unsafe { SetThreadPriority(GetCurrentThread(), level) != 0 }
  }

  /// Two threads at normal priority fixed to one CPU, each spinning on the clock: every gap in a thread's readings
  /// longer than `GAP` is a turn something else ran there, mostly the other thread. Prints each thread's gaps.
  fn quantum(label: &str, cpu: usize) {
    let barrier = std::sync::Barrier::new(2);
    let gaps: Vec<Vec<u64>> = std::thread::scope(|scope| {
      let handles: Vec<_> = (0..2)
        .map(|_| {
          scope.spawn(|| {
            pin(cpu);
            barrier.wait();
            let start = Instant::now();
            let mut last = start;
            let mut gaps = Vec::new();
            loop {
              let now = Instant::now();
              let gap = now.duration_since(last);
              if gap > GAP {
                gaps.push(u64::try_from(gap.as_micros()).unwrap_or(u64::MAX));
              }
              last = now;
              if now.duration_since(start) > QUANTUM_SPAN {
                break gaps;
              }
            }
          })
        })
        .collect();
      handles.into_iter().map(|handle| handle.join().unwrap_or_default()).collect()
    });
    for (thread, mut gaps) in gaps.into_iter().enumerate() {
      gaps.sort_unstable();
      let total: u64 = gaps.iter().sum();
      println!(
        "quantum {label} cpu={cpu} thread={thread}: gaps={} off_total_ms={:.1} p10={} p50={} p90={} max={} us; first gaps us: {:?}",
        gaps.len(),
        total as f64 / 1000.0,
        pick(&gaps, 10),
        pick(&gaps, 50),
        pick(&gaps, 90),
        gaps.last().copied().unwrap_or(0),
        gaps.iter().take(40).collect::<Vec<_>>()
      );
    }
  }

  /// The value at `percent` of a sorted list.
  fn pick(sorted: &[u64], percent: usize) -> u64 {
    if sorted.is_empty() {
      return 0;
    }
    let index = (sorted.len() * percent / 100).min(sorted.len() - 1);
    sorted[index]
  }

  /// The cost of raising the calling thread one level and putting it back, nanoseconds a pair.
  fn raise_cost() {
    const PAIRS: u32 = 10_000;
    let started = Instant::now();
    for _ in 0..PAIRS {
      set_priority(THREAD_PRIORITY_ABOVE_NORMAL);
      set_priority(THREAD_PRIORITY_NORMAL);
    }
    println!("raise_cost: {} ns a raise-and-restore pair", started.elapsed().as_nanos() / u128::from(PAIRS));
  }

  #[derive(Clone, Copy, PartialEq, Debug)]
  enum Primitive {
    Iocp,
    Event,
    Address,
    Runtime,
  }

  #[derive(Clone, Copy, Debug)]
  struct Arm {
    name: &'static str,
    primitive: Primitive,
    priority: i32,
    boost_off: bool,
    pinned: bool,
    raised_while_parked: bool,
  }

  const ARMS: [Arm; 10] = [
    Arm { name: "iocp", primitive: Primitive::Iocp, priority: THREAD_PRIORITY_NORMAL, boost_off: false, pinned: true, raised_while_parked: false },
    Arm { name: "iocp-unpinned", primitive: Primitive::Iocp, priority: THREAD_PRIORITY_NORMAL, boost_off: false, pinned: false, raised_while_parked: false },
    Arm { name: "iocp-noboost", primitive: Primitive::Iocp, priority: THREAD_PRIORITY_NORMAL, boost_off: true, pinned: true, raised_while_parked: false },
    Arm { name: "iocp-above", primitive: Primitive::Iocp, priority: THREAD_PRIORITY_ABOVE_NORMAL, boost_off: false, pinned: true, raised_while_parked: false },
    Arm { name: "iocp-raised-while-parked", primitive: Primitive::Iocp, priority: THREAD_PRIORITY_NORMAL, boost_off: false, pinned: true, raised_while_parked: true },
    Arm { name: "event", primitive: Primitive::Event, priority: THREAD_PRIORITY_NORMAL, boost_off: false, pinned: true, raised_while_parked: false },
    Arm { name: "event-noboost", primitive: Primitive::Event, priority: THREAD_PRIORITY_NORMAL, boost_off: true, pinned: true, raised_while_parked: false },
    Arm { name: "address", primitive: Primitive::Address, priority: THREAD_PRIORITY_NORMAL, boost_off: false, pinned: true, raised_while_parked: false },
    Arm { name: "rt", primitive: Primitive::Runtime, priority: THREAD_PRIORITY_NORMAL, boost_off: false, pinned: true, raised_while_parked: false },
    Arm { name: "rt-above", primitive: Primitive::Runtime, priority: THREAD_PRIORITY_ABOVE_NORMAL, boost_off: false, pinned: true, raised_while_parked: false },
  ];

  /// What the waiter saw of one wake.
  #[derive(Clone, Copy, Debug, Default)]
  struct Woke {
    at_ns: u64,
    slept: bool,
    priority_before: i32,
    priority_after: i32,
    base_after: i32,
    cpu: u32,
  }

  /// When the kicker kicked, and how long its call took.
  #[derive(Clone, Copy, Debug, Default)]
  struct Kick {
    at_ns: u64,
    call_ns: u64,
  }

  /// The handles one arm's two threads share, kept as their exposed addresses.
  struct Shared {
    port: usize,
    event: usize,
    ready: usize,
    word: AtomicU32,
    stop: AtomicBool,
  }

  fn handle(address: usize) -> HANDLE {
    std::ptr::with_exposed_provenance_mut(address)
  }

  fn wait(primitive: Primitive, shared: &Shared, expected: u32) {
    match primitive {
      Primitive::Iocp => {
        // SAFETY: an all-zero entry is a valid empty record.
        let mut entry: OVERLAPPED_ENTRY = unsafe { std::mem::zeroed() };
        let mut count = 0u32;
        // SAFETY: the arm's live port; one entry of this frame.
        unsafe { GetQueuedCompletionStatusEx(handle(shared.port), &raw mut entry, 1, &raw mut count, INFINITE, 0) };
      }
      Primitive::Event => {
        // SAFETY: the arm's live event.
        unsafe { WaitForSingleObject(handle(shared.event), INFINITE) };
      }
      Primitive::Address => {
        while shared.word.load(Ordering::Acquire) == expected {
          // SAFETY: the word lives as long as the arm; the compare value is this frame's.
          unsafe {
            WaitOnAddress(
              (&raw const shared.word).cast(),
              (&raw const expected).cast(),
              4,
              INFINITE,
            )
          };
        }
      }
      Primitive::Runtime => {}
    }
  }

  fn kick(primitive: Primitive, shared: &Shared, next: u32) {
    match primitive {
      Primitive::Iocp => {
        // SAFETY: the arm's live port.
        unsafe { PostQueuedCompletionStatus(handle(shared.port), 0, 1, std::ptr::null_mut()) };
      }
      Primitive::Event => {
        // SAFETY: the arm's live event.
        unsafe { SetEvent(handle(shared.event)) };
      }
      Primitive::Address => {
        shared.word.store(next, Ordering::Release);
        // SAFETY: the word lives as long as the arm.
        unsafe { WakeByAddressSingle((&raw const shared.word).cast()) };
      }
      Primitive::Runtime => {}
    }
  }

  fn switches() -> Option<u64> {
    slates_machine::switches::switches_now()
  }

  fn waiter(arm: Arm, samples: usize, shared: &Shared) -> Vec<Woke> {
    if arm.pinned {
      pin(WAITER_CPU);
    }
    set_priority(arm.priority);
    if arm.boost_off {
      // SAFETY: the calling thread's pseudo-handle.
      unsafe { SetThreadPriorityBoost(GetCurrentThread(), 1) };
    }
    let mut seen = Vec::with_capacity(samples);
    for index in 0..samples {
      let priority_before = priorities().0;
      let before = switches();
      if arm.raised_while_parked {
        set_priority(THREAD_PRIORITY_ABOVE_NORMAL);
      }
      // SAFETY: the arm's live event.
      unsafe { SetEvent(handle(shared.ready)) };
      wait(arm.primitive, shared, u32::try_from(index).unwrap_or(u32::MAX));
      let at_ns = now_ns();
      let (priority_after, base_after) = priorities();
      if arm.raised_while_parked {
        set_priority(arm.priority);
      }
      // SAFETY: takes no argument.
      let cpu = unsafe { GetCurrentProcessorNumber() };
      let slept = match (before, switches()) {
        (Some(before), Some(after)) => after > before,
        _ => true,
      };
      seen.push(Woke { at_ns, slept, priority_before, priority_after, base_after, cpu });
      if shared.stop.load(Ordering::Acquire) {
        break;
      }
    }
    seen
  }

  fn kicker(arm: Arm, samples: usize, shared: &Shared) -> Vec<Kick> {
    pin(KICKER_CPU);
    set_priority(THREAD_PRIORITY_TIME_CRITICAL);
    let started = Instant::now();
    let mut kicks = Vec::with_capacity(samples);
    for index in 0..samples {
      // SAFETY: the arm's live event.
      if unsafe { WaitForSingleObject(handle(shared.ready), READY_WAIT_MS) } != WAIT_OBJECT_0 {
        println!("  {}: the waiter never announced wait {index}", arm.name);
        // Release the waiter whenever it reaches its wait: the kick stays pending until then.
        shared.stop.store(true, Ordering::Release);
        kick(arm.primitive, shared, u32::try_from(index + 1).unwrap_or(u32::MAX));
        break;
      }
      std::thread::sleep(SETTLE);
      if started.elapsed() > ARM_BUDGET || index + 1 == samples {
        shared.stop.store(true, Ordering::Release);
      }
      let at_ns = now_ns();
      kick(arm.primitive, shared, u32::try_from(index + 1).unwrap_or(u32::MAX));
      kicks.push(Kick { at_ns, call_ns: now_ns().saturating_sub(at_ns) });
      if shared.stop.load(Ordering::Acquire) {
        break;
      }
    }
    kicks
  }

  fn raw_arm(arm: Arm, samples: usize) -> (Vec<Woke>, Vec<Kick>) {
    // SAFETY: a fresh port of one concurrent thread, as the runtime makes; checked below.
    let port = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 1) };
    // SAFETY: two fresh anonymous auto-reset events.
    let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
    // SAFETY: as above.
    let ready = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
    let shared = Shared {
      port: port.expose_provenance(),
      event: event.expose_provenance(),
      ready: ready.expose_provenance(),
      word: AtomicU32::new(0),
      stop: AtomicBool::new(false),
    };
    let result = std::thread::scope(|scope| {
      let waiter = scope.spawn(|| waiter(arm, samples, &shared));
      let kicker = scope.spawn(|| kicker(arm, samples, &shared));
      (waiter.join().unwrap_or_default(), kicker.join().unwrap_or_default())
    });
    // SAFETY: the three handles are this arm's own, closed once after both threads ended.
    unsafe {
      CloseHandle(port);
      CloseHandle(event);
      CloseHandle(ready);
    }
    result
  }

  /// A task on the runtime's shard that reports every poll (when, at what priority, on which CPU, whether the shard
  /// slept since the last) with its waker, and stays pending until it has reported `remaining` times.
  struct Reporter {
    reports: mpsc::Sender<(Woke, Waker)>,
    priority: i32,
    first: bool,
    remaining: usize,
    switches: Option<u64>,
  }

  impl Future for Reporter {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
      let at_ns = now_ns();
      let (priority_after, base_after) = priorities();
      // SAFETY: takes no argument.
      let cpu = unsafe { GetCurrentProcessorNumber() };
      let now = switches();
      let slept = match (self.switches, now) {
        (Some(before), Some(after)) => after > before,
        _ => true,
      };
      if self.first {
        self.first = false;
        set_priority(self.priority);
      }
      let woke = Woke { at_ns, slept, priority_before: 0, priority_after, base_after, cpu };
      if self.remaining == 0 {
        let _ = self.reports.send((woke, cx.waker().clone()));
        return Poll::Ready(());
      }
      self.remaining -= 1;
      // The stamp of switches is taken after the send, so the next poll's difference spans the park alone.
      let _ = self.reports.send((woke, cx.waker().clone()));
      self.switches = switches();
      Poll::Pending
    }
  }

  fn runtime_arm(arm: Arm, samples: usize) -> (Vec<Woke>, Vec<Kick>) {
    let config = RuntimeConfig {
      shards: 1,
      tasks_per_shard: 16,
      timers_per_shard: 16,
      ring_entries: 16,
      step_budget_ns: 1_000_000,
      timer_tick_ns: 100_000,
      batch: 16,
      pin: arm.pinned,
      cores: if arm.pinned { vec![u32::try_from(WAITER_CPU).unwrap_or(1)] } else { Vec::new() },
      page_bytes: 4096,
      spin_ns: 0,
      wake_tracking: None,
    };
    let runtime = match Runtime::start(&config) {
      Ok(runtime) => runtime,
      Err(refused) => {
        println!("  {}: the runtime refused to start: {refused:?}", arm.name);
        return (Vec::new(), Vec::new());
      }
    };
    let Some(shard) = runtime.shard_ids().first().copied() else {
      return (Vec::new(), Vec::new());
    };
    let (reports, received) = mpsc::channel();
    let spawned = runtime.spawn_on(
      shard,
      Reporter { reports, priority: arm.priority, first: true, remaining: samples, switches: None },
    );
    if let Err(refused) = spawned {
      println!("  {}: the spawn was refused: {refused:?}", arm.name);
      return (Vec::new(), Vec::new());
    }
    let result = std::thread::scope(|scope| {
      scope
        .spawn(move || {
          pin(KICKER_CPU);
          set_priority(THREAD_PRIORITY_TIME_CRITICAL);
          let started = Instant::now();
          let mut seen = Vec::with_capacity(samples);
          let mut kicks = Vec::with_capacity(samples);
          let mut first = true;
          while let Ok((woke, waker)) = received.recv_timeout(Duration::from_millis(u64::from(READY_WAIT_MS))) {
            if first {
              first = false;
            } else {
              seen.push(woke);
            }
            if seen.len() >= samples || started.elapsed() > ARM_BUDGET {
              // The runtime's drop cancels the task, still pending.
              break;
            }
            std::thread::sleep(SETTLE);
            let at_ns = now_ns();
            waker.wake();
            kicks.push(Kick { at_ns, call_ns: now_ns().saturating_sub(at_ns) });
          }
          (seen, kicks)
        })
        .join()
        .unwrap_or_default()
    });
    drop(runtime);
    result
  }

  fn report(arm: Arm, condition: &str, seen: &[Woke], kicks: &[Kick]) {
    let mut latencies = Vec::new();
    let mut unslept = 0usize;
    let mut slow_calls = 0usize;
    let mut after = BTreeMap::new();
    let mut before = BTreeMap::new();
    let mut base = BTreeMap::new();
    let mut cpus = BTreeMap::new();
    for (woke, kick) in seen.iter().zip(kicks) {
      if kick.call_ns > 1_000_000 {
        slow_calls += 1;
      }
      if !woke.slept {
        unslept += 1;
        continue;
      }
      latencies.push(woke.at_ns.saturating_sub(kick.at_ns) / 1_000);
      *after.entry(woke.priority_after).or_insert(0usize) += 1;
      *before.entry(woke.priority_before).or_insert(0usize) += 1;
      *base.entry(woke.base_after).or_insert(0usize) += 1;
      *cpus.entry(woke.cpu).or_insert(0usize) += 1;
    }
    latencies.sort_unstable();
    let mut buckets = vec![0usize; EDGES_US.len() + 1];
    for latency in &latencies {
      let bucket = EDGES_US.iter().position(|edge| latency < edge).unwrap_or(EDGES_US.len());
      buckets[bucket] += 1;
    }
    let mean = if latencies.is_empty() { 0 } else { latencies.iter().sum::<u64>() / latencies.len() as u64 };
    println!(
      "arm={} cond={condition} kept={} unslept={unslept} slow_kick_calls={slow_calls} p50={} p90={} p99={} max={} mean={} us; \
       buckets(<{:?}us,rest)={buckets:?}; priority_after={after:?} priority_before={before:?} base={base:?} cpus={cpus:?}",
      arm.name,
      latencies.len(),
      pick(&latencies, 50),
      pick(&latencies, 90),
      pick(&latencies, 99),
      latencies.last().copied().unwrap_or(0),
      mean,
      EDGES_US,
    );
    let all: Vec<String> = latencies.iter().map(|us| format!("{:.2}", *us as f64 / 1000.0)).collect();
    println!("  {} {condition} sorted ms: {}", arm.name, all.join(" "));
  }

  pub fn run(condition: &str, samples: usize) {
    let _ = now_ns();
    facts();
    if std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) < 2 {
      println!("wake_load: needs two CPUs");
      return;
    }
    raise_cost();
    quantum(condition, WAITER_CPU);
    let only = std::env::var("WAKE_ARMS").ok();
    for arm in ARMS {
      if let Some(only) = &only
        && !only.split(',').any(|name| name == arm.name)
      {
        continue;
      }
      let started = Instant::now();
      let (seen, kicks) = match arm.primitive {
        Primitive::Runtime => runtime_arm(arm, samples),
        _ => raw_arm(arm, samples),
      };
      report(arm, condition, &seen, &kicks);
      println!("  {} took {:.1} s", arm.name, started.elapsed().as_secs_f64());
    }
  }
}
