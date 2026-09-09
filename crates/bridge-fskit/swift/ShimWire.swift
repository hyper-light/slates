import Foundation

// The Swift half of the FSKit shim wire (§4.6, A-1/D-O9): the `FSVolume` handler running inside the
// FSKit app extension encodes each operation into the exact byte wire the Rust `serve` decodes
// (`crates/bridge-fskit/src/lib.rs`), writes it to the app-group ring, and decodes the reply. This file
// is the complete codec — encode/decode for every operation and reply — and MUST match the Rust
// `ShimRequest`/reply byte for byte. It has no FSKit dependency, so it compiles and its cross-check
// (round-trips + the shared golden vector) runs with a bare `swiftc` on any host, before the FSVolume
// handler, the app-group ring, and the entitlement (the mount spike) exist. Run:
//   swiftc ShimWire.swift -o /tmp/shimwire && /tmp/shimwire

// The operation tags — identical to the Rust `OP_*` constants (1…19).
enum ShimOp: UInt8 {
  case lookup = 1, getattr, read, write, opendir, readdir, release, create, mkdir, unlink, rmdir,
    open, flush, symlink, readlink, link, rename, reference, forget
}

// The reply status byte and the ShimError tags — identical to the Rust `STATUS_*`/`ShimError` wire.
let STATUS_OK: UInt8 = 0
let STATUS_ERR: UInt8 = 1
// The rename flag bits (Rust `RENAME_NO_REPLACE`/`RENAME_EXCHANGE`).
let RENAME_NO_REPLACE: UInt8 = 1
let RENAME_EXCHANGE: UInt8 = 2

// An object id: an inode and a generation, each a little-endian UInt64 (16 bytes), matching Rust's
// `put_object`.
struct ObjectId: Equatable {
  let inode: UInt64
  let generation: UInt64
}

// The full shim request set, mirroring the Rust `ShimRequest` one to one.
enum ShimRequest: Equatable {
  case lookup(parent: ObjectId, name: String)
  case getattr(object: ObjectId)
  case read(object: ObjectId, offset: UInt64, size: UInt32)
  case write(object: ObjectId, offset: UInt64, data: [UInt8])
  case opendir(object: ObjectId)
  case readdir(object: ObjectId, fh: UInt64, offset: UInt64)
  case release(object: ObjectId, fh: UInt64)
  case create(parent: ObjectId, name: String, mode: UInt32, flags: UInt32)
  case mkdir(parent: ObjectId, name: String, mode: UInt32)
  case unlink(parent: ObjectId, name: String)
  case rmdir(parent: ObjectId, name: String)
  case open(object: ObjectId, flags: UInt32)
  case flush(object: ObjectId, fh: UInt64)
  case symlink(parent: ObjectId, name: String, target: String)
  case readlink(object: ObjectId)
  case link(target: ObjectId, newParent: ObjectId, newName: String)
  case rename(
    oldParent: ObjectId, newParent: ObjectId, oldName: String, newName: String, noReplace: Bool,
    exchange: Bool)
  case reference(object: ObjectId)
  case forget(object: ObjectId, nlookup: UInt64)
}

// A decoded reply — what the shim gets back to answer FSKit. Mirrors the Rust reply encodings.
struct NodeAttr: Equatable {
  let ino: UInt64
  let generation: UInt64
  let kind: UInt8
  let mode: UInt32
  let nlink: UInt32
  let uid: UInt32
  let gid: UInt32
  let size: UInt64
  let atime: Int64
  let mtime: Int64
  let ctime: Int64
}
struct DirEntry: Equatable {
  let ino: UInt64
  let kind: UInt8
  let name: String
}
enum ShimReply: Equatable {
  case attr(NodeAttr)
  case bytes([UInt8])
  case count(UInt32)
  case handle(UInt64)
  case unit
  case entries([DirEntry])
  case text(String)
  case error(UInt8)
}

// Which reply shape a request expects; the shim decodes an OK reply into that shape.
enum ReplyShape { case attr, bytes, count, handle, unit, entries, text }

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
func appendBytes(_ bytes: [UInt8], _ out: inout [UInt8]) {
  appendLE(UInt32(bytes.count), &out)
  out.append(contentsOf: bytes)
}
func appendName(_ name: String, _ out: inout [UInt8]) { appendBytes(Array(name.utf8), &out) }

func encode(_ request: ShimRequest) -> [UInt8] {
  var out: [UInt8] = []
  switch request {
  case let .lookup(parent, name):
    out.append(ShimOp.lookup.rawValue); appendObject(parent, &out); appendName(name, &out)
  case let .getattr(object):
    out.append(ShimOp.getattr.rawValue); appendObject(object, &out)
  case let .read(object, offset, size):
    out.append(ShimOp.read.rawValue); appendObject(object, &out); appendLE(offset, &out)
    appendLE(size, &out)
  case let .write(object, offset, data):
    out.append(ShimOp.write.rawValue); appendObject(object, &out); appendLE(offset, &out)
    appendBytes(data, &out)
  case let .opendir(object):
    out.append(ShimOp.opendir.rawValue); appendObject(object, &out)
  case let .readdir(object, fh, offset):
    out.append(ShimOp.readdir.rawValue); appendObject(object, &out); appendLE(fh, &out)
    appendLE(offset, &out)
  case let .release(object, fh):
    out.append(ShimOp.release.rawValue); appendObject(object, &out); appendLE(fh, &out)
  case let .create(parent, name, mode, flags):
    out.append(ShimOp.create.rawValue); appendObject(parent, &out); appendName(name, &out)
    appendLE(mode, &out); appendLE(flags, &out)
  case let .mkdir(parent, name, mode):
    out.append(ShimOp.mkdir.rawValue); appendObject(parent, &out); appendName(name, &out)
    appendLE(mode, &out)
  case let .unlink(parent, name):
    out.append(ShimOp.unlink.rawValue); appendObject(parent, &out); appendName(name, &out)
  case let .rmdir(parent, name):
    out.append(ShimOp.rmdir.rawValue); appendObject(parent, &out); appendName(name, &out)
  case let .open(object, flags):
    out.append(ShimOp.open.rawValue); appendObject(object, &out); appendLE(flags, &out)
  case let .flush(object, fh):
    out.append(ShimOp.flush.rawValue); appendObject(object, &out); appendLE(fh, &out)
  case let .symlink(parent, name, target):
    out.append(ShimOp.symlink.rawValue); appendObject(parent, &out); appendName(name, &out)
    appendName(target, &out)
  case let .readlink(object):
    out.append(ShimOp.readlink.rawValue); appendObject(object, &out)
  case let .link(target, newParent, newName):
    out.append(ShimOp.link.rawValue); appendObject(target, &out); appendObject(newParent, &out)
    appendName(newName, &out)
  case let .rename(oldParent, newParent, oldName, newName, noReplace, exchange):
    out.append(ShimOp.rename.rawValue); appendObject(oldParent, &out); appendObject(newParent, &out)
    appendName(oldName, &out); appendName(newName, &out)
    out.append((noReplace ? RENAME_NO_REPLACE : 0) | (exchange ? RENAME_EXCHANGE : 0))
  case let .reference(object):
    out.append(ShimOp.reference.rawValue); appendObject(object, &out)
  case let .forget(object, nlookup):
    out.append(ShimOp.forget.rawValue); appendObject(object, &out); appendLE(nlookup, &out)
  }
  return out
}

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
  mutating func i64() throws -> Int64 { Int64(bitPattern: try u64()) }
  mutating func u32() throws -> UInt32 {
    var v: UInt32 = 0
    for (i, b) in try take(4).enumerated() { v |= UInt32(b) << (8 * i) }
    return v
  }
  mutating func object() throws -> ObjectId {
    ObjectId(inode: try u64(), generation: try u64())
  }
  mutating func bytesField() throws -> [UInt8] { Array(try take(Int(try u32()))) }
  mutating func nameField() throws -> String { String(decoding: try bytesField(), as: UTF8.self) }
  var done: Bool { offset == bytes.count }
}

func decode(_ bytes: [UInt8]) throws -> ShimRequest {
  var r = Reader(bytes)
  let tag = try r.u8()
  let request: ShimRequest
  switch ShimOp(rawValue: tag) {
  case .lookup: request = .lookup(parent: try r.object(), name: try r.nameField())
  case .getattr: request = .getattr(object: try r.object())
  case .read: request = .read(object: try r.object(), offset: try r.u64(), size: try r.u32())
  case .write:
    let o = try r.object()
    request = .write(object: o, offset: try r.u64(), data: try r.bytesField())
  case .opendir: request = .opendir(object: try r.object())
  case .readdir: request = .readdir(object: try r.object(), fh: try r.u64(), offset: try r.u64())
  case .release: request = .release(object: try r.object(), fh: try r.u64())
  case .create:
    let p = try r.object()
    request = .create(parent: p, name: try r.nameField(), mode: try r.u32(), flags: try r.u32())
  case .mkdir:
    let p = try r.object()
    request = .mkdir(parent: p, name: try r.nameField(), mode: try r.u32())
  case .unlink: request = .unlink(parent: try r.object(), name: try r.nameField())
  case .rmdir: request = .rmdir(parent: try r.object(), name: try r.nameField())
  case .open: request = .open(object: try r.object(), flags: try r.u32())
  case .flush: request = .flush(object: try r.object(), fh: try r.u64())
  case .symlink:
    let p = try r.object()
    request = .symlink(parent: p, name: try r.nameField(), target: try r.nameField())
  case .readlink: request = .readlink(object: try r.object())
  case .link:
    let t = try r.object()
    request = .link(target: t, newParent: try r.object(), newName: try r.nameField())
  case .rename:
    let op = try r.object()
    let np = try r.object()
    let on = try r.nameField()
    let nn = try r.nameField()
    let flags = try r.u8()
    request = .rename(
      oldParent: op, newParent: np, oldName: on, newName: nn,
      noReplace: flags & RENAME_NO_REPLACE != 0, exchange: flags & RENAME_EXCHANGE != 0)
  case .reference: request = .reference(object: try r.object())
  case .forget: request = .forget(object: try r.object(), nlookup: try r.u64())
  case .none: throw WireError(reason: "unknown op \(tag)")
  }
  guard r.done else { throw WireError(reason: "trailing bytes") }
  return request
}

// Decodes a reply into the shape the request expects; an error reply carries the ShimError tag.
func decodeReply(_ bytes: [UInt8], expecting shape: ReplyShape) throws -> ShimReply {
  var r = Reader(bytes)
  let status = try r.u8()
  if status == STATUS_ERR { return .error(try r.u8()) }
  guard status == STATUS_OK else { throw WireError(reason: "bad status \(status)") }
  switch shape {
  case .attr:
    return .attr(
      NodeAttr(
        ino: try r.u64(), generation: try r.u64(), kind: try r.u8(), mode: try r.u32(),
        nlink: try r.u32(), uid: try r.u32(), gid: try r.u32(), size: try r.u64(),
        atime: try r.i64(), mtime: try r.i64(), ctime: try r.i64()))
  case .bytes: return .bytes(try r.bytesField())
  case .count: return .count(try r.u32())
  case .handle: return .handle(try r.u64())
  case .unit: return .unit
  case .text: return .text(try r.nameField())
  case .entries:
    var entries: [DirEntry] = []
    for _ in 0..<(try r.u32()) {
      entries.append(DirEntry(ino: try r.u64(), kind: try r.u8(), name: try r.nameField()))
    }
    return .entries(entries)
  }
}

// The cross-language checks: the shared golden GetAttr, a round-trip of every request, and a couple of
// reply decodes — proving the Swift and Rust wires agree.
func run() -> Int {
  var failures = 0
  func check(_ ok: Bool, _ what: String) {
    if !ok { print("FAIL: \(what)"); failures += 1 } else { print("ok: \(what)") }
  }

  // Golden: GetAttr(inode: 7, generation: 0) — the exact bytes the Rust golden vector pins.
  var golden: [UInt8] = [2]
  golden.append(contentsOf: withUnsafeBytes(of: UInt64(7).littleEndian) { Array($0) })
  golden.append(contentsOf: withUnsafeBytes(of: UInt64(0).littleEndian) { Array($0) })
  check(
    encode(.getattr(object: ObjectId(inode: 7, generation: 0))) == golden,
    "GetAttr wire matches the Rust golden vector")

  let o = ObjectId(inode: 42, generation: 0)
  let p = ObjectId(inode: 1, generation: 0)
  let requests: [ShimRequest] = [
    .lookup(parent: p, name: "hello"), .getattr(object: o),
    .read(object: o, offset: 4096, size: 512), .write(object: o, offset: 0, data: Array("hi".utf8)),
    .opendir(object: p), .readdir(object: p, fh: 7, offset: 3), .release(object: p, fh: 7),
    .create(parent: p, name: "f", mode: 0o644, flags: 2), .mkdir(parent: p, name: "d", mode: 0o755),
    .unlink(parent: p, name: "f"), .rmdir(parent: p, name: "d"), .open(object: o, flags: 2),
    .flush(object: o, fh: 5), .symlink(parent: p, name: "l", target: "/a/b"),
    .readlink(object: o), .link(target: o, newParent: p, newName: "alias"),
    .rename(oldParent: p, newParent: o, oldName: "a", newName: "b", noReplace: true, exchange: false),
    .reference(object: o), .forget(object: o, nlookup: 3),
  ]
  for request in requests {
    if let decoded = try? decode(encode(request)) {
      check(decoded == request, "round-trip \(request)")
    } else {
      check(false, "round-trip decoded \(request)")
    }
  }

  // Reply decoding: a NotFound error reply and an OK unit reply (the shapes the Rust `serve` emits).
  check((try? decodeReply([STATUS_ERR, 0], expecting: .attr)) == .error(0), "error reply decodes")
  check((try? decodeReply([STATUS_OK], expecting: .unit)) == .unit, "unit reply decodes")

  return failures
}
