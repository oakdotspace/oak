// DaemonClient.swift — the channel from the sandboxed FSKit extension to the
// `oak` daemon that owns the filesystem logic.
//
// TRANSPORT
// ---------
// FSKit extensions are App-Sandboxed, so they cannot open the daemon's unix
// socket directly. Instead the extension connects to a single launchd-vended
// XPC mach service, **com.oakvcs.mount** (whitelisted in OakFS.entitlements), and
// includes the per-mount daemon socket path in each request. The broker
// (`oak mount __fskit-broker`, started on demand by the com.oakvcs.mount
// LaunchAgent) forwards the framed `Op`/`Reply` bytes to that socket. The wire
// contract (Wire.swift / protocol.rs) is unchanged; only the byte transport is
// XPC instead of a direct socket.

import Foundation
import XPC

/// The mach service the broker vends. Must match the LaunchAgent MachServices
/// key, the Rust broker, and the mach-lookup entitlement.
private let kMachService = "com.oakvcs.mount"

/// Thread-safe request/reply client. One in-flight request at a time per
/// connection; FSKit may call us concurrently, so we serialize on a queue.
/// `@unchecked Sendable`: all mutable state (`connection`) is touched only on
/// `queue`, which is what lets the XPC event handler hop back onto it.
final class DaemonClient: @unchecked Sendable {
    /// The per-mount daemon socket path (the `oak_socket` mount option). The
    /// broker connects to it on our behalf; the sandbox stops us doing so
    /// directly.
    private let socketPath: String
    private let queue = DispatchQueue(label: "com.oakvcs.mount.daemonclient")
    private var connection: xpc_connection_t?

    init(socketPath: String) {
        self.socketPath = socketPath
    }

    deinit { close() }

    private func connect() -> xpc_connection_t {
        if let c = connection { return c }
        let conn = xpc_connection_create_mach_service(kMachService, nil, 0)
        xpc_connection_set_event_handler(conn) { [weak self] event in
            // `interrupted` (the broker process restarted) leaves the connection
            // usable: libxpc relaunches the broker on the next message.
            // `invalid` (the broker's LaunchAgent was booted out and reloaded)
            // is permanent, so drop the connection and let the next request
            // look the service up again. Without this every live mount keeps
            // failing until it is unmounted and remounted.
            guard xpc_equal(event, XPC_ERROR_CONNECTION_INVALID) else { return }
            self?.queue.async { self?.discard(conn) }
        }
        xpc_connection_resume(conn)
        connection = conn
        return conn
    }

    /// Forget `conn` if it is still the cached connection. Call on `queue`.
    private func discard(_ conn: xpc_connection_t) {
        guard let c = connection, c === conn else { return }
        xpc_connection_cancel(c)
        connection = nil
    }

    func close() {
        queue.sync {
            if let c = connection {
                xpc_connection_cancel(c)
                connection = nil
            }
        }
    }

    /// Verify the daemon speaks our wire protocol before serving any VFS op.
    /// Sends `Op.hello` and checks the daemon's reported version against
    /// `kOakProtocolVersion`. Throws `DaemonError.protocolMismatch` on any
    /// disagreement — including a daemon too old to understand `Hello`, which
    /// fails to decode the request and answers with an errno rather than
    /// `.hello`; that non-`.hello` reply is exactly the skew we want to catch.
    /// Call once at mount time: a mismatched `Oak Mount` app and `oak` CLI
    /// would otherwise misinterpret each other's frames.
    func handshake() throws {
        let reply = try send(.hello(protocolVersion: kOakProtocolVersion))
        guard case let .hello(daemonVersion) = reply else {
            throw DaemonError.protocolMismatch(mounter: kOakProtocolVersion, daemon: nil)
        }
        if daemonVersion != kOakProtocolVersion {
            throw DaemonError.protocolMismatch(mounter: kOakProtocolVersion, daemon: daemonVersion)
        }
    }

    /// Send one operation and await the reply. Synchronous round trip over XPC.
    func send(_ op: Op) throws -> Reply {
        try queue.sync {
            let payload = try JSONEncoder().encode(op)

            let msg = xpc_dictionary_create(nil, nil, 0)
            socketPath.withCString { xpc_dictionary_set_string(msg, "socket", $0) }
            // Op JSON is never empty, so baseAddress is always non-nil.
            payload.withUnsafeBytes { (raw: UnsafeRawBufferPointer) in
                xpc_dictionary_set_data(msg, "frame", raw.baseAddress!, raw.count)
            }

            var conn = connect()
            var reply = xpc_connection_send_message_with_reply_sync(conn, msg)
            if xpc_equal(reply, XPC_ERROR_CONNECTION_INVALID) {
                // The broker was reloaded since our last request. Look it up
                // afresh and retry once. Usually the connection was already
                // dead and the op never left; one caught mid-flight by the
                // reload could reach the daemon twice, which beats failing
                // every op until a remount.
                discard(conn)
                conn = connect()
                reply = xpc_connection_send_message_with_reply_sync(conn, msg)
            }
            guard xpc_get_type(reply) == XPC_TYPE_DICTIONARY else {
                throw DaemonError.io("broker did not reply (service unavailable?)")
            }
            guard let frameVal = xpc_dictionary_get_value(reply, "frame"),
                xpc_get_type(frameVal) == XPC_TYPE_DATA
            else {
                throw DaemonError.io("broker reply had no frame")
            }
            let len = xpc_data_get_length(frameVal)
            let data: Data = {
                guard len > 0, let ptr = xpc_data_get_bytes_ptr(frameVal) else { return Data() }
                return Data(bytes: ptr, count: len)
            }()
            return try JSONDecoder().decode(Reply.self, from: data)
        }
    }
}

enum DaemonError: Error {
    case io(String)
    /// The daemon speaks a different wire-protocol version than this extension
    /// (`daemon == nil` means it's too old to even understand the handshake).
    case protocolMismatch(mounter: UInt32, daemon: UInt32?)
}
