// DaemonClient.swift — the channel from the FSKit extension to the `oak`
// daemon that owns the filesystem logic.
//
// TRANSPORT STATUS
// ----------------
// This implements the framed JSON protocol over a **Unix domain socket**,
// matching cli/src/commands/mount/fskit/ipc.rs byte-for-byte. That is the
// bring-up / development path: run the extension with App Sandbox disabled
// (see OakFS.entitlements) and it talks to the daemon's socket directly.
//
// For a SHIPPING (sandboxed) extension the sandbox forbids opening a unix
// socket. Production must carry the same Op/Reply frames over an **XPC mach
// service** instead — register the service name `com.oak.mount.<id>` (the
// `oak_id` mount option) in `volumeDidMount`, connect via NSXPCConnection /
// xpc_connection_create_mach_service, and forward frames. The request/reply
// contract is identical; only `send(_:)`'s transport changes. This is the
// last piece of the migration and needs the fskit entitlement to test.

import Foundation

/// Thread-safe request/reply client. One in-flight request at a time per
/// connection; FSKit may call us concurrently, so we serialize on a queue.
final class DaemonClient {
    private let socketPath: String
    private let queue = DispatchQueue(label: "com.oak.mount.daemonclient")
    private var fd: Int32 = -1

    init(socketPath: String) {
        self.socketPath = socketPath
    }

    deinit { close() }

    private func ensureConnected() throws {
        if fd >= 0 { return }
        let s = socket(AF_UNIX, SOCK_STREAM, 0)
        guard s >= 0 else { throw DaemonError.io("socket() failed: \(errno)") }

        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        let pathBytes = socketPath.utf8CString
        guard pathBytes.count <= MemoryLayout.size(ofValue: addr.sun_path) else {
            Darwin.close(s)
            throw DaemonError.io("socket path too long")
        }
        withUnsafeMutablePointer(to: &addr.sun_path) { ptr in
            ptr.withMemoryRebound(to: CChar.self, capacity: pathBytes.count) { dst in
                _ = strcpy(dst, pathBytes)
            }
        }
        let len = socklen_t(MemoryLayout<sockaddr_un>.size)
        let rc = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.connect(s, $0, len)
            }
        }
        guard rc == 0 else {
            Darwin.close(s)
            throw DaemonError.io("connect(\(socketPath)) failed: \(errno)")
        }
        fd = s
    }

    func close() {
        queue.sync {
            if fd >= 0 { Darwin.close(fd); fd = -1 }
        }
    }

    /// Send one operation and await the reply.
    func send(_ op: Op) throws -> Reply {
        try queue.sync {
            try ensureConnected()
            let payload = try JSONEncoder().encode(op)
            try writeFrame(payload)
            let respBytes = try readFrame()
            return try JSONDecoder().decode(Reply.self, from: respBytes)
        }
    }

    // ---- framing (mirror of ipc.rs) -------------------------------------

    private func writeFrame(_ payload: Data) throws {
        var len = UInt32(payload.count).bigEndian
        var header = Data(bytes: &len, count: 4)
        header.append(payload)
        try writeAll(header)
    }

    private func readFrame() throws -> Data {
        let header = try readExactly(4)
        let len = header.withUnsafeBytes { $0.load(as: UInt32.self).bigEndian }
        return try readExactly(Int(len))
    }

    private func writeAll(_ data: Data) throws {
        try data.withUnsafeBytes { (raw: UnsafeRawBufferPointer) in
            var off = 0
            let base = raw.baseAddress!
            while off < data.count {
                let n = Darwin.write(fd, base + off, data.count - off)
                if n <= 0 { throw DaemonError.io("write failed: \(errno)") }
                off += n
            }
        }
    }

    private func readExactly(_ count: Int) throws -> Data {
        var buf = Data(count: count)
        if count == 0 { return buf }
        try buf.withUnsafeMutableBytes { (raw: UnsafeMutableRawBufferPointer) in
            var off = 0
            let base = raw.baseAddress!
            while off < count {
                let n = Darwin.read(fd, base + off, count - off)
                if n == 0 { throw DaemonError.io("daemon closed connection") }
                if n < 0 { throw DaemonError.io("read failed: \(errno)") }
                off += n
            }
        }
        return buf
    }
}

enum DaemonError: Error {
    case io(String)
}
