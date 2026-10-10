# Two tests the Linux lane failed: a macOS expectation asked of Linux, and a race on a process-wide counter

**Found:** 2026-10-10, by the Linux lane over `3cf3265e`, CI's Linux test step reproduced in a container: io_uring
driver, non-root, 2 build jobs, 4 test threads. 251 of 253 test binaries passed; these two failed. Neither is a product
defect. **Status: fixed** the same day.

## 1. `an_in_place_write_to_an_opened_base_file_hints_its_directory` (slates-base `tests/host.rs`)

- **Symptom:** "a closed file is no longer watched": the append after `close_file` still produced
  `Entry(HostDir(1), "f")`. It failed 5 of 5 runs.
- **Cause:** the test came with A-122 (`c8ffc91c`, 2026-10-08), after the last commit CI ran (`8a18d907`), so no Linux
  run had seen it. Its last assertion encodes the kqueue mechanism:
  - on macOS and the other BSDs, a directory's `EVFILT_VNODE` reports namespace changes only, so the write is heard
    through the open file's own registration, which the close releases;
  - on Linux, the directory's inotify watch hears `IN_MODIFY` for any file in it, open or not. That is by design
    (`crates/base/src/unix.rs`: "Nothing on Linux: the directory's watch already reports a write to any file in it"),
    and it is what drift detection wants.
- **Fix:** the after-close expectation follows the watch backend's `cfg`. kqueue hints nothing; inotify hints the
  file again. It now passes on macOS, and 11 of 11 base host tests pass on Linux.

## 2. `locking_a_*_region_charges_it_without_faulting_it_in` (slates-mem `tests/lock_on_fault.rs`)

- **Symptom:** "the OS reports 0 bytes locked, below the 268435456-byte region".
- **First reading, wrong:** Docker's locked-memory limit. The container's default is unlimited, and the binary
  passed when rerun.
- **Cause:** both tests read the process-wide `VmLck` before and after their own 256 MiB lock. Running in parallel,
  the other test's lock, or its unlock as its region drops, landed inside the window. Measured on Linux:
  - on two test threads, 8 of 30 runs failed, each test among the failures;
  - on one thread, 0 of 10.

  CI runs them in parallel as well, so its passes were luck.
- **Fix:** a test-harness lock (`LOCKED_BYTES_WINDOW`, D-8 exception 3, the fleet suite's pattern). Each test holds it
  from before its region exists until after the region drops. After the fix: 30 of 30 on two threads.

## Sibling sweep

- **`crates/mem/tests/capacity.rs`** also asserts exact process-wide `VmLck` deltas. Its other test uses an unlocked
  arena, so nothing in that binary moves `VmLck` concurrently. It passed 30 of 30 on two threads.
- **`crates/cli/tests/cli.rs`** reads a daemon's own `/proc/PID/status`, so the reading is per test.
- **The other new base tests** (A-123's recall-only-the-named-entry) pass on Linux.
