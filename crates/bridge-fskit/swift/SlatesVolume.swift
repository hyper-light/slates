// SlatesVolume.swift — the macOS FSKit handler for a slates volume (§4.6, D-O9, A-1).
//
// This is the macOS half of the bridge. It makes a slates volume appear as a real mounted
// filesystem through Apple's FSKit: every FSKit operation is translated into a slates shim
// request (the wire defined in ShimWire.swift, byte-identical to the Rust `crates/bridge-fskit`
// codec and its golden vector), handed to the daemon over the app-group ring, and its reply is
// turned back into the FSKit reply objects the kernel expects.
//
// What this file proves by compiling: the FSKit API integration is correct — the handler
// conforms to `FSUnaryFileSystemOperations` and the `FSVolume` operation protocols against the
// real framework on this machine (macOS 26, FSKit availability macOS 15.4+). Two things are the
// Phase 4 mount spike, gated deliberately and marked below with `SPIKE:`:
//   1. the ring transport — the handler is written against the `ShimChannel` seam; the real
//      app-group shared-memory ring to the daemon is dropped in at the spike, where a live mount
//      exercises the FSItem lifecycle end to end; and
//   2. open-handle refcounting across multiple opens of one item (the single-open map here is a mount
//      refinement). The root object id and the time unit are no longer guesses: activate() learns the
//      real root via OP_ROOT (the daemon's root is compose(prefix, 1), not a constant), and the shim
//      times are Unix nanoseconds, both verified against the daemon's own code.
//
// The shim wire is 21 ops (LOOKUP..FORGET, then SETATTR and ROOT); `setAttributes` maps to OP_SETATTR
// (chmod/chown/truncate/utimes, returning the new attributes), and `activate` maps to OP_ROOT to learn
// the volume's real root object — both exercised by the `serve_*` by-use tests over a real bridge.

import FSKit
import Foundation

// MARK: - The transport seam to the daemon

// A request is the encoded ShimWire bytes; the reply is the daemon's framed answer
// (`STATUS_OK`/`STATUS_ERR` + payload) that `decodeReply` reads. The handler owns no transport of
// its own — it is written against this one method so it compiles and its translation logic is
// exercised here; the real ring is the mount-spike wiring.
protocol ShimChannel {
  func roundTrip(_ request: [UInt8]) throws -> [UInt8]
}

// SPIKE: stands in for the app-group ring until the mount spike wires the real one. Every call
// refuses, so a handler running without a daemon fails loudly rather than inventing an answer.
struct UnwiredChannel: ShimChannel {
  func roundTrip(_ request: [UInt8]) throws -> [UInt8] {
    throw ShimTransportError.unwired
  }
}

enum ShimTransportError: Error { case unwired }

// MARK: - The slates error taxonomy on the wire, mapped to POSIX

// The `ShimError` tags, identical in order and value to the Rust `enum ShimError` (`#[repr(u8)]`).
// Kept as a named enum, not bare integers, so the mapping to `errno` reads as prose (R3).
enum ShimErrorCode: UInt8 {
  case notFound = 0
  case alreadyExists
  case notDirectory
  case isDirectory
  case notEmpty
  case invalid
  case notPermitted
  case invalidName
  case tooManyLinks
  case noSpace
  case fileTooLarge
  case crossVolumeMove
  case staleHandle
  case destroying
  case pinned
  case baseUnavailable
  case other

  // The POSIX errno FSKit surfaces to the kernel for this refusal. Darwin's named constants carry
  // the numbers, so there is no magic here; the choices mirror the Rust `ShimError::to_errno`.
  var errno: Int32 {
    switch self {
    case .notFound: return ENOENT
    case .alreadyExists: return EEXIST
    case .notDirectory: return ENOTDIR
    case .isDirectory: return EISDIR
    case .notEmpty: return ENOTEMPTY
    case .invalid: return EINVAL
    case .notPermitted: return EPERM
    case .invalidName: return ENAMETOOLONG
    case .tooManyLinks: return EMLINK
    case .noSpace: return ENOSPC
    case .fileTooLarge: return EFBIG
    case .crossVolumeMove: return EXDEV
    case .staleHandle: return ESTALE
    case .destroying: return EIO
    case .pinned: return EBUSY
    case .baseUnavailable: return EIO
    case .other: return EIO
    }
  }
}

// Turns a shim error tag (or an unknown one) into the NSError FSKit wants.
private func posixError(_ tag: UInt8) -> NSError {
  let code = ShimErrorCode(rawValue: tag)?.errno ?? EIO
  return NSError(domain: NSPOSIXErrorDomain, code: Int(code))
}

// Turns a transport or decode failure into a generic I/O NSError; the daemon being unreachable is
// EIO to the kernel, which is the truthful answer when the ring is not wired.
private func ioError() -> NSError {
  NSError(domain: NSPOSIXErrorDomain, code: Int(EIO))
}

// MARK: - The FSItem a slates node presents as

// FSKit hands this object back to name a node across calls. It carries the slates object id
// (inode + generation) that every shim request routes by, and the node kind, which decides
// unlink-versus-rmdir on removal without a second round trip.
final class SlatesItem: FSItem {
  let object: ObjectId
  let kind: UInt8
  init(object: ObjectId, kind: UInt8) {
    self.object = object
    self.kind = kind
    super.init()
  }
}

// MARK: - Attribute translation

// The shim kind byte (KIND_FILE=0, KIND_DIR=1, KIND_SYMLINK=2 on the Rust side) as an FSItem type.
private func itemType(fromKind kind: UInt8) -> FSItem.ItemType {
  switch kind {
  case 0: return .file
  case 1: return .directory
  case 2: return .symlink
  default: return .unknown
  }
}

// The shim times are Unix nanoseconds (verified: the daemon copies the volume's `wall_ns()` clock
// straight into the attribute record); this splits one into a timespec for FSKit.
private func timespec(fromUnixNanos nanos: Int64) -> timespec {
  let billion: Int64 = 1_000_000_000
  return timespec(tv_sec: Int(nanos / billion), tv_nsec: Int(nanos % billion))
}

// The inverse of the above: a timespec (FSKit's time form) as Unix nanoseconds, the form the shim's
// setattr carries.
private func unixNanos(from time: timespec) -> Int64 {
  let billion: Int64 = 1_000_000_000
  return Int64(time.tv_sec) * billion + Int64(time.tv_nsec)
}

// Fills an `FSItem.Attributes` from the shim's `NodeAttr`. FSKit asks for a subset via the
// `wantedAttributes` mask, but over-reporting is allowed, so the handler fills what it has.
private func mappedAttributes(from attr: NodeAttr) -> FSItem.Attributes {
  let out = FSItem.Attributes()
  out.uid = attr.uid
  out.gid = attr.gid
  out.mode = attr.mode
  out.type = itemType(fromKind: attr.kind)
  out.linkCount = attr.nlink
  out.size = attr.size
  out.allocSize = attr.size
  out.fileID = FSItem.Identifier(rawValue: attr.ino) ?? .invalid
  out.modifyTime = timespec(fromUnixNanos: attr.mtime)
  out.changeTime = timespec(fromUnixNanos: attr.ctime)
  out.accessTime = timespec(fromUnixNanos: attr.atime)
  return out
}

// MARK: - The volume

// The FSKit volume handler for one mounted slates volume. Conforms to the full required operation
// set: `FSVolume.Operations` (which requires `FSVolumePathConfOperations`), plus read/write and
// open/close. Each method encodes a shim request, round-trips it through the channel, decodes the
// reply into the shape the request expects, and calls FSKit's reply block exactly once.
final class SlatesVolume: FSVolume, FSVolume.Operations, FSVolume.ReadWriteOperations,
  FSVolume.OpenCloseOperations
{
  private let channel: ShimChannel

  // SPIKE: FUSE-style open returns a file handle and release takes one, but FSKit's open/close are
  // handle-less (they carry the item and the modes). The handler keeps the daemon's handle per
  // inode so close can name it; single-open for now, refcounting is a spike refinement.
  private var openHandles: [UInt64: UInt64] = [:]

  init(volumeID: FSVolume.Identifier, volumeName: FSFileName, channel: ShimChannel) {
    self.channel = channel
    super.init(volumeID: volumeID, volumeName: volumeName)
  }

  // One round trip: encode, send, decode into the expected shape. A transport failure throws.
  private func call(_ request: ShimRequest, expecting shape: ReplyShape) throws -> ShimReply {
    let reply = try channel.roundTrip(encode(request))
    return try decodeReply(reply, expecting: shape)
  }

  // MARK: FSVolumePathConfOperations

  var maximumLinkCount: Int { Int(Int32.max) }
  var maximumNameLength: Int { 255 }
  var restrictsOwnershipChanges: Bool { false }
  var truncatesLongNames: Bool { false }

  // MARK: FSVolume.Operations required properties

  var supportedVolumeCapabilities: FSVolume.SupportedCapabilities {
    let caps = FSVolume.SupportedCapabilities()
    caps.supportsSymbolicLinks = true
    caps.supportsHardLinks = true
    caps.supportsPersistentObjectIDs = true
    caps.supports64BitObjectIDs = true
    return caps
  }

  var volumeStatistics: FSStatFSResult {
    FSStatFSResult(fileSystemTypeName: "slates")
  }

  // MARK: FSVolume.Operations lifecycle

  func mount(options: FSTaskOptions, replyHandler reply: @escaping (Error?) -> Void) {
    reply(nil)
  }

  func unmount(replyHandler reply: @escaping () -> Void) {
    reply()
  }

  func synchronize(flags: FSSyncFlags, replyHandler reply: @escaping (Error?) -> Void) {
    // The shim has no fsync op; a slates volume is RAM-resident, so a sync is a no-op success.
    reply(nil)
  }

  func activate(options: FSTaskOptions, replyHandler reply: @escaping (FSItem?, Error?) -> Void) {
    // Learn the real root object from the daemon (OP_ROOT) rather than assuming a fixed inode — the
    // daemon's root is compose(prefix, 1), per-volume prefixed, not a constant.
    do {
      switch try call(.root, expecting: .attr) {
      case .attr(let attr):
        reply(
          SlatesItem(
            object: ObjectId(inode: attr.ino, generation: attr.generation), kind: attr.kind), nil)
      case .error(let tag): reply(nil, posixError(tag))
      default: reply(nil, ioError())
      }
    } catch { reply(nil, ioError()) }
  }

  func deactivate(options: FSDeactivateOptions, replyHandler reply: @escaping (Error?) -> Void) {
    reply(nil)
  }

  // MARK: FSVolume.Operations metadata

  func getAttributes(
    _ desiredAttributes: FSItem.GetAttributesRequest, of item: FSItem,
    replyHandler reply: @escaping (FSItem.Attributes?, Error?) -> Void
  ) {
    guard let node = item as? SlatesItem else { return reply(nil, ioError()) }
    do {
      switch try call(.getattr(object: node.object), expecting: .attr) {
      case .attr(let attr): reply(mappedAttributes(from: attr), nil)
      case .error(let tag): reply(nil, posixError(tag))
      default: reply(nil, ioError())
      }
    } catch { reply(nil, ioError()) }
  }

  func setAttributes(
    _ newAttributes: FSItem.SetAttributesRequest, on item: FSItem,
    replyHandler reply: @escaping (FSItem.Attributes?, Error?) -> Void
  ) {
    guard let node = item as? SlatesItem else { return reply(nil, ioError()) }
    // Only the fields FSKit marks valid become Some; the shim's setattr leaves the rest unchanged,
    // filling the unset half of a uid/gid or atime/mtime pair from the current value (bridge-core).
    let request = ShimRequest.setattr(
      object: node.object,
      size: newAttributes.isValid(.size) ? newAttributes.size : nil,
      mode: newAttributes.isValid(.mode) ? newAttributes.mode : nil,
      uid: newAttributes.isValid(.uid) ? newAttributes.uid : nil,
      gid: newAttributes.isValid(.gid) ? newAttributes.gid : nil,
      atime: newAttributes.isValid(.accessTime) ? unixNanos(from: newAttributes.accessTime) : nil,
      mtime: newAttributes.isValid(.modifyTime) ? unixNanos(from: newAttributes.modifyTime) : nil)
    do {
      switch try call(request, expecting: .attr) {
      case .attr(let attr): reply(mappedAttributes(from: attr), nil)
      case .error(let tag): reply(nil, posixError(tag))
      default: reply(nil, ioError())
      }
    } catch { reply(nil, ioError()) }
  }

  func lookupItem(
    named name: FSFileName, inDirectory directory: FSItem,
    replyHandler reply: @escaping (FSItem?, FSFileName?, Error?) -> Void
  ) {
    guard let parent = directory as? SlatesItem, let text = name.string else {
      return reply(nil, nil, ioError())
    }
    do {
      switch try call(.lookup(parent: parent.object, name: text), expecting: .attr) {
      case .attr(let attr):
        reply(SlatesItem(object: ObjectId(inode: attr.ino, generation: attr.generation),
          kind: attr.kind), name, nil)
      case .error(let tag): reply(nil, nil, posixError(tag))
      default: reply(nil, nil, ioError())
      }
    } catch { reply(nil, nil, ioError()) }
  }

  func reclaimItem(_ item: FSItem, replyHandler reply: @escaping (Error?) -> Void) {
    guard let node = item as? SlatesItem else { return reply(nil) }
    // Reclaim drops FSKit's reference; forget it once at the daemon. A transport failure here is
    // not worth surfacing — the kernel is discarding the item regardless.
    _ = try? call(.forget(object: node.object, nlookup: 1), expecting: .unit)
    reply(nil)
  }

  func readSymbolicLink(
    _ item: FSItem, replyHandler reply: @escaping (FSFileName?, Error?) -> Void
  ) {
    guard let node = item as? SlatesItem else { return reply(nil, ioError()) }
    do {
      switch try call(.readlink(object: node.object), expecting: .text) {
      case .text(let target): reply(FSFileName(string: target), nil)
      case .error(let tag): reply(nil, posixError(tag))
      default: reply(nil, ioError())
      }
    } catch { reply(nil, ioError()) }
  }

  // MARK: FSVolume.Operations namespace mutation

  func createItem(
    named name: FSFileName, type: FSItem.ItemType, inDirectory directory: FSItem,
    attributes newAttributes: FSItem.SetAttributesRequest,
    replyHandler reply: @escaping (FSItem?, FSFileName?, Error?) -> Void
  ) {
    guard let parent = directory as? SlatesItem, let text = name.string else {
      return reply(nil, nil, ioError())
    }
    let mode = newAttributes.isValid(.mode) ? newAttributes.mode : 0o644
    let request: ShimRequest =
      type == .directory
      ? .mkdir(parent: parent.object, name: text, mode: mode)
      : .create(parent: parent.object, name: text, mode: mode, flags: 0)
    do {
      switch try call(request, expecting: .attr) {
      case .attr(let attr):
        reply(SlatesItem(object: ObjectId(inode: attr.ino, generation: attr.generation),
          kind: attr.kind), name, nil)
      case .error(let tag): reply(nil, nil, posixError(tag))
      default: reply(nil, nil, ioError())
      }
    } catch { reply(nil, nil, ioError()) }
  }

  func createSymbolicLink(
    named name: FSFileName, inDirectory directory: FSItem,
    attributes newAttributes: FSItem.SetAttributesRequest, linkContents contents: FSFileName,
    replyHandler reply: @escaping (FSItem?, FSFileName?, Error?) -> Void
  ) {
    guard let parent = directory as? SlatesItem, let text = name.string,
      let target = contents.string
    else { return reply(nil, nil, ioError()) }
    do {
      switch try call(.symlink(parent: parent.object, name: text, target: target),
        expecting: .attr) {
      case .attr(let attr):
        reply(SlatesItem(object: ObjectId(inode: attr.ino, generation: attr.generation),
          kind: attr.kind), name, nil)
      case .error(let tag): reply(nil, nil, posixError(tag))
      default: reply(nil, nil, ioError())
      }
    } catch { reply(nil, nil, ioError()) }
  }

  func createLink(
    to item: FSItem, named name: FSFileName, inDirectory directory: FSItem,
    replyHandler reply: @escaping (FSFileName?, Error?) -> Void
  ) {
    guard let target = item as? SlatesItem, let parent = directory as? SlatesItem,
      let text = name.string
    else { return reply(nil, ioError()) }
    do {
      switch try call(.link(target: target.object, newParent: parent.object, newName: text),
        expecting: .attr) {
      case .attr: reply(name, nil)
      case .error(let tag): reply(nil, posixError(tag))
      default: reply(nil, ioError())
      }
    } catch { reply(nil, ioError()) }
  }

  func removeItem(
    _ item: FSItem, named name: FSFileName, fromDirectory directory: FSItem,
    replyHandler reply: @escaping (Error?) -> Void
  ) {
    guard let node = item as? SlatesItem, let parent = directory as? SlatesItem,
      let text = name.string
    else { return reply(ioError()) }
    let request: ShimRequest =
      node.kind == 1
      ? .rmdir(parent: parent.object, name: text)
      : .unlink(parent: parent.object, name: text)
    do {
      switch try call(request, expecting: .unit) {
      case .unit: reply(nil)
      case .error(let tag): reply(posixError(tag))
      default: reply(ioError())
      }
    } catch { reply(ioError()) }
  }

  func renameItem(
    _ item: FSItem, inDirectory sourceDirectory: FSItem, named sourceName: FSFileName,
    to destinationName: FSFileName, inDirectory destinationDirectory: FSItem,
    overItem: FSItem?, replyHandler reply: @escaping (FSFileName?, Error?) -> Void
  ) {
    guard let source = sourceDirectory as? SlatesItem,
      let destination = destinationDirectory as? SlatesItem,
      let oldName = sourceName.string, let newName = destinationName.string
    else { return reply(nil, ioError()) }
    do {
      switch try call(
        .rename(oldParent: source.object, newParent: destination.object, oldName: oldName,
          newName: newName, noReplace: false, exchange: false), expecting: .unit)
      {
      case .unit: reply(destinationName, nil)
      case .error(let tag): reply(nil, posixError(tag))
      default: reply(nil, ioError())
      }
    } catch { reply(nil, ioError()) }
  }

  func enumerateDirectory(
    _ directory: FSItem, startingAt cookie: FSDirectoryCookie, verifier: FSDirectoryVerifier,
    attributes: FSItem.GetAttributesRequest?, packer: FSDirectoryEntryPacker,
    replyHandler reply: @escaping (FSDirectoryVerifier, Error?) -> Void
  ) {
    guard let node = directory as? SlatesItem else { return reply(verifier, ioError()) }
    do {
      // Open the directory for a handle, read from the cookie, then release it. Self-contained: no
      // persistent state, so a re-enumeration from a cookie is a fresh open.
      guard case .handle(let fh) = try call(.opendir(object: node.object), expecting: .handle)
      else { return reply(verifier, ioError()) }
      defer { _ = try? call(.release(object: node.object, fh: fh), expecting: .unit) }
      switch try call(.readdir(object: node.object, fh: fh, offset: cookie.rawValue), expecting: .entries) {
      case .entries(let entries):
        var nextRaw = cookie.rawValue
        for entry in entries {
          nextRaw += 1
          _ = packer.packEntry(
            name: FSFileName(string: entry.name),
            itemType: itemType(fromKind: entry.kind),
            itemID: FSItem.Identifier(rawValue: entry.ino) ?? .invalid,
            nextCookie: FSDirectoryCookie(rawValue: nextRaw), attributes: nil)
        }
        reply(verifier, nil)
      case .error(let tag): reply(verifier, posixError(tag))
      default: reply(verifier, ioError())
      }
    } catch { reply(verifier, ioError()) }
  }

  // MARK: FSVolume.ReadWriteOperations

  func read(
    from item: FSItem, at offset: off_t, length: Int, into buffer: FSMutableFileDataBuffer,
    replyHandler reply: @escaping (Int, Error?) -> Void
  ) {
    guard let node = item as? SlatesItem else { return reply(0, ioError()) }
    do {
      switch try call(
        .read(object: node.object, offset: UInt64(offset), size: UInt32(length)),
        expecting: .bytes)
      {
      case .bytes(let data):
        let n = min(data.count, buffer.length)
        buffer.withUnsafeMutableBytes { raw in
          data.prefix(n).withUnsafeBytes { src in
            raw.baseAddress?.copyMemory(from: src.baseAddress!, byteCount: n)
          }
        }
        reply(n, nil)
      case .error(let tag): reply(0, posixError(tag))
      default: reply(0, ioError())
      }
    } catch { reply(0, ioError()) }
  }

  func write(
    contents: Data, to item: FSItem, at offset: off_t,
    replyHandler reply: @escaping (Int, Error?) -> Void
  ) {
    guard let node = item as? SlatesItem else { return reply(0, ioError()) }
    do {
      switch try call(
        .write(object: node.object, offset: UInt64(offset), data: [UInt8](contents)),
        expecting: .count)
      {
      case .count(let written): reply(Int(written), nil)
      case .error(let tag): reply(0, posixError(tag))
      default: reply(0, ioError())
      }
    } catch { reply(0, ioError()) }
  }

  // MARK: FSVolume.OpenCloseOperations

  func openItem(
    _ item: FSItem, modes: FSVolume.OpenModes, replyHandler reply: @escaping (Error?) -> Void
  ) {
    guard let node = item as? SlatesItem else { return reply(ioError()) }
    do {
      switch try call(.open(object: node.object, flags: UInt32(modes.rawValue)),
        expecting: .handle) {
      case .handle(let fh):
        openHandles[node.object.inode] = fh
        reply(nil)
      case .error(let tag): reply(posixError(tag))
      default: reply(ioError())
      }
    } catch { reply(ioError()) }
  }

  func closeItem(
    _ item: FSItem, modes: FSVolume.OpenModes, replyHandler reply: @escaping (Error?) -> Void
  ) {
    guard let node = item as? SlatesItem else { return reply(ioError()) }
    guard let fh = openHandles.removeValue(forKey: node.object.inode) else { return reply(nil) }
    _ = try? call(.release(object: node.object, fh: fh), expecting: .unit)
    reply(nil)
  }
}

// MARK: - The file system entry point

// The FSUnaryFileSystem FSKit loads for a slates resource. `probeResource` recognizes a slates
// resource; `loadResource` builds the volume. The resource-to-daemon binding is the mount-spike
// wiring; here the volume is constructed against the `UnwiredChannel` seam so the type conforms.
final class SlatesFileSystem: FSUnaryFileSystem, FSUnaryFileSystemOperations {
  func probeResource(
    resource: FSResource, replyHandler reply: @escaping (FSProbeResult?, Error?) -> Void
  ) {
    // SPIKE: a real probe reads the resource's slates marker; here it is recognized unconditionally
    // so the load path type-checks. The name and container id come from the daemon at the spike.
    reply(.usableButLimited, nil)
  }

  func loadResource(
    resource: FSResource, options: FSTaskOptions,
    replyHandler reply: @escaping (FSVolume?, Error?) -> Void
  ) {
    let volume = SlatesVolume(
      volumeID: FSVolume.Identifier(uuid: UUID()),
      volumeName: FSFileName(string: "slates"),
      channel: UnwiredChannel())
    reply(volume, nil)
  }

  func unloadResource(
    resource: FSResource, options: FSTaskOptions, replyHandler reply: @escaping (Error?) -> Void
  ) {
    reply(nil)
  }
}
