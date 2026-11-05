# Publishing slates: the binary and the SDKs

How a release of the `slates` binary (GitHub releases), the Python SDK (`slates` on PyPI) and the Node SDK
(`@hyper-light/slates` on npm) is cut, what the tag does, the one-time setup a maintainer performs once, and the local
proof recorded for this lane. The design is §2.4 and §4.12 of
[the unified design](wip/SLATES_DESIGN.md); the ledger row is "SDK publishing" in
[GAPS.md](wip/GAPS.md) §1.

## One version

`[workspace.package] version` in the root `Cargo.toml` is the only version anyone edits.

- Every Rust crate inherits it.
- The Python wheel and sdist take it from the crate manifest through maturin
  (`crates/sdk-python/pyproject.toml` says `dynamic = ["version"]`; a static copy there is refused).
- npm needs a literal `version` in every `package.json`, so the Node copies are **derived**, never
  edited: `cargo xtask version --write` stamps the main package (`crates/sdk-node/package.json`),
  the nine platform packages under `crates/sdk-node/npm/`, the nine `optionalDependencies` pins and
  the platform READMEs from the workspace version and the main package's `name`, `napi.binaryName`
  and `napi.targets`. `cargo xtask version` refuses any drift — including a platform package whose
  `os`/`cpu`/`libc`/`main`/`files` would make npm pick the wrong binary — and runs inside
  `cargo xtask check`, in CI on every push, and as the first job of every tag lane
  (`.github/workflows/version-guard.yml`, `--expect-tag`: the tag must be `v` + the version).

## Cutting a release

1. Bump `version` in `[workspace.package]` (`Cargo.toml`).
2. `cargo xtask version --write`, then `cargo xtask check`.
3. Commit, then tag and push the tag: `git tag v0.2.0 && git push origin v0.2.0`. Pushing the tag
   is the human's act; nothing else publishes.
4. The tag starts three workflows, each beginning with the shared guard:
   - `release.yml` — the `slates` binary for seven targets, each smoke-tested on its own platform, attached
     with checksums and signed provenance to a **draft** release (below, "The binary release").
   - `publish-python.yml` — wheels and sdist to PyPI.
   - `publish-node.yml` — the addon for nine platforms and the ten packages to npm.
5. `workflow_dispatch` on any of the three lanes is a build-only dry run: nothing is published
   without a tag.
6. Publish the binary release: check the draft (its seven binaries, `SHA256SUMS`,
   `slates-provenance.json`, `slates-attestation.sigstore.json`), then
   `gh release edit v0.2.0 --draft=false --latest` or "Publish release" on the draft. That is
   the binary release's one approved step; until it, the draft is visible to maintainers only.

## The binary release

One `slates` binary is the anchor, the daemon and every client verb, so a release is one raw
binary per platform, no archive (Appendix B.2):

| Platform | Target | Runner | File |
|---|---|---|---|
| macOS arm64 | `aarch64-apple-darwin` | `macos-latest` | `slates-macos-arm64` |
| Linux x86_64, glibc | `x86_64-unknown-linux-gnu` | `ubuntu-latest` | `slates-linux-x64` |
| Linux arm64, glibc | `aarch64-unknown-linux-gnu` | `ubuntu-24.04-arm` | `slates-linux-arm64` |
| Linux x86_64, static | `x86_64-unknown-linux-musl` | `ubuntu-latest`, in `rust:1.98.1-alpine` | `slates-linux-x64-musl` |
| Linux arm64, static | `aarch64-unknown-linux-musl` | `ubuntu-24.04-arm`, in `rust:1.98.1-alpine` | `slates-linux-arm64-musl` |
| Windows x86_64 | `x86_64-pc-windows-msvc` | `windows-2025` | `slates-windows-x64.exe` |
| Windows arm64 | `aarch64-pc-windows-msvc` | `windows-11-arm` | `slates-windows-arm64.exe` |

Every target is built natively on its own runner. The Windows binaries link the C runtime
statically (`+crt-static`), so they need no Visual C++ runtime. The musl binaries are static.
The glibc binaries need the glibc of the runner's Ubuntu or newer.

For each built binary, the release smoke task (`cargo run -p xtask --bin release -- smoke`) runs on the same runner and does what a person
installing it would do:

1. `slates version` names the workspace version.
2. `slates anchor` starts with the full machine profile, and its daemon serves.
3. The consensus group is bootstrapped and a 64 MiB volume is provisioned.
4. One MCP client (`slates mcp`) attaches for writing, writes a file and reads it back.
5. A second client (`slates read`) reads the same bytes.
6. The volume is destroyed.
7. When the anchor is stopped, the daemon leaves with it.

The smoke test records the binary's SHA-256 and BLAKE3, its `rustc -vV`, the runner image and
each step's time. Two more checks run on some targets. The musl binaries must pass a staticness
gate (`ldd`). The Windows x86_64 binary must start in a process-isolated
`nanoserver:ltsc2025` container, which has no Visual C++ runtime.

On a tag, the release artifacts task (`cargo run -p xtask --bin release -- artifacts`) refuses a release that lacks a target, or whose binary
is not the one its smoke test digested. It writes:

- `SHA256SUMS`, in `sha256sum`'s format.
- `slates-provenance.json`: the tag, the commit, the run, and every target's record.

GitHub's artifact attestation (`actions/attest-build-provenance`) then signs SLSA build
provenance for every binary named in `SHA256SUMS`. It signs keylessly through Sigstore with the
job's OIDC identity, so no key or secret exists. Everything is attached to a **draft** release.
Verify a download with `sha256sum -c SHA256SUMS --ignore-missing` (on macOS
`shasum -a 256 -c`) and `gh attestation verify <file> --repo hyper-light/slates`.

**A test of the lane** is a tag `v<version>-test.<N>` (e.g. `v0.1.0-test.1`). The release lane's
guard accepts it (`cargo xtask version --expect-tag TAG --allow-test-tag`) and makes a draft
pre-release titled `TEST … not a release`. Two things keep the SDK publish lanes from publishing
on a test tag. Their triggers exclude `v*-test.*`, and their guard (without `--allow-test-tag`)
refuses such a tag. A test's draft is never published. Delete it with
`gh release delete <tag> --yes` when it is no longer needed.

## What the tag does

**`publish-python.yml`.** Builds the `cp39-abi3` wheel (one wheel per platform serves every
CPython 3.9 or newer) for manylinux and musllinux (x86_64, aarch64 — the aarch64 lanes build
natively on the arm64 runners), macOS (x86_64, arm64) and Windows (x64), plus the sdist; runs
`twine check --strict` over every artifact; then publishes with `pypa/gh-action-pypi-publish`
through **trusted publishing** (OIDC from the `pypi` environment). No API token exists anywhere.
`skip-existing` makes a re-run after a moved tag harmless.

**`publish-node.yml`.** Builds `slates.<platform>.node` for every target in `package.json`
`napi.targets` through the package's own `npm run build` (natively per platform; musl inside
`node:24-alpine`; Windows arm64 and ia32 by MSVC's cross tools), collects the binaries with
`napi artifacts`, **refuses** to publish if any platform package lacks its binary (napi would
otherwise skip it silently), then `npm publish --access public`: `prepublishOnly` runs
`napi prepublish`, which publishes each platform package, and the main package follows. Publishing
is **trusted publishing** (OIDC from the `npm` environment; npm 11.5.1 or newer). A version
already live is skipped.

Adding a required reviewer to the `pypi` and `npm` environments on GitHub makes the publish step
itself wait for a human — the same shape as a landing grant (R10), at the registry boundary.

## One-time setup (a maintainer, once)

**PyPI — a pending publisher, no upload from a laptop.** PyPI can register a trusted publisher
for a project that does not exist yet. On pypi.org: your account → Publishing → "Add a new
pending publisher": PyPI project name `slates`, owner `hyper-light`, repository `slates`, workflow
`publish-python.yml`, environment `pypi`. The first tag run creates the project and its release.

**npm — a stub publish creates each name, then the trusted publisher is attached.** npm can only
attach a trusted publisher to a package that already exists (npm/cli#8544), so each of the ten
names — `@hyper-light/slates` and `@hyper-light/slates-{darwin-arm64, darwin-x64, linux-x64-gnu,
linux-arm64-gnu, linux-x64-musl, linux-arm64-musl, win32-x64-msvc, win32-arm64-msvc,
win32-ia32-msvc}` — is created once at version `0.0.0` by a maintainer, exactly as vorpal's
packages were. The stubs are derived from the real manifests, so the reserved names are the
published names:

```
npm login                                     # a member of the hyper-light organization; npm >= 11.5.1
cargo xtask npm-reserve --out "$OUT"          # ten directories: main/ and one per platform id
for dir in "$OUT"/*/; do (cd "$dir" && npm publish --access public); done
for name in @hyper-light/slates @hyper-light/slates-darwin-arm64 @hyper-light/slates-darwin-x64 \
  @hyper-light/slates-linux-x64-gnu @hyper-light/slates-linux-arm64-gnu @hyper-light/slates-linux-x64-musl \
  @hyper-light/slates-linux-arm64-musl @hyper-light/slates-win32-x64-msvc @hyper-light/slates-win32-arm64-msvc \
  @hyper-light/slates-win32-ia32-msvc; do npm view "$name" version; done   # 0.0.0, ten times
```

Then, on npmjs.com, for each of the ten packages: Settings → Trusted publisher → GitHub Actions,
organization `hyper-light`, repository `slates`, workflow `publish-node.yml`, environment `npm`.

Status on 2026-09-14: the ten stubs are generated, packed and dry-run clean (see below); the
publish itself awaits the maintainer's login (`npm whoami` answered 401 on this machine on
2026-09-14) and is recorded here with its date once done.

## Never from a laptop

The real packages (`0.1.0` and later) are published only by the tag lanes. On a laptop, the
closest step to a registry is a dry run: `npm publish --dry-run --ignore-scripts --access public`
(`--ignore-scripts` matters — `prepublishOnly` runs `napi prepublish`, which publishes the nine
platform packages **for real**) and `twine check --strict`. Neither SDK's test suite touches a
registry; the local proof below ran every npm command with an empty user config
(`NPM_CONFIG_USERCONFIG` pointed at an empty file), so no token could be involved.

## Local proof by use (2026-09-14)

Host: macOS 26.4.1 (Darwin 25.4.0), Apple M5 Max, 18 cores, 128 GiB; rustc 1.98.0; maturin
1.14.1; Python 3.9.6 (system) in a fresh venv with twine 6.2.0; Node v24.14.1, npm 11.11.0,
`@napi-rs/cli` 3.7.2. Tree: this lane's worktree at commit `5de244d` plus this change. The daemon
the suites spawn is `target/debug/slates` (`cargo build -p slates-cli`, 8.7 s incremental).
Every scratch path is under the OS temp directory, outside the tree, and removed afterwards.

**Python.**

```
maturin build --release -m crates/sdk-python/Cargo.toml --target aarch64-apple-darwin \
  --interpreter "$VENV/bin/python" --out "$DIST"      # slates-0.1.0-cp39-abi3-macosx_11_0_arm64.whl, 336,237 B; 5.4 s wall (incremental release build)
maturin sdist -m crates/sdk-python/Cargo.toml --out "$DIST"   # slates-0.1.0.tar.gz, 1,948,557 B, 423 members (the workspace crates the SDK depends on); 6.7 s
"$VENV/bin/twine" check --strict "$DIST"/*                    # both PASSED
"$VENV/bin/pip" install --no-index --force-reinstall "$DIST"/slates-0.1.0-*.whl
SLATES_DAEMON="$PWD/target/debug/slates" "$VENV/bin/python" -m unittest discover -s crates/sdk-python/tests -v
                                                              # 5 tests (sync + async suites, each over a spawned anchor+daemon) OK in 1.63 s; 0 slates processes after
tar xzf "$DIST"/slates-0.1.0.tar.gz && cd slates-0.1.0 && RUSTUP_TOOLCHAIN=1.98.0 maturin build --release --interpreter "$VENV/bin/python" --out ../dist
                                                              # the sdist rebuilds from its own bytes: a 341,062 B wheel in 8.2 s
```

The sdist carries no `rust-toolchain.toml` (maturin refuses `..` in `include` patterns, so the
workspace's pin cannot ride along — measured and rejected 2026-09-14); with a default toolchain
older than 1.98 the build refuses with the typed MSRV message `slates-wire@0.1.0 requires rustc
1.98`, which is the requirement the README states.

**Node.**

```
cd crates/sdk-node && npm run build -- --target aarch64-apple-darwin
                                                              # slates.darwin-arm64.node, 723,392 B; 4.7 s wall (incremental release build); index.js and index.d.ts untouched
SLATES_NODE_ADDON="$PWD/slates.darwin-arm64.node" SLATES_DAEMON="$PWD/../../target/debug/slates" \
  node --test tests/sdk.test.mjs tests/sdk_async.test.mjs      # 5 pass, 0 fail, 1.01 s
cp slates.darwin-arm64.node npm/darwin-arm64/ && (cd npm/darwin-arm64 && npm pack --pack-destination "$OUT")
                                                              # hyper-light-slates-darwin-arm64-0.1.0.tgz: 316.1 kB packed, 724.3 kB unpacked, 3 files
npm pack --pack-destination "$OUT"                            # hyper-light-slates-0.1.0.tgz: 7.4 kB packed, 24.0 kB unpacked, 6 files (index.js, package.json, README.md, async.mjs, index.mjs, index.d.ts)
mkdir "$PROJECT" && cd "$PROJECT" && npm init -y && npm install "$OUT"/hyper-light-slates-*.tgz
                                                              # "added 2 packages" in 0.58 s; the eight other optional platform packages, unpublished, are skipped without error
cp "$TREE/crates/sdk-node/tests/packaged.test.mjs" . && SLATES_DAEMON="$TREE/target/debug/slates" node --test packaged.test.mjs
                                                              # 3 pass, 0 fail, 1.04 s: the addon loads through exactly one platform package, a missing daemon is a typed error, and AsyncClient drives create → snapshot → status → list → destroy over a live daemon
npm publish --dry-run --ignore-scripts --access public        # in crates/sdk-node and in npm/darwin-arm64: "Publishing to https://registry.npmjs.org/ with tag latest and public access (dry-run)"
```

**The ten stubs.**

```
cargo xtask npm-reserve --out "$OUT/reserve"                  # 10 stub packages at 0.0.0
for dir in "$OUT"/reserve/*/; do (cd "$dir" && npm pack && npm publish --dry-run --access public); done
                                                              # ten tarballs of 533–585 B (package.json + README.md), every dry run "public access (dry-run)", no error
```

**Gates on the same tree:** `cargo fmt --all --check` clean; `cargo clippy --workspace
--all-targets -- -D warnings` clean (11.6 s incremental); `cargo xtask check` — structural,
literals, unsafe budget and version — ok; `cargo test -p xtask version` 9 passed (the pure
audit: napi's platform rule for the nine triples and six strangers, every kind of drift named
with its file, the stamping functions keeping every other byte).

## What is not covered yet

- The design's `abi3-py312` and `cp314t` (free-threaded) wheels: PyO3 0.22 cannot build
  free-threaded extensions; the wheel is `cp39-abi3`.
- Windows arm64 and ia32 wheels (the Node lane ships those platforms).
- The publish lanes have not yet run on a tag: the first run is Ada's, after the one-time setup.
