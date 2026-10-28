// OakFSVolume.swift — the volume. Every FSKit operation maps to one daemon
// call (see Wire.swift / protocol.rs). The volume holds no filesystem state
// itself; the daemon owns the inode tree, overlay, and reconciliation.
//
// See the API-surface note in OakFSItem.swift — the FSVolume.Operations /
// ReadWriteOperations conformances are reconciled against the macOS 26.1 FSKit
// SDK. The body of each method (the daemon round-trip + reply mapping) is the
// stable, oak-specific part.

import FSKit
import Foundation
import os

private let ROOT_INO: UInt64 = 1

final class OakFSVolume: FSVolume {
    private let daemon: DaemonClient
    private let log = Logger(subsystem: "com.oakvcs.mount.fskit", category: "volume")
    // The owning unary file system, so volume activation can drive the shared
    // container state (`ready` ⇄ `active`). Weak: FSKit owns the volume and the
    // file system outlives it.
    private weak var fileSystem: OakFSUnaryFileSystem?

    // Inode → item cache so FSKit gets stable FSItem identities.
    private let lock = NSLock()
    private var items: [UInt64: OakFSItem] = [:]

    init(daemon: DaemonClient, volumeName: String, fileSystem: OakFSUnaryFileSystem) {
        self.daemon = daemon
        self.fileSystem = fileSystem
        let volumeID = FSVolume.Identifier(uuid: UUID())
        super.init(volumeID: volumeID, volumeName: FSFileName(string: volumeName))
        let root = OakFSItem(ino: ROOT_INO, name: FSFileName(string: ""))
        items[ROOT_INO] = root
    }

    private func item(ino: UInt64, name: FSFileName) -> OakFSItem {
        lock.lock(); defer { lock.unlock() }
        if let existing = items[ino] { return existing }
        let made = OakFSItem(ino: ino, name: name)
        items[ino] = made
        return made
    }

    private func errno(from reply: Reply) -> Error {
        if case let .errno(code) = reply { return fs_errorForPOSIXError(code) }
        return fs_errorForPOSIXError(EIO)
    }
}

// MARK: - FSVolume.Operations
extension OakFSVolume: FSVolume.Operations {
    var supportedVolumeCapabilities: FSVolume.SupportedCapabilities {
        let caps = FSVolume.SupportedCapabilities()
        caps.supportsHardLinks = false
        caps.supportsSymbolicLinks = true
        caps.supportsPersistentObjectIDs = true
        caps.caseFormat = .sensitive
        return caps
    }

    var volumeStatistics: FSStatFSResult {
        // Synthetic volume — report a large nominal capacity.
        let r = FSStatFSResult(fileSystemTypeName: "OakFS")
        r.blockSize = 4096
        r.ioSize = 1 << 20
        return r
    }

    func activate(options: FSTaskOptions, replyHandler: @escaping (FSItem?, Error?) -> Void) {
        // The single volume is now live → drive the shared container to `active`
        // (loadResource left it at `ready`). See FSFileSystemBase.containerStatus.
        fileSystem?.containerStatus = .active
        replyHandler(items[ROOT_INO], nil)
    }

    func deactivate(options: FSDeactivateOptions, replyHandler: @escaping (Error?) -> Void) {
        fileSystem?.containerStatus = .ready
        daemon.close()
        replyHandler(nil)
    }

    func mount(options: FSTaskOptions, replyHandler: @escaping (Error?) -> Void) {
        replyHandler(nil)
    }

    func unmount(replyHandler: @escaping () -> Void) {
        daemon.close()
        replyHandler()
    }

    func getAttributes(
        _ desired: FSItem.GetAttributesRequest,
        of item: FSItem,
        replyHandler: @escaping (FSItem.Attributes?, Error?) -> Void
    ) {
        guard let oakItem = item as? OakFSItem else {
            replyHandler(nil, fs_errorForPOSIXError(EINVAL)); return
        }
        do {
            let reply = try daemon.send(.getattr(ino: oakItem.ino))
            guard case let .attr(a) = reply else { replyHandler(nil, errno(from: reply)); return }
            let attrs = FSItem.Attributes()
            a.apply(to: attrs)
            replyHandler(attrs, nil)
        } catch {
            replyHandler(nil, error)
        }
    }

    func lookupItem(
        named name: FSFileName,
        inDirectory directory: FSItem,
        replyHandler: @escaping (FSItem?, FSFileName?, Error?) -> Void
    ) {
        guard let dir = directory as? OakFSItem, let nameStr = name.string else {
            replyHandler(nil, nil, fs_errorForPOSIXError(EINVAL)); return
        }
        do {
            let reply = try daemon.send(.lookup(parent: dir.ino, name: nameStr))
            guard case let .attr(a) = reply else { replyHandler(nil, nil, errno(from: reply)); return }
            replyHandler(item(ino: a.ino, name: name), name, nil)
        } catch {
            replyHandler(nil, nil, error)
        }
    }

    func enumerateDirectory(
        _ directory: FSItem,
        startingAt cookie: FSDirectoryCookie,
        verifier: FSDirectoryVerifier,
        attributes: FSItem.GetAttributesRequest?,
        packer: FSDirectoryEntryPacker,
        replyHandler: @escaping (FSDirectoryVerifier, Error?) -> Void
    ) {
        guard let dir = directory as? OakFSItem else {
            replyHandler(verifier, fs_errorForPOSIXError(EINVAL)); return
        }
        do {
            let reply = try daemon.send(.readdir(ino: dir.ino))
            guard case let .entries(entries) = reply else { replyHandler(verifier, errno(from: reply)); return }
            // `cookie` is the next FSKit-visible index to emit. The daemon
            // keeps FUSE-style "."/".." entries in its readdir contract, but
            // FSKit directory packing expects only real children.
            let visibleEntries = entries.filter { $0.name != "." && $0.name != ".." }
            var index = UInt64(cookie.rawValue)
            while index < UInt64(visibleEntries.count) {
                let attemptedIndex = index
                let e = visibleEntries[Int(index)]
                let kind = WireKind(rawValue: e.kind) ?? .file
                index += 1
                // When FSKit asks for an attributed enumeration (`attributes`
                // non-nil, e.g. readdirattr), every entry MUST be packed *with*
                // its attributes — passing nil makes packEntry fail ("No
                // attributes found") and the listing comes back empty. Fetch the
                // entry's attributes from the daemon in that case. (The daemon
                // resolves these from its in-memory tree, so this is cheap.)
                var entryAttrs: FSItem.Attributes?
                if attributes != nil {
                    let a = FSItem.Attributes()
                    if case let .attr(wire) = try daemon.send(.getattr(ino: e.ino)) {
                        wire.apply(to: a)
                    }
                    entryAttrs = a
                }
                let packed = packer.packEntry(
                    name: FSFileName(string: e.name),
                    itemType: kind.fsItemType,
                    itemID: FSItem.Identifier(rawValue: e.ino) ?? .invalid,
                    nextCookie: FSDirectoryCookie(rawValue: index),
                    attributes: entryAttrs)
                if !packed {
                    if attemptedIndex == UInt64(cookie.rawValue) {
                        log.error("FSKit refused to pack first visible directory entry '\(e.name, privacy: .public)' for ino \(dir.ino)")
                        replyHandler(verifier, fs_errorForPOSIXError(EINVAL)); return
                    }
                    // Buffer full → rewind so the next call re-emits this entry.
                    index -= 1
                    break
                }
            }
            replyHandler(verifier, nil)
        } catch {
            replyHandler(verifier, error)
        }
    }

    func createItem(
        named name: FSFileName,
        type: FSItem.ItemType,
        inDirectory directory: FSItem,
        attributes: FSItem.SetAttributesRequest,
        replyHandler: @escaping (FSItem?, FSFileName?, Error?) -> Void
    ) {
        guard let dir = directory as? OakFSItem, let nameStr = name.string else {
            replyHandler(nil, nil, fs_errorForPOSIXError(EINVAL)); return
        }
        do {
            let op: Op
            if type == .directory {
                op = .mkdir(parent: dir.ino, name: nameStr)
            } else {
                let exec = (attributes.mode & 0o111) != 0
                op = .create(parent: dir.ino, name: nameStr, exec: exec)
            }
            let reply = try daemon.send(op)
            guard case let .attr(a) = reply else { replyHandler(nil, nil, errno(from: reply)); return }
            replyHandler(item(ino: a.ino, name: name), name, nil)
        } catch {
            replyHandler(nil, nil, error)
        }
    }

    func removeItem(
        _ item: FSItem,
        named name: FSFileName,
        fromDirectory directory: FSItem,
        replyHandler: @escaping (Error?) -> Void
    ) {
        guard let dir = directory as? OakFSItem, let oakItem = item as? OakFSItem,
              let nameStr = name.string else {
            replyHandler(fs_errorForPOSIXError(EINVAL)); return
        }
        do {
            // Pick rmdir vs unlink from the item's attributes.
            let attrReply = try daemon.send(.getattr(ino: oakItem.ino))
            let isDir: Bool
            if case let .attr(a) = attrReply { isDir = (a.kind == WireKind.dir.rawValue) } else { isDir = false }
            let op: Op = isDir
                ? .rmdir(parent: dir.ino, name: nameStr)
                : .unlink(parent: dir.ino, name: nameStr)
            let reply = try daemon.send(op)
            if case .ok = reply { replyHandler(nil) } else { replyHandler(errno(from: reply)) }
        } catch {
            replyHandler(error)
        }
    }

    func renameItem(
        _ item: FSItem,
        inDirectory sourceDirectory: FSItem,
        named sourceName: FSFileName,
        to destinationName: FSFileName,
        inDirectory destinationDirectory: FSItem,
        overItem: FSItem?,
        replyHandler: @escaping (FSFileName?, Error?) -> Void
    ) {
        guard let srcDir = sourceDirectory as? OakFSItem,
              let dstDir = destinationDirectory as? OakFSItem,
              let srcName = sourceName.string, let dstName = destinationName.string else {
            replyHandler(nil, fs_errorForPOSIXError(EINVAL)); return
        }
        do {
            let reply = try daemon.send(.rename(
                parent: srcDir.ino, name: srcName,
                newparent: dstDir.ino, newname: dstName))
            if case .ok = reply { replyHandler(destinationName, nil) } else { replyHandler(nil, errno(from: reply)) }
        } catch {
            replyHandler(nil, error)
        }
    }

    func setAttributes(
        _ request: FSItem.SetAttributesRequest,
        on item: FSItem,
        replyHandler: @escaping (FSItem.Attributes?, Error?) -> Void
    ) {
        guard let oakItem = item as? OakFSItem else {
            replyHandler(nil, fs_errorForPOSIXError(EINVAL)); return
        }
        do {
            // Only size changes (truncate) are honored; mode/uid/times are
            // read-only in oak mounts.
            if request.isValid(.size) {
                let reply = try daemon.send(.truncate(ino: oakItem.ino, size: request.size))
                guard case let .attr(a) = reply else { replyHandler(nil, errno(from: reply)); return }
                let attrs = FSItem.Attributes(); a.apply(to: attrs); replyHandler(attrs, nil)
                return
            }
            // No-op: re-stat and return current attributes.
            let reply = try daemon.send(.getattr(ino: oakItem.ino))
            guard case let .attr(a) = reply else { replyHandler(nil, errno(from: reply)); return }
            let attrs = FSItem.Attributes(); a.apply(to: attrs); replyHandler(attrs, nil)
        } catch {
            replyHandler(nil, error)
        }
    }

    // No write-back cache in the extension — writes go straight to the daemon
    // overlay — so there's nothing to flush.
    func synchronize(flags: FSSyncFlags, replyHandler: @escaping (Error?) -> Void) {
        replyHandler(nil)
    }

    // FSKit is done with this item; drop our inode-cache entry.
    func reclaimItem(_ item: FSItem, replyHandler: @escaping (Error?) -> Void) {
        if let oakItem = item as? OakFSItem {
            lock.lock(); items[oakItem.ino] = nil; lock.unlock()
        }
        replyHandler(nil)
    }

    // Resolve an existing symlink. oak stores a symlink's target as its blob
    // content, so the daemon's `Readlink` op hands back the raw target bytes
    // (see protocol.rs / server.rs); we wrap them in an `FSFileName`.
    func readSymbolicLink(
        _ item: FSItem,
        replyHandler: @escaping (FSFileName?, Error?) -> Void
    ) {
        guard let oakItem = item as? OakFSItem else {
            replyHandler(nil, fs_errorForPOSIXError(EINVAL)); return
        }
        do {
            let reply = try daemon.send(.readlink(ino: oakItem.ino))
            guard case let .data(data) = reply else { replyHandler(nil, errno(from: reply)); return }
            replyHandler(FSFileName(string: String(decoding: data, as: UTF8.self)), nil)
        } catch {
            replyHandler(nil, error)
        }
    }

    // Creating new symbolic/hard links is still unsupported: the daemon has no
    // create-link op, and a mount's overlay doesn't model new links. Report
    // "not supported" rather than fail opaquely — reading existing symlinks
    // (above) and regular files/directories work fully.
    func createSymbolicLink(
        named name: FSFileName,
        inDirectory directory: FSItem,
        attributes: FSItem.SetAttributesRequest,
        linkContents contents: FSFileName,
        replyHandler: @escaping (FSItem?, FSFileName?, Error?) -> Void
    ) {
        replyHandler(nil, nil, fs_errorForPOSIXError(ENOTSUP))
    }

    func createLink(
        to item: FSItem,
        named name: FSFileName,
        inDirectory directory: FSItem,
        replyHandler: @escaping (FSFileName?, Error?) -> Void
    ) {
        replyHandler(nil, fs_errorForPOSIXError(ENOTSUP))
    }
}

// MARK: - FSVolume.ReadWriteOperations
extension OakFSVolume: FSVolume.ReadWriteOperations {
    func read(
        from item: FSItem,
        at offset: off_t,
        length: Int,
        into buffer: FSMutableFileDataBuffer,
        replyHandler: @escaping (Int, Error?) -> Void
    ) {
        guard let oakItem = item as? OakFSItem else {
            replyHandler(0, fs_errorForPOSIXError(EINVAL)); return
        }
        do {
            let reply = try daemon.send(.read(
                ino: oakItem.ino, offset: Int64(offset), size: UInt32(length)))
            guard case let .data(data) = reply else { replyHandler(0, errno(from: reply)); return }
            let n = buffer.withUnsafeMutableBytes { dst -> Int in
                let count = min(data.count, dst.count)
                data.copyBytes(to: dst.bindMemory(to: UInt8.self), count: count)
                return count
            }
            replyHandler(n, nil)
        } catch {
            replyHandler(0, error)
        }
    }

    func write(
        contents data: Data,
        to item: FSItem,
        at offset: off_t,
        replyHandler: @escaping (Int, Error?) -> Void
    ) {
        guard let oakItem = item as? OakFSItem else {
            replyHandler(0, fs_errorForPOSIXError(EINVAL)); return
        }
        do {
            let reply = try daemon.send(.write(
                ino: oakItem.ino, offset: Int64(offset), data: data))
            guard case let .written(n) = reply else { replyHandler(0, errno(from: reply)); return }
            replyHandler(Int(n), nil)
        } catch {
            replyHandler(0, error)
        }
    }
}

// MARK: - FSVolume.PathConfOperations
// `FSVolume.Operations` refines this, so the volume must supply these limits.
extension OakFSVolume: FSVolume.PathConfOperations {
    // oak doesn't expose hard links (see supportsHardLinks = false above).
    var maximumLinkCount: Int { 1 }
    var maximumNameLength: Int { 255 }
    var restrictsOwnershipChanges: Bool { false }
    // Over-long names are rejected with ENAMETOOLONG rather than truncated.
    var truncatesLongNames: Bool { false }
}
