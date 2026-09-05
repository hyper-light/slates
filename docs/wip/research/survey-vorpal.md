# Survey: vorpal — engineering conventions slates must match

Surveyed tree: `/Users/adalundhe/Projects/vorpal` (git `main`, workspace version 0.7.1, surveyed 2026-09-03).
All paths below are relative to that tree unless they start with `/`. Quotes are verbatim from
source comments; `file:line` references point at the line where the quoted comment or code begins.

This document is written in plain English and organised by the nine questions the slates
design brief asked. Section 9 is the actionable checklist; sections 1–8 are the evidence.

---

## 0. Orientation: what vorpal is, and how the workspace is shaped

vorpal began as a fork of ast-grep (the workspace `authors` field still names ast-grep's author,
`Cargo.toml:14`; the CI comment at `.github/workflows/ci.yml:24-28` talks about "inherited ast-grep
code") and has grown into a cross-platform code-search engine: a streaming ingest pipeline, a
knowledge graph persisted as immutable mmap'd segments, an ANN tier, an MCP daemon, an LSP, a
remote "fleet" protocol, and Node/Python/WASM bindings.

Workspace facts (`Cargo.toml`):

| Key | Value | Where |
|---|---|---|
| members | `crates/*` + `xtask`; `default-members = ["crates/*"]` | `Cargo.toml:2-6` |
| resolver | `"2"` | `Cargo.toml:7` |
| edition | `2024` | `Cargo.toml:15` |
| rust-version (MSRV) | `1.98` | `Cargo.toml:20` |
| license | MIT | `Cargo.toml:16` |
| release profile | `lto = true` only (no `codegen-units`, `panic`, `opt-level`, `strip` overrides) | `Cargo.toml:9-10` |
| lints | `[workspace.lints.clippy]` with three `allow`s; every crate opts in with `[lints] workspace = true` | `Cargo.toml:134-137` |
| version scheme | one workspace version; every internal dep is `{ path = "crates/x", version = "0.7.1" }` | `Cargo.toml:24-44` |

Crate naming: every library is `vorpal-<role>` living in `crates/<role>/`; the CLI crate is
`crates/cli` but its package name is plain `vorpal` (`crates/cli/Cargo.toml:2`) so the binary is
`vorpal`. Bindings crates are `vorpal-napi` (`crates/napi`), `vorpal-py` (`crates/pyo3`), and the
wasm crate is literally named `wasm` (`crates/wasm/Cargo.toml:2`); all three carry `publish = false`.

The 25 crates and their stated roles (from each `Cargo.toml` `description`):

| Crate | Package | Role (verbatim description) |
|---|---|---|
| `crates/core` | `vorpal-core` | ast-grep core: patterns, matchers, replacers ("Search and Rewrite code at large scale using precise AST pattern") |
| `crates/config` | `vorpal-config` | rule/config YAML schema (same inherited description) |
| `crates/language` | `vorpal-language` | grammar bindings; `builtin-parser` feature lists ~47 tree-sitter grammars (`crates/language/Cargo.toml:71-120`) |
| `crates/lang-registry` | `vorpal-lang-registry` | "The runtime language universe for vorpal: builtin + dynamic grammars, globs, injections, and grammar identity" |
| `crates/dynamic` | `vorpal-dynamic` | "Load tree-sitter dynamic library for vorpal" (libloading) |
| `crates/outline` | `vorpal-outline` | "Code outline extraction primitives for vorpal" |
| `crates/mem` | `vorpal-mem` | "Adaptive memory substrate (huge pages, TLB-aware layout, prefetch, arenas) for vorpal" |
| `crates/segment` | `vorpal-segment` | "Immutable columnar `.vseg` segment container + dense-id segment directory for vorpal" |
| `crates/canonical` | `vorpal-canonical` | "Canonical blake3→NodeId identity/dedup/skip index (LSM-shaped) for vorpal" |
| `crates/graph` | `vorpal-graph` | "Edge graph LSM (delta-log ⋈ CSR/CSC) with compaction-time locality relabel for vorpal" |
| `crates/kg` | `vorpal-kg` | "Knowledge-graph assembly (L1→L3): outline extraction → interned nodes + graph edges" |
| `crates/resolve` | `vorpal-resolve` | "Cross-file reference resolution (§3.3): references → definitions with confidence" |
| `crates/ingest` | `vorpal-ingest` | "Streaming bounded-memory ingest pipeline (walk → parse → extract → KG) for vorpal" |
| `crates/ann` | `vorpal-ann` | "Adaptive approximate-nearest-neighbor tier (flat / quantized / Vamana + exact rerank) for vorpal" |
| `crates/index` | `vorpal-index` | "CLI over the vorpal ingest → resolve → persist → query pipeline" (library + `vorpal-index` binary) |
| `crates/query` | `vorpal-query` | "Cypher-shaped read-only query language over the vorpal knowledge graph" |
| `crates/mcp` | `vorpal-mcp` | "Warm-index MCP daemon: vorpal knowledge-graph queries served to agents over stdio" |
| `crates/lsp` | `vorpal-lsp` | LSP server (tower-lsp-server) |
| `crates/wire` | `vorpal-wire` | "Vorpal fleet wire protocol: framing, messages, and version-stable hashing" |
| `crates/transport` | `vorpal-transport` | "Vorpal fleet transports: exec-shaped async connections to remote nodes (subprocess, SSH, …)" |
| `crates/loader` | `vorpal-loader` | stage-0 loader: "a tiny library (framing + Ed25519/blake3 sign+verify …) and the pushed binary (verify + zero-residue exec on the node)" (`crates/loader/Cargo.toml:9-10`) |
| `crates/cli` | `vorpal` | the shipped CLI binary; hosts MCP/LSP/remote entry points |
| `crates/napi` | `vorpal-napi` | Node bindings (napi-rs v3, cdylib) |
| `crates/pyo3` | `vorpal-py` | Python bindings (pyo3 0.29, abi3-py39, cdylib) |
| `crates/wasm` | `wasm` | wasm-bindgen bindings (cdylib + rlib) |
| `xtask` | `xtask` | release/schema/eval tasks (`cargo xtask …`, alias in `.cargo/config.toml:17-18`) |

Dependency-policy patterns visible in the manifests (worth copying):

- Shared third-party versions are pinned once in `[workspace.dependencies]` (`Cargo.toml:46-62`) and
  pulled with `foo.workspace = true`. Crate-local deps that only one crate needs are declared locally.
- Exact pins (`=x.y.z`) are used only where an ABI/contract is version-specific, and each pin carries
  a paragraph explaining why, e.g. `wgpu = "=30.0.0"` (`crates/ann/Cargo.toml:20-28`): "Pinned exactly:
  the shader/pipeline contract is version-specific and re-verified by the gated parity oracle."
- Optional heavy deps are behind named features that are documented as "NEVER a default", e.g.
  `bench-internals` (`crates/index/Cargo.toml:58-61`): "Benchmark-only internal seams … NEVER a default:
  production builds carry no measurement entry points".
- Vendored crates are wired through `[patch.crates-io]` with rationale comments (see §3.1).

---

## 1. Target/OS matrix and build recipes

### 1.1 Toolchain pin (copy verbatim)

`rust-toolchain.toml:1-7`:

```toml
[toolchain]
# Exact version, deliberately (not "stable"): the channel name resolves differently on
# every machine — CI's runner and a dev laptop can be four stables apart, which made the
# clippy -D warnings gate unreproducible. Bump this file (and the matching pre-install
# pins in .github/workflows/) in one commit, after a local `cargo clippy` pass.
channel = "1.98.0"
components = ["rustfmt", "clippy"]
```

The same `1.98.0` is repeated in every workflow step with the comment `# keep in sync with
rust-toolchain.toml` (`ci.yml:21,52`, `release.yml:54,79,149`, `publish-node.yml:41,87,139`,
`publish-python.yml:43,60`), and the alpine containers use `rust:1.98-alpine` (`release.yml:96,169`)
or `rustup … --default-toolchain 1.98.0` (`publish-node.yml:87`). MSRV bump history is recorded in
`Cargo.toml:130-133`: "MSRV moved 1.85 → 1.98 at v0.7.1 (the tree already used 1.88 let-chains)".

Formatting/editor: `rustfmt.toml` is one line, `tab_spaces = 2`; `.editorconfig` sets 2-space
indent, LF, UTF-8, trailing-whitespace trim (except `*.md`), `insert_final_newline = false`, and
4-space indent for `*.{py,pyi}`. `clippy.toml` is one line: `ignore-interior-mutability =
["fluent_uri::Uri"]`. `.pre-commit-config.yaml` runs `doublify/pre-commit-rust` v1.0 hooks `fmt`,
`cargo-check`, and `clippy --all-targets --all-features -- -D clippy::all`.

### 1.2 CI gate (`.github/workflows/ci.yml`)

One `ubuntu-latest` job "fmt + clippy + test": `dtolnay/rust-toolchain@stable` with
`toolchain: 1.98.0`, `Swatinem/rust-cache@v2`, then

```
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy -p vorpal-py --features python --all-targets -- -D warnings
cargo test --workspace
```

The rationale for **no `cargo fmt --check`** is stated at `ci.yml:24-28`:

> No `cargo fmt --check` gate, deliberately: the tree predates rustfmt conformance (inherited
> ast-grep code plus repo idiom), and a whole-repo reformat would poison blame and permanently
> diverge every inherited file from upstream. The gate failed every push since the workflow
> landed, which also meant Clippy and Test NEVER ran — a red-forever check is worse than no
> check. Clippy at -D warnings is the lint gate.

(For slates, which has no inherited code, a fmt gate is appropriate; the lesson to keep is "a
red-forever check is worse than no check".) The python-bindings clippy pass exists because
"vorpal-py's bindings are behind the off-by-default `python` feature, so the workspace pass never
compiles them — lint them the way the wheel build does" (`ci.yml:31-32`).

A second job `encoder-x86` exists because the dev machine is Apple silicon (`ci.yml:38-44`): "The
encoder's x86 GEMM rungs … can only be MEASURED on x86 hardware … This job is the x86 datum: it
prints the runner's ISA, runs every present kernel against the exact reference … and records the
GEMM rate". Pattern: CI as the measurement datum for ISAs the laptop lacks; weights-gated
(547 MB download) steps run only on `workflow_dispatch`.

### 1.3 Release matrix (`.github/workflows/release.yml`) — copy this shape

Header comment (`release.yml:1-16`) states the asset contract:

> CLI release: build the `vorpal` binary for every supported platform and attach each one to the
> GitHub Release DIRECTLY — one raw binary per platform, named `vorpal-<os>-<arch>` (Windows:
> `vorpal-windows-<arch>.exe`), no archive to unpack. cargo-binstall's per-target overrides in
> crates/cli/Cargo.toml expect exactly these names. … Platform coverage: aarch64/arm64 +
> x86_64/amd64 across macOS, Linux glibc (Debian, Fedora, Ubuntu, Arch, …), Linux musl (Alpine),
> and Windows (plus 32-bit Windows).

Triggers: `push: tags: ['v*']` releases; `workflow_dispatch` = build-only dry run. Jobs: `guard`
(tag must equal workspace version: `release.yml:40-45`), `test` (`cargo test --workspace`), `build`
(matrix), `agent-bundle`, `release` (`softprops/action-gh-release@v2`, `generate_release_notes:
true`), `publish-cli-npm`.

The build matrix (`release.yml:64-74`), which is **vorpal's target matrix** and therefore slates':

| target | runner | asset name | notes |
|---|---|---|---|
| `aarch64-apple-darwin` | `macos-latest` | `vorpal-macos-arm64` | native |
| `x86_64-apple-darwin` | `macos-latest` | `vorpal-macos-x64` | cross from arm64 runner via `--target`; per-arch, **not** universal |
| `x86_64-unknown-linux-gnu` | `ubuntu-latest` | `vorpal-linux-x64` | native |
| `aarch64-unknown-linux-gnu` | `ubuntu-24.04-arm` | `vorpal-linux-arm64` | native arm64 runner, no cross toolchain |
| `x86_64-unknown-linux-musl` | `ubuntu-latest` | `vorpal-linux-x64-musl` | `musl: true` → alpine docker, fully static |
| `aarch64-unknown-linux-musl` | `ubuntu-24.04-arm` | `vorpal-linux-arm64-musl` | `musl: true` → alpine docker, fully static |
| `x86_64-pc-windows-msvc` | `windows-latest` | `vorpal-windows-x64.exe` | native |
| `aarch64-pc-windows-msvc` | `windows-latest` | `vorpal-windows-arm64.exe` | cross via `--target` on x64 runner |
| `i686-pc-windows-msvc` | `windows-latest` | `vorpal-windows-x86.exe` | 32-bit, cross via `--target` |

Non-musl build step is just `cargo build --release -p vorpal --target ${{ matrix.target }}`.

**musl recipe** (`release.yml:24-32` and `87-114`), quoted because it took several dead ends:

> Musl lanes build inside official rust:alpine containers, NATIVE per arch (x86_64 on
> ubuntu-latest, aarch64 on ubuntu-24.04-arm) — the same proven pattern as publish-node.yml's
> build-musl. Alpine IS musl: apk's build-base ships a native musl gcc AND g++ (tree-sitter-vue's
> C++ scanner needs the latter; Debian's musl-tools has no C++ compiler, and musl.cc is
> unreachable from GitHub runners — both tried, both dead ends). Static libstdc++/libgcc come
> from the same apk toolchain, and each build proves the result with an `ldd` staticness gate.

```yaml
docker run --rm -v "$PWD:/w" -w /w \
  -e CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=cc \
  -e CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=cc \
  rust:1.98-alpine sh -c '
  set -e
  apk add --no-cache build-base
  export RUSTFLAGS="-C target-feature=+crt-static"
  cargo build --release -p vorpal --target ${{ matrix.target }}
'
sudo chown -R "$(id -u):$(id -g)" target   # container wrote target/ as root
out=$(ldd "target/${{ matrix.target }}/release/vorpal" 2>&1 || true)
case "$out" in
  *"not a dynamic executable"*|*"statically linked"*) ;;
  *) echo "musl binary is not fully static" >&2; exit 1 ;;
esac
```

with the accompanying comment (`release.yml:105-108`): "Two static shapes exist: classic static
("not a dynamic executable") and static-PIE ("statically linked") — rustc builds x86_64-musl as
static-PIE since 1.85 while aarch64-musl stays classic. Anything else is a defect." The `LINKER`
env vars exist because `.cargo/config.toml` names cross linkers (`aarch64-linux-musl-gcc`) that
do not exist in alpine: "Env outranks config" (`publish-python.yml:16-19`, `release.yml:90-92`).

Note the tension between `.cargo/config.toml:7-15` (musl targets get `-C target-feature=-crt-static`
— for the **cdylib** napi/pyo3 builds, which cannot be `+crt-static`) and the release workflow,
which re-enables `+crt-static` via `RUSTFLAGS` for the **binary**. Both are deliberate; the
comment at `publish-node.yml:60-66` explains the cdylib side: "+crt-static is rejected for a cdylib".

Windows staging uses `shell: pwsh` + `Copy-Item` (`release.yml:118-121`); Unix uses `cp`.

`cargo-binstall` metadata mirrors the asset names one override per target with "unsupported
targets fail instead of guessing" (`crates/cli/Cargo.toml:78-103`): `pkg-fmt = "bin"`,
`bin-dir = "{ bin }{ binary-ext }"`, `disabled-strategies = ["quick-install"]`, and e.g.
`pkg-url = "{ repo }/releases/download/v{ version }/vorpal-macos-arm64"`.

Supply-chain extras: `cargo xtask release-artifacts <dist-dir>` (`xtask/src/main.rs:79-160`) writes
`SHA256SUMS`, `provenance.json` (`"format": "vorpal-provenance/1"`, git commit, rustc version,
per-file sha256 + blake3 + size), and — when `VORPAL_SIGN_KEY_HEX` is set ("a CI secret, never a
file") — an ed25519 `SHA256SUMS.sig`. The `agent-bundle` job builds a static-musl stage-0 loader
plus a signed agent binary "execs it from a memfd (zero on-disk residue)" (`release.yml:128-132`).

### 1.4 `.cargo/config.toml` (verbatim)

```toml
[target.aarch64-unknown-linux-gnu]
linker = "aarch64-linux-gnu-gcc"

[target.armv7-unknown-linux-gnueabihf]
linker = "arm-linux-gnueabihf-gcc"

[target.x86_64-unknown-linux-musl]
rustflags = ["-C", "target-feature=-crt-static"]

[target.aarch64-unknown-linux-musl]
linker = "aarch64-linux-musl-gcc"
rustflags = ["-C", "target-feature=-crt-static"]

[alias]
xtask = "run --manifest-path ./xtask/Cargo.toml --"
```

(`armv7` is a leftover from ast-grep; nothing in the release matrix targets it.)

### 1.5 npm CLI wrapper (`npm/`) — exact recipe

`npm/package.json`: `@hyper-light/vorpal-cli`, `engines.node >= 12.0.0`, `files: ["vorpal",
"postinstall.js"]`, one real dependency `detect-libc@2.1.2`, `scripts.postinstall = node
postinstall.js`, `bin.vorpal = vorpal`, and eight `optionalDependencies` all at the same version:

```
@hyper-light/vorpal-cli-win32-arm64-msvc, -win32-ia32-msvc, -win32-x64-msvc,
@hyper-light/vorpal-cli-darwin-arm64,
@hyper-light/vorpal-cli-linux-arm64-gnu, -linux-x64-gnu, -linux-arm64-musl, -linux-x64-musl
```

Each `npm/platforms/<plat>/package.json` declares `os`, `cpu`, and (Linux only) `libc: ["glibc"]`
or `["musl"]`, `files: ["vorpal"]` (or `["vorpal.exe"]`), `engines.node >= 10`, `publishConfig
{ registry, access: public }`. Naming follows napi-rs conventions: `darwin-arm64`,
`linux-x64-gnu`, `linux-arm64-musl`, `win32-ia32-msvc`, etc.

`npm/postinstall.js` (`:6-27`) maps `process.platform`/`process.arch` (+ `detect-libc` `familySync()
=== MUSL`) to the package name, resolves it with `require.resolve(pkg/package.json, { paths:
[__dirname] })`, falls back to `../target/{release,debug}` for local dev, then **hard-links** the
binary into place (`fs.linkSync`, falling back to `copyFileSync`, `:63-72`). Intel macOS is
deliberately not published: "Intel macOS (x64) is not published; build from source" (`:11`).
`npm/vorpal` (`:3-5`) is a JS shim kept because "On Windows, npm-generated global wrappers call
this JS bin target through node, so it must stay in place and spawn the `.exe`"; on Unix the
postinstall replaces it with the native binary. The shim forwards SIGINT/SIGTERM/SIGHUP to the
child and re-raises the child's signal on exit (`npm/vorpal:33-48`).

The `publish-cli-npm` job (`release.yml:234-281`) copies each release asset into its platform dir
(`for pair in vorpal-macos-arm64=darwin-arm64 …`), `chmod +x`, then `npm publish --access public`
in every `npm/platforms/*/` and finally in `npm/`. **No NPM_TOKEN**: npm trusted publishing (OIDC)
with `permissions: id-token: write`; the bootstrap caveat is documented at `release.yml:10-13`:
"each package was created once by a local maintainer stub publish (npm/cli#8544: a trusted
publisher can only be attached to an existing package), then registered on npmjs.com against this
repo + release.yml." Node 24 + `npm install -g npm@latest` ("Ensure npm supports trusted
publishing (>= 11.5.1)").

### 1.6 Node bindings (`.github/workflows/publish-node.yml`)

Matrix (`:30-36`): `aarch64-apple-darwin` (macos-latest), `x86_64-unknown-linux-gnu`,
`aarch64-unknown-linux-gnu` (ubuntu-24.04-arm), `x86_64-pc-windows-msvc`,
`aarch64-pc-windows-msvc`, `i686-pc-windows-msvc` (all windows-latest). Note: **no x86_64 macOS
napi package**. Build: `npm install -g @napi-rs/cli@3.7.2` then in `crates/napi`:

```
napi build --no-const-enum --dts ignore.d.ts --platform --release --target <triple>
```

musl `.node` builds run inside `node:24-alpine` via `docker run` (not `container:`) with the reason
at `:60-66`: "Alpine is musl, so its native toolchain ships a musl libgcc and the dynamic cdylib
links cleanly — Debian's musl-tools only carries glibc's libgcc (wrong ABI) … and +crt-static is
rejected for a cdylib. The container is launched via `docker run`, NOT `container:`, so
checkout/upload stay on the host and never trip GitHub's 'JS actions in Alpine containers are
x64-only' rule on the arm64 runner." Publish: `napi artifacts` then a single `npm publish
--access public` in `crates/napi` (napi's prepublish publishes platform packages first). A separate
`wasm` job runs `wasm-pack build crates/wasm --release --scope hyper-light`, renames the package
to `@hyper-light/vorpal-wasm` with an inline `node -e` script, and publishes from `crates/wasm/pkg`.

### 1.7 Python wheels (`.github/workflows/publish-python.yml`)

Header (`:1-3`): "build vorpal-py wheels (abi3, one wheel per platform covering CPython >= 3.9)
for manylinux, musllinux (Alpine), macOS, and Windows, plus an sdist, and publish to PyPI."
Matrix (`:29-35`): `linux-x86_64` (manylinux auto), `linux-aarch64` (ubuntu-24.04-arm, **no
explicit target**: "native build: an explicit target makes maturin demand a cross linker the
image lacks"), `musllinux-x86_64` + `musllinux-aarch64` (`manylinux: musllinux_1_2`),
`macos-x86_64` + `macos-aarch64` (both on macos-latest via `target:`), `windows-x64`. **No
Windows arm64/i686 wheels.** Step: `PyO3/maturin-action@v1` with `rust-toolchain: 1.98.0`,
`args: --release --out dist -m crates/pyo3/Cargo.toml`; `sdist` job with `command: sdist`;
publish via `pypa/gh-action-pypi-publish@release/v1` with `password: ${{ secrets.PYPI_API_TOKEN
}}` and `skip-existing: true` ("a re-run must not 400 on duplicates"). Unlike npm, PyPI here still
uses a token secret rather than OIDC trusted publishing.

There are **two** pyproject files: the root `pyproject.toml` packages the CLI binary itself as a
PyPI package (`[tool.maturin] bindings = "bin"`, `manifest-path = "crates/cli/Cargo.toml"`,
`strip = true`; `pyproject.toml:40-43`), and `crates/pyo3/pyproject.toml` packages the pyo3
extension (see §7). `uv.lock` pins `requires-python = ">=3.14"` for the dev venv only.

### 1.8 Scheduled supply-chain audit (`.github/workflows/grammar-audit.yml`)

Weekly cron (`"17 6 * * 1"`) + `workflow_dispatch`; runs `python3 scripts/grammar_audit.py --report
audit.md` and on failure opens/updates one tracking issue with `gh`. Header comment (`:1-13`)
describes the two-layer discipline: every push runs offline provenance/corpus tests
(`grammar_provenance`, `grammar_corpus` over `grammars/PROVENANCE.json`); weekly re-verifies that
"the pinned commits still exist upstream and that our vendored parser sources still byte-match
them … Drift opens/updates a single tracking issue; it never edits the tree." For slates the
analogue is vendored allocator / runtime sources (see §3).


---

## 2. ARC POLICY — shared-state audit (Arc / Rc / Mutex / RwLock / atomics / OnceLock / statics)

### 2.0 The written policy

vorpal has an explicit, written, non-negotiable rule, at `docs/wip/ARCHITECTURE.md:288-297`:

> Non-negotiable: **ingest and search run fully in parallel, and no `Arc` refcount is touched
> on any hot path** (per-element, per-node, per-edge, per-query-data). `Arc<T>` clone/drop are
> atomic RMWs on a *shared* counter; when N cores clone/drop the same `Arc` the counter's cache
> line ping-pongs (each RMW needs the line in MESI Modified/Exclusive), serializing work that
> should scale linearly and creating a false-sharing hotspot. At 10⁹ LOC with a long-lived
> daemon, that contention is the scaling wall. We remove `Arc` from hot paths by design, not by
> tuning.

with its honest scope (`:298-302`):

> **Honest scope.** `Arc` is unavoidable *inside* `tokio` task allocation and *inside* channel
> handles (`crossbeam-channel`/`flume` `Sender` clones bump an internal count). Those are
> constant, off the per-item/per-node path. "Arc-free hot paths" = zero `Arc` on the data a
> query traverses or an ingest worker produces — which we achieve completely.

§7.1 (`:303-312`) is a table "Where `Arc` sneaks in — and the replacement": `Arc<DashMap>` +
`Arc` values → `papaya`/`scc::HashIndex` with values by index; `Arc<Node>` pointer graphs →
generational-index handles + CSR; `Arc<AppState>` per tokio task → `&'static Index` singleton
(`OnceLock`/`Box::leak`), tasks borrow; `Arc` into `tokio::spawn` → `rayon::scope` /
`std::thread::scope` so workers borrow `&` config; `Arc<Mutex<T>>` → single-writer-per-shard +
RCU/`left-right`/seqlock for read-mostly; `Arc` around parsed trees → per-worker `bumpalo`
arena. §7.2 "Ownership without refcounts" (`:314-329`): "Handles, not pointers. `NodeId/EdgeId/
SegmentId/ChunkId` are `Copy` 8-byte values … Cross-references … store *handles*, never
`Arc`/`&`. A stale handle to a reused slot fails the generation check → `None`, which turns
logical use-after-free / ABA into a safe miss." §7.3 picks a two-tier reclamation scheme (coarse
version pinning for mmap readers; `seize`/Hyaline for fine-grained lock-free maps) and rejects
plain `crossbeam-epoch` ("unbounded limbo under a stalled pinner") and global hazard pointers
("per-pointer advertise + fence on *every* edge chase wrecks traversal throughput"). §7.4 lists
wait-free read patterns (RCU `AtomicPtr` swap, `left-right`, seqlock for POD). §7.9
(`:546-562`) is the pitfalls list: `CachePadded` every hot atomic; stalled pinners; guards
across `.await`; "rayon×tokio — never block a tokio worker on rayon; bridge via `flume`/oneshot";
and the benchmark that proves Arc-avoidance: "a **read-throughput-vs-cores scalability curve**
(target near-linear) measured against an `Arc<DashMap>` baseline".

Two more standing rules bear on shared state. `docs/wip/SUBSECOND.md:25-50` "Design rules
(standing)": rule 2 "Cache-line pads via `crossbeam_utils::CachePadded` (already per-arch aware:
128B on aarch64, 64B on x86)"; rule 3 "**Hardware/data-derived parameters, never constants tuned
to a benchmark.** Constants become policies". And the "no-panics law" cited in
`crates/core/src/meta_var.rs:121`: "Locks recover from poisoning instead of panicking (no-panics
law)" — the idiom everywhere is `.lock().unwrap_or_else(|p| p.into_inner())`
(`crates/index/src/lib.rs:2855`, `crates/mcp/src/server.rs:19-20`, `crates/mcp/src/watch.rs:73`).

Where the doctrine is *not* yet realized, the code says so: `crates/config/src/rule/
referent_rule.rs:90` (`// these are shit code`, inherited ast-grep rule registry built on
`Arc<HashMap>` + `Weak`), and ARCHITECTURE §7.1's first row names the inherited LSP model
(`Arc<DashMap>` + `Arc<String>` values, `crates/lsp/src/lib.rs:28-47`) as the anti-pattern. Both
live outside the index/search hot path and are kept for upstream compatibility (`docs/wip/
UPSTREAM.md:6-9`: "Compatibility with upstream patterns, rules, and CLI behavior is a maintained
contract").

Raw counts (grep over `crates/` + `xtask/`, `*.rs`, scratch file `arc_grep.txt`, 458 lines):
`Arc<` 97, `Arc::` 87, `Rc<` 2, `Mutex` 79, `RwLock` 17, `AtomicU64` 56, `AtomicUsize` 22,
`AtomicBool` 17, `AtomicU32` 2, `OnceLock` 79, `LazyLock` 4, `thread_local!` 5, `static ` 107.
Roughly a third of the `Arc` lines are tests (`tests/` dirs, `#[cfg(test)]` modules, russh test
servers) and four "hits" are C/Rust *source text inside test fixtures* (`crates/outline/src/
default_rule.rs:373`, `crates/ingest/src/tree_cache.rs:373`, `crates/ingest/src/walk_reuse.rs:
807,817`, `crates/outline/tests/*`). Crates with **zero** `Arc` in production code: `mem` (only
`Arc<MappedStore>` as an *input* type in `pod.rs`), `segment`, `canonical`, `graph` (one
`Arc<MappedStore>` parameter), `resolve`, `query`, `outline`, `language`, `lang-registry`,
`dynamic`, `loader`, `wire`, `core` (apart from the interned-name statics), `xtask`.

### 2.1 Every production `Arc` site

Legend for "avoidable?": **N** = load-bearing (two `'static` owners, or forced by a dependency's
API); **C** = cold-path convenience (one clone per build/session/node, never per item) that could
be replaced but has no measurable cost; **Y** = could and arguably should be removed. "Model"
says who shares it.

| file:line | what is shared | model | stated reason (verbatim) | avoidable? |
|---|---|---|---|---|
| `crates/mem/src/pod.rs:25,49,71` | `Arc<MappedStore>` inside `PodColumn::Mapped { _store, ptr, len }` | many typed columns view one mmap; `Send`/`Sync` via `unsafe impl` (`:31-34`) | "Keeps the mapping alive for as long as the column exists." (`:24`); "SAFETY: the mapped variant points into an immutable, read-only mapping owned (via Arc) by the column itself" (`:31-32`) | **C**. One clone per column at open, zero per element (`Deref` is a raw slice). Alternative: `PodColumn<'store>` borrowing `&'store MappedStore`, which pushes a lifetime onto `Kg`, `Graph`, `ModelView`, `AnnIndex`; or an owner struct with (offset,len) pairs resolved against `&store` at access. vorpal chose `'static` columns so `Kg` can be `Arc`-shared/`Send` to bindings. Acceptable for slates only if columns are opened O(1) times per generation, as here. |
| `crates/ann/src/index.rs:692`, `crates/ann/src/learned/persist.rs:523,554`, `crates/graph/src/graph.rs:172`, `crates/kg/src/kg.rs:355,1856,1949,2015`, `crates/kg/src/writer.rs:355`, `crates/ingest/src/pack.rs:233,277-278,323` | the same `Arc<MappedStore>` handed to `PodColumn::from_mapped_le` / `Graph::open_mapped` / `PackReader.stores: Vec<Option<Arc<MappedStore>>>` | open-time only | `graph.rs:170-171`: "Zero-copy open … sections become mapped columns; pages fault in as queries touch them. The mapping stays alive for the graph's lifetime." `persist.rs:548-552`: "Map, checksum, and section the file: O(hash) once, then every lookup is a binary search over mapped bytes" | **C** (same as above). |
| `crates/index/src/lib.rs:383,464,473,1226`; `crates/index/src/live.rs:472,499,595`; `crates/mcp/src/server.rs:100,645,1008,1165,1416,2185` | `Arc<Kg>` — the sealed in-memory graph | daemon serves it while a background thread persists it | `mcp/server.rs:98-99`: "`Arc` because the live-rebuild path serves the sealed graph from RAM while the deferred persistence tail still holds a reference on its background thread." `index/lib.rs:1216-1219`: "Deferred persistence: the sealed graph is answer-complete NOW — hand it to the daemon and move the artifact writes + content-addressed commit onto its background thread." | **N/C**. Two genuine owners (server + persistence tail) for one build; one clone per build, never per query. Could be avoided by having the tail *own* the `Kg` and the daemon re-`mmap` after commit (costs edit→query latency, which is the very thing this path exists to remove) or by an epoch-pinned `&'static` (their §7.4 RCU design, not yet built). This is the kind of documented, per-build `Arc` the slates brief means by "proven necessary". |
| `crates/pyo3/src/repo.rs:234,246`; `crates/napi/src/repo.rs:234,246` | `Arc<Kg>` inside the SDK `Index` session object | async methods read on a worker pool / libuv pool while the host-language object stays alive | pyo3: "Shared, immutable, mmap-backed graph — `Arc` so `*_async` methods can run their reads GIL-free on the worker pool while the Python object stays alive." napi: "… `Arc` so async methods can run their reads on the uv pool while the JS object stays alive (the daemon shares `Kg` the same way)." | **N** for a bindings crate: the FFI object and the in-flight task are independent `'static` owners (the GC may drop the object mid-call). One clone per async call. |
| `crates/index/src/lib.rs:6045-6080` (`open_searcher`, `cached_searcher`), `:6410-6435` (`cached_pack`), `:6441-6460` (`cached_runs`); consumers `compose.rs:264,780,1372`, `lib.rs:840`, `dense.rs:345`, `live.rs:654` | `Arc<Searcher>`, `Arc<PackReader>`, `Arc<Vec<FileRun>>` in three process-wide LRU caches (`OnceLock<Mutex<Vec<(PathBuf, Arc<_>)>>>`, `CAP = 8`) | any thread doing a query | `:6050-6054`: "Process-wide LRU cache of open [`Searcher`]s, keyed by the immutable generation dir. Repeated searches (a daemon, MCP, the async pool) reuse one set of mappings instead of re-`mmap`ing every tier per call. Safe by construction: generation dirs are content-addressed and immutable, so a cached entry is never stale … Bounded, so retired generations' mappings are released." `:6069-6070`: "Open (mmap all tiers) outside the lock — a load must not serialize searches on other generations." | **C**. One clone per query (or per batch: `async_bridge.rs:295-300` shares one across N jobs because "Opening per job would re-lock the searcher cache N times, which measurably capped core utilization on large batches"). The refcount lets bounded eviction coexist with in-flight readers; the alternative is epoch/RCU reclamation (§7.3 Tier A). Note the *lock* here (a `Mutex` around a `Vec`) is the actual serialization point, not the `Arc`. |
| `crates/index/src/lib.rs:2852-2858` | `Arc<Mutex<()>>` per index directory, in `OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>>` | ANN/postings warm builds | `:2846-2851`: "One build at a time **per index directory**: an eager background warm and a foreground search on the same index must not both build … but a host serving several indexes warms them concurrently — the old process-wide mutex serialized unrelated indexes for no reason. Late entrants re-check freshness under the dir lock" | **C**. A keyed lock; could be a `Mutex<HashSet<PathBuf>>` + `Condvar`. Cold. |
| `crates/index/src/lib.rs:3618,3696-3701` | `Option<Arc<WarmRoot>>` cache | search-feeds-index banking | "Process-wide cache of discovered warm roots (one per index root encountered)." | **C**; `&'static` via leak would do (bounded by distinct roots). |
| `crates/ingest/src/outline_extractor.rs:131,136,234,237,250,366` | `Arc<ExtractorSet>` (`DEFAULT_EXTRACTORS: OnceLock<Result<Arc<ExtractorSet>, String>>`) | every `OutlineExtractor::new()` clones the process-wide default set; user rule sets are owned | `:25-37`: "`Lazy` holds the BUNDLED docs bucketed per language (zero-copy `&'static` slices …) and serde-parses + matcher-compiles a language on FIRST USE: the ledger measured the eager all-49 compile at 158,737 allocations / 44 MB per run" | **Y**. The default set lives in a `static OnceLock`, so `&'static ExtractorSet` suffices; the `Arc` exists only so the field type unifies default and user-supplied sets. An `enum { Static(&'static), Owned(Box) }` or `Cow` removes it. Cold either way. |
| `crates/ingest/src/references.rs:1397,1412,1552`; `outline_extractor.rs:353` | `Arc<RefSpecData>` inside `ResolvedRefSpec` (`RESOLVED_SPECS: LazyLock<HashMap<SgLang, ResolvedRefSpec>>`) | process-wide table + dynamic-language overrides | `:1509`: "Kind ids resolved once per language, process-wide." | **Y** (same shape as above: `&'static` for builtins, owned for dynamic). Cold. |
| `crates/config/src/rule/referent_rule.rs:18,31,87,114-116,178-190,210-227`; `crates/config/src/rule/parameterized_util.rs:37,43-44,103-110,180,232,281-285,316,363-373` | `Registration<R>(Arc<HashMap<String, R>>)` with `Weak` back-references; `Arc<HashSet<String>>`; `Arc<HashMap<String, Arc<Rule>>>`; `Arc<BindingFrame>` stack in `thread_local!` | rule registry cloned into every `RuleConfig`; utility rules reference each other by id | `referent_rule.rs:29-30`: "SAFETY: `write` will only be called during initialization and it only insert new item to the hashmap." `:90`: "// these are shit code" | **Y** but inherited from ast-grep and covered by the upstream-compat contract. The §7.2 replacement is an arena of rules + `u32` ids. Not on the index hot path; it *is* on the `scan` hot path per rule evaluation only through `&`, not clones. |
| `crates/cli/src/utils/worker.rs:54,169,175,218,245`; `crates/cli/src/remote/mod.rs:386,489,503,523`; `crates/cli/src/remote/agent.rs:329,359,369,389,408`; `crates/cli/src/remote/producer.rs:671-673` | `Arc<W: PathWorker>` — the scan/run worker shared between the walker threads and the consumer thread | `ignore::WalkParallel::run` closures + main-thread consumer | `worker.rs:13-17`: "It follows multiple-producer-single-consumer pattern. vorpal will produce items in one or more separate thread(s) and `consume_items` in the main thread" | **Y** (inherited). `WalkParallel::run` accepts non-`'static` closures, so `std::thread::scope` + `&W` works — which is exactly what the ingest pipeline does (§7.5: "scoped extraction workers (borrowing config by `&` — no `Arc` on the hot path)"). Cost today: one clone per walker thread, not per file. |
| `crates/cli/src/remote/producer.rs:45` (`NodeOutcomes { inner: Arc<Mutex<OutcomeInner>> }`), `:128,139,172-175,232-235,486-503,556-557`; `crates/cli/src/remote/session.rs:59-62`; `crates/cli/src/remote/mod.rs:385` | `Arc<JobSpec>`, `Option<Arc<DoneSink>>`, `Option<Arc<MaxItemCounter>>`, `Arc<Option<PathBuf>>`, `Arc<SshDialOpts>` | one tokio task per remote node (`tokio::spawn` needs `'static`) | `producer.rs:123-127`: "Global `--max-results` counter, shared across every node session. Each forwarded fragment claims its `match_count`; once reached, forwarding stops fleet-wide" ; `:178-179`: "A dedicated tokio runtime on this producer thread (the lsp.rs pattern); the async fan-out never blocks the sync engine." | **C**. This is §7.1 row 4 ("`Arc` into `tokio::spawn`") — per node, per run. Replaceable with `futures::join_all` over borrowed futures on the dedicated runtime thread, or a `JoinSet` with a leaked `&'static JobSpec`. |
| `crates/cli/src/remote/agent.rs:50,129,312,353,391,408` (`Arc<Mutex<FrameWriter<Stdout>>>`), `:417-419` (`Arc<AtomicU64>` ×2, `Arc<Mutex<Option<String>>>`) | one framed stdout writer shared by walker threads + heartbeat thread; match/seq counters | agent-side fan-in | `:404-407`: "each rendered item is framed and streamed instead of channel-sent, tagged with its true match count so the coordinator can enforce a global `--max-results`" | **Y**. `heartbeat_loop(stop: &AtomicBool, writer: &Mutex<FrameWriter<Stdout>>)` at `:220` already takes plain references; with `std::thread::scope` the whole thing borrows. Better still: MPSC into one writer thread (single owner, no lock). |
| `crates/transport/src/spawn.rs:48-49,61` | `Arc<tokio::sync::Mutex<Child>>` shared by the wait future and the kill handle | tokio | `:46-47`: "The child is shared between the wait future and the kill handle — genuinely two independent `'static` owners of one OS process, so an `Arc` here is load-bearing, not incidental." | **N** — the exemplar of a documented, accepted `Arc`. |
| `crates/transport/src/process.rs:87,96,110` | `Arc<dyn RemoteKiller>` | `RemoteProcess` + any watchdog | `:109`: "A cheap clone of the kill handle, e.g. for a watchdog." | **N** (same object as above, type-erased). |
| `crates/transport/src/ssh.rs:77,83,104,118,125,175` | `Arc<russh::client::Handle<ClientHandler>>`, `Arc<client::Config>`, `Arc<PrivateKey>` | russh's API takes `Arc<Config>` and `Arc<key>`; the handle is shared between the transport and `SshKiller` | `:122`: "The SSH channel has no separate kill; closing the session takes the remote process with it." | **N** (dependency-imposed + two owners). |
| `crates/mcp/src/watch.rs:38,44,63-67` | `Arc<AtomicBool>` dirty flag, `Arc<Mutex<Option<HashSet<PathBuf>>>>` captured changes | shared with the `notify` callback thread (requires `'static + Send` closure) | `:1-3`: "a recursive watch … marks a dirty flag, and queries revalidate lazily — so the steady-state freshness check is one atomic load instead of a stat sweep." ARCHITECTURE §7.5: "making steady-state freshness a single atomic check (measured 2.8 µs per full tool call)". | **C**. Daemon-lifetime state; a `static AtomicBool` or `Box::leak` would remove the refcount, which is never touched on the query path anyway (only the `AtomicBool` load is). |
| `crates/pyo3/src/async_bridge.rs:308-311` | `Arc<Batch { slots: Vec<Mutex<Option<Result<..>>>>, remaining: AtomicUsize, loop_, fut }>` | N pool workers fill N slots; last one resolves the Python `Future` | `:263-265`: "Scatter-gather state for a batched await: N pool jobs fill their own slot, and the last to finish resolves the single Future. `Arc` is the legitimate case here — a genuinely shared, `'static` result buffer written by many worker threads (borrows can't express it)." Module doc `:1-3`: "driven by a **Rust-owned** worker pool. No tokio, no `asyncio` thread pool, and no `Arc` in our code." (for the single-await path) | **N** — the second exemplar of a documented, accepted `Arc`. |
| `crates/lsp/src/lib.rs:28,41,43,47,231,236,332` | `Arc<String>` note interner in `DashMap`, `Arc<RwLock<RuleCollection>>`, `Arc<RwLock<ClientCapabilities>>` | tower-lsp async handlers | comment `:42`: "interner for rule ids to note, to avoid duplication" | **Y** — the anti-pattern ARCHITECTURE §7.1 row 1 names; inherited; not on the index path. |
| `crates/ann/src/encoder/gemm_wgpu.rs:382` | `Arc<dyn Fn(wgpu::Error)>` | `wgpu::Device::on_uncaptured_error` signature | `:49-51`: "process-global because the handlers must be `'static`" | **N** (dependency-imposed). |
| `crates/cli/src/verify.rs:78` | `&Arc<Mutex<Reporter>>` | rayon test-case closures | none stated | **Y** — already passed by reference, so `&Mutex<R>` suffices. Inherited. |
| `crates/cli/src/outline.rs:145`, `crates/cli/src/outline/extract.rs:137` | `Arc<OutlineExtractors>` | producer thread behind a `sync_channel(256)` (adopted upstream commit `fe3607ea`, UPSTREAM.md:43) | none stated | **Y** via scoped thread. Cold (one clone). |
| `crates/wasm/src/sg_node.rs:43,55,79,89` | `Rc<Vorpal<WasmDoc>>` (not `Arc`) | single-threaded wasm; `SgNode` keeps the root alive | `:50-53`: "SAFETY: WasmDoc's Node type wraps a JS SyntaxNode (GC-managed, Clone). It does not actually borrow from the Rust tree. The Rc keeps the Vorpal alive as long as any SgNode references it." | **C** — correct choice of `Rc` over `Arc` on a single-threaded target. |

Test-only `Arc`s (not policy-relevant, listed for completeness): `crates/transport/tests/
ssh_smoke.rs:32-38,124` and `crates/cli/src/remote/testserver.rs:24,30,119` (russh server
config/handler), `crates/ingest/src/pack.rs:1195,1267,1401,1529`, `crates/cli/src/remote/
remote_stream.rs:629-658`, `crates/cli/src/utils/worker.rs:398-403`.

### 2.2 Locks

All production locks are `std::sync::{Mutex, RwLock}` — **no `parking_lot`** anywhere in the
workspace (the only `Mutex` not from std is `tokio::sync::Mutex` in `crates/transport/src/
spawn.rs:12` for the child handle awaited across `.await`, and in tests). Poisoning is recovered,
never propagated (see 2.0).

| file:line | lock | guards | stated reason / note |
|---|---|---|---|
| `crates/resolve/src/intern.rs:27,78-90,139-146` | `[CachePadded<RwLock<Shard>>; 64]` | session-scoped string interner shards | `:9-13`: "**sharded 64 ways** by string hash: parallel link passes make ~10M interner calls across all worker threads, and a single lock measurably serialized them. Each id carries its shard in its high bits". `:136-138`: "Cache-line padded so one shard's lock RMW never invalidates a neighbor shard's line (unpadded, the 112-byte shards packed ~1.14 per 128-byte Apple-Silicon line; `CachePadded` picks the right alignment per architecture)." Contention is counted under `alloc-ledger` via `try_read` (`:75-90`); measured "≤2.2 K contended of ~10 M+ acquisitions" (BENCHMARKS.md:1098-1099). |
| `crates/core/src/meta_var.rs:125-142` | `OnceLock<RwLock<HashSet<&'static str>>>` + `thread_local!` cache | meta-variable name interner | `:118-121`: "The per-thread cache makes the hot path (one probe per fresh capture) a thread-local hash lookup with zero shared-memory traffic; the global set is consulted only the first time a thread meets a name." |
| `crates/ingest/src/product.rs:265-267` | `OnceLock<RwLock<HashSet<&'static str>>>` | grammar node-kind name interner | `:258-262`: "bounded by the union of kind names across the compiled grammars (a few thousand short strings, leaked once each) and is read-mostly after warmup." |
| `crates/ingest/src/pipeline.rs:1982-1994` | `ByteBudget { used: CachePadded<AtomicU64>, peak, gate: Mutex<()>, room: Condvar }` | in-flight byte budget | `:1974-1980`: "Reservation is a CAS on a cache-padded atomic (the hot path); exhaustion parks on a condvar until a release makes room. A single item larger than the whole budget reserves the full capacity instead of deadlocking — progress over precision for the pathological case." |
| `crates/ingest/src/manifest.rs:42-55` | `Mutex<Vec<FileStat>>` + `Mutex<Option<io::Error>>` | parallel stat sweep sink | `:45-49`: "Per-walker-thread accumulation: each visitor pushes into its own vector and flushes once into the shared sink when the walker retires it (the Drop below) — the previous form took the global mutex once per accepted file (~72k lock round-trips at kernel scale …)" — the canonical "batch under a lock" fix. |
| `crates/ingest/src/pipeline.rs:2383-2384`, `crates/index/src/lib.rs:867-872` | `AtomicBool abort` + `Mutex<Option<io::Error>> first_error`; `Mutex<Vec<(String,f64)>>` | first-error capture across rayon workers | cold error path |
| `crates/ingest/src/tree_cache.rs:124-126`, `:51` | `OnceLock<Mutex<Cache>>`, `OnceLock<Policy>` | giant-file incremental-parse cache | `:1-10` rationale quoted in §3; `VORPAL_TREE_CACHE=0` disables |
| `crates/ingest/src/selfcheck.rs:478-479,518` | `OnceLock<Mutex<HashMap<SupportLang, Result<(),String>>>>`, `OnceLock<Result<..>>` | memoized per-language canary verdicts | BENCHMARKS.md:1123-1127 (fix 5) |
| `crates/index/src/lib.rs:2852`, `:3618`, `:4616`, `:6055`, `:6412`, `:6443` | see 2.1 caches; `encoder_cache: Mutex<EncoderCache>` | per-`Searcher` FIFO-bounded embedding cache | `:4613-4615`: "node surfaces are immutable per pinned generation, so rows never go stale within a handle. FIFO-bounded at [`ENCODER_CACHE_ROWS`]." |
| `crates/mcp/src/server.rs:15-20` | `static Mutex<()>` | serializes in-process builds | `:12-14`: "staging dirs are per-PID, so two same-process builds … would share — and clobber — one staging directory. Child-process builds need no lock (their PIDs differ)." |
| `crates/kg/src/ledger.rs:204` | `static Mutex<Vec<(u64,u64,u64,String)>>` | backtrace sample table (profiling builds only) | `:200-201`: "Linear-scanned — sampling is rare and the table small." |
| `crates/ann/src/encoder/gemm_wgpu.rs:52,281-282` | `static Mutex<Option<String>>` UNCAPTURED; `Mutex<Scratch>`, `Mutex<Option<String>>` fault | GPU error capture / scratch buffers | `:49-51` quoted above |
| `crates/ann/src/vamana.rs:286,447` | `Mutex<Vec<VisitStamps>>` stamp pool | reused per-task scratch arrays | `:444-446`: "A fresh zero-filled ~9 MB array per task per round re-faulted its pages on every allocation under immediate-decay jemalloc — ~700k minor faults per kernel-scale build." |
| `crates/ann/src/learned/spill.rs:546-556` | `Vec<Mutex<(BufWriter<File>, u64)>>` | fixed pool of spill writers, one per rayon thread | `:543-544`: "Which file a range's run lands in is scheduling-shaped only; the run SET is deterministic." |
| `crates/cli/src/utils/inspect.rs:56-115` | `Mutex<W: Write>` | trace output serialization | inherited |
| `crates/wasm/src/wasm_lang.rs:113` | `static Mutex<Vec<Inner>>` | registered wasm languages | single-threaded target; `Mutex` used as a `const`-constructible cell |

No lock-ordering document exists; no lock is held across an `.await` except the deliberate
`tokio::sync::Mutex<Child>` in `spawn.rs`. `dashmap` appears only in the inherited LSP and CLI
crates (`Cargo.toml:61`), never in the index/ingest/kg layers.

### 2.3 Atomics, `OnceLock`/`LazyLock`, statics, `thread_local!`

| site | purpose | ordering | gated? |
|---|---|---|---|
| `crates/kg/src/ledger.rs:24-57` `static SLOTS: [Slot; 32]`, `#[repr(align(128))] struct Slot { AtomicU64 ×8, AtomicBool }` | allocation/fault/contention ledger; "Hot counters are therefore sharded across 32 cache-line-aligned slots picked by pthread identity — allocator-context-safe (no TLS init, no allocation) — and summed at snapshot time." (`:12-18`); "**The measurement must not manufacture the contention it measures**: the first ledger build used four global atomics and doubled kernel-scale user CPU purely on cache-line ping-pong" | Relaxed | feature `alloc-ledger` ("NEVER a default", `crates/kg/Cargo.toml:27-30`) |
| `crates/kg/src/ledger.rs:144-150` `INTERN_READ_CONTENDED`, `INTERN_WRITES`, `BUDGET_PARKS`, `CHAN_FULL` | slow-path contention counters: "Each lives on a slow path already (a failed try-lock, a park, a full channel), so a single shared atomic is honest" (`:137-140`) | Relaxed | `alloc-ledger` |
| `crates/kg/src/ledger.rs:161-174` `SAMPLE_MASK`, `TS_SAMPLE_MASK`, `REALLOC_SAMPLE_MASK` | callsite sampling masks from env: "Written ONCE from [`init_sampling_from_env`] … never lazily from allocator context (env reads allocate)." | Relaxed | `alloc-ledger` |
| `crates/ingest/src/pipeline.rs:1982-1983` `CachePadded<AtomicU64>` used/peak | byte-budget admission (CAS) | CAS (see file) | always |
| `crates/index/src/lib.rs:1874` `static NONCE: AtomicU64` | process-unique staging suffix: "concurrent builds in ONE process (the daemon's background canonicalizer racing a synchronous rebuild) must never share a staging directory." | Relaxed `fetch_add` | always |
| `crates/index/src/autowarm.rs:26-27` `REGISTERED`, `SPAWNED: AtomicBool` | "Only registered binaries spawn … One spawn per process" (`:8-15`) | Release/Acquire | always |
| `crates/query/src/exec.rs:243-251` `Budget { left: AtomicU64, exhausted: AtomicBool }` | "every edge scanned by the engine or an EXISTS probe costs one unit; exhaustion is a typed ceiling, never a partial answer." `MAX_EDGE_VISITS` named constant | see file | always |
| `crates/ann/src/vamana.rs:140-149` `BUILD_DIST_EVALS`, `BUILD_EXPANSIONS`, `OnceLock<bool> ON` | "distance evaluations and beam expansions are pure functions of (input, binary) under the deterministic build, so two builds' counters are exactly comparable even when wall time drowns in machine noise." | Relaxed | `VORPAL_PHASE_TRACE` env |
| `crates/ingest/src/walk_reuse.rs:752-754` `SPLICES`, `FALLBACKS` | "telemetry + oracle non-vacuity: tests assert the counter moved, so a silently-dead reuse path can never masquerade as a passing oracle" | Relaxed | always |
| `crates/cli/src/utils/worker.rs:252-262` `MaxItemCounter(AtomicUsize)` | packs max and current into one atomic: "The baseline is to pack two usize (max and curr item) into one atomic usize without underflowing", `BASELINE = 2usize << 20` | see file | inherited |
| `crates/cli/src/utils/inspect.rs:86-88` `FileTrace { files_scanned, files_skipped: AtomicUsize }` | scan statistics | AcqRel/Acquire | inherited |
| `crates/pyo3/src/async_bridge.rs:41-49` `Pool { spawned, idle: AtomicUsize }`, `static POOL: OnceLock<Pool>` | lazily-grown bounded worker pool: "`idle == 0` is an accurate 'every worker is busy' signal — unlike an active-count that lags behind `recv`" (`:42-46`); "The pool is a `OnceLock` singleton (immutable after init — not a rebindable global), sized from `VORPAL_ASYNC_WORKERS` or `8× cores`" (`:16-18`) | Acquire / AcqRel CAS | always |
| `crates/ann/src/encoder/{gemm_x86.rs:71, gemm_i8.rs:142, cache.rs:11}` `OnceLock<Isa>`, `OnceLock<Option<usize>>` | "CPUID is read once; the answer is a process constant." (`gemm_x86.rs:68-69`); L2 size "read from the platform's own enumeration (never guessed)" (`cache.rs:1-5`) | — | always |
| `crates/ann/src/encoder/mod.rs:139-142` `rung: OnceLock<DocSideRung>`, `int8: OnceLock<Result<..>>` | per-object lazy init ("chosen on first use") | — | always |
| `crates/kg/src/kg.rs:210,513` `dense: OnceLock<Vec<(u64,u32)>>`, `communities: OnceLock<Option<Vec<u32>>>` | per-object lazy tables: "built on first bulk use (the WRITE paths convert tens of millions of endpoints — a binary search each measured ~0.6 s per kernel edit; readers doing per-query point lookups never pay the table)" | — | always |
| `crates/language/src/lib.rs:590,606`; `crates/lang-registry/src/{lib.rs:292, injection.rs:117-118, lang_globs.rs:12}`; `crates/dynamic/src/lib.rs:145-146` | grammar digests, injections, globs, dynamic languages: process constants after one-shot registration ("Registration is one-shot before extraction", `lang-registry/lib.rs:305-306`; "there is no stale-`OnceLock` hazard", `:315`) | — | always |
| `crates/ingest/src/references.rs:1509,1606` `LazyLock<HashMap<SgLang, ResolvedRefSpec>>`, `LazyLock<HashMap<..ResolvedTypeFacts>>` | "Kind ids resolved once per language, process-wide." | — | always |
| env-flag reads: `crates/core/src/tree_sitter/mod.rs:52` (`VORPAL_PARSER_REUSE`), `crates/ann/src/index.rs:189` (`VORPAL_ANN_BEAM`), `crates/ann/src/encoder/forward.rs:129` (`VORPAL_ENCODER_TRACE`), `crates/ingest/src/outline_extractor.rs:595`, `crates/ingest/src/tree_cache.rs:51` | read once into `OnceLock<bool/usize>` — the convention for every `VORPAL_*` knob | — | always |
| `thread_local!`: `crates/core/src/tree_sitter/mod.rs:30-38` `REUSED_PARSER` | "One reusable parser per worker thread. A fresh `Parser::new()` per file made bulk indexing recreate the GLR stack's node pool and the lexer's buffers tens of millions of times per large corpus … Parser state never influences tree CONTENT" | — | `VORPAL_PARSER_REUSE=0` opts out |
| `thread_local!`: `crates/core/src/meta_var.rs:126-127`, `crates/config/src/rule/parameterized_util.rs:35-39`, `crates/ingest/src/tree_cache.rs:234-235`, `crates/wasm/src/ts_types.rs:25-27` | per-thread caches / frame stacks / init flags | — | — |
| `#[global_allocator] static ALLOC` + `#[unsafe(export_name = "_rjem_malloc_conf")] pub static MALLOC_CONF: SyncPtr` | see §3 | — | cfg-gated |
| `crates/cli/src/utils/print_diff.rs:27-30` `static THISTLE1: Color = …` | inherited const-like colors | — | — |

### 2.4 Async runtime: tokio, at the edge only

- **Where**: `crates/transport` (tokio `rt, rt-multi-thread, io-util, io-std, process, net, time,
  sync, macros` + `async-trait` + `russh`; `Cargo.toml:17-22`), `crates/wire` behind an
  **optional** `tokio` feature with only `io-util` (`Cargo.toml:11-14`: "Async frame I/O over
  tokio streams — pulled in by the coordinator/transport layer, kept out of the grammar-sliced
  agent binary (which speaks the protocol with the blocking `io` module)"), `crates/cli`
  (`rt-multi-thread, io-std, io-util, sync, time, macros`) for the remote coordinator and the
  LSP entry, `crates/lsp` (`tower-lsp-server 0.23`).
- **Why**: `crates/transport/src/lib.rs:9-13`: "The layer is **async on tokio** — three of the four
  planned backends (SSH, k8s, docker) are async-native with no sync equivalent, and one runtime
  multiplexing I/O over hundreds of nodes × several streams is the only thing that scales. The
  sync scan/index engine is never touched: the agent reuses it verbatim behind a thin async I/O
  shell, and the coordinator bridges async results into the existing sync printer channel with a
  dedicated blocking forwarder." The `dyn` boundary is deliberately coarse (`:5-7`): "boxing is
  amortized over whole streams, not per byte."
- **How the runtime is hosted**: not a `#[tokio::main]`; a runtime is built on the producer's own
  thread (`crates/cli/src/remote/producer.rs:178-181`: "A dedicated tokio runtime on this producer
  thread (the lsp.rs pattern); the async fan-out never blocks the sync engine.") and results cross
  back over `std::sync::mpsc::sync_channel(REMOTE_WINDOW = 256)` (`:31-33`: "the printer's drain
  rate paces the network end-to-end (§3.1), so no more than this many rendered fragments buffer
  ahead of the single-threaded consumer").
- **Where tokio is absent, deliberately**: `mcp` (blocking stdio JSON-RPC, see §6), `index`,
  `ingest`, `kg`, `mem`, `segment`, `graph`, `ann`, `resolve`, `query`, `canonical`, `loader`,
  `pyo3` ("No tokio, no `asyncio` thread pool" — std threads + `crossbeam-channel`), `napi`
  (napi's own libuv pool, §7), `wasm` (`wasm-bindgen-futures`). CPU parallelism everywhere is
  `rayon` (`rayon::scope`, `par_chunks_mut`) plus `crossbeam-channel` bounded queues and
  `crossbeam-utils::CachePadded`.

---

## 3. Allocation strategy

### 3.1 jemalloc: which binaries, how gated, how tuned

jemalloc is the **binary's** allocator, never the library's. Four `#[global_allocator]` sites
exist (`crates/cli/src/main.rs:10-12`, `crates/index/src/main.rs:10-16`, `crates/mcp/src/
main.rs:8-10`, `crates/pyo3/src/lib.rs:8-10`), plus two profiling examples
(`crates/index/examples/{overlay_probe,parse_probe}.rs`). The gate is the same expression
everywhere except pyo3:

```rust
#[cfg(not(any(target_env = "msvc", all(target_env = "musl", target_arch = "aarch64"))))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;
```

with the dependency gated identically in `Cargo.toml` (`crates/cli/Cargo.toml:105-109`):
"jemalloc is the binary's allocator on every platform except MSVC (tikv-jemalloc-sys does not
build there); main.rs cfg-gates the #[global_allocator] to match." (`aarch64-musl` is excluded
by the same cfg without a stated reason — presumably the alpine arm64 build.) The Python
extension gates only on `not(target_env = "msvc")` (`crates/pyo3/Cargo.toml:38-43`): "The
extension's Rust allocations use jemalloc off Windows: macOS/glibc default malloc lock-contends
hard under the async pool's concurrent searches (measured ~3.5x CPU amplification at N=32).
jemalloc's per-thread arenas remove that. Python's own allocations are unaffected (they use
pymalloc). MSVC keeps the system allocator."

The library contract (`docs/wip/EMBEDDING.md:35-39`): "**Allocator**: the library never sets a
global allocator. jemalloc (`#[global_allocator]`, decay tuning, tree-sitter allocator
unification) is the *binary's* memory profile, behind the default `jemalloc` feature of
`vorpal-index` — hosts take `default-features = false` and keep their own allocator. Measured:
allocator choice does not change index output (same generation id with and without)." The
feature comment (`crates/index/Cargo.toml:42-45`) says the same.

**Compiled-in tuning** is exported as the `_rjem_malloc_conf` symbol (a C string pointer wrapped in
a `SyncPtr` newtype with `unsafe impl Sync`, `crates/index/src/main.rs:101-112`, repeated in
`cli/main.rs:14-22` and `mcp/main.rs:12-23`):

```rust
#[unsafe(export_name = "_rjem_malloc_conf")]
pub static MALLOC_CONF: SyncPtr = SyncPtr(c"narenas:8,dirty_decay_ms:0,muzzy_decay_ms:0".as_ptr().cast());
```

Rationale (`index/main.rs:95-100`): "Compiled-in jemalloc tuning (overridable at runtime via
`_RJEM_MALLOC_CONF`): zero decay returns freed pages to the OS immediately — a bulk pipeline's
phases hand memory back instead of stacking retained garbage under the next phase's live set —
and a bounded arena count stops 4×ncpu arenas from each holding a retention tail (~140 MB
spread at kernel scale). Measured on the Linux tree: default malloc 2.05 GB peak → this config
1.13 GB, equal wall time." And why jemalloc at all (`:6-9`): "at kernel scale, roughly 45% of
the default macOS allocator's peak footprint was freed-but-retained magazine pages (2.05 GB
observed vs a ~0.95 GB live set). jemalloc's decay returns those pages while running, and its
thread-local caches are also simply faster under the pipeline's multithreaded churn."

**Batch-run decay off** (`index/main.rs:18-45`, called only in the `index` command arm, `:221-222`):

> Batch-index runs retain dirty pages for the process lifetime (decay off): with default decay,
> jemalloc purged pages mid-run while ~65 GB of churn cycled through a ~1 GB live set, and
> re-touching them cost a kernel-scale build 2.25 M soft faults and ~4 s of sys time (A/B:
> faults 2,245,820 → 474,390, sys 10.6 s → 6.6 s, wall −0.7 s; peak footprint 2.58 → 4.36 GB —
> retained pages die with the process at exit, so the tail cost is zero). Scoped to the `index`
> COMMAND only: long-lived daemon/serve paths keep default decay, which is exactly what returns
> their idle memory to the OS. (`oversize_threshold:0` was measured and rejected: faults flat,
> +5 s user CPU combined.)

The knob writes `arenas.{dirty,muzzy}_decay_ms = -1` (default for future arenas) and
`arena.4096.{dirty,muzzy}_decay_ms = -1` (4096 = `MALLCTL_ARENAS_ALL`, every existing arena)
through `tikv_jemalloc_ctl::raw::write`, discarding results: "A refused knob merely leaves
default decay in place — this is a performance hint, never a correctness input — so results are
deliberately discarded." (`:37-38`).

**Tree-sitter's C allocations are routed through jemalloc too** (`cli/main.rs:24-37`,
`index/main.rs:137-163`): "Without this the parser's tree memory lives in the macOS default
zone — outside jemalloc's decay policy — and freed-but-retained tree pages (~150–250 MB at
kernel scale) ride under the link phase's peak. One allocator, one policy." via
`tree_sitter::set_allocator(Some(tikv_jemalloc_sys::malloc), …)`.

**Phase-boundary purge** (`crates/ingest/src/pipeline.rs:814-870`, `release_freed_pages`): under
the `jemalloc` feature it spawns a detached thread that calls `mallctl("arena.4096.purge")`
("Action node: no old/new value. The ALL sentinel is genuinely supported for purge …  A refused
call only means pages return later; never a correctness input."); non-jemalloc macOS builds keep
`malloc_zone_pressure_relief` (declared as a bare `extern "C"`), with the honest note that "The
macOS `malloc_zone_pressure_relief` addresses the SYSTEM allocator, which a jemalloc-global
binary no longer routes through — it silently released nothing for years of jemalloc builds".

### 3.2 The vendored jemalloc and the BENCHMARKS passes it came from

`Cargo.toml:75-80`:

> Vendored jemalloc (tikv-jemalloc-sys 0.7.1 / jemalloc 5.3.1): fixes the ctl handler for
> `arena.<MALLCTL_ARENAS_ALL>.{dirty,muzzy}_decay_ms` — upstream admits the ALL sentinel through
> the mib resolver but never checks for it, indexing one past the arenas array (observed
> EXC_BAD_ACCESS in pac_decay_ms_set). The batch indexer sets decay off through that sentinel
> (fault economics; see BENCHMARKS). Sync ledger: docs/UPSTREAM.md.

The ledger row (`docs/wip/UPSTREAM.md:233-241`) pins the exact crate version
(`0.7.1+5.3.1-0-g81034ce…`, "vendored verbatim … same mechanism as the tree-sitter runtime")
and describes the one-line delta in `jemalloc/src/ctl.c` (`arena_i_decay_ms_ctl_impl` gains an
explicit `MALLCTL_ARENAS_ALL` branch; writes apply to every initialized arena, reads return
`EINVAL`; "Upstream-worthy"). The BENCHMARKS doc referenced is `docs/wip/BENCHMARKS.md`, "Pass 12 —
fault economics: decay-off for batch runs (2026-09-01)" (`:1569-1605`): per-phase attribution
"put 71 % of the kernel build's 2.25 M page faults in the stream phase … almost all were soft
faults re-touching pages jemalloc's default decay had purged mid-run"; the knob battery table
(defaults 2,245,820 faults / 10.6 s sys vs `dirty_decay_ms:-1,muzzy_decay_ms:-1` 474,390 /
6.60 s; `oversize_threshold:0` "measured-and-rejected"); and the crash: "Shipping the sentinel
exposed an upstream bug … Fixed at the source: `tikv-jemalloc-sys 0.7.1` is vendored … with the
handler mirroring `arena_i_decay`'s ALL branch … the smoke test is the exact crashing path."
"Pass 13 — the RSS giveback: real phase-boundary purges" (`:1536-1567`) records the follow-up:
`narenas` consolidation rejected ("arena-mutex blocking under 14–18 allocation-heavy threads"),
the `malloc_zone_pressure_relief` no-op discovered, and the frontier "{decay-on: 2.58 GB peak,
2.25 M faults, sys 10.6 s} ↔ {decay-off + purges: 4.15 GB peak / 1.83 GB final, 0.58 M faults,
sys 7.3 s}".

The measurement apparatus behind those numbers is the **allocation ledger** (`docs/wip/BENCHMARKS.md
:1078-1091`, "Hyper-optimization campaign, pass 1"): "feature `alloc-ledger` (opt-in, never
default) wraps the binary's jemalloc in exact event counters — Rust and tree-sitter C churn
attributed separately via `set_allocator` counting shims — plus mach `TASK_EVENTS_INFO` faults,
per-phase `getrusage` …, and contention counters at the pipeline's known serialization points";
headline "444 M allocator events per build — 273 M tree-sitter C (~3,600 per file) + 171 M Rust
with 57.8 GB cumulative churn against a 1.4 GB live peak (40×)". Implementation: `LedgerAlloc`
wrapping allocator (`index/main.rs:53-93`, "Two relaxed atomic adds per event; profiling builds
only"), `crates/kg/src/ledger.rs` (sharded `#[repr(align(128))]` slots, §2.3), features
`alloc-ledger`/`alloc-stats` (`crates/kg/Cargo.toml:25-30`, `crates/index/Cargo.toml:65-72`,
"NEVER a default — profiling builds only (the bench-internals discipline)"), and the vendored
tree-sitter's thread-local children-block/leaf caches (`docs/wip/UPSTREAM.md:105-106`: C-side
allocations "273 M → 16.3 M (−94 %)").

### 3.3 `crates/mem` — what the substrate provides

`crates/mem/src/lib.rs:1-17` states the design goal: "**one code path from 1 file to 10⁹ LOC**
with resource use proportional to input: nothing is pre-sized for 'huge,' and the baseline
touches native pages and a small arena only. Every heavy knob (huge pages, big arenas, prefetch,
NUMA) is *derived* from two cheap probes and stays dormant until the data justifies it." Deps
(`Cargo.toml:14-17`): `memmap2 0.9.11`, `libc 0.2`, `bumpalo 3.20`, `bytemuck 1.25.2`.

| Module | Provides | Notable rules |
|---|---|---|
| `probe.rs` | `HardwareProbe::detect()` (base page via `sysconf(_SC_PAGESIZE)`, THP state from `/sys/kernel/mm/transparent_hugepage/enabled`, hugetlb pools from `/sys/kernel/mm/hugepages/…/nr_hugepages`, NUMA node count from `/sys/devices/system/node`, STLB reach estimates); `CorpusProbe { total_bytes, file_count }` with per-store hot-byte projections | `const STLB_ENTRIES: u64 = 1536` documented as "a safe lower bound … the estimate only gates *escalation*, so a low bound is conservative (we escalate slightly later, never wrongly)" (`:10-16`); Windows page size is the honest constant 4096 ("This feeds TLB-reach heuristics only — the honest constant beats importing a syscall binding for it", `:68-73`); named per-store constants (`BYTES_PER_NODE = 40`, `AVG_DEGREE = 16`, `RABITQ_CODE_BYTES = 96` …) |
| `policy.rs` | `ResourcePolicy` (probe pair) → `StorePolicy { page, access, hotness }`; `arena_chunk_bytes = clamp(next_pow2(batch), 64 KiB, 2 MiB)`; `prefetch_distance` 0/4/8 by corpus size; `numa_enabled` only ≥2 sockets and ≥10⁸ nodes | `const HUGE_PAGES_SUPPORTED: bool = cfg!(target_os = "linux")` ("macOS Apple Silicon has no superpages", `:10-12`); `decide_page` is a **pure function** "factored out so it is testable on every platform (independent of the `cfg` gate …)" (`:49-50`); cold stores never escalate |
| `store.rs` | `MappedStore` (read-only file mmap + `madvise` Random/Sequential + `MADV_HUGEPAGE` on Linux; `advise_willneed`/`advise_dontneed`), `AnonStore` (anonymous, `MAP_HUGETLB` via `opts.huge(Some(21))`/`Some(30)` with the comment "21 = log2(2 MiB) = MAP_HUGE_2MB; 30 = log2(1 GiB) = MAP_HUGE_1GB"), `ScratchMmap` (file-backed writable scratch: "multi-GB working sets … ride the OS pager instead of anonymous RSS … a crash can only leave a dead file, never a wrong artifact"), `available_disk_bytes` (`statvfs`) | mmap `unsafe` justified by construction: "SAFETY: sealed append-only segment; not mutated or truncated while mapped (§9.1)"; advice failures are non-fatal (`let _ =`) |
| `pod.rs` | `PodColumn<T: bytemuck::Pod>` — owned `Vec<T>` or a zero-copy view into a mapped section; validates bounds, element-size divisibility and alignment; big-endian targets decode an owned copy | `unsafe impl Send/Sync` with SAFETY comment; "Cloning materializes: clones are for tests and small structures, not the bulk path" |
| `prefetch.rs` | `prefetch_read` (`_mm_prefetch` T0 on x86_64; inline `asm!("prfm pldl1keep")` on aarch64; no-op elsewhere), `prefetch_read_nta`, `prefetch_slice_ahead` | "All forms are hints — never a correctness dependency" (`:6`) |
| `csr.rs` | `Csr { row_offsets: Vec<u64>, col_indices: Vec<u32> }` built by counting-scatter; `for_each_neighbor_prefetched` one-hop-lookahead walk | "keyed by dense `u32` node ids — never pointers" |
| `arena.rs` | `BatchArena` over `bumpalo::Bump`: `alloc`, `alloc_slice_copy`, `reset()` (O(1) bulk free, chunks retained) | "Each ingest worker owns one arena … Peak RAM is O(batch × workers), independent of repo size." |
| `lib.rs:20-26` | `carry_libc` re-export of `getrusage`/`rusage` | "vorpal-mem owns the libc dependency; callers avoid growing their own. Unix-only" |

`tests/adaptive.rs` is the end-to-end contract: "the same calls drive a few-file baseline here and
would drive 10⁹ LOC unchanged — only the derived policy differs."

### 3.4 Other allocation patterns worth copying

- **Every number is a named constant with a measured rationale**, usually next to the env knob that
  overrides it: `crates/ingest/src/tree_cache.rs:29-44` (`DEFAULT_MIN_BYTES = 1 << 20`,
  `DEFAULT_BUDGET_BYTES = (64 << 20) * 4` with the 2.0–2.8× snapshot-mass measurement and date),
  `crates/kg/src/identity.rs:56-62` (`BUCKET_MAX = 4096` with the sweep it came from),
  `crates/ingest/src/pack.rs:720-721` (`POOL_SPARES = 64`, `POOL_MAX_BUF = 8 << 20`),
  `crates/mcp/src/watch.rs:30-32` (`MAX_CAPTURED_CHANGES = 4096`), `crates/wire/src/frame.rs:23-24`
  (`DEFAULT_MAX_FRAME = 64 MiB`), `crates/cli/src/remote/producer.rs:31-33` (`REMOTE_WINDOW = 256`),
  `crates/index/src/lib.rs:3225-3226` (`CHUNK_ROWS = 4096` "the crate-standard deterministic
  shape"). The SUBSECOND standing rule 3 makes this policy: "Constants become policies" and
  "Derivations must be deterministic (seeded, pure functions of input) and stamped into provenance."
- **Scratch reuse over allocation**: per-thread parser reuse (`core/src/tree_sitter/mod.rs:30-38`),
  vamana stamp pool (`ann/src/vamana.rs:444-446`), `PackBufPool`, "source-read and product-encode
  buffers amortize to zero per file" (ARCHITECTURE §7.5), the manifest's per-thread `Flush`
  accumulator (`ingest/src/manifest.rs:45-49`).
- **Lazy compilation behind `OnceLock`** to cut startup allocations: `ExtractorSet::Lazy`
  ("Startup: 158,737 allocs / 44 MB → 78 allocs", BENCHMARKS.md:1116-1121).
- **Interning with `&'static str` leaks bounded by a small vocabulary** (`ingest/src/product.rs:
  258-262`, `core/src/meta_var.rs:118-121`) versus a **session-scoped interner with lifetime
  branding** for unbounded vocabularies (`resolve/src/intern.rs:15-22`; EMBEDDING.md:9-31 — reclaim is
  `Drop`, enforced by a `compile_fail` doctest).
- **Bounded transit memory** via byte-budget admission + bounded channels (`ingest/src/pipeline.rs:
  1974-1994`, `:2397` `crossbeam_channel::bounded((threads * 64).clamp(64, 4096))`).

### 3.5 Lints, `unsafe`, and error policy

- `[workspace.lints.clippy]` (`Cargo.toml:130-137`) allows exactly three lints (`collapsible_if`,
  `manual_is_multiple_of`, `chunks_exact_to_as_chunks`) with the reason quoted in §0; every crate
  opts in via `[lints] workspace = true`. The gate is `cargo clippy --workspace --all-targets --
  -D warnings` (CI) and the pre-commit hook's `-D clippy::all`.
- There is **no** `#![deny(unsafe_code)]`, `#![forbid(...)]`, `missing_docs`, or `clippy::pedantic`
  anywhere; only three file-level allows exist (`crates/core/src/replacer/indent.rs:1`
  `doc_overindented_list_items`, `crates/config/src/rule/selector.rs:1` `doc_lazy_continuation`,
  `crates/wasm/src/ts_types.rs:34` `non_snake_case`). `clippy.toml` has one line:
  `ignore-interior-mutability = ["fluent_uri::Uri"]` (for the LSP's URI keys).
- `unsafe` policy is **edition-2024 idiom + a `// SAFETY:` comment at every block**: nested
  `unsafe {}` inside `unsafe fn` bodies (`unsafe_op_in_unsafe_fn` is warn-by-default in 2024 and
  the tree complies, e.g. `crates/index/src/main.rs:66-86`, `crates/loader/src/main.rs:135-160`,
  `crates/transport/src/spawn.rs:79-89`), `#[unsafe(export_name = …)]` attribute syntax, `unsafe
  extern "C" { … }` blocks, and even test-only `std::env::set_var` wrapped as `unsafe { … }` with
  a SAFETY note (`crates/mcp/src/registry.rs:133-134`). Counts of `unsafe` tokens per crate: ann
  62, index 53, wasm 19, mem 14, core 13, cli 12, napi 9, mcp 9, kg 7 — concentrated in SIMD kernels,
  mmap views, FFI shims, and allocator plumbing. SAFETY comments: ann 33, mem 12.
- **Errors, not panics** ("no-panics law"): typed refusals (`FetchSpanError::Stale`, `WireError::
  TooLarge`, `SegmentError::TooSmall`), `Result<_, String>` across thread boundaries ("`String`
  error (not `Box<dyn Error>`) because this crosses a thread boundary", `crates/index/src/lib.rs:
  395-396`), lock-poison recovery, "sparsity/ceiling guards return errors (no-panics rule)"
  (`docs/wip/SEMANTIC_TIER.md:170`), "typed refusals, never panics" (`docs/wip/BENCHMARKS.md:2578`).
  `expect("…")` is reserved for statically-impossible failures and always carries a message.
- Third-party crate discipline visible in `Cargo.lock` (565 packages): the `vorpal` binary's
  direct deps are ansi_term, anyhow, atty, base64, blake3, clap, clap_complete, codespan-reporting,
  crossterm, dashmap, ignore, inquire, libc, memchr, rayon, regex, russh, serde, serde-sarif,
  serde_json, serde_yaml, similar, smallvec, termimad, terminal-light, tikv-jemalloc-sys,
  tikv-jemallocator, tokio, tracing-subscriber, tree-sitter + the vorpal-* crates. `parking_lot`,
  `rustix`, and `windows-sys` appear only transitively; `criterion`, `proptest`, `insta`, `loom`
  do not appear at all.

---

## 4. Cross-platform code patterns

### 4.1 How platform code is organised

There are **no per-platform modules and no `build.rs`** in the workspace apart from
`crates/napi/build.rs` (`napi_build::setup()`). Platform variation is expressed as **paired
function definitions with identical signatures**, one per `cfg` arm, so call sites are
unconditional:

```rust
#[cfg(target_os = "linux")]              fn exec_agent(agent: &[u8], argv: &[String]) -> io::Error { memfd … tmpfs fallback }
#[cfg(all(unix, not(target_os = "linux")))] fn exec_agent(…) -> io::Error { tempfile_exec(agent, argv) }
#[cfg(not(unix))]                        fn exec_agent(…) -> io::Error { Unsupported("the loader only execs on unix nodes") }
```

(`crates/loader/src/main.rs:112-133`). The same shape appears for `base_page_bytes`,
`thp_enabled`, `hugetlb_reserved`, `numa_nodes` (`crates/mem/src/probe.rs:61-122`),
`apply_advice`/`apply_advice_mut` (`crates/mem/src/store.rs:223-244`), `apply_nice`,
`cgroup_cpu_budget` (`crates/cli/src/remote/agent.rs:64-115`), `dir_identity`
(`crates/cli/src/remote/discovery.rs:125-134`), `adjust_dir_separator`
(`crates/cli/src/print/colored_print/styles.rs:104-116`), `probe()` for the L2 cache size
(`crates/ann/src/encoder/cache.rs:15-104`), `retain_dirty_pages_for_batch_run`
(`crates/index/src/main.rs:26-51`). Where a single function needs a platform-specific *step*, an
inline `#[cfg]` block is used with the other arm neutralising unused bindings (`entry_is_hidden`,
`discovery.rs:208-220`; `replace_file`, `crates/kg/src/kg.rs:396-401`). Pure decision logic is kept
`cfg`-free so it is unit-tested on every host (`decide_page`, `crates/mem/src/policy.rs:49-77`;
`huge_pages_track_platform_support` test at `:254-257`).

184 `cfg` sites in `crates/` (scratch `cfg_grep.txt`), concentrated in: `ann/encoder/gemm_i8.rs`
16, `mem/store.rs` 15, `index/main.rs` 13, `ann/qmatrix.rs` 11, `mem/probe.rs` 9, `loader/main.rs`
8, `kg/ledger.rs` 8, `ann/kernels.rs` 8. Every other crate has ≤7.

### 4.2 The `cfg` vocabulary actually used

| Predicate | Used for | Examples |
|---|---|---|
| `target_os = "linux"` | huge pages / THP / hugetlb / NUMA probes, `memfd_create`+`fexecve`, cgroup CPU budget, sysfs L2 size, `/proc`-free page-fault reads | `mem/probe.rs`, `mem/store.rs:92-102`, `loader/main.rs:112-160`, `cli/remote/agent.rs:89-113`, `ann/encoder/cache.rs:78`, `kg/ledger.rs:406` |
| `target_os = "macos"` | Accelerate `cblas_sgemm` throughput GEMM, `sysctlbyname("hw.l2cachesize")`, `malloc_zone_pressure_relief`, mach task fault counters | `ann/encoder/forward.rs:99,196,365`, `ann/encoder/cache.rs:49`, `ingest/pipeline.rs:853-870`, `kg/ledger.rs:355` |
| `unix` / `not(unix)` | every `libc` call, `MetadataExt::{ino,dev}` hard-link identity, `CommandExt::process_group(0)`, `FileExt::read_exact_at` (pread), `statvfs`, `SIGPIPE` reset, `PermissionsExt::from_mode` | `cli/main.rs:44-47`, `index/autowarm.rs:109-113`, `transport/spawn.rs:30-36`, `index/lib.rs:1460-1472`, `index/lib.rs:3436-3444`, `ingest/pack.rs:1431-1445`, `mem/lib.rs:23` |
| `windows` / `target_os = "windows"` | `remove_file` before `rename` (no atomic replace-over), `FILE_ATTRIBUTE_HIDDEN`, `USERPROFILE` vs `HOME`, `\\?\` verbatim prefix stripping, `\`→`/` | `kg/kg.rs:396-401`, `ann/index.rs:656-658`, `index/annfiles.rs:128-130`, `cli/remote/discovery.rs:209-216`, `index/models.rs:29-32`, `styles.rs:110-116` |
| `target_arch = "x86_64"` / `"aarch64"` / `not(any(..))` | SIMD kernels (AVX2/FMA/AVX-512/VNNI vs NEON/dotprod) with a **scalar fallback arm always present**; `_mm_prefetch` vs `prfm` inline asm; CPUID leaf 4 / 0x8000001D | `ann/src/kernels.rs:72-234`, `ann/src/qmatrix.rs:286-583`, `ann/src/encoder/gemm_i8.rs:144-490`, `mem/prefetch.rs:11-44`, `ann/encoder/cache.rs:15-46` |
| `target_env = "msvc"` / `all(target_env = "musl", target_arch = "aarch64")` | allocator gating (§3.1) | `cli/main.rs:10`, `index/main.rs:10-16`, `mcp/main.rs:8`, `pyo3/lib.rs:8` |
| `cfg!(target_endian = "little")` (runtime `cfg!`) | zero-copy column cast vs decoded copy | `mem/pod.rs:64` |
| `target_arch = "wasm32"` | wasm-only test crate | `wasm/tests/web.rs:9` |
| `feature = …` | `jemalloc`, `alloc-ledger`, `alloc-stats`, `bench-internals`, `keygen`, `ssh`, `python`, `napi-noop-in-unit-test` | see §0 and §3 |

ISA selection is **runtime** and cached: `is_x86_feature_detected!("avx512f")` /
`is_aarch64_feature_detected!("dotprod")` in a `OnceLock` ("CPUID is read once; the answer is a
process constant", `ann/encoder/gemm_x86.rs:68-69`). The standing rule (`docs/wip/SUBSECOND.md:
36-41`): "**Platform-agnostic, correctly.** Portable baseline always present; platform fast paths
behind cfg/runtime dispatch with bit-exact-vs-baseline tests (the `dot_i8` pattern). Integer
kernels with fixed summation shape so an index built on ARM equals one built on x86 … No
fork-based snapshots (epoch read-views instead). No macOS-hugepage assumptions". CI's
`encoder-x86` job (§1.2) exists precisely because the laptop is aarch64.

### 4.3 i686 / 32-bit handling

- `i686-pc-windows-msvc` is built and shipped (CLI asset `vorpal-windows-x86.exe`; napi
  `win32-ia32-msvc`; no Python wheel), but **no test job runs on it** — it is build-only in CI.
- There is **no `target_pointer_width` cfg anywhere** in the workspace. The 32-bit story is
  handled at the *format* level instead: node/edge/name ids and heap offsets are `u32` on disk
  by design, and the ceilings are checked, not wrapped — `crates/index/src/lib.rs:1092-1110`
  (`const U32_CEIL: usize = u32::MAX as usize;` with errors "… split the corpus into multiple
  indexes"), `crates/kg/src/dataflow.rs:115`, `crates/kg/src/writer.rs:518,1034-1035`,
  `crates/ingest/src/walk_reuse.rs:481-482` (`range.start.min(u32::MAX as usize) as u32`
  saturation). `docs/wip/IMPROVEMENTS.md:447-461` ("Treat larger-than-32-bit indexing as
  conditional work"): "README documents up to `2^32 - 1` definitions, 32-bit graph/name ids, a
  4 GiB string heap, and saturated per-file byte spans. Do not make 64-bit ids, sharding,
  distributed indexes, or new codecs an immediate priority without evidence."
- Caveat for slates: `u64 → usize` casts on mmap'd lengths (`header.len as usize` in
  `wire/io.rs:48`, `frame.rs:157`; `end as usize` in `mcp/tools.rs:241`; segment offsets) are
  bounded by other ceilings (64 MiB frames, 4 GiB heaps) rather than by a `try_from`. On a
  32-bit host a >4 GiB file would truncate silently; vorpal never maps such a file because its
  own limits sit below that. Sentinel spans use `usize::MAX` (`kg/writer.rs:259`) which is
  4 GiB on i686 — still above any real file. A VFS that maps arbitrary user data must guard
  these with `usize::try_from(u64)` explicitly.

### 4.4 Page-size and cache-line assumptions

- Page size is **probed**, never assumed, on Unix (`sysconf(_SC_PAGESIZE)`, `mem/probe.rs:61-66`)
  with the doc "OS base page size in bytes (4 KiB on x86-64 Linux; 16 KiB on Apple Silicon)"
  (`:24`); Windows uses the constant 4096 with an honesty comment (`:68-73`).
- The `.vseg` header/footer are 4096 bytes (`segment/format.rs:16-17`) — a **format constant**,
  not an OS-page assumption; HOT stripes are aligned to `ALIGN = 64` "so a point lookup never
  straddles [a cache line]" (`:21-22`). Note this hard-codes a 64-byte line even though Apple
  Silicon uses 128-byte lines; the in-memory structures that matter for contention use
  `crossbeam_utils::CachePadded` ("already per-arch aware: 128B on aarch64, 64B on x86",
  SUBSECOND.md:38-39; `resolve/src/intern.rs:136-139`, `ingest/src/pipeline.rs:1982-1983`) or an
  explicit `#[repr(align(128))]` (`kg/src/ledger.rs:26`).
- Huge pages are Linux-only by construction (`HUGE_PAGES_SUPPORTED = cfg!(target_os = "linux")`);
  the L2 cache size feeding GEMM tiling is read from CPUID/sysctl/sysfs and treated as `None`
  (assume no reuse) when unenumerable ("never guessed", `ann/encoder/cache.rs:1-5`).
- Endianness: on-disk metadata is explicit little-endian everywhere (`to_le_bytes`/`from_le_bytes`,
  `put_u64`/`get_u64` helpers in `segment/format.rs`); column payloads are native-endian with
  the stated justification "all supported targets are little-endian, so these coincide"
  (`segment/format.rs:7-8`) and a big-endian decode fallback in `PodColumn` so "the numeric values
  are identical everywhere" (`mem/pod.rs:45-47`). The wire header is "explicit little-endian,
  independent of host endianness, so an arm64 pod and an amd64 coordinator interoperate"
  (`wire/src/lib.rs:12-13`).

### 4.5 `libc` vs `windows-sys` vs `rustix`

- **`libc` 0.2** is the only OS binding used directly, and only under `cfg(unix)`: declared by
  `mem` (the owner — "vorpal-mem owns the libc dependency; callers avoid growing their own",
  `mem/src/lib.rs:20-22`, re-exported as `carry_libc`), `cli` (`[target.'cfg(unix)'.dependencies]`,
  for `signal(SIGPIPE, SIG_DFL)` and `setpriority`), `loader` (`memfd_create`, `write`, `fexecve`,
  raw `read(0, …)`), and `kg` (optional, ledger only).
- Where one symbol is needed, crates declare it themselves instead of adding a dependency:
  `unsafe extern "C" { fn kill(pid: i32, sig: i32) -> i32; }` ("We avoid a `libc` dep in this
  crate by declaring the one symbol", `transport/src/spawn.rs:79-89`), `sysctlbyname`
  (`ann/encoder/cache.rs:49-63`), `malloc_zone_pressure_relief` (`ingest/pipeline.rs:862`),
  `mach_task_self_` (`kg/ledger.rs:374`).
- **No `windows-sys`/`winapi`/`rustix`/`nix` direct dependencies** (all appear only transitively
  in `Cargo.lock`). Windows behaviour is reached through `std::os::windows::fs::MetadataExt` and
  env vars; Unix through `std::os::unix::{fs::MetadataExt, fs::FileExt, fs::PermissionsExt,
  process::CommandExt, ffi::OsStrExt}`.
- Higher-level OS crates: `memmap2 0.9` (mmap + `advise`/`unchecked_advise`), `fd-lock 4`
  (cross-process advisory file lock for warms), `notify 8` (FSEvents/inotify), `ignore`
  (gitignore-aware parallel walk), `libloading 0.9` (dynamic grammars; the only `dlopen`, done at
  launch), `tempfile` (tests/dev only).
- Process hygiene patterns: restore default `SIGPIPE` so `vorpal … | head` exits quietly and
  stdio daemons die with their client (`cli/main.rs:40-47`); detached children get their own
  process group (`autowarm.rs:105-113`; `transport/spawn.rs:30-36` "Own process group so killing
  takes down any grandchildren"); agents `nice +10` themselves and cap threads to the cgroup CPU
  quota (`cli/remote/agent.rs:64-115`); atomic publish is tmp + `rename` with
  `remove_file` first on Windows.

---

## 5. Binary formats and the wire

### 5.1 Index format policy (`docs/INDEX_FORMAT.md`, 84 lines)

Store identity (`:6-13`): an index root holds `CURRENT` (a pointer file naming `gen/<content-id>`),
immutable content-addressed generation directories, and a loose `products/` bank; "A build stages
a complete new generation and commits it with one atomic pointer swap; readers see the whole old
index or the whole new one, never a mixture. GC keeps the new and prior generations."

The five policies (`:37-56`), which slates should adopt nearly verbatim for any persisted or
exported artifact:

1. "**Rebuild is the migration.** Graph segments are never migrated in place … Builds are
   bit-reproducible, so migration is exact by construction."
2. "**Caches retire by version; they are never reinterpreted.** … Version bumps are mandatory
   whenever extraction output changes shape **or semantics** (the constant's doc comment records
   the history)."
3. "**Readers fail loudly or degrade honestly — never misread.** Foreign or newer graph segments
   fail `Kg::load` with an explicit error. Optional sidecars … that are missing, stale, or foreign
   make their features answer 'unavailable' … while queries stay correct."
4. "**Additive sidecars are the only writes an existing generation admits**, and each must be
   self-validating: … stamped with the node-segment hash".
5. "**The durable identities are external ids (`eid:<32 hex>`) and the source tree**, not the
   on-disk format. Pre-1.0, the index format is explicitly not a cross-version interchange format".

The version table (`:60-78`, 15 artifacts each with its constant, source file, value, and
on-mismatch behaviour) is **generated by a test**: `crates/index/tests/format_policy.rs:1-12` —
"the table is generated from the version constants in source, so a format bump that forgets the
compatibility document is impossible. The normal run ASSERTS ONLY — a stale table fails with
instructions, never a write … To refresh the table after bumping a constant: `cargo test -p
vorpal-index --test format_policy -- --ignored regenerate`". The test parses `const NAME: u32 = N;`
lines out of the source files (`:24-42`). The same "assert-only, `--ignored regenerate`"
convention drives `docs/LANGUAGES.md` (`crates/ingest/tests/language_matrix.rs`) and
`grammars/PROVENANCE.json`.

### 5.2 The `.vseg` container (`crates/segment/src/format.rs`)

```
[4 KiB header] [column directory] [HOT stripes, 64 B-aligned] [4 KiB footer]
```

- Magics `b"VSEG0001"` / `b"VSEGEND1"`, `FORMAT_VERSION: u32 = 1`, `HEADER_LEN = FOOTER_LEN =
  4096`, `DIR_ENTRY_LEN = 64`, `ALIGN = 64` (`:12-22`); `align_up(v, align) = (v + align - 1) &
  !(align - 1)` (`:196-197`).
- Header bytes `[0..4064)` are covered by a blake3 stored in the last 32 header bytes
  (`HEADER_HASH_OFF`); each 64-byte column-directory entry carries `name_hash u64, type_tag u8,
  placement u8, [2 reserved], stride u32, data_offset u64, data_len u64, xxh3 u64, min [8], max
  [8], [8 reserved]` (`:82-124`) — zone maps and reserved bytes for forward compatibility.
- "Metadata integers are little-endian; column payloads are native-endian (all supported targets
  are little-endian, so these coincide) and read back zero-copy via `bytemuck`." (`:7-8`).
  Point lookup is "`base + row·stride`, one cache line, zero decode, zero deserialize"
  (`segment/src/lib.rs:6-8`). Whole-segment integrity is a blake3 computed with rayon at seal
  ("same digest as the serial hash by construction (blake3 is a tree hash)",
  `segment/Cargo.toml:17-19`). Logical types `U8/U32/U64/F32/Bytes`; placements `Hot/Warm/Cold`
  with only `Hot` implemented ("WARM/COLD are reserved for the codec layer").
- Loading validates magic, version, sizes (`Header::parse`, `SegmentError::TooSmall`) — a foreign
  file is a typed error, never a panic.

### 5.3 The product pack (`crates/ingest/src/pack.rs:1-55`)

Magics `VPPK`/`VPPI`/`VPPB`/`VPPT`, `PACK_VERSION = 2`, `BUCKET_VERSION = 1`. Design rules stated in
the module doc: the sidecar index is "**an optimization, not a source of truth**: a run killed
after appending but before the sidecar lands loses no work, because open() scans any records
beyond `covered_len` (bounds-checked; a torn tail record simply ends the scan) and products remain
self-validating at decode time"; buckets are `file_key & (B-1)` with `B` "a **pure function of the
live file count** … stamping B at creation would make an incremental build that grows past a
threshold diverge byte-wise from a scratch build of the same tree, violating the convergence
law"; "Buckets land `.tmp` + rename with the TOC last"; "unchanged bucket files **hard-link** into
the next generation — sealed generations are immutable, rename-over never writes through a link,
and GC of an old generation is refcount-safe"; "every publish rewrites the pack in **canonical
order** … so the published bytes are a pure function of the `(path, body)` set". Paths are
tree-relative so pack bytes are mount-independent.

### 5.4 The wire protocol (`crates/wire`)

Design goals (`src/lib.rs:5-16`): "**A node is authenticated but not trusted.** Every read
validates length against a negotiated ceiling *before* allocating and verifies a payload
checksum"; "**Version-stable identity (I3).** Hashing that crosses the wire or feeds
identity/dedup uses a fixed algorithm (`xxh3` / `blake3`), never `std::hash::DefaultHasher`";
"**Portable framing.** The 16-byte header is explicit little-endian"; "**Rules travel as
canonicalized opaque bytes + digest.**"

Frame header (`src/frame.rs:5-13`), hand-encoded byte-by-byte (not a `#[repr(C)]` Pod struct as the
design sketch in `docs/wip/REMOTE.md:149-161` shows — the implementation chose explicit
`to_le_bytes` so host layout never matters):

```
  0  magic:u16   2  version:u8  3  flags:u8
  4  channel:u16 6  msg_type:u16
  8  len:u32     12 checksum:u32
```

`MAGIC = 0x5650` ("VP"), `PROTOCOL_VERSION = 1` ("A mismatch is rejected outright (no
partial-compat matrix)"), `FRAME_HEADER_LEN = 16`, `DEFAULT_MAX_FRAME = 64 MiB`, flags `ZSTD |
STRUCTURED | CHECKSUM` (`:17-34`). `channel` multiplexes streams (0 = control); `msg_type` mirrors
the message discriminant "so a receiver can skip a frame it does not care about — or a future one
it cannot decode — purely by `len`" (`:12-13`); discriminants are "Stable: append only, never
renumber" (`src/msg.rs:23-35`).

Serialization: **postcard** bodies (`Message::encode/decode`, `msg.rs:68-76`). The rationale
(`docs/wip/REMOTE.md:139-146`): "serde is already a workspace dep … postcard is `no_std+alloc`,
tiny (keeps the pushed agent small), and its number/struct encoding is byte-stable. Rejected:
`bincode` (less wire-stable across versions), `rkyv` (archived layout is fragile across
heterogeneous arch — arm64 pod ↔ amd64 coordinator), `prost` (needs codegen; codebase is
serde-native). Bulk payloads … travel as **raw byte ranges**, never re-encoded." Message types
are "**byte-stable**: no `HashMap` appears in any type whose bytes are hashed or compared —
ordered collections (`Vec`, `BTreeMap`) only" (`msg.rs:3-5`); anything content-addressed goes
through `canon.rs` (recursive key-sorted compact JSON, `canonical_json_digest` computed "**once
over exactly these bytes**"). `hash.rs` pins the algorithms with **golden vectors** in tests
(`GOLDEN_EMPTY = 0x2d06_8005_38d3_94c2`, "a change to the hashing function … trips these").

Resource safety, enforced on both ends: `parse_frame`/`FrameReader::read_frame` check `header.len >
max_frame` **before** `vec![0u8; len]` and verify the checksum before surfacing (`frame.rs:146-172`,
`io.rs:25-59`); the **writer** enforces the same ceiling ("an oversized payload fails **here**,
with a clear error, instead of being written and then killing the session at the receiver",
`io.rs:75-80`); `WireError::Incomplete` is a control-flow signal for streaming readers, and EOF
inside a frame is an error ("truncation is never silently dropped"). Tests cover the hostile
`len = u32::MAX` header, bit-flipped payloads, bad magic/version, and truncation.

Zero-copy: not on the frame payload (it is copied into an owned `Vec<u8>`; bulk bytes are shipped
as raw ranges instead). Backpressure: protocol-level `Control::Credit { channel, frames, bytes }`
messages (`msg.rs:609`), the negotiated `Caps.max_frame`, and on the coordinator a bounded
`sync_channel(REMOTE_WINDOW = 256)` paced by the printer's drain rate (§2.4). Two transports for
one framing: blocking `io` (agent side, no tokio) and async `aio` (feature `tokio`, `io-util`
only), each ~150 lines and mirror images.

### 5.5 Transports (`crates/transport`)

The abstraction (`src/lib.rs:3-7`): "Everything a coordinator needs from a remote node reduces
to **'run argv, pipe bytes, get exit code'**. That is the [`Transport`] trait; `push_file`/
`pull_file` default over `exec`. The `dyn` boundary is coarse (control-plane granularity via
`async_trait`); the per-byte result copy runs through the monomorphized `AsyncRead`/`AsyncWrite`
halves handed back from `exec`, so boxing is amortized over whole streams, not per byte." Trait
methods: `descriptor()` (redacted `NodeDescriptor { scheme: &'static str, address }`),
`hints()`, `exec`, `exec_capture`, `push_file` (default `cat > dest && chmod`, then explicit
`flush` + `shutdown` because "a bare drop races byte delivery against pipe close"), `pull_file`
(returns a `PulledStream` that owns its process because "backends spawn children kill-on-drop, so
handing back the pipe alone dropped the last process handle and killed the remote `cat`
mid-stream"), `health_check`, `shutdown`. Backends: `SubprocessTransport` (loopback),
`CommandTransport` (kubectl/docker wrappers), `SshTransport` (russh, feature `ssh`; host-key
verifier `AcceptAny` logs "accepting UNVERIFIED ssh host key (TOFU) — pin the key to harden"
vs `Pinned(keys)`), with k8s/docker/containerd/vsock reserved in the `Backend` enum.

Negotiation (`src/negotiate.rs`): one POSIX-`sh` probe script reports OS/arch, primitive
inventory, and the first writable **and executable** landing spot (`/dev/shm` > `$XDG_RUNTIME_DIR`
> `/tmp`, tested by writing and running a tiny script — "this catches `noexec` mounts");
`decide()` fuses probe + `RemotePolicy` + `ForcedMode` into `ExecMode::Agent { landing } |
Stream`. Policy (`src/policy.rs`): `allow_push_exec`, backend and host allow/deny sets,
`max_nodes: 4096`, `max_streams_per_node: 64`, permissive by default with the reason "this is an
operator-run dev tool on a laptop-local cluster. The gate that actually matters is the explicit
`--remote <target>` opt-in". `Redacted<T>` newtype whose `Debug`/`Display` print `<redacted>`;
`TransportError` variants are "redacted by construction (they never carry credentials)".
Provisioning pushes a signed agent that the stage-0 loader verifies (Ed25519 over blake3) and
execs from a memfd (§1.3, §4.2).

---

## 6. MCP, skills, and evals

### 6.1 The MCP crate (`crates/mcp`) — a hand-written JSON-RPC server

- **No MCP SDK.** `crates/mcp/Cargo.toml:13-26` depends only on `serde`, `serde_json`, `serde_yaml`,
  `notify 8`, `ignore`, and the vorpal crates. `src/lib.rs:3-11`: "The Model Context Protocol is
  JSON-RPC 2.0, one message per line, over stdio … The protocol layer is a pure function
  ([`Server::handle_line`]: line in, optional line out), so the whole daemon is testable without a
  process; `main` is a thin stdio loop. The protocol is implemented directly on `serde_json` —
  small, dependency-light, and swappable for an SDK transport later without touching the tool
  logic." (`rmcp` is listed in ARCHITECTURE §7.8 as the eventual SDK but is not a dependency.)
- **Transport**: stdio, **newline-delimited JSON** (no `Content-Length` headers). The pump
  (`src/lib.rs:129-166`): a reader thread forwards `stdin.lock().lines()` over an
  `std::sync::mpsc` channel; the serve loop does `recv_timeout(250 ms)`, handles a line or, on
  timeout, calls `server.tick()` ("the quiet pulse" that drives proactive rebuilds); responses are
  `writeln!` + `flush` per message; sender drop on EOF ends the daemon. Blank lines are skipped.
  No tokio anywhere in the crate.
- **Dispatch** (`src/server.rs:1191-1208`, mirrored in `router.rs:63-80`): parse error → `-32700`;
  `method` matched by string: `initialize`, `ping`, `tools/list`, `tools/call`; anything else →
  `-32601 "method not found"`; **notifications (no `id`) get no response** (`let id =
  msg.get("id").cloned()?`). Responses are `{"jsonrpc":"2.0","id":…,"result":…}` or
  `error_response(id, code, message)` (`:2681`). Protocol revisions (`:27-30`):
  `PROTOCOL_VERSIONS = ["2025-06-18", "2025-03-26", "2024-11-05"]`, echoing the client's if listed,
  else the oldest ("most widely supported"). `serverInfo.name = "vorpal-mcp"`, capabilities
  advertise `tools` only — **no `resources/*` or `prompts/*` methods exist** (grep confirms).
- **Tool declaration**: hand-written `serde_json::json!` `inputSchema` objects returned by
  `tools_list(profile)` (`server.rs` ~`:2210-2560`, each property with a `description`), dispatched
  by a `match` on the tool name in `tools_call`. No `schemars`, no macros. The membership authority
  is one enum (`server.rs:41-77`): `Profile::{Full, Analysis, Scout}` with `allows(tool)` — "The
  single authority on membership: tools_list filters by it and run_tool gates on it, so the
  advertised surface and the callable surface can never drift apart." `SCOUT = [node, search,
  snippet, schema, fetch_span]`; `ANALYSIS_EXTRA` adds callers, references, importers,
  implementors, type_users, similar, reachable, why, health, dead_code, coverage, impact,
  compare_generations, architecture, code_search, data_flow, observed, query; `Full` adds index,
  structural_search, rule_search, ast_dump (~28 tools; per-tool descriptions in `docs/mcp.md:
  101-163`). The multi-project router injects a `project` property into every schema and adds
  `list_projects` (`router.rs:82-117`).
- **Result shape**: `{"content":[{"type":"text","text":…}], "structuredContent": {records… |
  code}, "isError": bool}` (`router.rs:206-220`). Pagination contract (`server.rs:2565-2568`):
  "results are deterministic vectors, `cursor` is an opaque `o:<offset>` into that order, …
  `total`, `truncated`, and `nextCursor` when more remain". Stable error codes ride in
  `structuredContent.code` (`bad-argument`, `stale-source` — the latter typed as
  `FetchSpanError::Stale`, `tools.rs:204-211`, "so the MCP envelope can carry its stable error
  code without string matching").
- **Named constants** (relevant to slates' no-magic-numbers rule): `BACKSTOP_OVERHEAD_INVERSE:
  u32 = 100` (`server.rs:218`; watcher liveness backstop runs when `elapsed >= cost × 100`),
  `MAX_CAPTURED_CHANGES = 4096` (`watch.rs:32`), `TEXT_ROWS = 200`, `TEXT_CAP = 200`
  (`server.rs:1980,2161`), `max_bytes` "default 16384, clamp 64..262144" (schema text `:2515`),
  pump period 250 ms (`lib.rs:148`; the only unnamed literal), quiet threshold "half a second"
  (`docs/mcp.md:208`), `VORPAL_MCP_BUILD_TIMEOUT_S` default 1800 (`supervised.rs:47-52`).
- **Freshness** (`src/watch.rs:1-19`): a `notify` recursive watch marks an `AtomicBool` dirty flag;
  "The watch is a **necessary-condition filter** …: it may only skip revalidation when nothing
  relevant can have changed, and every doubt fails open to revalidation — the flag starts dirty …
  watcher errors and event overflows mark dirty, and a failed rebuild re-marks dirty"; a documented
  FSEvents silent-non-delivery hazard is closed by the amortized backstop.
- **Crash isolation** (`src/supervised.rs:1-15`): "the daemon runs the indexer as a **child
  process**, so one pathological file — a scanner segfault, a runaway allocation, an OOM kill —
  costs one build attempt and an error string, never the server … only the atomic `CURRENT` swap
  publishes the child's work." Binary discovery: `VORPAL_INDEX_BIN`, else own exe if it is
  `vorpal`/`vorpal-index`, else a sibling `vorpal-index` "in the SAME directory only (never parent
  dirs: a test binary under target/debug/deps must not discover target/debug/vorpal-index and
  start spawning it)". A failed/timed-out child is an `Err` and "the caller must NOT retry
  in-process".
- **Security posture**: enrollment of servable roots is human-only through the CLI (`registry.rs:
  3-8`: "a confirmation delivered through the MCP surface would be answered by the same agent that
  may have been influenced — so the surface never gets the question"); the registry is
  `~/.config/vorpal/projects.yml` written tmp+rename; retargeting an enrolled name is refused;
  the only `dlopen` happens in the launcher before serving (`lib.rs:53-55`, `docs/mcp.md:186-195`).
- **Tests**: `tests/protocol.rs` (701 lines, in-process `Server::handle_line` end-to-end on a temp
  tree), `tests/stdio.rs` (spawns the real binary via `env!("CARGO_BIN_EXE_vorpal-mcp")` and
  speaks JSON lines), `tests/watch.rs`, `tests/projects*.rs`, `tests/live_differential*.rs`.

### 6.2 Installing into agent clients (`crates/cli/src/mcp_install.rs`)

`vorpal mcp install [--client …] [--command …] [--dry-run]` (`:1-5`): "idempotent JSON edits with a
timestamped backup before any modification, `--dry-run` to preview, and an explicit report of
every file touched or skipped. Project-scoped files are preferred wherever the client supports
them". Targets (`:39-87`): Claude Code `./.mcp.json` (`mcpServers`), Claude Desktop
`~/Library/Application Support/Claude/claude_desktop_config.json` (global), Cursor
`./.cursor/mcp.json`, VS Code `./.vscode/mcp.json` (key `servers`), Windsurf
`~/.codeium/windsurf/mcp_config.json` (global). Global targets are skipped unless the client's
config dir exists. The entry written is `{"command": <absolute path of this exe>, "args": ["mcp"]}`
("survives PATH-less launchers"). `docs/mcp.md` (220 lines) covers: setup (fast path + manual
Claude Code/Desktop JSON, "Use an absolute path"), how freshness works, profiles table,
multi-project daemon, per-tool descriptions grouped Build&health / Repo shape / Graph navigation /
Search / Evidence, example asks, troubleshooting, custom languages, supervision.

### 6.3 Skills

Nine Claude Code skills live in `.claude/skills/<name>/SKILL.md` (28–62 lines each): `vorpal-mcp`,
`vorpal-search`, `vorpal-index`, `vorpal-graph`, `vorpal-query`, `vorpal-outline`, `vorpal-rules`,
`vorpal-structural`, `vorpal-semantic`. Conventions:

- Frontmatter is exactly `name` + `description` (no `allowed-tools`, no version); the description
  is one long sentence naming the capability, the CLI surface, and "Use when …" triggers.
- Body: an H1, a fenced usage line (`vorpal graph <VERB> [NAME] [--index DIR] [filters...]`), then
  tables of verbs/flags, a "Recipes" section of one-line commands, "Pitfalls" or "Output" notes,
  and a pointer to the deeper doc (`Full integration guide with per-tool arguments: docs/MCP.md`).
- They teach the **CLI first**; MCP tool names are listed only in `vorpal-mcp` (profiles table,
  "The tools, briefly"). Outputs are described as "byte-stable (safe to diff in scripts)" with
  `--format` records envelopes for machines.
- **No installer, no packaging, no skills-over-MCP**: the only distribution is the repo itself —
  `examples/README.md:49-50`: "Claude Code skills for all of this ship in `.claude/skills/` — open
  this repo in Claude Code and ask it to search, scan, or explore the graph." Nothing in
  `crates/cli` or `crates/mcp` references skills, and the MCP server exposes no `prompts/list` or
  `resources/list`. (Slates' brief asks for skills-over-MCP and raw skills; vorpal supplies only
  the raw-skill precedent.)

### 6.4 How MCP tool quality is evaluated

- `evals/mcp_linux_eval.py` (169 lines): drives the **installed** `vorpal mcp --index …` over stdio
  with hand-rolled JSON-RPC (`initialize` with `protocolVersion 2024-11-05`, then
  `notifications/initialized`, then `tools/call`), measures per-call latency, and grades each
  answer against independent ground truth — `rg` over the Linux tree, byte-exact source lines, or
  an injected edit — printing a PASS/FAIL scorecard. Its first check is a contract test: "the
  no-fake-edges contract: kmalloc_slab is `static inline` in mm/slab.h, so cross-file callers are
  MASKED (counted, never guessed). PASS = the node exists and callers is honestly empty — if this
  ever 'improves' without include-graph awareness, it means edges are being faked."
- `evals/repo_eval.py` (184 lines): for ~10 real repositories (cpython, django, next.js,
  kubernetes, actix-web, laravel, folly, kafka, …) "index it twice with the release-built CLI
  (content-id determinism is a hard gate), record timing and graph shape, summarize parse health,
  and run probe checks graded against the repo itself".
- `cargo xtask eval` (`xtask/src/eval.rs:1-17`): a fixed question suite over the vorpal repo answered
  by vorpal vs a grep-and-open-five-files baseline, scoring invocations, bytes returned, wall
  time, correctness; `--write` regenerates the marked section of `docs/wip/BENCHMARKS.md` "so the
  published table always carries the exact command that produced it".
- `cargo xtask searcheval <index> <labels.json> [--overlap] [--root]` (`xtask/src/searcheval.rs:
  1-24`): NDCG@10 / MRR / recall@5 with standard IR definitions ("no house variants"), labels as
  data in `xtask/labels/*.json` each with an `.evidence.md` sidecar "citing the source line that
  proves every grade-3 answer", every label existence-checked first, a double-run determinism gate,
  and the index "measured AS IT IS: absent/stale warm tiers are reported … never built behind the
  caller's back".

---

## 7. Bindings: napi, pyo3, wasm

### 7.1 Node (`crates/napi`, package `@hyper-light/vorpal-node`)

- **Stack**: `napi = { version = "3.7.0", features = ["serde-json", "napi4", "error_anyhow"] }`,
  `napi-derive 3.4.0`, build-dep `napi-build 2.2.2` (`build.rs` is the one-liner
  `napi_build::setup()`), `crate-type = ["cdylib"]`, `publish = false`. The `napi-noop-in-unit-test`
  feature (`Cargo.toml:32-34`, "this feature is only for cargo test to avoid napi_ symbol undefined
  error — see napi-rs/napi-rs#1005, #1099, #1032") turns the derive into a no-op; `src/lib.rs:1`
  gates the whole crate with `#![cfg(not(feature = "napi-noop-in-unit-test"))]` and async fns carry
  `#[cfg_attr(test, allow(dead_code))]`.
- **Async model**: napi's `AsyncTask<T: Task>` on the **libuv thread pool** — no tokio, no
  `ThreadsafeFunction` for results. `src/repo_async.rs:1-7`: "every blocking operation as an
  `AsyncTask` computing on libuv's thread pool, so an `indexBuild` of a multi-million-line tree
  never freezes the event loop. Naming follows the parser precedent (`parse` / `parseAsync`): each
  sync function in [`crate::repo`] gains an `Async`-suffixed twin returning a `Promise`. The
  `Index` class stays synchronous by design — its queries answer in well under a millisecond from
  a pinned, mmapped generation, so a Promise would cost more than the call." One generic
  `RepoTask<T> { work: Option<Box<dyn FnOnce() -> Result<T> + Send>> }` implements `Task`
  (`compute` runs the closure, `resolve` returns it); a `Json(serde_json::Value)` newtype supplies
  `TypeName`/`ToNapiValue` so JSON results ride the same task. Streaming callbacks
  (`find_in_files`) take `Function<Vec<SgNode>, ()>` and run the walker inside `Task::compute`
  with an `AtomicU32` file counter (`find_files.rs:45-56`). Errors cross via `napi::Error`
  (`error_anyhow`).
- **Shared state**: `Index { kg: Arc<Kg> }` (§2.1); language modules generated by a macro
  (`impl_lang_mod!(html|js|jsx|ts|tsx|css)`, `lib.rs:29-73`) because the napi build only compiles the
  `napi-lang` grammar subset (`crates/language/Cargo.toml:121-126`).
- **Packaging** (`crates/napi/package.json`): `"napi": { "binaryName": "vorpal-napi", "targets": [
  x86_64-unknown-linux-gnu, x86_64-pc-windows-msvc, i686-pc-windows-msvc, aarch64-apple-darwin,
  aarch64-pc-windows-msvc, aarch64-unknown-linux-gnu, aarch64-unknown-linux-musl,
  x86_64-unknown-linux-musl ] }` (no Intel macOS), `engines.node >= 10`, `files: [index.d.ts,
  index.js, types/*.ts, lang/*.ts]`; scripts `build = napi build --no-const-enum --dts ignore.d.ts
  --platform --release`, `prepublishOnly = napi prepublish -t npm --no-gh-release`, `artifacts =
  napi artifacts`, `version = napi version`, `test = tsc --noEmit && ava`, `typegen = tsimp
  scripts/generateTypes.ts`; devDeps `@napi-rs/cli 3.7.2`, `ava 7.0.0`, `typescript 6.0.3`,
  `oxlint 1.71.0`, `dprint 0.55.1`, `tsimp`. `index.js` is the napi-generated loader with musl
  detection (`/usr/bin/ldd`, `process.report`, child process). Per-platform packages live in
  `crates/napi/npm/<target>/package.json`; CI (§1.6) uses Node 24 and OIDC trusted publishing.
  Hand-written `.d.ts` in `types/` is concatenated by `scripts/generateTypes.ts`.
- **Docs floor** (`docs/typescript.md`): install `npm install @hyper-light/vorpal-node`; "Every
  blocking repository call has an `Async`-suffixed twin returning a `Promise` that computes on
  libuv's thread pool"; `Index` sync methods "read from the pinned, mmapped generation in well
  under a millisecond".

### 7.2 Python (`crates/pyo3`, package `vorpal-py`, module `vorpal_py`)

- **Stack**: `pyo3 = { version = "0.29.0", optional = true, features = ["anyhow", "py-clone"] }`,
  `pythonize 0.29.0`, `crossbeam-channel`; the `python` feature = `["pythonize", "pyo3",
  "pyo3/extension-module", "pyo3/abi3-py39"]` — **abi3, CPython ≥ 3.9, one wheel per platform**.
  `Cargo.toml:33-35`: "uncomment default features when developing pyo3" (`# default = ["python"]`).
  `crate-type = ["cdylib"]`, `publish = false`, `src/lib.rs:1-2` `#![cfg(not(test))]
  #![cfg(feature = "python")]`. jemalloc off-MSVC (§3.1). CI lints it separately
  (`cargo clippy -p vorpal-py --features python`).
- **Async model** (`src/async_bridge.rs:1-18`, quoted in §2.1/§2.3): a Rust-owned, lazily grown,
  bounded `std::thread` pool (`VORPAL_ASYNC_WORKERS` or 8× cores) — **no tokio, no
  pyo3-async-runtimes, no asyncio executor**. `dispatch(py, work)` (`:153-187`): get the running
  loop, `loop.create_future()`, return the future immediately; submit the boxed job; the worker
  runs it GIL-free, then `Python::attach(|py| …)` (pyo3 0.29 API) and schedules a cached
  `PyCFunction` resolver via `loop.call_soon_threadsafe(resolver, fut, payload, is_exc)` — "the
  sole thread-safe way to touch a Future from off-loop. A cancelled Future is left untouched."
  `search_many` batches N jobs behind one future (`Arc<Batch>`, §2.1). Every module-level function
  has an awaitable twin (`build`, `build_report`, `search`, `search_many`, `node`, `graph`,
  `search_ranked`, `tune`, `install`, `enable`) and `Index` mirrors `*_async` methods.
- **Packaging** (`crates/pyo3/pyproject.toml`): `[build-system] requires = ["maturin>=1.1,<2.0"]`,
  `requires-python = ">=3.9"`, `[tool.maturin] features = ["python"]` (bindings default to pyo3;
  the module name comes from `[lib] name = "vorpal_py"`), optional `test = ["pytest >= 7"]`;
  Python-side package `vorpal_py/__init__.py` re-exports the native symbols and adds `TypedDict`
  rule types, `py.typed`, and `vorpal_py.pyi` stubs (including `async def` signatures); pytest
  suites in `crates/pyo3/tests/*.py`. Wheels: `PyO3/maturin-action@v1` per platform (§1.7),
  `manylinux: auto` / `musllinux_1_2`, sdist, PyPI via token. `docs/python.md:7-8`: "Wheels are
  published for CPython 3.9+ on macOS, Linux (manylinux + musllinux), and Windows. The import name
  is `vorpal_py`."
- Root `pyproject.toml` separately publishes the **CLI binary** to PyPI as `vorpal-cli` with
  `bindings = "bin"`, `strip = true` (§1.7).

### 7.3 WASM (`crates/wasm`, package `@hyper-light/vorpal-wasm`)

- **Stack**: `wasm-bindgen = "=0.2.126"` (exact pin — the reason is recorded next to the `wgpu` pin
  in `crates/ann/Cargo.toml:24-27`: wgpu 30.0.1 "raised its (wasm32-only, optional) wasm-bindgen
  floor to 0.2.127, which the workspace's `crates/wasm` pins at =0.2.126 and the resolver unifies
  platform-independently"), `wasm-bindgen-futures = "=0.4.76"`, `serde-wasm-bindgen 0.6.5`,
  `js-sys 0.3.83`, dev `wasm-bindgen-test 0.3.42`; `crate-type = ["cdylib", "rlib"]`;
  `vorpal-core`/`vorpal-config` with `default-features = false` (no tree-sitter C runtime — parsing
  is delegated to `web-tree-sitter` via JS). Grammars are registered at runtime from `.wasm`
  parsers (`registerDynamicLanguage`), so the package "has no predefined language support".
- **Async**: `pub async fn` exported through `wasm-bindgen-futures` (`initialize_tree_sitter`,
  `register_dynamic_language`, `src/lib.rs:18-40`); JS-visible TypeScript is added with
  `#[wasm_bindgen(typescript_custom_section)]` and `skip_typescript`. `Rc`, not `Arc` (§2.1).
- **Packaging**: `wasm-pack build --scope hyper-light --release --target bundler --out-dir pkg`
  then `scripts/patch-pkg.mjs` (adds `peerDependencies: { "web-tree-sitter": "^0.26.0" }` copied
  from the root package.json and normalises `repository.url` to `git+…git`); CI renames the
  package with an inline `node -e` and publishes `crates/wasm/pkg` with OIDC. Tests: `wasm-pack
  test --node` (`tests/web.rs` is `#![cfg(target_arch = "wasm32")]`) and `ava` over
  `__test__/*.spec.mjs`.

### 7.4 Version floors and publish channels, consolidated

| Surface | Floor / pin | Where |
|---|---|---|
| Rust | MSRV 1.98, toolchain pinned 1.98.0 | `Cargo.toml:20`, `rust-toolchain.toml` |
| Node runtime | `>= 10` (napi + platform pkgs), `>= 12.0.0` (CLI wrapper); CI builds/publishes with Node 24; npm ≥ 11.5.1 for trusted publishing | `crates/napi/package.json:35-37`, `npm/package.json:11-13`, workflows |
| napi | `@napi-rs/cli 3.7.2`, `napi 3.7.0`, `napi-derive 3.4.0`, `napi-build 2.2.2` | `crates/napi/{package.json,Cargo.toml}` |
| Python | CPython ≥ 3.9 via `abi3-py39`; `maturin >=1.1,<2.0`; CI host Python 3.11 | `crates/pyo3/{Cargo.toml,pyproject.toml}`, `publish-python.yml` |
| pyo3 | 0.29.0 (`Python::attach`, `IntoPyObjectExt`) | `crates/pyo3/Cargo.toml:28` |
| wasm | `wasm-bindgen =0.2.126`, `web-tree-sitter ^0.26.0` peer | `crates/wasm/{Cargo.toml,package.json}` |
| CLI to PyPI | `vorpal-cli` via maturin `bindings = "bin"` | root `pyproject.toml` |
| CLI to npm | `@hyper-light/vorpal-cli` + 8 platform packages, postinstall hard-link | `npm/` (§1.5) |
| CLI to GitHub | raw per-platform binaries + `cargo-binstall` metadata | `release.yml`, `crates/cli/Cargo.toml:78-103` |

Runnable examples (`examples/README.md`): six CLI shell scripts, four Python files (`04_async_
pipeline.py` shows `build`/`search`/`search_many`/`graph`), four Node `.mjs` files (`03-graph-
walk.mjs` the pinned `Index` class; `04-find-in-files.mjs` streaming), and a single-file wasm
browser playground. "Each file states its own prerequisites at the top."

---

## 8. Benchmarks, tests, and design-doc conventions

### 8.1 Benchmark harness — release binaries, exact commands, no `criterion`

There is **no criterion/divan/`cargo bench`** in the workspace (`Cargo.lock` has none of them; no
`benches/` directory exists). Performance is measured by running the **release binaries** under
`/usr/bin/time -l` with the exact command recorded, and by purpose-built examples and xtasks:

- The contract, `docs/wip/BENCHMARKS.md:1-9`: "Reproducible measurements only (release builds,
  stated machine state). Every number below was produced by the exact command shown, on the stated
  hardware and dataset commits … Numbers are honest points, not marketing: re-run them on your
  hardware; the commands are the contract. `VORPAL_NO_AUTOWARM=1` is set throughout so background
  warms never blur a measurement, and every timed run was taken with the 1-minute load average
  below 3 (a loaded machine doubled cold wall time with the same user CPU — those runs were
  discarded, not averaged)." Hardware/toolchain/dataset commits are listed once (`:11-17`) and
  definitions (cold, warm-unchanged, one-file update, tiers warm) once (`:19-26`).
- What is measured: cold / one-file / touch / unchanged wall (README `:167-181`: kernel 8.2 s cold,
  0.5 s edit, 0.13 s unchanged), peak RSS and page faults (`/usr/bin/time -l`), user/sys split,
  allocator events (`alloc-ledger`), interleaved base/new A/B tables, retrieval quality
  (NDCG/MRR/recall via `searcheval`), agent-task efficiency (`xtask eval`), GEMM rate
  (`cargo run --release -p vorpal-ann --example gemm_bench -- 4690 3`), and generation-id
  determinism.
- Measurement seams are gated features, never defaults: `bench-internals` on `vorpal-index`
  (`Cargo.toml:58-61`) unlocks `Searcher::open_exact`, the `bench` module, and the `[[example]]`s
  `sweep_semantic`/`sweep_encoder` with `required-features = ["bench-internals"]`; `alloc-ledger`
  for churn attribution; `VORPAL_PHASE_TRACE=1` prints phase stamps that `ledger_deltas.py`
  diffs. Examples double as probes: `crates/index/examples/{content_id_sweep, overlay_probe,
  parse_probe, product_hashes, replay_probe, sweep_cost}.rs`, `crates/ann/examples/{gemm_bench,
  gpu_gemm_probe, overlay_recall_probe}.rs`, `crates/ingest/examples/{snapshot_mass,
  structural_coverage, tree_cache_bench, walk_split}.rs`.
- **Perf gates in CI**: none are numeric thresholds. CI records the x86 GEMM rate (`ci.yml:60-61`)
  as a datum; correctness-side gates are tests: retrieval floors pinned in `retrieval_eval`
  ("paraphrase / sparse-name / conjunctive are PINNED at their honest lexical floors so any
  movement is loud", BENCHMARKS.md:220-224), double-run determinism gates, and the release-time
  `scripts/convergence_battery.sh` ("Exit: non-zero on ANY convergence failure").
- Reporting idiom: each optimization is a dated "Pass N — title (YYYY-MM-DD)" subsection with
  hypothesis, A/B table, verdict ("measured-and-rejected" is a first-class outcome, e.g.
  `oversize_threshold:0`, "Pass 14 — root-scratch env seeding: measured-and-null, reverted"), and
  the exact knob/command. Numbers in code comments cite the pass (e.g. `vamana.rs:444-446`,
  `tree_cache.rs:29-44`).

### 8.2 Test style

- Volume: 1,320 `#[test]`, 24 `#[tokio::test]` (wire `aio`, transport), 14 `#[ignore]`, 3
  `compile_fail` doctests (the `Interner` lifetime brand, EMBEDDING.md:17-20). No `proptest`,
  `quickcheck`, `insta`, `loom`, or `miri` in use (ARCHITECTURE §7.8 names `loom`/`shuttle`/`miri`
  as intended for the lock-free glue — not yet adopted).
- Layout: unit tests in `#[cfg(test)] mod tests` beside the code; integration tests in
  `crates/<crate>/tests/<topic>.rs` (19 crates have a `tests/` dir; `crates/index/tests` has 31
  files, `crates/cli/tests` 7); process-level tests spawn the real binary through
  `env!("CARGO_BIN_EXE_<name>")` (`crates/mcp/tests/stdio.rs:23`); temp trees are
  `std::env::temp_dir().join(format!("vorpal-<tag>-{}", std::process::id()))` and removed at the
  end (`tempfile` only in the CLI dev-deps); the CLI uses `assert_cmd` + `predicates`.
- Recurring test *kinds* (worth naming in slates' conventions):
  - **Oracle tests** — a serial "specification" implementation kept in the test module and the
    parallel/sharded implementation must equal it (`ingest/src/pipeline.rs:1843-1845`: "The serial
    specification: single-pass insertion … The sharded build must produce an equal table.").
  - **Non-vacuity counters** — a fast path exports an atomic the test asserts moved, "so a
    silently-dead reuse path can never masquerade as a passing oracle" (`walk_reuse.rs:749-754`).
  - **Byte-identity / determinism gates** — build twice, compare generation ids and `diff -rq`
    (BENCHMARKS.md:235-243; `incremental_replay`, `multi_phrase` double-run gates).
  - **Golden vectors** for anything hashed on the wire (`wire/src/hash.rs:46-58`).
  - **Doc-truth tests** — assert-only checks that a generated table in docs matches source
    constants, with an `--ignored regenerate` writer (`format_policy.rs`, `language_matrix.rs`,
    `grammar_provenance.rs`): "a test that mutates the working tree collides with concurrent
    sessions and read-only checkouts".
  - **Hostile-input tests** on every parser of external bytes (frame with `len = u32::MAX`,
    truncated headers, bit flips, foreign magic).
  - **Differential tests** against a pinned upstream binary, env-gated and "skip loudly"
    (`crates/cli/tests/differential.rs:5-8`: "Without it the tests **skip loudly** (they print the
    skip and pass) rather than fail on machines without the pin").
  - **Corpus tests** — 5,787 upstream tree-sitter cases in 0.65 s with per-skip written reasons
    (BENCHMARKS.md:217-219).
  - **Snapshot tests** exist only as a product feature (`vorpal test` for rule YAML,
    `crates/cli/src/verify/snapshot.rs`), not as a Rust test technique.
- Tests never write outside temp dirs; env-var mutation is `unsafe` with a SAFETY note
  (`registry.rs:133-134`); every test that depends on hardware or network is gated
  (`workflow_dispatch`-only weights download, `VORPAL_ASTGREP_BIN`).

### 8.3 Design-doc conventions in `docs/wip/`

- **Two tiers of docs**: `docs/*.md` is user-facing and short (getting-started 150 lines, mcp 220,
  python 173, typescript 155, INDEX_FORMAT 84, LANGUAGES 70 generated); `docs/wip/*.md` are living
  design/measurement records (ARCHITECTURE 1,065 lines, BENCHMARKS 3,802, SUBSECOND 1,415, REMOTE
  736, IMPROVEMENTS 531, ENCODER_RESEARCH 433, IMPROVEMENT_PLAN 418, SEMANTIC_TIER 331, ANN_FRONTIER
  271, UPSTREAM 268, ADOPTION 174, EMBEDDING 71, RELEASING 32).
- **Numbered sections are load-bearing identifiers**: code comments cite them (`§7.5`, `§9.1`,
  `IMPROVEMENTS #7`, `ADOPTION #29`, `D3`/`D4`/`D5` (decisions), `F-M4`/`F-M6`, `G-M1`, `P4.5c`
  (plan slices), invariants `I1`–`I4` in REMOTE.md:37-56). ARCHITECTURE opens with "Vision &
  non-negotiables" and "Locked decisions"; REMOTE with "Load-bearing invariants (violate any of
  these and results silently diverge)" and "Decisions (locked)".
- **Status blockquotes** inside design sections record what is implemented vs planned
  (ARCHITECTURE §7.5 "> **Status:** …"); IMPROVEMENTS items carry "Done when" bars; ADOPTION is
  P0–P4 with S/M/L/XL effort tags and an "Explicitly not taking" list.
- **Every claim carries a number, a date, and a command**; rejected experiments are kept
  ("measured-and-rejected", "Prototype kept in scratch"); ledgers (UPSTREAM.md) are tables of
  `commit | change | status` with a written reason per divergence.
- **Docs that can drift from code are generated by tests** (INDEX_FORMAT version table, LANGUAGES
  matrix, provenance JSON) and the release checklist lives next to the xtask that implements it
  (RELEASING.md ↔ `cargo xtask release-artifacts`).
- Code-comment style matches: paragraph-length `//!` module docs stating the design and its
  measured justification; `///` on every constant explaining the value; inline comments that
  name the alternative that was tried and why it lost.

---

## 9. Actionable conventions checklist for slates

Each line is something vorpal does that slates should copy (or, where marked ⚠, deliberately do
differently), with the evidence section.

**Workspace and toolchain**
- [ ] `edition = "2024"`, `rust-version = "1.98"`, `rust-toolchain.toml` pinning the exact
      `channel = "1.98.0"` with the reproducibility comment; repeat the pin in every workflow with
      `# keep in sync with rust-toolchain.toml` (§1.1).
- [ ] Workspace layout `crates/<role>/` + `xtask/`; package names `slates-<role>`; the shipped
      binary crate named for the product; bindings crates `publish = false` (§0).
- [ ] One `[workspace.package]` version; internal deps `{ path, version }`; shared third-party
      versions in `[workspace.dependencies]`; exact `=` pins only with a paragraph of reason (§0).
- [ ] `[profile.release] lto = true`; consider adding `codegen-units = 1`/`panic = "abort"` only
      after measuring — vorpal did not (§0).
- [ ] `rustfmt.toml` `tab_spaces = 2`, `.editorconfig` (2-space, LF, 4-space for Python),
      `.pre-commit-config.yaml` (fmt, check, clippy `-D clippy::all`) (§1.1).
- [ ] ⚠ Add `cargo fmt --check` to CI (slates has no inherited code; vorpal's reason for omitting
      it does not apply) but keep the lesson: never leave a red-forever check (§1.2).
- [ ] `[workspace.lints]` with a **short**, commented allow list; every crate `[lints] workspace =
      true`; CI `cargo clippy --workspace --all-targets -- -D warnings` plus a separate clippy pass
      for any feature-gated binding crate (§1.2, §3.5).
- [ ] `cargo xtask` alias in `.cargo/config.toml`; xtask holds release/version-bump/schema/eval
      tasks and the `release-artifacts` checksum+provenance+signature step (§1.3).

**Shared state (the Arc rule)**
- [ ] Write the policy down in the architecture doc exactly as ARCHITECTURE §7 does: hot paths
      (per-item/per-node/per-request data) are `Arc`-free; handles not pointers; scoped threads
      that borrow `&` config; single-writer-per-shard; bounded queues move owned items; RCU /
      epoch pinning for readers of immutable mapped data (§2.0).
- [ ] Every remaining `Arc` gets a comment naming the two `'static` owners, in the style of
      `transport/src/spawn.rs:46-47` and `pyo3/src/async_bridge.rs:263-265`; dependency-imposed
      `Arc`s (russh, wgpu callbacks) are noted as such (§2.1).
- [ ] Prefer `&'static` via `OnceLock`/`Box::leak` for process-lifetime singletons (watch flags,
      default rule sets) rather than `Arc` (§2.1 rows marked **Y**).
- [ ] Locks: `std::sync` only; recover from poisoning (`unwrap_or_else(PoisonError::into_inner)`);
      batch work per thread and lock once (manifest `Flush` pattern); shard + `CachePadded` when a
      lock is measured hot; count contention under a profiling feature (§2.2).
- [ ] Atomics: `CachePadded` every hot counter; shard profiling counters per thread with
      `#[repr(align(128))]`; single atomics only on paths that already blocked; `Relaxed` for
      statistics, `Acquire/Release` for flags (§2.3).
- [ ] Read every `SLATES_*` env knob once into a `OnceLock` (§2.3).
- [ ] tokio only at the I/O edge (server/transport crates), constructed on a dedicated thread,
      bridged to sync engines by bounded channels; core crates have no async dependency; the
      protocol crate's tokio support is an optional feature with `io-util` only (§2.4).

**Allocation**
- [ ] jemalloc as the **binary's** allocator, cfg-gated `not(any(target_env = "msvc", all(target_env
      = "musl", target_arch = "aarch64")))`, with the dependency gated identically; compiled-in
      `_rjem_malloc_conf` via the `SyncPtr` static; library crates never set an allocator (§3.1).
- [ ] Vendor the allocator source under `vendor/` through `[patch.crates-io]` only when a measured
      bug forces it, and keep a sync ledger row (UPSTREAM.md style) with pin, delta, and reason (§3.2).
- [ ] Decide decay/purge policy per process lifetime (batch vs daemon) and document the A/B numbers
      next to the knob (§3.1). ⚠ For slates' <50 µs provisioning target, measure page-fault cost of
      first touch (vorpal's fault-economics pass is the template).
- [ ] Keep an opt-in `alloc-ledger`-style feature (wrapping allocator + sharded counters + phase
      stamps) that is never a default (§3.2).
- [ ] Build a `mem`-style substrate crate: probe → policy → store, pure decision functions testable
      everywhere, huge pages Linux-only, `PodColumn`-style zero-copy typed views with validated
      alignment/bounds and a big-endian fallback, per-worker `bumpalo` arenas reset per batch,
      software prefetch as hints only (§3.3).
- [ ] No magic numbers: every literal is a `const` with a doc comment stating the measurement (or
      derivation) and the env override, per SUBSECOND rule 3 (§3.4).
- [ ] `unsafe` per edition-2024 idiom with a `// SAFETY:` line on every block; no `deny(unsafe_code)`
      but no bare `unsafe` either; typed errors instead of panics; `expect()` only for statically
      impossible failures (§3.5).

**Cross-platform**
- [ ] Target matrix exactly: `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-
      {gnu,musl}`, `aarch64-unknown-linux-{gnu,musl}`, `x86_64-pc-windows-msvc`, `aarch64-pc-windows-
      msvc`, `i686-pc-windows-msvc`; native arm64 runners (`ubuntu-24.04-arm`), musl via `docker run
      rust:<ver>-alpine` with `+crt-static` for binaries (`-crt-static` for cdylibs) and an `ldd`
      staticness gate (§1.3).
- [ ] Release assets named `<bin>-<os>-<arch>[.exe]` as raw binaries + `cargo-binstall` overrides;
      `SHA256SUMS`, `provenance.json`, optional ed25519 signature (§1.3).
- [ ] npm: wrapper package with `optionalDependencies` per platform (`os`/`cpu`/`libc` fields),
      `detect-libc`, postinstall hard-link with copy fallback, JS shim kept for Windows; OIDC
      trusted publishing (stub-publish each package once first) (§1.5).
- [ ] Platform code as paired `#[cfg]` functions with identical signatures; pure logic kept
      cfg-free and unit-tested on every host; SIMD behind runtime detection cached in `OnceLock`
      with a scalar arm always present and bit-exact tests (§4.1, §4.2).
- [ ] Probe page size (`sysconf`), never assume; `CachePadded`/`repr(align(128))` for contended
      lines; document any hard 64-byte alignment as a format constant (§4.4).
- [ ] ⚠ Guard every `u64 → usize` on mapped sizes with `usize::try_from` — slates maps user data;
      vorpal relies on format ceilings instead. Decide explicitly whether i686 is build-only (as in
      vorpal) or tested (§4.3).
- [ ] `libc` only under `cfg(unix)` and owned by one crate; declare single `extern "C"` symbols
      rather than adding deps; `std::os::{unix,windows}` extension traits for the rest; tmp+rename
      publish with `remove_file` first on Windows; own process group for detached children;
      default `SIGPIPE` in CLIs (§4.5).

**Formats and wire**
- [ ] Adopt INDEX_FORMAT's five policies for every persisted/exported artifact; magic + `u32`
      version in every file; readers fail typed on foreign versions; sidecars self-validating; a
      docs table generated from source constants by an assert-only test with `--ignored regenerate`
      (§5.1).
- [ ] Container shape: page-sized header/footer, 64-byte directory entries with reserved bytes,
      cache-line-aligned hot stripes, LE metadata, blake3 over the header and over the whole file at
      seal, zero-copy `bytemuck` views (§5.2).
- [ ] Wire: 16-byte explicit-LE hand-encoded header (magic, version, flags, channel, msg_type,
      len, checksum); postcard bodies; append-only discriminants; length checked before allocation
      on both reader and writer; checksum before decode; `Incomplete` as control flow; golden
      hash vectors; canonical JSON for anything digested; blocking and async transports as mirror
      modules with the async one feature-gated (§5.4).
- [ ] Transport trait = "run argv, pipe bytes, get exit code"; redacted descriptors and errors;
      least-privilege policy object; negotiation probe with typed outcomes (§5.5).

**MCP and skills**
- [ ] Protocol layer as a pure `handle_line` function over `serde_json`, tested in-process and via
      the real binary; stdio newline-delimited JSON; reader thread + `recv_timeout` pump for
      background ticks; notifications get no reply; `-32700`/`-32601` codes; protocol-version echo
      with oldest fallback (§6.1).
- [ ] Tool membership from one enum (`Profile::allows`) that both `tools/list` and `tools/call`
      consult; hand-written `inputSchema` with descriptions; results `{content, structuredContent,
      isError}` with stable `structuredContent.code`s and an opaque cursor pagination contract (§6.1).
- [ ] Daemon hygiene: child-process supervision for risky work, atomic publish, human-only
      enrollment of servable roots, no code loading after startup, fail-open freshness flags with a
      documented backstop (§6.1).
- [ ] `<bin> mcp install` writing Claude Code / Claude Desktop / Cursor / VS Code / Windsurf configs
      idempotently with backups and `--dry-run` (§6.2).
- [ ] Skills: `.claude/skills/<name>/SKILL.md` with `name` + `description` frontmatter, usage line,
      tables, recipes, pitfalls, doc pointer (§6.3). ⚠ slates additionally needs an installer and
      skills-over-MCP (prompts/resources) — vorpal has no precedent for either.
- [ ] Evals as scripts driving the installed server over stdio against independent ground truth,
      with contract checks ("no fake edges"), latency columns, and determinism as a hard gate (§6.4).

**Bindings**
- [ ] napi-rs 3.x, `AsyncTask` on the libuv pool via one generic boxed-closure `Task`, `Async`-suffixed
      twins for blocking calls, sync methods for sub-millisecond reads, `napi-noop-in-unit-test`
      feature, `napi build --platform`, per-target `npm/` packages, `napi prepublish` (§7.1).
- [ ] pyo3 0.29 with `abi3-py39` behind a `python` feature, maturin `features = ["python"]`,
      `requires-python >= 3.9`, `.pyi` + `py.typed`; awaitables via a Rust-owned thread pool +
      `call_soon_threadsafe` (no tokio, no pyo3-async-runtimes) unless a measured reason appears (§7.2).
- [ ] wasm-bindgen with exact pins reconciled against other crates' floors, `wasm-pack` +
      `patch-pkg.mjs`, `Rc` not `Arc` (§7.3).

**Benchmarks, tests, docs**
- [ ] Benchmarks as recorded release-binary commands with hardware, dataset commits, load
      discipline, and `NO_AUTOWARM`-style quiescing; measurement seams behind `bench-internals`;
      dated "Pass N" write-ups that keep rejected experiments (§8.1).
- [ ] Test kinds to institutionalise: oracle (serial spec vs parallel impl), non-vacuity counters,
      double-run determinism, golden vectors, doc-truth generators, hostile-input parsers,
      env-gated differential tests that skip loudly (§8.2). ⚠ Add `loom`/`miri` for slates'
      lock-free code — vorpal only plans them.
- [ ] Docs: short user docs in `docs/`, living design docs in `docs/wip/` with numbered sections
      that code comments cite, "Locked decisions" and "Load-bearing invariants" up front, status
      blockquotes, ledgers for vendored/upstream drift, and a release doc paired with its xtask (§8.3).
