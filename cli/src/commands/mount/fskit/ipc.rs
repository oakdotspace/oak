//! Length-prefixed JSON framing + a Unix-socket request/reply server.
//!
//! Each message is a big-endian `u32` byte length followed by that many bytes
//! of JSON ([`Op`] requests, [`Reply`] responses). One request, one reply,
//! per round trip; a connection may carry many in sequence.
//!
//! ## Transport note (sandbox)
//!
//! The daemon listens on a Unix domain socket under the mount's state dir.
//! That is the channel used in development and tests. **The shipped FSKit
//! extension is App-Sandboxed and cannot open a unix socket directly** — in
//! production the extension talks to the daemon over an XPC mach service, and
//! a thin bridge forwards those XPC messages to this same framed protocol. The
//! request/reply contract ([`Op`]/[`Reply`]) is identical either way; only the
//! byte transport changes. See `macos/OakFS/README.md`.

use std::io::{self, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::protocol::{Op, Reply};
use super::server::MountServer;

const MAX_FRAME: u32 = 256 * 1024 * 1024; // 256 MiB ceiling, guards against junk lengths

/// Write one length-prefixed frame.
pub fn write_frame<W: Write>(w: &mut W, payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame too large"))?;
    w.write_all(&len.to_be_bytes())?;
    w.write_all(payload)?;
    w.flush()
}

/// Read one length-prefixed frame. Returns `Ok(None)` on a clean EOF at a
/// frame boundary (the peer closed the connection).
pub fn read_frame<R: Read>(r: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame length {len} exceeds maximum"),
        ));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    Ok(Some(buf))
}

/// A running IPC server. Dropping it (or calling [`IpcServer::shutdown`])
/// stops the accept loop and removes the socket file.
pub struct IpcServer {
    shutdown: Arc<AtomicBool>,
    socket_path: PathBuf,
    accept_thread: Option<std::thread::JoinHandle<()>>,
}

impl IpcServer {
    /// Bind a Unix socket at `socket_path` and start accepting connections,
    /// dispatching each through `server` on its own OS thread.
    pub fn start(socket_path: &Path, server: Arc<MountServer>) -> io::Result<Self> {
        // Clear any stale socket from a previous run.
        let _ = std::fs::remove_file(socket_path);
        let listener = UnixListener::bind(socket_path)?;
        listener.set_nonblocking(true)?;

        let shutdown = Arc::new(AtomicBool::new(false));
        let accept_thread = {
            let shutdown = shutdown.clone();
            let server = server.clone();
            std::thread::Builder::new()
                .name("oak-fskit-ipc".into())
                .spawn(move || accept_loop(listener, server, shutdown))?
        };

        Ok(Self {
            shutdown,
            socket_path: socket_path.to_path_buf(),
            accept_thread: Some(accept_thread),
        })
    }

    /// Signal the accept loop to stop and wait for it to wind down.
    pub fn shutdown(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(t) = self.accept_thread.take() {
            let _ = t.join();
        }
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn accept_loop(listener: UnixListener, server: Arc<MountServer>, shutdown: Arc<AtomicBool>) {
    while !shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _addr)) => {
                let server = server.clone();
                // One thread per connection; blocking fetches inside handle()
                // are fine here (see server.rs).
                std::thread::spawn(move || {
                    if let Err(e) = serve_connection(stream, server) {
                        tracing::debug!(?e, "fskit ipc connection ended");
                    }
                });
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                tracing::warn!(?e, "fskit ipc accept failed");
                break;
            }
        }
    }
}

fn serve_connection(mut stream: UnixStream, server: Arc<MountServer>) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    loop {
        let Some(frame) = read_frame(&mut stream)? else {
            return Ok(()); // peer closed
        };
        let reply = match serde_json::from_slice::<Op>(&frame) {
            Ok(op) => server.handle(op),
            Err(e) => {
                tracing::warn!(?e, "fskit ipc: bad request frame");
                Reply::Errno(libc::EINVAL)
            }
        };
        let bytes = serde_json::to_vec(&reply)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        write_frame(&mut stream, &bytes)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"hello frame").unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        let got = read_frame(&mut cursor).unwrap().unwrap();
        assert_eq!(got, b"hello frame");
        // A second read at EOF yields None.
        assert!(read_frame(&mut cursor).unwrap().is_none());
    }

    #[test]
    fn empty_frame_roundtrip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"").unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        assert_eq!(read_frame(&mut cursor).unwrap().unwrap(), b"");
    }

    #[test]
    fn oversized_length_is_rejected() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(MAX_FRAME + 1).to_be_bytes());
        let mut cursor = std::io::Cursor::new(buf);
        assert!(read_frame(&mut cursor).is_err());
    }
}
