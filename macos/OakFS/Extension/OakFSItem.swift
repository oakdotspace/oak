// OakFSItem.swift — an FSItem backed by an oak inode number.
//
// NOTE ON FSKit API SURFACE
// -------------------------
// FSKit's Swift API changed across macOS 15.x betas. The conformances here are
// reconciled against the macOS 26.1 SDK (FSKit.framework/Headers/FSVolume.h),
// which is what this target builds with. One deliberate divergence from Apple's
// PassthroughFS sample: that sample (an older beta) declares `renameItem` under
// `FSVolume.RenameOperations`, but in 26.1 `renameItem` belongs to
// `FSVolume.Operations` and `RenameOperations` only carries `setVolumeName` —
// so OakFSVolume keeps `renameItem` in its `FSVolume.Operations` conformance.
// The oak-specific logic — mapping each operation to a `DaemonClient` call and
// translating the reply — is what matters here and is stable regardless.

import FSKit
import Foundation

final class OakFSItem: FSItem {
    let ino: UInt64
    var name: FSFileName

    init(ino: UInt64, name: FSFileName) {
        self.ino = ino
        self.name = name
        super.init()
    }
}

extension WireKind {
    /// Map the wire file-type tag to an FSKit item type.
    var fsItemType: FSItem.ItemType {
        switch self {
        case .dir: return .directory
        case .file: return .file
        case .symlink: return .symlink
        }
    }
}

extension AttrWire {
    /// Populate an `FSItem.Attributes` from a daemon stat reply.
    func apply(to attrs: FSItem.Attributes) {
        let kind = WireKind(rawValue: kind) ?? .file
        attrs.type = kind.fsItemType
        attrs.mode = UInt32(perm)
        attrs.size = size
        attrs.allocSize = size
        attrs.fileID = FSItem.Identifier(rawValue: ino) ?? .invalid
        attrs.linkCount = (kind == .dir) ? 2 : 1
        let secs = Double(mtime_ns) / 1_000_000_000.0
        let ts = timespec(tv_sec: Int(secs), tv_nsec: Int(mtime_ns % 1_000_000_000))
        attrs.modifyTime = ts
        attrs.changeTime = ts
        attrs.accessTime = ts
    }
}
