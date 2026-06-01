// OakFSVolume.swift — the volume. Every FSKit operation maps to one daemon
// call (see Wire.swift / protocol.rs). The volume holds no filesystem state
// itself; the daemon owns the inode tree, overlay, and reconciliation.
//
// See the API-surface note in OakFSItem.swift — the FSVolume.Operations /
// ReadWriteOperations method signatures must be reconciled with the macOS 26
// FSKit SDK. The body of each method (the daemon round-trip + reply mapping)
// is the stable, oak-specific part.

import FSKit
import Foundation
import os

private let ROOT_INO: UInt64 = 1

final class OakFSVolume: FSVolume {
    private let daemon: DaemonClient
    private let mountID: String
    private let log = Logger(subsystem: "com.oak.mount.fskit", category: "volume")

    // Inode → item cache so FSKit gets stable FSItem identities.
    private let lock = NSLock()
    private var items: [UInt64: OakFSItem] = [:]

    init(daemon: DaemonClient, mountID: String) {
        self.daemon = daemon
        self.mountID = mountID
        let volumeID = FSVolume.Identifier(uuid: UUID())
        super.init(volumeID: volumeID, volumeName: FSFileName(string: "oak-\(mountID)"))
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
        replyHandler(items[ROOT_INO], nil)
    }

    func deactivate(options: FSDeactivateOptions, replyHandler: @escaping (Error?) -> Void) {
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
            // `cookie` is the next index to emit; the daemon returns the full
            // list (incl. "." and "..") in stable order.
            var index = UInt64(cookie.rawValue)
            while index < UInt64(entries.count) {
                let e = entries[Int(index)]
                let kind = WireKind(rawValue: e.kind) ?? .file
                index += 1
                let packed = packer.packEntry(
                    name: FSFileName(string: e.name),
                    itemType: kind.fsItemType,
                    itemID: FSItem.Identifier(rawValue: e.ino) ?? .invalid,
                    nextCookie: FSDirectoryCookie(rawValue: Int(index)),
                    attributes: nil)
                if !packed { break } // packer buffer full; FSKit will call again
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
}

// MARK: - FSVolume.ReadWriteOperations
extension OakFSVolume: FSVolume.ReadWriteOperations {
    func read(
        from item: FSItem,
        offset: off_t,
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
        offset: off_t,
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
