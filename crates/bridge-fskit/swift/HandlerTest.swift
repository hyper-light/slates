// HandlerTest.swift — by-use tests for the FSKit FSVolume handler (§4.6, R5), run on the macOS CI lane.
//
// These drive the real `SlatesVolume` (constructed against the real FSKit framework, no mount) through
// a mock `ShimChannel` that records the shim requests it is handed and returns canned replies. They
// assert on what the daemon would see and on the FSKit objects the handler builds — observable
// behavior, not internal fields — so the handler's translation logic is exercised here, on this
// machine, with the live-mount FSItem lifecycle left to the Phase 4 spike. Built and run together with
// ShimWire.swift and SlatesVolume.swift; `swiftc ... -framework FSKit`.

import FSKit
import Foundation

// A channel that records each request and returns canned replies in order — the daemon's stand-in.
final class MockChannel: ShimChannel {
  var requests: [[UInt8]] = []
  let replies: [[UInt8]]
  var index = 0
  init(replies: [[UInt8]]) { self.replies = replies }
  func roundTrip(_ request: [UInt8]) throws -> [UInt8] {
    requests.append(request)
    defer { index += 1 }
    return replies[index]
  }
}

// Reply encoders mirroring the Rust `ok_*` helpers (STATUS_OK = 0, then the payload).
private func le64(_ value: UInt64, _ out: inout [UInt8]) {
  for i in 0..<8 { out.append(UInt8((value >> (8 * UInt64(i))) & 0xff)) }
}
private func le32(_ value: UInt32, _ out: inout [UInt8]) {
  for i in 0..<4 { out.append(UInt8((value >> (8 * UInt32(i))) & 0xff)) }
}
func okHandle(_ fh: UInt64) -> [UInt8] {
  var out: [UInt8] = [0]
  le64(fh, &out)
  return out
}
func okUnit() -> [UInt8] { [0] }
func okAttr(ino: UInt64, kind: UInt8, mode: UInt32 = 0o644, size: UInt64 = 0) -> [UInt8] {
  var out: [UInt8] = [0]
  le64(ino, &out)
  le64(0, &out)  // generation (a stable 0)
  out.append(kind)
  le32(mode, &out)
  le32(1, &out)  // nlink
  le32(0, &out)  // uid
  le32(0, &out)  // gid
  le64(size, &out)
  le64(0, &out)  // atime
  le64(0, &out)  // mtime
  le64(0, &out)  // ctime
  return out
}

// The op tag and the release handle out of a recorded request (OP_RELEASE = 7; object is 16 bytes).
private func opOf(_ request: [UInt8]) -> UInt8 { request[0] }
private func releaseFh(_ request: [UInt8]) -> UInt64 {
  var fh: UInt64 = 0
  for i in 0..<8 { fh |= UInt64(request[17 + i]) << (8 * UInt64(i)) }
  return fh
}

private var failures = 0
private func check(_ condition: Bool, _ name: String) {
  print(condition ? "ok: \(name)" : "FAIL: \(name)")
  if !condition { failures += 1 }
}

private func makeVolume(_ channel: ShimChannel) -> SlatesVolume {
  SlatesVolume(
    volumeID: FSVolume.Identifier(uuid: UUID()), volumeName: FSFileName(string: "test"),
    channel: channel)
}

// Two opens then two closes of one item must send two OP_OPEN requests and then release BOTH handles
// (last-in first-out), so no daemon open-reference leaks — the multiple-open bug the per-inode handle
// stack fixes.
private func testOpenCloseReleasesEveryHandle() {
  let channel = MockChannel(replies: [okHandle(11), okHandle(22), okUnit(), okUnit()])
  let volume = makeVolume(channel)
  let item = SlatesItem(object: ObjectId(inode: 42, generation: 0), kind: 0)
  volume.openItem(item, modes: []) { _ in }
  volume.openItem(item, modes: []) { _ in }
  volume.closeItem(item, modes: []) { _ in }
  volume.closeItem(item, modes: []) { _ in }
  check(channel.requests.count == 4, "two opens and two closes make four requests")
  check(opOf(channel.requests[0]) == 12, "the first request is an open")
  check(opOf(channel.requests[1]) == 12, "the second request is an open")
  check(releaseFh(channel.requests[2]) == 22, "the first close releases the second open's handle")
  check(releaseFh(channel.requests[3]) == 11, "the second close releases the first open's handle")
}

// A third close of an item with no outstanding handles releases nothing (a no-op), rather than
// releasing a stale handle.
private func testCloseWithoutOpenIsANoOp() {
  let channel = MockChannel(replies: [okHandle(5), okUnit(), okUnit()])
  let volume = makeVolume(channel)
  let item = SlatesItem(object: ObjectId(inode: 9, generation: 0), kind: 0)
  volume.openItem(item, modes: []) { _ in }
  volume.closeItem(item, modes: []) { _ in }
  volume.closeItem(item, modes: []) { _ in }  // nothing left to release
  check(channel.requests.count == 2, "the extra close sends no request")
}

// A lookup turns the daemon's attribute reply into a SlatesItem naming the found inode and kind.
private func testLookupBuildsTheItem() {
  let channel = MockChannel(replies: [okAttr(ino: 7, kind: 0)])
  let volume = makeVolume(channel)
  let root = SlatesItem(object: ObjectId(inode: 1, generation: 0), kind: 1)
  var found: FSItem?
  volume.lookupItem(named: FSFileName(string: "child"), inDirectory: root) { item, _, _ in
    found = item
  }
  let slates = found as? SlatesItem
  check(slates?.object.inode == 7, "lookup builds a SlatesItem naming the found inode")
  check(slates?.kind == 0, "the found item carries its kind (a file)")
}

// A refused lookup (a NotFound error reply) surfaces as a nil item and an error, not a crash.
private func testLookupNotFoundSurfacesError() {
  let channel = MockChannel(replies: [[1, 0]])  // STATUS_ERR, ShimError::NotFound
  let volume = makeVolume(channel)
  let root = SlatesItem(object: ObjectId(inode: 1, generation: 0), kind: 1)
  var found: FSItem?
  var error: Error?
  volume.lookupItem(named: FSFileName(string: "missing"), inDirectory: root) { item, _, err in
    found = item
    error = err
  }
  check(found == nil, "a NotFound lookup returns no item")
  check(error != nil, "a NotFound lookup returns an error")
}

// createItem routes a directory to OP_MKDIR (op 9) and everything else to OP_CREATE (op 8) — the
// type branch, checked by the op byte the daemon receives.
private func testCreateItemBranchesOnType() {
  let dir = SlatesItem(object: ObjectId(inode: 1, generation: 0), kind: 1)

  let fileChannel = MockChannel(replies: [okAttr(ino: 2, kind: 0)])
  let fileVolume = makeVolume(fileChannel)
  fileVolume.createItem(
    named: FSFileName(string: "f"), type: .file, inDirectory: dir,
    attributes: FSItem.SetAttributesRequest()
  ) { _, _, _ in }
  check(opOf(fileChannel.requests[0]) == 8, "creating a file sends OP_CREATE")

  let dirChannel = MockChannel(replies: [okAttr(ino: 3, kind: 1)])
  let dirVolume = makeVolume(dirChannel)
  dirVolume.createItem(
    named: FSFileName(string: "d"), type: .directory, inDirectory: dir,
    attributes: FSItem.SetAttributesRequest()
  ) { _, _, _ in }
  check(opOf(dirChannel.requests[0]) == 9, "creating a directory sends OP_MKDIR")
}

// removeItem routes a directory item to OP_RMDIR (op 11) and a file to OP_UNLINK (op 10) — the branch
// on the item's own kind, so no extra round trip is needed to decide.
private func testRemoveItemBranchesOnKind() {
  let dir = SlatesItem(object: ObjectId(inode: 1, generation: 0), kind: 1)

  let fileChannel = MockChannel(replies: [okUnit()])
  let fileVolume = makeVolume(fileChannel)
  let file = SlatesItem(object: ObjectId(inode: 2, generation: 0), kind: 0)
  fileVolume.removeItem(file, named: FSFileName(string: "f"), fromDirectory: dir) { _ in }
  check(opOf(fileChannel.requests[0]) == 10, "removing a file sends OP_UNLINK")

  let dirChannel = MockChannel(replies: [okUnit()])
  let dirVolume = makeVolume(dirChannel)
  let subdir = SlatesItem(object: ObjectId(inode: 3, generation: 0), kind: 1)
  dirVolume.removeItem(subdir, named: FSFileName(string: "d"), fromDirectory: dir) { _ in }
  check(opOf(dirChannel.requests[0]) == 11, "removing a directory sends OP_RMDIR")
}

// getAttributes turns the daemon's attribute reply into an FSItem.Attributes with the same mode and
// size — the read side of the metadata path.
private func testGetAttributesMapsTheReply() {
  let channel = MockChannel(replies: [okAttr(ino: 5, kind: 0, mode: 0o600, size: 100)])
  let volume = makeVolume(channel)
  let item = SlatesItem(object: ObjectId(inode: 5, generation: 0), kind: 0)
  var attrs: FSItem.Attributes?
  volume.getAttributes(FSItem.GetAttributesRequest(), of: item) { a, _ in attrs = a }
  check((attrs?.mode ?? 0) & 0o777 == 0o600, "getAttributes carries the mode through")
  check(attrs?.size == 100, "getAttributes carries the size through")
}

// write hands the daemon the bytes and reports the count it stored back to FSKit.
private func testWriteReportsTheCount() {
  let channel = MockChannel(replies: [{ var out: [UInt8] = [0]; le32(5, &out); return out }()])
  let volume = makeVolume(channel)
  let item = SlatesItem(object: ObjectId(inode: 5, generation: 0), kind: 0)
  var written = -1
  volume.write(contents: Data([1, 2, 3, 4, 5]), to: item, at: 0) { count, _ in written = count }
  check(opOf(channel.requests[0]) == 4, "write sends OP_WRITE")
  check(written == 5, "write reports the stored byte count")
}

@main
struct HandlerTest {
  static func main() {
    testOpenCloseReleasesEveryHandle()
    testCloseWithoutOpenIsANoOp()
    testLookupBuildsTheItem()
    testLookupNotFoundSurfacesError()
    testCreateItemBranchesOnType()
    testRemoveItemBranchesOnKind()
    testGetAttributesMapsTheReply()
    testWriteReportsTheCount()
    print(failures == 0 ? "ALL PASS" : "\(failures) FAILURES")
    exit(failures == 0 ? 0 : 1)
  }
}
