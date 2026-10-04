# Vendored crates

slates' cryptography is AWS-LC, through its Rust binding aws-lc-rs (A-66; the workspace `Cargo.toml`). Both crates
are vendored here so slates owns the exact bytes it builds. The root `Cargo.toml` patches crates.io to these copies,
and `vendor/` is excluded from the workspace, so slates' lint wall, fmt and xtask's checks do not apply to it.

Each crate is its crates.io package as published, unpacked unchanged, its `.crate` file's SHA-256 checked against the
checksum `Cargo.lock` records for it (2026-10-03). There are no local changes.

| Crate | Package SHA-256 |
|---|---|
| aws-lc-rs 1.18.1 | `b281d307588d634de920874890732659e2e7672f72b5e10e81badc1a8a83621e` |
| aws-lc-sys 0.45.0 | `9bff6c3b54fad79a2e60b8102caf565819711497c1f5f092f49508e2f5c31b27` |

Build settings that would otherwise be patches:
- CPU jitter entropy is left out through aws-lc-sys's own switch, `AWS_LC_SYS_NO_JITTER_ENTROPY=1`, set in
  `.cargo/config.toml` (BENCHMARKS.md: a fresh process's first random bytes take 12–17 µs without it, 17 ms with it).
- `prebuilt-nasm` is on (the workspace `Cargo.toml`), so x86_64 Windows uses AWS-LC's assembled objects when NASM is
  absent.

Not carried: ../mantle's copy has local changes (aws/aws-lc-rs#1241 vectored TLS 1.3 sealing, #1165 `Clone` for
`LessSafeKey`, #617's JWE primitives, the RNDR retry and its operating-system fallback, a system-library guard). None
is used by slates today. Whether to carry them is Ada's decision.

## Updating

1. Download the new `.crate` files from crates.io (or let cargo fetch them), check each SHA-256 against the index and
   the checksum `Cargo.lock` records, and unpack each over its directory here.
2. Update the versions and checksums in this file and in the workspace `Cargo.toml`.
3. Run slates' gates.
