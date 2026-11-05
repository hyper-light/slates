//! The targets slates ships (Appendix B.2, A-210; CLAUDE.md: Windows, macOS and Linux on arm64 and x86_64 only), named
//! once for every lane that ships something: the binary release's assets (`release.rs`) and the Node SDK's platform
//! packages, which `version.rs` holds to exactly these triples. Shared by the `xtask` and `release` binaries.

/// What a release ships: one raw binary per target, named as Appendix B.2 names it. The release lane's matrix
/// builds exactly these, and the artifacts task refuses a release that lacks one or carries another; the Node SDK's
/// `napi.targets` must name exactly these triples (`version.rs`).
pub(crate) const ASSETS: &[(&str, &str)] = &[
  ("aarch64-apple-darwin", "slates-macos-arm64"),
  ("x86_64-unknown-linux-gnu", "slates-linux-x64"),
  ("aarch64-unknown-linux-gnu", "slates-linux-arm64"),
  ("x86_64-unknown-linux-musl", "slates-linux-x64-musl"),
  ("aarch64-unknown-linux-musl", "slates-linux-arm64-musl"),
  ("x86_64-pc-windows-msvc", "slates-windows-x64.exe"),
  ("aarch64-pc-windows-msvc", "slates-windows-arm64.exe"),
];
