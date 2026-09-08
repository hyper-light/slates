//! On macOS a Python extension module must defer the CPython C-API symbols to the interpreter that
//! loads it, so the cdylib links with `-undefined dynamic_lookup`. maturin sets this itself; a plain
//! `cargo build` (the workspace gate) does not, so this build script emits it — scoped to *this
//! crate's* cdylib (`rustc-cdylib-link-arg`), never the whole workspace, so no other binary inherits
//! the deferred-symbol linkage. Linux resolves the symbols at load time already; Windows links the
//! Python import library through PyO3, so neither needs the flag.
fn main() {
  // PyO3 0.22's macros reference a `gil-refs` feature cfg that this crate does not enable; declare it
  // known so the workspace-wide `unexpected_cfgs` deny does not fire on the generated code.
  println!("cargo:rustc-check-cfg=cfg(feature, values(\"gil-refs\"))");
  if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
    println!("cargo:rustc-cdylib-link-arg=-undefined");
    println!("cargo:rustc-cdylib-link-arg=dynamic_lookup");
  }
}
