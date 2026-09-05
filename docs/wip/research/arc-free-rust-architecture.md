# Building an Arc-free, allocation-disciplined, thread-per-core async server in Rust

Research note for slates. Date: 2026-09-04. Status: complete draft, written serially after the
subagent research was stopped; it consolidates evidence already on file in the sibling notes
(`low-latency-ipc-and-runtime.md`, `database-design.md`, `survey-vorpal.md`, `survey-hecate.md`)
plus the fetches listed in §6.

Evidence tiers: [A] peer-reviewed paper or thesis; [B] standard, Rust Reference/Nomicon, official
vendor documentation; [C] deployed implementation source or design document; [D] blog or
individual measurement, flagged.

## 1. Questions answered

1. What does `Arc` cost, concretely, at slates' latency targets?
2. Which ownership patterns replace it, and what is the written policy with its allowed exceptions?
3. How is an executor built without `Arc`, and how big is it compared with adopting a crate?
4. Which I/O driver per OS, with which fallbacks and container caveats?
5. How do the FUSE, NFS, and WinFsp bridges bind to the core without `Arc`?
6. What is the unsafe policy and which verification tools become CI gates?
7. Which cross-platform facts constrain the design (i686, Windows ARM64, musl, macOS 16K pages, minimum OS versions, stable-only Rust)?

## 2. Findings by sub-question

### 2.1 The cost of Arc, quantified

- An `Arc` clone or drop is an atomic read-modify-write on a shared counter. Uncontended, an atomic RMW costs roughly the same as a store plus a barrier: about 5-10 ns more than a plain access on the E/M-state line, CAS ≈ FAA ≈ SWP [A: Schweizer et al. PACT'15 Table 2]. Contended across cores, the counter's cache line ping-pongs: 109 cycles (~51 ns) same-die, 289-400 cycles across QPI hops on Westmere-EX; a store to a line shared by all cores 445 cycles [A: David et al. SOSP'13 Tables 2-3]; 22-28 ns same-socket and 58-100+ ns cross-socket on Nehalem [A: Molka et al. PACT'09]; 40-53 ns within/across P-clusters and 145 ns across the P/E boundary on Apple M1 Pro [D: nviennot]. Memory ordering: on x86 acquire/release loads and stores are plain instructions and only `SeqCst` stores need a fence; on ARM acquire/release use LDAR/STLR and `SeqCst` adds DMB barriers, so ordering costs more on ARM (the measured fence in Hart et al. was 76-78 ns on 2006 POWER) [B: Rust Reference/Nomicon atomics; A: Hart et al. 2007]. vorpal's own ledger found four global atomics "doubled kernel-scale user CPU purely on cache-line ping-pong" [C: survey-vorpal.md §2.3].
- Conclusion: a single uncontended atomic (~5-10 ns) is not measurable against a 50 µs budget; a *shared* counter touched by several cores per request costs 50-400 ns per touch and serializes cores, which is measurable and, at millions of operations per second, the scaling wall vorpal's architecture document describes [C: survey-vorpal.md §2.0]. The rule "no `Arc`" therefore targets the hot path exactly where it hurts, and the allowed exceptions are the cold, one-per-lifetime cases.

### 2.2 Ownership patterns that replace Arc

- (a) Thread-per-core shared-nothing: each shard owns its data; other shards send `Copy` messages over bounded SPSC/MPSC rings (Seastar; monoio/glommio; hecate RUNTIME §1); within a shard, plain references and `Rc` (no atomics) are legal [C: Seastar docs; C: survey-hecate.md §3.8].
- (b) Arenas plus generational handles: a handle is (index, generation) into an owner-managed slab; a stale handle is a typed miss, never a dangling pointer (slotmap's design; ECS practice; hecate's memory doctrine; vorpal §7.2 "handles, not pointers") [C: slotmap docs; C: survey-hecate.md §3.8; C: survey-vorpal.md §2.0].
- (c) Reclamation for read-mostly shared structures: crossbeam-epoch's `Collector` is an `Arc<Global>` with a `SeqCst` fence per pin; seize (Hyaline) has no `Arc`, reference-counts retired batches, and pays a `SeqCst` fence per retire batch; haphazard has no `Arc` [C: crate sources, read in database-design.md §2.3]. A per-core QSBR keyed on the executor loop generation has zero fences on the read path [A: Hart et al. 2007 §3.1] and is the chosen scheme for the few cross-shard tables.
- (d) `Pin` plus intrusive lists for zero-allocation wait queues (intrusive-collections; tokio's intrusive waiter lists) [C: intrusive-collections docs; C: tokio source].
- (e) Process-lifetime singletons via `static`/`Box::leak` initialized once through `OnceLock`: one atomic at initialization, none afterwards; acceptable [C: survey-vorpal.md §2.1 rows marked Y].
- (f) `Rc` within one shard is a non-atomic count; acceptable where a handle would be awkward (bindings objects), but handles are preferred inside the core because they are `Copy`, checkable, and serializable.
- Std traps and their escapes: `Waker::from(Arc<W>)` is not the only constructor: `RawWakerVTable` lets the data pointer be anything, and thread safety is required only when constructing a `Waker` (not a `LocalWaker`) [B: std::task::RawWakerVTable docs, fetched 2026-09-04]; `std::thread::scope` lets worker threads borrow `&` state without `Arc` [B: std::thread::scope docs]; FFI callbacks that demand `'static + Send` state get a `static` registry keyed by handle, not an `Arc`.

Written policy (adopted in the design): `Arc` and `Rc` are denied by lint workspace-wide. Allowed exceptions, each with a comment naming the owners: (1) bindings objects whose host garbage collector may drop them mid-call (PyO3/napi objects) may hold an `Rc`/`Arc` to the client session; (2) a foreign API that takes `Arc` by signature (none is expected in the core; vorpal's `russh`/`wgpu` cases are the pattern); (3) test harnesses. Everything else uses shards, handles, moves over bounded channels, epoch-published immutable roots, `&'static` singletons, and intrusive lists.

### 2.3 Executor design without Arc

- Task storage: a per-shard fixed-capacity arena of task slots; each slot holds the pinned future (boxed into the arena, never reallocated), a state word, and a generation; the run queue is an intrusive list through the slots; timers are a hierarchical timing wheel [A: Varghese & Lauck SOSP'87].
- Wakers: `RawWaker` data pointer = packed (shard id, slot index, generation); `wake_by_ref` on the owning shard pushes the slot; from another shard it enqueues (slot, generation) on the target's bounded MPSC ring and kicks the driver; `clone` and `drop` are no-ops (the encoding is `Copy`), which satisfies the vtable contract without any reference count; a stale generation is ignored. This is embassy-executor's model on std [C: embassy-executor src/raw; B: RawWakerVTable docs].
- Cross-shard wakeups: eventfd (Linux, also usable as an io_uring registered eventfd), `EVFILT_USER` (kqueue), `PostQueuedCompletionStatus` (IOCP) [B: eventfd(2); B: kqueue(2); B: Microsoft Learn PostQueuedCompletionStatus].
- Cancellation and structured concurrency: futures are cancelled by drop; every operation that owns a resource is cancel-safe by construction (the resource is in the arena, not in the future); child tasks are tracked by their parent's slot and are cancelled and joined on parent completion; no untracked tasks (hecate's task-lifecycle law).
- Size estimate (from the sibling note): executor + timers ~2 kLOC, three drivers ~1.5 kLOC each, simulation driver ~1 kLOC; total 6-7 kLOC plus tests. Crate comparison: monoio has io_uring/epoll/kqueue and experimental Windows [C: monoio README]; compio is thread-per-core with IOCP/io_uring/polling and no stated maturity [C: compio README]; both would need audit for `Arc` on the request path and driver seams for FUSE-over-io_uring and our rings; tokio is excluded by design.

### 2.4 I/O drivers per OS

- Linux: io_uring with registered files and buffers, `IORING_SETUP_SINGLE_ISSUER` + `DEFER_TASKRUN` (6.1+) for low jitter, `COOP_TASKRUN` (5.19+), multishot accept/recv where available, FUSE-over-io_uring on 6.14+ [B: io_uring_setup(2); B: fuse-io-uring.rst]; epoll fallback is first-class because io_uring is blocked by Docker's default seccomp profile since 2023 and by some hardened kernels, and SQPOLL is avoided (30 µs idle-thread wake) [C: Docker seccomp default profile; D: io_uring for DBMSs 2025]. Syscall surface via `rustix` (Linux/macOS) and raw `libc` where rustix lacks a call; musl is supported by both.
- macOS: kqueue with `EVFILT_READ/WRITE/USER/TIMER`; no completion I/O, so the NFS server's sockets are readiness-driven with non-blocking reads into arena buffers [B: kqueue(2)].
- Windows: IOCP with overlapped I/O for the control pipe and loopback sockets; WinFsp dispatches file-system requests on its own threads, so the binding hands each request to the owning shard by handle through the shard's ring and completes it from the shard (WinFsp permits asynchronous completion with `FspFileSystemSendResponse`) [C: WinFsp API reference]. `windows-sys` for the API surface.

### 2.5 FFI to the bridges

- FUSE: drive `/dev/fuse` directly (fuser does this with threads and `Arc`; fuse-backend-rs adds an experimental io_uring transport); the wire layout is `fuse_in_header`/`fuse_out_header` plus per-opcode structs from `include/uapi/linux/fuse.h`; version negotiation in `FUSE_INIT`; splice for large replies; one channel per shard via `FUSE_DEV_IOC_CLONE` [C: fuse.h; C: fuser source]. Our driver: ~3-5 kLOC.
- NFSv3: ONC RPC record marking over TCP (RFC 5531), XDR (RFC 4506), NFSv3 (RFC 1813) and MOUNT procedures, a minimal portmap responder; nfsserve is the structural precedent (tokio-based) [B: RFCs; C: nfsserve]. Ours: ~4-6 kLOC.
- WinFsp: thin binding over the DLL's C API (~25 callbacks); the `winfsp` crate's callbacks require `Send + Sync` and use `Arc` in places, so we bind `winfsp-sys` directly [C: winfsp-rs]. Ours: ~2-3 kLOC.
- One `Bridge` trait implemented by the core: `lookup`, `getattr`, `setattr`, `readdir(+plus)`, `open`, `read`, `write`, `create`, `mkdir`, `unlink`, `rmdir`, `rename`, `link`, `symlink`, `readlink`, `flush`, `release`, `fsync` (no-op success), `statfs`, `xattr*` (optional), `notify` (invalidation), each taking a request handle and replying by reference into arena memory.

### 2.6 Unsafe policy and verification

- Rules: edition-2024 idiom (`unsafe_op_in_unsafe_fn` denied; every `unsafe` block carries a `// SAFETY:` comment); no `deny(unsafe_code)` because the arenas, rings, and FFI require it; the Rust Reference and Nomicon define validity and aliasing rules; the Unsafe Code Guidelines refine them [B: Rust Reference; B: Nomicon; C: UCG].
- Tools as CI gates: Miri (Stacked Borrows and Tree Borrows) on the `mem`, `rt`, and `wire` crates' unit tests on every change [A: Jung et al. POPL'20; A: Villani et al. PLDI'25]; loom on the ring and handle cores [A: Norris & Demsky OOPSLA'13]; shuttle (PCT) nightly on task schedules [A: Burckhardt et al. ASPLOS'10]; ThreadSanitizer on integration binaries nightly; Kani bounded proofs for the arena and ring invariants [C: Kani]; Verus reserved for the handle-generation proof if the team adopts it [A: Lattuada et al. SOSP'24]; `cargo-careful` in nightly runs.

### 2.7 Cross-platform constraints

- i686 (32-bit `usize`): every `u64 → usize` on sizes uses `usize::try_from`; volume caps derive from the measured address space (2 GB, or 4 GB large-address-aware); `AtomicU64` exists on i686 (x86 has 64-bit atomics via `cmpxchg8b`; the std docs single out only PowerPC/MIPS 32-bit as lacking `AtomicU64`) and `#[cfg(target_has_atomic = "64")]` guards uses [B: std::sync::atomic docs, fetched 2026-09-04]; reserve-then-commit regions are bounded by `ullAvailVirtual` [B: MEMORYSTATUSEX].
- Windows ARM64: WinFsp ships `winfsp-a64.dll` [C: WinFsp License.txt]; napi and PyO3 builds are cross-compiled from x64 runners (vorpal publishes napi win32-arm64, no arm64 Python wheel) [C: survey-vorpal.md §1.6-1.7].
- musl: static binaries with `+crt-static` (cdylibs with `-crt-static`), `getauxval` for `AT_PAGESZ` (musl provides it), no glibc-only `sysconf` cache keys (read sysfs instead), musl's allocator is slow but irrelevant when the hot path never allocates [C: survey-vorpal.md §1.3; B: musl docs].
- macOS: 16 KiB pages on Apple silicon; no superpages; `os_sync_wait_on_address` requires macOS 14.4 (minimum version) [B: os_sync header]; thread affinity is a hint only; QoS classes steer P/E placement.
- Minimum OS versions: Linux kernel 5.10 baseline with feature detection (io_uring modes 5.19/6.1, FUSE io_uring 6.14, MADV_POPULATE 5.14, futex_waitv 5.16); macOS 14.4; Windows 10 1809+ (AF_UNIX not used; `WaitOnAddress` 8+; named pipes and sections everywhere) with WinFsp installed.
- Stable Rust 1.98 only: no `-Z` flags; no `#![feature]`; `LocalWaker` is stable-usable only if stabilized by 1.98 (verify; otherwise use `Waker` with thread-safe no-op vtable functions, which the encoding already satisfies); `std::thread::scope`, `OnceLock`, edition 2024 `unsafe extern` blocks are stable.

## 3. Measured numbers table

| Value | Platform / conditions | Source (tier) |
|---|---|---|
| Atomic RMW ≈ store + barrier; 5-10 ns over plain access; CAS ≈ FAA ≈ SWP | Haswell/Ivy Bridge/Opteron | Schweizer et al. PACT'15 (A) |
| Modified-line load 109 cycles same die, 289/400 across hops; all-core shared store 445 cycles | Xeon E7-8867L | David et al. SOSP'13 (A) |
| Same-socket exclusive line 22.2 ns; cross-socket 58-100+ ns | Xeon X5570 | Molka et al. PACT'09 (A) |
| M1 Pro 40/53/145 ns (within P-cluster / across / P↔E) | M1 Pro | nviennot (D) |
| Fence 76-78 ns; CAS 52-59 ns; lock 231-243 ns | POWER4+/G5, 2006 | Hart et al. (A) |
| Four global atomics doubled kernel-scale user CPU | vorpal ledger | survey-vorpal.md (C) |
| crossbeam-epoch: `Arc<Global>`, SeqCst fence per pin; seize: no `Arc`, fence per 32-retire batch | crate sources | (C) |
| SQPOLL idle wake ~30 µs; DEFER_TASKRUN ping-pong 8-9 µs | AMD 3.7 GHz, Linux 6.15/6.17 | io_uring for DBMSs (D) |

## 4. Recommendation for slates

1. **Ownership policy**: the written policy of §2.2 with its three exceptions; lint-enforced; handles everywhere in the core; QSBR only for cross-shard read-mostly tables.
2. **Executor and drivers**: custom (6-7 kLOC), arena task slots, `RawWaker` encodings, hierarchical timing wheel, io_uring/epoll, kqueue, IOCP, simulation driver; compio as the audited runner-up, monoio once Windows lands, tokio never.
3. **Bridges**: own FUSE driver, own NFSv3 server, thin WinFsp binding, one `Bridge` trait.
4. **Unsafe and verification**: edition-2024 idiom with SAFETY comments; Miri and loom on every change for the unsafe and lock-free crates; shuttle, TSan, Kani nightly; `cargo-careful` nightly.
5. **Cross-platform constraints**: the list in §2.7 becomes Appendix C of the design.

## 5. Risks, unknowns, must-measure

- Apple silicon and Windows atomic/fence costs; the `LocalWaker` stabilization status on 1.98; compio's request-path ownership (audit before any adoption); container io_uring availability (detect and fall back).

## 6. Bibliography

- H. Schweizer, M. Besta, T. Hoefler. PACT 2015; T. David, R. Guerraoui, V. Trigonakis. SOSP 2013; D. Molka et al. PACT 2009; T. Hart et al. IPDPS 2006 / JPDC 2007.
- Rust: std::task::RawWakerVTable (fetched 2026-09-04); std::sync::atomic Portability (fetched 2026-09-04); std::thread::scope; The Rust Reference; The Rustonomicon; Unsafe Code Guidelines.
- monoio README (fetched 2026-09-04); compio README (fetched 2026-09-04); embassy-executor; Seastar; slotmap; intrusive-collections; crossbeam-epoch, seize, haphazard sources.
- io_uring_setup(2); kernel fuse-io-uring.rst; Docker default seccomp profile; kqueue(2); Microsoft Learn IOCP/PostQueuedCompletionStatus; WinFsp API reference and License; fuser; fuse-backend-rs; nfsserve; winfsp-rs; RFC 5531, 4506, 1813.
- R. Jung et al. Stacked Borrows. POPL 2020; N. Villani et al. Tree Borrows. PLDI 2025; B. Norris, B. Demsky. CDSChecker. OOPSLA 2013; S. Burckhardt et al. PCT. ASPLOS 2010; Kani; A. Lattuada et al. Verus. SOSP 2024; cargo-careful.
- Microsoft Learn MEMORYSTATUSEX; musl documentation; Apple os_sync_wait_on_address header.
