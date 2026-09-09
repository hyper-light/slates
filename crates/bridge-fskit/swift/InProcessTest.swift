// InProcessTest.swift — the end-to-end by-use test of the FSKit handler over the in-process transport
// (§4.6, R5), for the macOS CI lane.
//
// HandlerTest.swift drives the handler through a mock channel; this one links the real Rust core (the
// `test-harness` cdylib) and drives the handler through `serve()` over a real `VolumeBridge` on a real
// scratch volume — the whole handler↔codec↔bridge stack in one process, no ring and no mount. It closes
// the one link the Rust `serve` tests and the Swift mock tests each cover only half of.

import FSKit
import Foundation

// The C ABI the `test-harness` cdylib exports (crates/bridge-fskit/src/ffi.rs).
@_silgen_name("slates_fskit_open_test_volume")
func slates_fskit_open_test_volume() -> OpaquePointer?
@_silgen_name("slates_fskit_serve")
func slates_fskit_serve(
  _ handle: OpaquePointer, _ request: UnsafePointer<UInt8>?, _ requestLen: Int,
  _ out: UnsafeMutablePointer<UInt8>?, _ outCap: Int
) -> Int
@_silgen_name("slates_fskit_free")
func slates_fskit_free(_ handle: OpaquePointer)

// A ShimChannel that calls `serve()` in the linked-in Rust core over its C ABI — the in-process form
// (§4.6). It holds the opaque volume handle for the session and frees it on deinit.
final class InProcessChannel: ShimChannel {
  private let handle: OpaquePointer
  init?() {
    guard let handle = slates_fskit_open_test_volume() else { return nil }
    self.handle = handle
  }
  deinit { slates_fskit_free(handle) }
  func roundTrip(_ request: [UInt8]) throws -> [UInt8] {
    var out = [UInt8](repeating: 0, count: 1 << 16)  // 64 KiB reply buffer, ample for these ops
    let n = request.withUnsafeBufferPointer { req in
      out.withUnsafeMutableBufferPointer { buffer in
        slates_fskit_serve(handle, req.baseAddress, req.count, buffer.baseAddress, buffer.count)
      }
    }
    guard n > 0 else { throw ShimTransportError.unwired }
    return Array(out[0..<n])
  }
}

private var failures = 0
private func check(_ condition: Bool, _ name: String) {
  print(condition ? "ok: \(name)" : "FAIL: \(name)")
  if !condition { failures += 1 }
}

@main
struct InProcessTest {
  static func main() {
    guard let channel = InProcessChannel() else {
      print("FAIL: could not open the in-process test volume")
      exit(1)
    }
    let volume = SlatesVolume(
      volumeID: FSVolume.Identifier(uuid: UUID()), volumeName: FSFileName(string: "harness"),
      channel: channel)

    // Learn the real root through the channel (OP_ROOT) — activate() needs an FSTaskOptions we cannot
    // construct standalone, but OP_ROOT is exactly what activate() calls.
    guard let rootBytes = try? channel.roundTrip(encode(.root)),
      case .attr(let rootAttr) = try? decodeReply(rootBytes, expecting: .attr)
    else {
      print("FAIL: the volume has no root")
      exit(1)
    }
    let root = SlatesItem(
      object: ObjectId(inode: rootAttr.ino, generation: rootAttr.generation), kind: rootAttr.kind)
    check(rootAttr.kind == 1, "the real root is a directory")
    check(rootAttr.ino != 1, "the real root is compose(prefix, 1), not the assumed inode 1")

    // Create a file in the real volume.
    var created: FSItem?
    volume.createItem(
      named: FSFileName(string: "hello"), type: .file, inDirectory: root,
      attributes: FSItem.SetAttributesRequest()
    ) { item, _, _ in created = item }
    guard let file = created as? SlatesItem else {
      print("FAIL: create returned no item")
      exit(1)
    }

    // Look it up — the created and looked-up inodes must match (the real namespace remembers it).
    var lookedUp: UInt64?
    volume.lookupItem(named: FSFileName(string: "hello"), inDirectory: root) { item, _, _ in
      lookedUp = (item as? SlatesItem)?.object.inode
    }
    check(lookedUp == file.object.inode, "the created file is found by lookup in the real volume")

    // Write bytes, then read attributes — the size must reflect the write (the real data path, end to
    // end: Swift handler → C ABI → serve → VolumeBridge → volume → back).
    var written = -1
    volume.write(contents: Data("world".utf8), to: file, at: 0) { count, _ in written = count }
    check(written == 5, "wrote 5 bytes through the real bridge")

    var size: UInt64?
    volume.getAttributes(FSItem.GetAttributesRequest(), of: file) { attrs, _ in size = attrs?.size }
    check(size == 5, "the file size reflects the written bytes")

    // Remove it, then a lookup must fail — the namespace mutation reached the real volume.
    var removeError: Error?
    volume.removeItem(file, named: FSFileName(string: "hello"), fromDirectory: root) { err in
      removeError = err
    }
    check(removeError == nil, "the file was removed")
    var afterRemoval: FSItem?
    volume.lookupItem(named: FSFileName(string: "hello"), inDirectory: root) { item, _, _ in
      afterRemoval = item
    }
    check(afterRemoval == nil, "the removed file is gone from the real volume")

    print(failures == 0 ? "ALL PASS" : "\(failures) FAILURES")
    exit(failures == 0 ? 0 : 1)
  }
}
