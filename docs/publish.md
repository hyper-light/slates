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
  the seven platform packages under `crates/sdk-node/npm/`, the seven `optionalDependencies` pins and
  the platform READMEs from the workspace version and the main package's `name`, `napi.binaryName`
  and `napi.targets`. `napi.targets` must name exactly the targets slates ships (`xtask/src/targets.rs`: macOS
  arm64, Linux x86_64 and arm64 with glibc and musl, Windows x86_64 and arm64). `cargo xtask version` refuses any
  drift — a target that is not shipped, a shipped one missing, or a platform package whose
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
   - `publish-python.yml` — wheels for the shipped targets and the sdist to PyPI.
   - `publish-node.yml` — the addon for the seven shipped targets and the eight packages to npm.
5. `workflow_dispatch` on any of the three lanes is a dry run: it builds every artifact and runs every
   check (the npm lane assembles every platform package and refuses one without its binary; the PyPI lane
   runs `twine check --strict` over every artifact), and nothing is published without a tag. The binary
   lane's dry run stages the whole release, short of the signature and the draft (A-320; "The binary
   release" below): `gh workflow run release.yml --ref <branch>`.
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

Every target is built twice, each copy on a runner of its own, and the release is refused unless the two copies are
the same bytes (AC-9.5). Both copies must run on one runner image: the Windows binaries link the image's MSVC C runtime, so
two images can build two binaries from one tag. A pair from two images is refused with both images named (A-407);
run one copy again (`gh run rerun <run> --job <job>`) until the pair shares an image. The Windows binaries are linked with `/Brepro`, so MSVC's linker takes its timestamps and
PDB GUID from the content, not the clock.

The smoke test records the binary's SHA-256 and BLAKE3, its `rustc -vV`, the runner image and
each step's time. Two more checks run on some targets. The musl binaries must pass a staticness
gate (`ldd`). The Windows x86_64 binary must start in a process-isolated
`nanoserver:ltsc2025` container, which has no Visual C++ runtime. CI's Windows lane starts its own debug build there
on every push too, so an import Nano Server lacks fails the push that adds it, not the next tag (A-357).

In every run, a tag's or a dry run's, the `stage` job runs the release artifacts task
(`cargo run -p xtask --bin release -- artifacts`, with `--tag TAG` or `--dry-run`). It refuses a release that lacks a
target, whose binary is not the one its smoke test digested, or whose two copies differ. It writes:

- `SHA256SUMS`, in `sha256sum`'s format.
- `slates-provenance.json`: the tag (none on a dry run, which says `"dry_run": true`), the commit, the run, and every
  target's record with its rebuild's.
- The draft's notes, title and kind, and `plan.md`: every staged file with its size and SHA-256, then what follows.
  The plan is the job's summary on the run's page.

`sha256sum --check --strict SHA256SUMS` then holds the staged binaries to the checksums, and the job keeps the
binaries, `SHA256SUMS`, the provenance and the notes as the run's **`release-staged`** artifact. The job runs with the
workflow's read-only permissions.

On a tag, the `draft release` job, the only job with write permissions, takes `release-staged` and checks it against
`SHA256SUMS` again. GitHub's artifact attestation (`actions/attest-build-provenance`) then signs SLSA build
provenance for every binary named in `SHA256SUMS`. It signs keylessly through Sigstore with the
job's OIDC identity, so no key or secret exists; on this public repository Sigstore also records the signature in its
public transparency log. Everything is attached to a **draft** release.
Verify a download with `sha256sum -c SHA256SUMS --ignore-missing` (on macOS
`shasum -a 256 -c`) and `gh attestation verify <file> --repo hyper-light/slates`.

**The dry run** is `workflow_dispatch` (`gh workflow run release.yml --ref <branch>`; A-320). Guard, the fourteen
builds and their smoke tests, and `stage` run exactly as on a tag. The staged files are labelled a dry run: the title
and notes say "not a release", and the provenance has no tag. The `draft release` job does not run, so a dry run
signs nothing and makes no release. Its plan says what the tag `v<version>` would do with the same files. Before
A-320 a dry run stopped after the builds, and only the two test tags had ever checked the checksums, the copies'
comparison and the provenance.

**Proven 2026-11-05 on the test tag `v0.1.0-test.1`** (run 41983664060): all seven binaries built and passed their
smoke test, and the draft carried them with `SHA256SUMS` (all seven `OK` after download), the provenance and the
attestation bundle (`gh attestation verify` exits 0, and 1 for a tampered copy). Details: A-210 in
[the design](wip/SLATES_DESIGN.md).

**The dry run proven 2026-11-18** (run 44261010043, `workflow_dispatch` at commit 44c7cc17, 40 minutes). All fourteen
copies passed their smoke tests, and `stage` passed in 63 s: the seven pairs were byte for byte the same,
`sha256sum --check --strict` answered OK seven times, and the staged release was kept as `release-staged`. The draft
job did not run. No tag, draft or attestation was made. Its `SHA256SUMS`:

```
fc53d4c9ed2cd59b0a78bf092df683a7d0f04f94febc1a4a331e38462fa8f0ad  slates-macos-arm64
5e3d51c6d4572b4175481aa0371b0815cf3cc80d795316bb096ae4222ba9815a  slates-linux-x64
46bba07ff638c7910ca6cc1bb623c29783eee36829f78645f9ae02b7cc4e5687  slates-linux-arm64
69395796e317958ec07e2db6f31294bb0f131cc176dbefb9c758a0fc3e4f05e8  slates-linux-x64-musl
05f4724df922383c978c83b833a400525b39ca653195eafa38c824167c4e7ad4  slates-linux-arm64-musl
e689e299f08a7ba9c4aa19bdead07751f5426c5d42273ab690fd67e5a58a5f93  slates-windows-x64.exe
e35ff7594d4cac6a394a98f3eccad54996e76babbcf6f225a1d34abd735a9ba4  slates-windows-arm64.exe
```

Downloaded with `gh run download 44261010043 --name release-staged`, the seven verified with `shasum -a 256 -c`. The
macOS binary answered `slates 0.1.0`. Each step's time is in A-320 in [the design](wip/SLATES_DESIGN.md).

**A test of the lane** is a tag `v<version>-test.<N>` (e.g. `v0.1.0-test.1`). The release lane's
guard accepts it (`cargo xtask version --expect-tag TAG --allow-test-tag`) and makes a draft
pre-release titled `TEST … not a release`. Two things keep the SDK publish lanes from publishing
on a test tag. Their triggers exclude `v*-test.*`, and their guard (without `--allow-test-tag`)
refuses such a tag. A test's draft is never published. Delete it and its tag with
`gh release delete <tag> --cleanup-tag --yes` when it is no longer needed.

## The publish step: what it does and what it needs

Publishing is a maintainer's act, never a lane's, as a landing is a grant's (R10). For `v0.1.0`, in order:

1. **A dry run of the commit to release has passed:** `gh workflow run release.yml --ref main`. Its `stage` summary
   names each of the seven binaries with its SHA-256, and `release-staged` holds them. The commit is the run's
   (`gh run view <run> --json headSha`), not main's head later, since main moves.
2. **The tag:** `git tag v0.1.0 <that commit> && git push origin v0.1.0`. This is the first outward act, and it
   starts three lanes:
   - `release.yml` runs the dry run's steps again from the tag. Its `draft release` job then signs the provenance,
     which puts a public record in Sigstore's transparency log. It creates the draft `v0.1.0`, titled
     "slates 0.1.0", with the seven binaries, `SHA256SUMS`, `slates-provenance.json` and
     `slates-attestation.sigstore.json`. Only the repository's maintainers see a draft.
   - `publish-python.yml` and `publish-node.yml` build the SDKs and publish them to PyPI and npm straight away,
     with no draft, through trusted publishing. Until a registry's one-time setup below is done, its publish
     step is refused and publishes nothing; a run whose npm setup is only partly done publishes only part.
   - On 2026-11-17 the repository had no GitHub environments, so the first tag would create `pypi` and `npm`
     with no required reviewer.
3. **Check the draft:** `gh release download v0.1.0 --dir <an empty directory>`, then `shasum -a 256 -c
   SHA256SUMS` (seven `OK`) and `gh attestation verify <binary> --repo hyper-light/slates` for each binary.
4. **Publish the draft:** `gh release edit v0.1.0 --draft=false --latest`. This step makes the binaries public.
5. **Say so in the README:** remove "No release is published yet" from its Install section and the "no release
   yet" notice above its capabilities table, in a commit on main.

What it needs from Ada; no agent can supply these:

- Her go for the tag (step 2) and for publishing the draft (step 4).
- Whether the SDKs ship with the binary. If they do, the one-time setup below comes first. If they do not, add
  `pypi` and `npm` environments with a required reviewer (Settings → Environments) before the tag, and reject
  their jobs. Otherwise expect both lanes to fail at their publish step.
- Whether the test drafts `v0.1.0-test.1` and `v0.1.0-test.2` are deleted first, with their tags:
  `gh release delete <tag> --cleanup-tag --yes`.
- Whether `v0.1.0` ships unsigned. The binaries carry no Apple Developer ID signature or notarization, and no
  Windows Authenticode signature (B.7; each needs Ada's identity). The macOS binary carries only the linker's
  ad-hoc signature (`codesign -dv`: `adhoc,linker-signed`, no team identifier; 2026-11-18). Fetched with
  `gh run download`, it carried no quarantine attribute and ran. Expected but not measured here: macOS
  Gatekeeper refuses to run a browser-downloaded binary that carries the quarantine attribute until
  `xattr -d com.apple.quarantine <file>`, and Windows SmartScreen warns when a browser-downloaded `.exe` is
  opened from Explorer.

## What the tag does

**`publish-python.yml`.** Builds the `cp39-abi3` wheel (one wheel per platform serves every
CPython 3.9 or newer) for manylinux and musllinux (x86_64, aarch64 — the aarch64 lanes build
natively on the arm64 runners), macOS (x86_64, arm64) and Windows (x64), plus the sdist; runs
`twine check --strict` over every artifact; then publishes with `pypa/gh-action-pypi-publish`
through **trusted publishing** (OIDC from the `pypi` environment). No API token exists anywhere.
`skip-existing` makes a re-run after a moved tag harmless.

**`publish-node.yml`.** Builds `slates.<platform>.node` for every target in `package.json`
`napi.targets` through the package's own `npm run build` (natively, each on a runner of its own platform; musl
inside `node:24-alpine`; Windows arm64 on `windows-11-arm` with clang-cl for AWS-LC), collects the binaries with
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
attach a trusted publisher to a package that already exists (npm/cli#8544), so each of the eight
names — `@hyper-light/slates` and `@hyper-light/slates-{darwin-arm64, linux-x64-gnu, linux-arm64-gnu,
linux-x64-musl, linux-arm64-musl, win32-x64-msvc, win32-arm64-msvc}` — is created once at version `0.0.0` by a maintainer, exactly as vorpal's
packages were. The stubs are derived from the real manifests, so the reserved names are the
published names:

```
npm login                                     # a member of the hyper-light organization; npm >= 11.5.1
cargo xtask npm-reserve --out "$OUT"          # eight directories: main/ and one per platform id
for dir in "$OUT"/*/; do (cd "$dir" && npm publish --access public); done
for name in @hyper-light/slates @hyper-light/slates-darwin-arm64 \
  @hyper-light/slates-linux-x64-gnu @hyper-light/slates-linux-arm64-gnu @hyper-light/slates-linux-x64-musl \
  @hyper-light/slates-linux-arm64-musl @hyper-light/slates-win32-x64-msvc \
  @hyper-light/slates-win32-arm64-msvc; do npm view "$name" version; done   # 0.0.0, eight times
```

Then, on npmjs.com, for each of the eight packages: Settings → Trusted publisher → GitHub Actions,
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
- The publish lanes have not yet run on a tag: the first run is Ada's, after the one-time setup.
