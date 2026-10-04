// mantle: the hardware rng's bounded retry (AWS-LC #3475) and the fallback to the operating
// system when every attempt fails (vendor/UPSTREAM.md). A hardware rng cannot be made to fail
// on demand, so a fake one stands in, as in AWS-LC's own entropy_source_test.cc, whose retry
// cases the first test carries over.

use std::cell::Cell;
use std::os::raw::c_int;

type HwRng = extern "C" fn(buf: *mut u8, len: usize) -> c_int;

extern "C" {
    #[link_name = "aws_lc_0_45_0_hw_rng_multiple8_with_retry_FOR_TESTING"]
    fn hw_rng_multiple8_with_retry_for_testing(
        hw_rng: HwRng,
        buf: *mut u8,
        len: usize,
        max_attempts: usize,
    ) -> c_int;

    #[link_name = "aws_lc_0_45_0_hw_rng_or_os_multiple8_FOR_TESTING"]
    fn hw_rng_or_os_multiple8_for_testing(
        hw_rng: HwRng,
        buf: *mut u8,
        len: usize,
        max_attempts: usize,
    );
}

thread_local! {
    /// Leading calls that fail.
    static FAILURES: Cell<usize> = const { Cell::new(0) };
    /// Bytes a failing call writes before failing: RNDR and RDRAND return 8 bytes at a time,
    /// so a failed call can leave a prefix of the buffer written.
    static PREFIX_ON_FAILURE: Cell<usize> = const { Cell::new(0) };
    static CALLS: Cell<usize> = const { Cell::new(0) };
}

/// What a failing call writes; it differs from `SUCCESS_FILL` so a stale prefix shows.
const FAILURE_FILL: u8 = 0xaa;
const SUCCESS_FILL: u8 = 0x55;
const LEN: usize = 16 * 8;
/// A single attempt, and `RNDR_MAX_ATTEMPTS` and `RDRAND_MAX_ATTEMPTS`, both 10.
const ATTEMPTS: [usize; 2] = [1, 10];

extern "C" fn fake(buf: *mut u8, len: usize) -> c_int {
    let calls = CALLS.with(|c| {
        c.set(c.get() + 1);
        c.get()
    });
    // SAFETY: AWS-LC passes the buffer it was given, `len` bytes long.
    let buf = unsafe { std::slice::from_raw_parts_mut(buf, len) };
    if calls <= FAILURES.with(Cell::get) {
        let prefix = PREFIX_ON_FAILURE.with(Cell::get).min(len);
        buf[..prefix].fill(FAILURE_FILL);
        return 0;
    }
    buf.fill(SUCCESS_FILL);
    1
}

fn arm(failures: usize, prefix_on_failure: usize) {
    FAILURES.with(|f| f.set(failures));
    PREFIX_ON_FAILURE.with(|p| p.set(prefix_on_failure));
    CALLS.with(|c| c.set(0));
}

fn calls() -> usize {
    CALLS.with(Cell::get)
}

fn retry(buf: &mut [u8], max_attempts: usize) -> c_int {
    // SAFETY: `buf` is valid for `buf.len()` bytes and `fake` writes only within it.
    unsafe { hw_rng_multiple8_with_retry_for_testing(fake, buf.as_mut_ptr(), buf.len(), max_attempts) }
}

fn retry_or_os(buf: &mut [u8], max_attempts: usize) {
    // SAFETY: as for `retry`; the operating system fills at most `buf.len()` bytes.
    unsafe { hw_rng_or_os_multiple8_for_testing(fake, buf.as_mut_ptr(), buf.len(), max_attempts) }
}

#[test]
fn the_hardware_rng_is_retried_a_bounded_number_of_times() {
    for max in ATTEMPTS {
        // Succeeding on the first call reads the hardware rng once.
        let mut buf = [0u8; LEN];
        arm(0, 0);
        assert_eq!(retry(&mut buf, max), 1);
        assert_eq!(calls(), 1);
        assert!(buf.iter().all(|&b| b == SUCCESS_FILL));

        // Succeeding on the last attempt still succeeds, and writes the whole buffer.
        let mut buf = [0u8; LEN];
        arm(max - 1, LEN);
        assert_eq!(retry(&mut buf, max), 1);
        assert_eq!(calls(), max);
        assert!(buf.iter().all(|&b| b == SUCCESS_FILL));

        // Exhausting the attempts fails after exactly `max` calls, leaving what the last failed
        // call wrote: callers must not consume the buffer.
        let mut buf = [0u8; LEN];
        arm(max + 1, LEN);
        assert_eq!(retry(&mut buf, max), 0);
        assert_eq!(calls(), max);
        assert!(buf.iter().all(|&b| b == FAILURE_FILL));

        // A failed call's prefix is overwritten by the call that succeeds.
        if max > 1 {
            let mut buf = [0u8; LEN];
            arm(1, LEN / 2);
            assert_eq!(retry(&mut buf, max), 1);
            assert!(buf.iter().all(|&b| b == SUCCESS_FILL));
        }

        // A length that is not a positive multiple of 8 is refused without a read.
        for len in [0, 12] {
            let mut buf = vec![0u8; len];
            arm(0, 0);
            assert_eq!(retry(&mut buf, max), 0);
            assert_eq!(calls(), 0);
        }
    }
}

#[test]
fn a_hardware_rng_that_keeps_failing_gives_way_to_the_os() {
    for max in ATTEMPTS {
        // Every attempt fails, each leaving the whole buffer stale: the operating system
        // overwrites all of it, and two fallbacks differ.
        let mut first = [0u8; LEN];
        arm(usize::MAX, LEN);
        retry_or_os(&mut first, max);
        assert_eq!(calls(), max);
        let stale = first.iter().filter(|&&b| b == FAILURE_FILL).count();
        assert!(stale < LEN / 8, "{stale} bytes still hold the failed reads' fill");
        let mut second = [0u8; LEN];
        arm(usize::MAX, LEN);
        retry_or_os(&mut second, max);
        assert_ne!(first, second);

        // A hardware rng that succeeds within its attempts is what fills the buffer.
        let mut buf = [0u8; LEN];
        arm(max - 1, LEN);
        retry_or_os(&mut buf, max);
        assert_eq!(calls(), max);
        assert!(buf.iter().all(|&b| b == SUCCESS_FILL));
    }
}

#[test]
fn random_bytes_come_from_the_default_entropy_source() {
    let mut a = [0u8; 64];
    let mut b = [0u8; 64];
    // SAFETY: each buffer is valid for its length.
    unsafe {
        assert_eq!(aws_lc_sys::RAND_bytes(a.as_mut_ptr(), a.len()), 1);
        assert_eq!(aws_lc_sys::RAND_bytes(b.as_mut_ptr(), b.len()), 1);
    }
    assert_ne!(a, b);
    assert_ne!(a, [0u8; 64]);
}
