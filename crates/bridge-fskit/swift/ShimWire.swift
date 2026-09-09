import Foundation
// The Swift half of the FSKit shim wire (§4.6, A-1/D-O9): the `FSVolume` handler running inside the
// FSKit app extension encodes each operation into the exact byte wire the Rust `serve` decodes
// (`crates/bridge-fskit/src/lib.rs`), writes it to the app-group ring, and decodes the reply. This file
// is the codec — the encode/decode that MUST match the Rust `ShimRequest`/reply byte for byte. It has
// no FSKit dependency, so it compiles and its cross-check against the Rust golden vector runs with a
// bare `swiftc` on any host, before the FSVolume handler, the app-group ring, and the entitlement (the
// mount spike) exist. Run: `swiftc ShimWire.swift -o /tmp/shimwire && /tmp/shimwire`.

// The operation tags — identical to the Rust `OP_*` constants.
enum ShimOp: UInt8 {
  case lookup = 1, getattr = 2, read = 3, write = 4
}

// An object id: an inode and a generation, each a little-endian UInt64 (16 bytes), matching Rust's
// `put_object`.
struct ObjectId: Equatable {
  let inode: UInt64
  let generation: UInt64
}

// The subset of shim requests this codec pins against the Rust wire (the read/write path). The full set
// mirrors the Rust `ShimRequest`; these are enough to prove the two wires agree.
enum ShimRequest: Equatable {
  case lookup(parent: ObjectId, name: String)
  case getattr(object: ObjectId)
  case read(object: ObjectId, offset: UInt64, size: UInt32)
  case write(object: ObjectId, offset: UInt64, data: [UInt8])
}

func appendLE(_ value: UInt64, _ out: inout [UInt8]) {
  withUnsafeBytes(of: value.littleEndian) { out.append(contentsOf: $0) }
}
func appendLE(_ value: UInt32, _ out: inout [UInt8]) {
  withUnsafeBytes(of: value.littleEndian) { out.append(contentsOf: $0) }
}
func appendObject(_ object: ObjectId, _ out: inout [UInt8]) {
  appendLE(object.inode, &out)
  appendLE(object.generation, &out)
}
// A length-prefixed byte field: a UInt32 little-endian length then the bytes (Rust's `put_bytes`).
func appendBytes(_ bytes: [UInt8], _ out: inout [UInt8]) {
  appendLE(UInt32(bytes.count), &out)
  out.append(contentsOf: bytes)
}

func encode(_ request: ShimRequest) -> [UInt8] {
  var out: [UInt8] = []
  switch request {
  case let .lookup(parent, name):
    out.append(ShimOp.lookup.rawValue)
    appendObject(parent, &out)
    appendBytes(Array(name.utf8), &out)
  case let .getattr(object):
    out.append(ShimOp.getattr.rawValue)
    appendObject(object, &out)
  case let .read(object, offset, size):
    out.append(ShimOp.read.rawValue)
    appendObject(object, &out)
    appendLE(offset, &out)
    appendLE(size, &out)
  case let .write(object, offset, data):
    out.append(ShimOp.write.rawValue)
    appendObject(object, &out)
    appendLE(offset, &out)
    appendBytes(data, &out)
  }
  return out
}

// A tiny cursor decoder mirroring the Rust `take_*`; a malformed message throws rather than crashing.
struct WireError: Error { let reason: String }

struct Reader {
  let bytes: [UInt8]
  var offset = 0
  init(_ bytes: [UInt8]) { self.bytes = bytes }
  mutating func u8() throws -> UInt8 {
    guard offset < bytes.count else { throw WireError(reason: "truncated") }
    defer { offset += 1 }
    return bytes[offset]
  }
  mutating func take(_ n: Int) throws -> ArraySlice<UInt8> {
    guard offset + n <= bytes.count else { throw WireError(reason: "truncated") }
    defer { offset += n }
    return bytes[offset..<offset + n]
  }
  mutating func u64() throws -> UInt64 {
    var v: UInt64 = 0
    for (i, b) in try take(8).enumerated() { v |= UInt64(b) << (8 * i) }
    return v
  }
  mutating func u32() throws -> UInt32 {
    var v: UInt32 = 0
    for (i, b) in try take(4).enumerated() { v |= UInt32(b) << (8 * i) }
    return v
  }
  mutating func object() throws -> ObjectId { ObjectId(inode: try u64(), generation: try u64()) }
  mutating func bytesField() throws -> [UInt8] {
    let n = Int(try u32())
    return Array(try take(n))
  }
  var done: Bool { offset == bytes.count }
}

func decode(_ bytes: [UInt8]) throws -> ShimRequest {
  var r = Reader(bytes)
  let tag = try r.u8()
  let request: ShimRequest
  switch ShimOp(rawValue: tag) {
  case .lookup:
    let parent = try r.object()
    let name = String(decoding: try r.bytesField(), as: UTF8.self)
    request = .lookup(parent: parent, name: name)
  case .getattr: request = .getattr(object: try r.object())
  case .read:
    let object = try r.object()
    request = .read(object: object, offset: try r.u64(), size: try r.u32())
  case .write:
    let object = try r.object()
    let offset = try r.u64()
    request = .write(object: object, offset: offset, data: try r.bytesField())
  case .none: throw WireError(reason: "unknown op \(tag)")
  }
  guard r.done else { throw WireError(reason: "trailing bytes") }
  return request
}

// The cross-language checks: the same golden GetAttr the Rust test pins, and a round-trip of each op.
func run() -> Int {
  var failures = 0
  func check(_ ok: Bool, _ what: String) {
    if !ok { print("FAIL: \(what)"); failures += 1 } else { print("ok: \(what)") }
  }

  // Golden: GetAttr(inode: 7, generation: 0) — the exact bytes the Rust golden vector pins.
  var golden: [UInt8] = [2]
  golden.append(contentsOf: withUnsafeBytes(of: UInt64(7).littleEndian) { Array($0) })
  golden.append(contentsOf: withUnsafeBytes(of: UInt64(0).littleEndian) { Array($0) })
  check(encode(.getattr(object: ObjectId(inode: 7, generation: 0))) == golden,
        "GetAttr wire matches the Rust golden vector")

  let requests: [ShimRequest] = [
    .lookup(parent: ObjectId(inode: 1, generation: 0), name: "hello"),
    .getattr(object: ObjectId(inode: 42, generation: 0)),
    .read(object: ObjectId(inode: 42, generation: 0), offset: 4096, size: 512),
    .write(object: ObjectId(inode: 42, generation: 0), offset: 4096, data: Array("payload".utf8)),
  ]
  for request in requests {
    if let decoded = try? decode(encode(request)) {
      check(decoded == request, "round-trip \(request)")
    } else {
      check(false, "round-trip decoded \(request)")
    }
  }
  return failures
}

let failures = run()
print(failures == 0 ? "ALL PASS" : "\(failures) FAILURES")
exit(failures == 0 ? 0 : 1)
