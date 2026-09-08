// OakFSUnaryFileSystem.swift — the FSKit module entry point.
//
// An `FSUnaryFileSystem` presents a single `FSResource` as a single
// `FSVolume`. For oak the resource is synthetic (no block device): macOS 26's
// `FSPathURLResource` lets us mount against a path URL (the mount state dir)
// rather than a /dev node. The daemon passes the per-mount id and IPC socket
// path through the `mount -o` options (`oak_id`, `oak_socket`); we read them
// here to find the `oak` daemon.
//
// See the API-surface note in OakFSItem.swift — the conformances here are
// reconciled against the macOS 26.1 FSKit SDK.

import ExtensionFoundation
import FSKit
import Foundation
import os

/// `@main` entry point for the FSKit app extension. On macOS the module is a
/// `UnaryFileSystemExtension` (macOS 15.4+) that vends the file-system object —
/// `@main` lives here, not on `FSUnaryFileSystem` (which has no `static main`).
@main
struct OakFSModule: UnaryFileSystemExtension {
    var fileSystem = OakFSUnaryFileSystem()
}

final class OakFSUnaryFileSystem: FSUnaryFileSystem, FSUnaryFileSystemOperations {
    private let log = Logger(subsystem: "com.oakvcs.mount.fskit", category: "module")

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
        // We deliberately do NOT read the daemon socket from `-o` mount options:
        // mount(8) silently drops unknown `-o key=value` pairs, so they never
        // reach FSKit (`options.taskOptions` arrives empty). Instead we derive
        // everything from the resource path URL, which IS forwarded: `oak mount`
        // passes the per-mount state dir (`~/.oak/mounts/<id>`) as the resource,
        // and the daemon listens on `<state_dir>/fskit.sock` (see
        // cli/.../fskit/mod.rs: socket_path / state_dir_for). The mount id is the
        // state dir's final path component.
        log.info("loadResource: taskOptions=[\(options.taskOptions.joined(separator: " ⎮ "), privacy: .public)]")

        guard let pathResource = resource as? FSPathURLResource else {
            log.error("loadResource: resource is not an FSPathURLResource")
            replyHandler(nil, fs_errorForPOSIXError(EINVAL))
            return
        }
        let stateDir = pathResource.url
        // The state dir lives in `~/.oak`, outside the extension's sandbox
        // container; FSKit hands it over as a security-scoped URL
        // (FSRequiresSecurityScopedPathURLResources). Claim access so we can
        // reach the unix socket inside it; released in unloadResource.
        _ = stateDir.startAccessingSecurityScopedResource()

        let mountID = stateDir.lastPathComponent
        let socket = stateDir.appendingPathComponent("fskit.sock").path
        log.info("loadResource: oak mount \(mountID, privacy: .public) via \(socket, privacy: .public)")

        // Volume label shown in Finder. `oak mount` writes a `volname` file
        // (the mount dir's leaf, e.g. the repo name) into the state dir; fall
        // back to the opaque `oak-<id>` if it's missing.
        let volnameURL = stateDir.appendingPathComponent("volname")
        let friendlyName = (try? String(contentsOf: volnameURL, encoding: .utf8))?
            .trimmingCharacters(in: .whitespacesAndNewlines)
        let volumeName = (friendlyName?.isEmpty == false) ? friendlyName! : "oak-\(mountID)"

        let client = DaemonClient(socketPath: socket)

        // Refuse to mount if the daemon speaks a different wire protocol than
        // this extension: an `Oak Mount` app and `oak` CLI from different
        // releases would otherwise trade misinterpreted frames. The mount fails
        // cleanly; the fix is to update both to the same Oak release.
        do {
            try client.handshake()
        } catch let DaemonError.protocolMismatch(mounter, daemon) {
            let daemonStr = daemon.map { "v\($0)" } ?? "older (pre-handshake)"
            log.error(
                "loadResource: wire-protocol mismatch — Oak Mount speaks v\(mounter, privacy: .public), oak daemon speaks \(daemonStr, privacy: .public). Update both Oak Mount and the oak CLI to the same release.")
            client.close()
            stateDir.stopAccessingSecurityScopedResource()
            replyHandler(nil, fs_errorForPOSIXError(EPROTONOSUPPORT))
            return
        } catch {
            log.error("loadResource: daemon handshake failed: \(error.localizedDescription, privacy: .public)")
            client.close()
            stateDir.stopAccessingSecurityScopedResource()
            replyHandler(nil, fs_errorForPOSIXError(EIO))
            return
        }

        let volume = OakFSVolume(daemon: client, volumeName: volumeName, fileSystem: self)

        // Transition the container out of `notReady`. FSKit checks
        // `containerStatus` immediately after loadResource returns; while it's
        // `notReady` the mount fails with "unexpected container state … Code=35"
        // (EAGAIN). loadResource must move it to `.ready` (NOT `.active` — that's
        // also "unexpected" here and surfaces as EPROTONOSUPPORT). The
        // `ready → active` step happens when the single volume activates (see
        // OakFSVolume.activate and the FSFileSystemBase.containerStatus diagram).
        containerStatus = .ready

        replyHandler(volume, nil)
    }

    func unloadResource(
        resource: FSResource,
        options: FSTaskOptions,
        replyHandler: @escaping (Error?) -> Void
    ) {
        if let pathResource = resource as? FSPathURLResource {
            pathResource.url.stopAccessingSecurityScopedResource()
        }
        containerStatus = .notReady(status: fs_errorForPOSIXError(EAGAIN))
        replyHandler(nil)
    }
}
