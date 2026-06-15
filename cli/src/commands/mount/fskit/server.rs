//! Daemon-side dispatch: turn a wire [`Op`] into a call on [`MountCore`] and
//! produce a [`Reply`]. Transport-agnostic and synchronous — the IPC layer
//! (`ipc.rs`) calls [`MountServer::handle`] from a dedicated OS thread per
//! connection, so blocking blob fetches here are fine (they never stall a
//! shared dispatch loop the way the single-threaded FUSE backend would).

use std::ffi::OsStr;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use super::protocol::{kind_to_u8, AttrWire, DirEntryWire, Op, Reply, PROTOCOL_VERSION};
use crate::commands::mount::core::{MountCore, NodeAttr, ReadSource};

pub struct MountServer {
    core: Arc<MountCore>,
}

impl MountServer {
    pub fn new(core: Arc<MountCore>) -> Self {
        Self { core }
    }

    /// Serve one operation. Never panics on bad input — malformed requests map
    /// to an errno reply.
    pub fn handle(&self, op: Op) -> Reply {
        match op {
            Op::Hello { protocol_version } => {
                // The extension is announcing its wire-protocol version. We only
                // report ours back; the extension decides whether the pair is
                // compatible and refuses the mount if not (it owns the user-facing
                // UI). Log loudly on a mismatch so `oak --verbose` / OAK_LOG shows
                // the skew from the daemon side too.
                if protocol_version != PROTOCOL_VERSION {
                    tracing::warn!(
                        extension = protocol_version,
                        daemon = PROTOCOL_VERSION,
                        "fskit: wire-protocol mismatch — the Oak Mount app and the \
                         oak CLI are different versions; the extension will refuse the \
                         mount. Update both to the same Oak release."
                    );
                }
                Reply::Hello {
                    protocol_version: PROTOCOL_VERSION,
                }
            }
            Op::Getattr { ino } => {
                self.core.reconcile_if_external_change();
                match self.core.attr(ino) {
                    Some(a) => Reply::Attr(attr_wire(&a)),
                    None => Reply::Errno(libc::ENOENT),
                }
            }
            Op::Lookup { parent, name } => {
                self.core.reconcile_if_external_change();
                match self.core.lookup(parent, OsStr::new(&name)) {
                    Some(a) => Reply::Attr(attr_wire(&a)),
                    None => Reply::Errno(libc::ENOENT),
                }
            }
            Op::Readdir { ino } => {
                self.core.reconcile_if_external_change();
                match self.core.readdir(ino) {
                    Ok(entries) => {
                        tracing::debug!(ino, count = entries.len(), "fskit: readdir");
                        // Listing a directory almost always precedes opening its
                        // files — warm them in the background in one batched
                        // round-trip so the reads land on the local cache.
                        self.core.prefetch_dir_children(ino);
                        Reply::Entries(
                            entries
                                .into_iter()
                                .map(|e| DirEntryWire {
                                    ino: e.ino,
                                    kind: kind_to_u8(e.kind),
                                    name: e.name.to_string_lossy().into_owned(),
                                })
                                .collect(),
                        )
                    }
                    Err(errno) => {
                        tracing::debug!(ino, errno, "fskit: readdir error");
                        Reply::Errno(errno)
                    }
                }
            }
            Op::Read { ino, offset, size } => {
                self.core.reconcile_if_external_change();
                match self.core.read_source(ino) {
                    Ok(ReadSource::Dir) => Reply::Errno(libc::EISDIR),
                    Ok(ReadSource::Ready(content)) => Reply::Data(
                        crate::commands::mount::core::slice_range(&content, offset, size).to_vec(),
                    ),
                    Ok(ReadSource::Fetch(hash)) => {
                        // Cache miss → hydrate from the remote. We're on a
                        // per-connection OS thread (not a runtime worker), so
                        // blocking on the runtime handle is safe.
                        let ctx = self.core.fetch_ctx();
                        let rt = ctx.rt.clone();
                        match rt.block_on(ctx.hydrate(hash)) {
                            Ok(content) => Reply::Data(
                                crate::commands::mount::core::slice_range(&content, offset, size)
                                    .to_vec(),
                            ),
                            Err(e) => {
                                tracing::warn!(?e, ino, "fskit read fetch failed");
                                Reply::Errno(libc::EIO)
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(?e, ino, "fskit read failed");
                        Reply::Errno(libc::EIO)
                    }
                }
            }
            Op::Readlink { ino } => {
                self.core.reconcile_if_external_change();
                // A symlink is stored as a file node whose blob content is the
                // target path, so resolving it is just reading that blob in full
                // (no offset/length). The kernel only issues readlink against an
                // inode it already saw as a symlink in `getattr`, so we trust the
                // kind and return the bytes; a directory is a defensive `EINVAL`.
                match self.core.read_source(ino) {
                    Ok(ReadSource::Dir) => Reply::Errno(libc::EINVAL),
                    Ok(ReadSource::Ready(content)) => Reply::Data(content),
                    Ok(ReadSource::Fetch(hash)) => {
                        let ctx = self.core.fetch_ctx();
                        let rt = ctx.rt.clone();
                        match rt.block_on(ctx.hydrate(hash)) {
                            Ok(content) => Reply::Data(content),
                            Err(e) => {
                                tracing::warn!(?e, ino, "fskit readlink fetch failed");
                                Reply::Errno(libc::EIO)
                            }
                        }
                    }
                    Err(e) => Reply::Errno(e.raw_os_error().unwrap_or(libc::EIO)),
                }
            }
            Op::Write { ino, offset, data } => match self.core.write(ino, offset, &data) {
                Ok(n) => Reply::Written(n),
                Err(errno) => Reply::Errno(errno),
            },
            Op::Create { parent, name, exec } => {
                match self.core.create(parent, OsStr::new(&name), exec) {
                    Ok(a) => Reply::Attr(attr_wire(&a)),
                    Err(errno) => Reply::Errno(errno),
                }
            }
            Op::Mkdir { parent, name } => match self.core.mkdir(parent, OsStr::new(&name)) {
                Ok(a) => Reply::Attr(attr_wire(&a)),
                Err(errno) => Reply::Errno(errno),
            },
            Op::Unlink { parent, name } => match self.core.unlink(parent, OsStr::new(&name)) {
                Ok(()) => Reply::Ok,
                Err(errno) => Reply::Errno(errno),
            },
            Op::Rmdir { parent, name } => match self.core.rmdir(parent, OsStr::new(&name)) {
                Ok(()) => Reply::Ok,
                Err(errno) => Reply::Errno(errno),
            },
            Op::Rename {
                parent,
                name,
                newparent,
                newname,
            } => match self
                .core
                .rename(parent, OsStr::new(&name), newparent, OsStr::new(&newname))
            {
                Ok(()) => Reply::Ok,
                Err(errno) => Reply::Errno(errno),
            },
            Op::Truncate { ino, size } => match self.core.truncate(ino, size) {
                Ok(a) => Reply::Attr(attr_wire(&a)),
                Err(errno) => Reply::Errno(errno),
            },
        }
    }
}

fn attr_wire(a: &NodeAttr) -> AttrWire {
    let mtime_ns = a
        .mtime
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    AttrWire {
        ino: a.ino,
        size: a.size,
        kind: kind_to_u8(a.kind),
        perm: a.perm,
        mtime_ns,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::mount::core::MountCore;
    use crate::commands::mount::state::MountConfig;
    use oak_core::{
        Blob, FileChange, FileMode, Hash, Manifest, ManifestEntry, Repository, SqliteRepository,
    };
    use std::collections::HashMap;
    use std::time::SystemTime;
    use tempfile::TempDir;

    use crate::commands::mount::fskit::protocol::{KIND_DIR, KIND_FILE};

    fn server_with(files: &[(&str, &[u8])]) -> (MountServer, tokio::runtime::Runtime, TempDir) {
        let dir = TempDir::new().unwrap();
        let state_dir = dir.path().to_path_buf();
        let cache = Arc::new(SqliteRepository::open(&state_dir.join("cache.db")).unwrap());

        let mut entries = Vec::new();
        for (path, content) in files {
            let blob = Blob::new(content.to_vec());
            cache.store_blob(&blob).unwrap();
            entries.push(ManifestEntry {
                path: (*path).to_string(),
                blob_hash: blob.hash.clone(),
                mode: FileMode::Regular,
            });
        }
        let manifest = Manifest::new(entries);
        let mh = cache.put_manifest(manifest.entries.clone()).unwrap();
        let no_files: Vec<FileChange> = Vec::new();
        let head: Hash = cache
            .put_commit(
                "vb".into(),
                None,
                None,
                mh,
                "tester".into(),
                None,
                chrono::Utc::now(),
                no_files,
            )
            .unwrap();
        cache.set_branch_head("vb", &head).unwrap();

        let cfg = MountConfig {
            id: "testid".into(),
            mount_point: state_dir.join("mnt"),
            remote_url: "https://example.invalid".into(),
            owner: "o".into(),
            repo: "r".into(),
            base_branch: "main".into(),
            base_commit: head.as_str().to_string(),
            virtual_branch: "vb".into(),
        };
        let mut sizes = HashMap::new();
        for e in &manifest.entries {
            let b = cache.get_blob(&e.blob_hash).unwrap().unwrap();
            sizes.insert(e.blob_hash.as_str().to_string(), b.size);
        }
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let core = MountCore::new(
            cfg,
            cache,
            &manifest,
            &sizes,
            None,
            rt.handle().clone(),
            state_dir,
            SystemTime::now(),
        )
        .unwrap();
        (MountServer::new(Arc::new(core)), rt, dir)
    }

    #[test]
    fn hello_reports_daemon_protocol_version() {
        let (srv, _rt, _dir) = server_with(&[]);
        // The daemon reports its own version regardless of what the extension
        // sent (here a bogus future version); the extension does the comparing.
        assert_eq!(
            srv.handle(Op::Hello {
                protocol_version: 9999
            }),
            Reply::Hello {
                protocol_version: PROTOCOL_VERSION
            }
        );
    }

    #[test]
    fn getattr_root_is_dir() {
        let (srv, _rt, _dir) = server_with(&[("foo.txt", b"hi")]);
        match srv.handle(Op::Getattr { ino: 1 }) {
            Reply::Attr(a) => assert_eq!(a.kind, KIND_DIR),
            other => panic!("expected dir attr, got {other:?}"),
        }
    }

    #[test]
    fn lookup_then_read_serves_content() {
        let (srv, _rt, _dir) = server_with(&[("foo.txt", b"hello")]);
        let ino = match srv.handle(Op::Lookup {
            parent: 1,
            name: "foo.txt".into(),
        }) {
            Reply::Attr(a) => {
                assert_eq!(a.kind, KIND_FILE);
                assert_eq!(a.size, 5);
                a.ino
            }
            other => panic!("expected attr, got {other:?}"),
        };
        match srv.handle(Op::Read {
            ino,
            offset: 0,
            size: 1024,
        }) {
            Reply::Data(d) => assert_eq!(d, b"hello"),
            other => panic!("expected data, got {other:?}"),
        }
    }

    #[test]
    fn lookup_missing_is_enoent() {
        let (srv, _rt, _dir) = server_with(&[("foo.txt", b"hi")]);
        assert_eq!(
            srv.handle(Op::Lookup {
                parent: 1,
                name: "nope".into()
            }),
            Reply::Errno(libc::ENOENT)
        );
    }

    #[test]
    fn create_write_read_via_server() {
        let (srv, _rt, _dir) = server_with(&[]);
        let ino = match srv.handle(Op::Create {
            parent: 1,
            name: "new.txt".into(),
            exec: false,
        }) {
            Reply::Attr(a) => a.ino,
            other => panic!("expected attr, got {other:?}"),
        };
        assert_eq!(
            srv.handle(Op::Write {
                ino,
                offset: 0,
                data: b"data!".to_vec()
            }),
            Reply::Written(5)
        );
        match srv.handle(Op::Read {
            ino,
            offset: 0,
            size: 1024,
        }) {
            Reply::Data(d) => assert_eq!(d, b"data!"),
            other => panic!("expected data, got {other:?}"),
        }
    }

    #[test]
    fn readdir_returns_entries() {
        let (srv, _rt, _dir) = server_with(&[("a.txt", b"1"), ("b.txt", b"2")]);
        match srv.handle(Op::Readdir { ino: 1 }) {
            Reply::Entries(es) => {
                let names: Vec<String> = es.into_iter().map(|e| e.name).collect();
                assert!(names.contains(&".".to_string()));
                assert!(names.contains(&"a.txt".to_string()));
                assert!(names.contains(&"b.txt".to_string()));
            }
            other => panic!("expected entries, got {other:?}"),
        }
    }
}
