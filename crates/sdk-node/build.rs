//! napi-rs's build setup: it registers the addon's entry points and, on macOS, sets the cdylib to
//! defer Node's symbols to the loading process (`-undefined dynamic_lookup`) — the same reason the
//! Python extension needs it, handled here by `napi_build` so no linker flag is hand-written.
fn main() {
  napi_build::setup();
}
