//! A server that accepts connections and then never answers must not hang
//! the CLI. Before the shared API client carried a per-read stall timeout,
//! `oak clone` against such a server waited forever.

use std::io::Read;
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Accept every connection, read whatever arrives, and never reply.
fn silent_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                while matches!(stream.read(&mut buf), Ok(n) if n > 0) {}
            });
        }
    });
    format!("http://{addr}")
}

#[test]
fn clone_fails_instead_of_hanging_on_a_silent_server() {
    let remote = silent_server();
    let home = tempfile::TempDir::new().unwrap();
    let work = tempfile::TempDir::new().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_oak"))
        .current_dir(work.path())
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("OAK_REMOTE", &remote)
        .env("OAK_API_KEY", "oak_test_dummy")
        .env("OAK_NO_UPDATE_CHECK", "1")
        .env("OAK_HTTP_READ_TIMEOUT_SECS", "2")
        .env("OAK_PROBE_TIMEOUT_SECS", "2")
        .args(["clone", "acme/widgets", "dest", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(90);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("`oak clone` was still waiting on a silent server after 90s");
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let out = child.wait_with_output().unwrap();
    assert!(
        !status.success(),
        "clone against a silent server must fail: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(
        status.code(),
        Some(6),
        "a stalled server is a network failure (exit 6): stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
