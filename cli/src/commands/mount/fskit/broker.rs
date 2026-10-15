//! XPC mach-service broker for the sandboxed OakFS extension.
//!
//! macOS requires FSKit extensions to be App-Sandboxed, and the sandbox forbids
//! opening the daemon's Unix socket directly. The extension instead connects to
//! a single launchd-vended mach service, **`com.oakvcs.mount`** (the exact name
//! whitelisted in `Extension/OakFS.entitlements`), and includes the per-mount
//! `oak_id` in each request. This broker — run on demand by launchd as
//! `oak __fskit-broker` — receives those XPC requests and forwards the framed
//! `Op`/`Reply` bytes to the right mount's existing Unix socket
//! (`~/.oak/mounts/<id>/fskit.sock`). All filesystem logic stays in the
//! unchanged per-mount daemon; the broker only bridges bytes.
//!
//! Why a broker (not a per-mount mach service): launchd only routes
//! `mach-lookup` to a name declared in a LaunchAgent's `MachServices`; a
//! transient `oak mount` process can't register one. One on-demand LaunchAgent
//! owning `com.oakvcs.mount`, routing by `oak_id`, solves it.
//!
//! A request carrying a `socket` key is forwarded to that mount's daemon (see
//! [`forward`]); a request without one is echoed, which is what
//! `oak __fskit-broker-ping` uses to smoke-test that launchd vends
//! `com.oakvcs.mount` and the XPC listener round-trips bytes — no
//! Swift/notarization needed. That ping covers only the echo path; the
//! forwarding path still wants an on-device run through the signed, sandboxed
//! extension before shipping.

#![cfg(target_os = "macos")]

use std::ffi::{c_char, c_void, CString};

use block2::RcBlock;

/// The mach service name. Must match the LaunchAgent `MachServices` key, the
/// extension's `mach-lookup` entitlement, and the Swift client connect string.
pub const MACH_SERVICE: &str = "com.oakvcs.mount";

#[allow(non_camel_case_types)]
type xpc_object_t = *mut c_void;
#[allow(non_camel_case_types)]
type xpc_connection_t = *mut c_void;
#[allow(non_camel_case_types)]
type xpc_type_t = *const c_void;
#[allow(non_camel_case_types)]
type dispatch_queue_t = *mut c_void;

// xpc/connection.h
const XPC_CONNECTION_MACH_SERVICE_LISTENER: u64 = 1 << 0;

extern "C" {
    fn xpc_connection_create_mach_service(
        name: *const c_char,
        targetq: dispatch_queue_t,
        flags: u64,
    ) -> xpc_connection_t;
    fn xpc_connection_set_event_handler(
        connection: xpc_connection_t,
        handler: &block2::Block<dyn Fn(xpc_object_t)>,
    );
    fn xpc_connection_resume(connection: xpc_connection_t);
    fn xpc_connection_send_message(connection: xpc_connection_t, message: xpc_object_t);
    fn xpc_connection_send_message_with_reply_sync(
        connection: xpc_connection_t,
        message: xpc_object_t,
    ) -> xpc_object_t;

    fn xpc_dictionary_create(
        keys: *const *const c_char,
        values: *const xpc_object_t,
        count: usize,
    ) -> xpc_object_t;
    fn xpc_dictionary_create_reply(original: xpc_object_t) -> xpc_object_t;
    fn xpc_dictionary_get_value(xdict: xpc_object_t, key: *const c_char) -> xpc_object_t;
    fn xpc_dictionary_get_remote_connection(xdict: xpc_object_t) -> xpc_connection_t;
    fn xpc_dictionary_set_data(
        xdict: xpc_object_t,
        key: *const c_char,
        bytes: *const c_void,
        length: usize,
    );
    fn xpc_dictionary_get_string(xdict: xpc_object_t, key: *const c_char) -> *const c_char;

    fn xpc_data_get_bytes_ptr(xdata: xpc_object_t) -> *const c_void;
    fn xpc_data_get_length(xdata: xpc_object_t) -> usize;

    fn xpc_get_type(object: xpc_object_t) -> xpc_type_t;
    fn xpc_release(object: xpc_object_t);
    fn xpc_retain(object: xpc_object_t) -> xpc_object_t;

    fn dispatch_main() -> !;

    // xpc type tag symbols (compared against xpc_get_type()).
    static _xpc_type_connection: c_void;
    static _xpc_type_dictionary: c_void;
}

fn type_of(obj: xpc_object_t) -> xpc_type_t {
    unsafe { xpc_get_type(obj) }
}
fn is_connection(obj: xpc_object_t) -> bool {
    type_of(obj) == unsafe { &_xpc_type_connection as *const c_void }
}
fn is_dictionary(obj: xpc_object_t) -> bool {
    type_of(obj) == unsafe { &_xpc_type_dictionary as *const c_void }
}

/// Pull the bytes of a `data` value out of an xpc dictionary by key.
fn dict_data(msg: xpc_object_t, key: &str) -> Option<Vec<u8>> {
    let ckey = CString::new(key).ok()?;
    let val = unsafe { xpc_dictionary_get_value(msg, ckey.as_ptr()) };
    if val.is_null() {
        return None;
    }
    let len = unsafe { xpc_data_get_length(val) };
    let ptr = unsafe { xpc_data_get_bytes_ptr(val) } as *const u8;
    if ptr.is_null() {
        return Some(Vec::new());
    }
    Some(unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec())
}

/// Pull a string value out of an xpc dictionary by key.
fn dict_string(msg: xpc_object_t, key: &str) -> Option<String> {
    let ckey = CString::new(key).ok()?;
    let p = unsafe { xpc_dictionary_get_string(msg, ckey.as_ptr()) };
    if p.is_null() {
        return None;
    }
    Some(
        unsafe { std::ffi::CStr::from_ptr(p) }
            .to_string_lossy()
            .into_owned(),
    )
}

const KEY_FRAME: &[u8] = b"frame\0";

/// Forward one framed `Op` to the per-mount daemon's Unix socket and return the
/// framed `Reply` bytes. One short-lived connection per request — the daemon
/// spawns a thread per connection, so this is cheap and keeps the broker
/// stateless. Any failure becomes a `Reply::Errno(EIO)` frame so the extension
/// surfaces a clean error to the kernel rather than hanging.
fn forward(socket: &str, frame: &[u8]) -> Vec<u8> {
    let errno_eio =
        || serde_json::to_vec(&super::protocol::Reply::Errno(libc::EIO)).unwrap_or_default();

    // Only ever connect to a socket inside the user's ~/.oak/mounts/ tree — the
    // path comes from a sandboxed extension, so don't let it point us anywhere.
    let allowed_root = dirs::home_dir().map(|h| h.join(".oak").join("mounts"));
    let path = std::path::Path::new(socket);
    let under_oak = allowed_root
        .as_deref()
        .zip(path.parent().and_then(|p| p.parent()))
        .map(|(root, grandparent)| grandparent == root)
        .unwrap_or(false);
    if !under_oak || path.file_name().and_then(|f| f.to_str()) != Some("fskit.sock") {
        tracing::warn!(
            socket,
            "broker: rejecting socket path outside ~/.oak/mounts"
        );
        return errno_eio();
    }

    match (|| -> std::io::Result<Vec<u8>> {
        let mut stream = std::os::unix::net::UnixStream::connect(socket)?;
        super::ipc::write_frame(&mut stream, frame)?;
        super::ipc::read_frame(&mut stream)?
            .ok_or_else(|| std::io::Error::other("daemon closed without reply"))
    })() {
        Ok(reply) => reply,
        Err(e) => {
            tracing::warn!(?e, socket, "broker: forward to daemon failed");
            errno_eio()
        }
    }
}

/// Handle one request: `{ socket, frame }` → forward to that mount's daemon and
/// reply `{ frame }`. With no `socket` key it echoes (the ping smoke-test path).
fn handle_message(msg: xpc_object_t) {
    if !is_dictionary(msg) {
        return; // errors (e.g. CONNECTION_INVALID) land here too; ignore.
    }
    let frame = dict_data(msg, "frame").unwrap_or_default();
    let reply_frame = match dict_string(msg, "socket") {
        Some(socket) if !socket.is_empty() => forward(&socket, &frame),
        _ => frame, // echo (ping)
    };

    let reply = unsafe { xpc_dictionary_create_reply(msg) };
    if reply.is_null() {
        return;
    }
    unsafe {
        xpc_dictionary_set_data(
            reply,
            KEY_FRAME.as_ptr() as *const c_char,
            reply_frame.as_ptr() as *const c_void,
            reply_frame.len(),
        );
        let remote = xpc_dictionary_get_remote_connection(msg);
        if !remote.is_null() {
            xpc_connection_send_message(remote, reply);
        }
        xpc_release(reply);
    }
}

/// Run the broker: vend `com.oakvcs.mount` and serve requests until killed.
/// launchd hands us the listener receive right because the LaunchAgent declares
/// `com.oakvcs.mount` in `MachServices`.
pub fn run() -> ! {
    let name = CString::new(MACH_SERVICE).unwrap();
    let listener = unsafe {
        xpc_connection_create_mach_service(
            name.as_ptr(),
            std::ptr::null_mut(),
            XPC_CONNECTION_MACH_SERVICE_LISTENER,
        )
    };
    if listener.is_null() {
        eprintln!("oak __fskit-broker: failed to create mach service listener");
        std::process::exit(1);
    }

    // Listener handler: each event is a new peer connection. Give every peer a
    // message handler, then resume it.
    let listener_handler = RcBlock::new(move |peer: xpc_object_t| {
        if !is_connection(peer) {
            return;
        }
        let peer_msg_handler = RcBlock::new(move |msg: xpc_object_t| {
            handle_message(msg);
        });
        unsafe {
            xpc_connection_set_event_handler(peer, &peer_msg_handler);
            xpc_connection_resume(peer);
        }
    });

    unsafe {
        xpc_connection_set_event_handler(listener, &listener_handler);
        xpc_connection_resume(listener);
        dispatch_main()
    }
}

/// Test client (`oak __fskit-broker-ping <text>`): look up `com.oakvcs.mount`,
/// send `{ oak_id, frame }`, print the echoed reply. Unsandboxed, so it needs
/// no entitlement — proves launchd vends the service and the round-trip works.
pub fn ping(text: &str) -> Result<(), String> {
    let name = CString::new(MACH_SERVICE).unwrap();
    let conn =
        unsafe { xpc_connection_create_mach_service(name.as_ptr(), std::ptr::null_mut(), 0) };
    if conn.is_null() {
        return Err("could not create client connection".into());
    }
    // A client connection still requires an event handler before resume.
    let noop = RcBlock::new(|_obj: xpc_object_t| {});
    unsafe {
        xpc_connection_set_event_handler(conn, &noop);
        xpc_connection_resume(conn);
    }

    let msg = unsafe { xpc_dictionary_create(std::ptr::null(), std::ptr::null(), 0) };
    let bytes = text.as_bytes();
    // No `socket` key → the broker echoes, so this stays a pure XPC round-trip
    // smoke test.
    unsafe {
        xpc_dictionary_set_data(
            msg,
            KEY_FRAME.as_ptr() as *const c_char,
            bytes.as_ptr() as *const c_void,
            bytes.len(),
        );
    }

    let reply = unsafe { xpc_connection_send_message_with_reply_sync(conn, msg) };
    unsafe { xpc_release(msg) };
    if reply.is_null() {
        return Err("no reply (service did not respond)".into());
    }
    let echoed = dict_data(reply, "frame");
    unsafe { xpc_release(reply) };
    match echoed {
        Some(b) => {
            crate::output::print_line(&format!(
                "broker echoed {} bytes: {:?}",
                b.len(),
                String::from_utf8_lossy(&b)
            ));
            let _ = xpc_retain; // keep linker happy if unused elsewhere
            Ok(())
        }
        None => Err("reply had no `frame`".into()),
    }
}
