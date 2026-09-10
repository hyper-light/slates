//! Links the WinFsp user-mode DLL's import library on a real Windows build.
//!
//! The `FspFileSystem*` functions the host calls are exported from `winfsp-x64.dll` (or
//! `winfsp-x86.dll` on 32-bit), whose import library ships in the WinFsp SDK under `<install>\lib`.
//! WinFsp installs to `%ProgramFiles(x86)%\WinFsp` by default; a build sets `WINFSP_LIB` to the lib
//! directory to override (the CI does, from the choco install path). This only affects the link step
//! of a real Windows build — a cross-target `cargo check` from another host type-checks the FFI
//! declarations without linking, so the bridge is verifiable off Windows exactly as the AFD reactor is.

fn main() {
  let target = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
  if target != "windows" {
    return;
  }
  let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
  // WinFsp names its DLL and import library by architecture: winfsp-x64 / winfsp-x86 / winfsp-a64.
  let lib = match arch.as_str() {
    "x86_64" => "winfsp-x64",
    "x86" => "winfsp-x86",
    "aarch64" => "winfsp-a64",
    _ => "winfsp-x64",
  };
  // The lib directory: `WINFSP_LIB` if set, else the default install's `lib` folder.
  if let Ok(dir) = std::env::var("WINFSP_LIB") {
    println!("cargo:rustc-link-search=native={dir}");
  } else if let Ok(program_files) = std::env::var("ProgramFiles(x86)") {
    println!("cargo:rustc-link-search=native={program_files}\\WinFsp\\lib");
  }
  println!("cargo:rustc-link-lib=dylib={lib}");
  println!("cargo:rerun-if-env-changed=WINFSP_LIB");
}
