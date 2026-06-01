// OakFSUnaryFileSystem.swift — the FSKit module entry point.
//
// An `FSUnaryFileSystem` presents a single `FSResource` as a single
// `FSVolume`. For oak the resource is synthetic (no block device): macOS 26's
// `FSPathURLResource` lets us mount against a path URL (the mount state dir)
// rather than a /dev node. The daemon passes the per-mount id and IPC socket
// path through the `mount -o` options (`oak_id`, `oak_socket`); we read them
// here to find the `oak` daemon.
//
// See the API-surface note in OakFSItem.swift — reconcile signatures with the
// macOS 26 FSKit SDK.

import ExtensionFoundation
import FSKit
import Foundation
import os

@main
final class OakFSUnaryFileSystem: FSUnaryFileSystem, FSUnaryFileSystemOperations {
    private let log = Logger(subsystem: "com.oak.mount.fskit", category: "module")

    override init() {
        super.init()
    }

    // Probe whether we can handle this resource. For a synthetic path URL
    // resource we always can (the daemon decides what's behind it).
    func probeResource(
        resource: FSResource,
        replyHandler: @escaping (FSProbeResult?, Error?) -> Void
    ) {
        // `usable` with a name + a stable container/volume identifier.
        let result = FSProbeResult.usable(
            name: "OakFS",
            containerID: FSContainerIdentifier(uuid: UUID()))
        replyHandler(result, nil)
    }

    func loadResource(
        resource: FSResource,
        options: FSTaskOptions,
        replyHandler: @escaping (FSVolume?, Error?) -> Void
    ) {
        // Pull `oak_id` / `oak_socket` out of the mount options.
        let opts = MountOptions(taskOptions: options)
        guard let socket = opts["oak_socket"] else {
            log.error("loadResource: missing oak_socket mount option")
            replyHandler(nil, fs_errorForPOSIXError(EINVAL))
            return
        }
        let mountID = opts["oak_id"] ?? "unknown"
        log.info("loadResource: oak mount \(mountID, privacy: .public) via \(socket, privacy: .public)")

        let client = DaemonClient(socketPath: socket)
        let volume = OakFSVolume(daemon: client, mountID: mountID)
        replyHandler(volume, nil)
    }

    func unloadResource(
        resource: FSResource,
        options: FSTaskOptions,
        replyHandler: @escaping (Error?) -> Void
    ) {
        replyHandler(nil)
    }
}

/// Tiny helper to parse `key=value,key2=value2` style mount options out of the
/// FSKit task options. (FSKit hands options as an array of strings; the exact
/// accessor name varies by SDK — adjust `rawOptions` accordingly.)
private struct MountOptions {
    private var map: [String: String] = [:]

    init(taskOptions: FSTaskOptions) {
        for token in taskOptions.rawOptions {
            for pair in token.split(separator: ",") {
                let kv = pair.split(separator: "=", maxSplits: 1)
                if kv.count == 2 {
                    map[String(kv[0])] = String(kv[1])
                }
            }
        }
    }

    subscript(_ key: String) -> String? { map[key] }
}
