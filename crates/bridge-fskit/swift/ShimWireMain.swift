// ShimWireMain.swift — the standalone entry for the codec cross-check (§4.6, the CI macOS lane).
//
// This lives apart from ShimWire.swift on purpose: ShimWire.swift is a pure library of the codec
// types (no top-level code), so the FSKit handler in SlatesVolume.swift can compile against it with
// `-parse-as-library`. Here the cross-check `run()` is driven from a normal `@main` entry, which
// the CI macOS lane builds together with ShimWire.swift and runs to assert byte-for-byte agreement
// with the Rust golden vector.
import Foundation

@main
struct ShimWireCrossCheck {
  static func main() {
    let failures = run()
    print(failures == 0 ? "ALL PASS" : "\(failures) FAILURES")
    exit(failures == 0 ? 0 : 1)
  }
}
