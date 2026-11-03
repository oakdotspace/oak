//! Daemon-side dispatch: turn a wire [`Op`] into a call on [`MountCore`] and
//! produce a [`Reply`]. Transport-agnostic and synchronous — the IPC layer
//! (`ipc.rs`) calls [`MountServer::handle`] from a dedicated OS thread per
//! connection, so blocking blob fetches here are fine (they never stall a
//! shared dispatch loop the way the single-threaded FUSE backend would).

use std::ffi::OsStr;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use super::protocol::{
    kind_to_u8, AttrWire, DirEntryWire, Op, Reply, MIN_EXTENSION_PROTOCOL_VERSION,
    PROTOCOL_VERSION, XATTR_ALWAYS, XATTR_CREATE, XATTR_REPLACE,
};
use crate::commands::mount::core::{MountCore, NodeAttr, ReadSource, XattrPolicy};

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
                // The extension is announcing its wire-protocol version, and it
                // refuses the mount unless we answer with exactly that version
                // (it owns the user-facing UI). Newer versions only added ops, so
                // an older extension we can still serve gets its own version
                // back. Anything else gets ours, and the extension refuses. Log
                // loudly then so `oak --verbose` / OAK_LOG shows the skew from
                // the daemon side too.
                if (MIN_EXTENSION_PROTOCOL_VERSION..=PROTOCOL_VERSION).contains(&protocol_version) {
                    if protocol_version != PROTOCOL_VERSION {
                        tracing::info!(
                            extension = protocol_version,
                            daemon = PROTOCOL_VERSION,
                            "fskit: serving an older Oak Mount app"
                        );
                    }
                    return Reply::Hello { protocol_version };
                }
                tracing::warn!(
                    extension = protocol_version,
                    daemon = PROTOCOL_VERSION,
                    "fskit: wire-protocol mismatch — the Oak Mount app and the \
                     oak CLI are different versions; the extension will refuse the \
                     mount. Update both to the same Oak release."
                );
                Reply::Hello {
                    protocol_version: PROTOCOL_VERSION,
                }
            }
            Op::Getattr { ino } => {
                self.core.reconcile_if_external_change();
                match self.core.attr(ino) {
                    Ok(a) => Reply::Attr(attr_wire(&a)),
                    Err(errno) => Reply::Errno(errno),
                }
            }
            Op::Lookup { parent, name } => {
                self.core.reconcile_if_external_change();
                match self.core.lookup(parent, OsStr::new(&name)) {
                    Ok(a) => Reply::Attr(attr_wire(&a)),
                    Err(errno) => Reply::Errno(errno),
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
                                .filter(|e| !is_appledouble(&e.name))
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
                                // Restricted (permission-withheld) content
                                // maps to EACCES; `e` carries the "ask an org
                                // admin" guidance.
                                tracing::warn!(%e, ino, "fskit read fetch failed");
                                Reply::Errno(crate::commands::mount::core::hydrate_errno(&e))
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
                                tracing::warn!(%e, ino, "fskit readlink fetch failed");
                                Reply::Errno(crate::commands::mount::core::hydrate_errno(&e))
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
            Op::Rmdir { parent, name } => {
                self.remove_orphaned_appledouble(parent, OsStr::new(&name));
                match self.core.rmdir(parent, OsStr::new(&name)) {
                    Ok(()) => Reply::Ok,
                    Err(errno) => Reply::Errno(errno),
                }
            }
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
            Op::GetXattr { ino, name } => match self.core.get_xattr(ino, &name) {
                Ok(value) => Reply::Data(value),
                Err(errno) => Reply::Errno(errno),
            },
            Op::SetXattr {
                ino,
                name,
                value,
                policy,
            } => {
                let policy = match policy {
                    XATTR_ALWAYS => XattrPolicy::Always,
                    XATTR_CREATE => XattrPolicy::Create,
                    XATTR_REPLACE => XattrPolicy::Replace,
                    _ => return Reply::Errno(libc::EINVAL),
                };
                match self.core.set_xattr(ino, &name, &value, policy) {
                    Ok(()) => Reply::Ok,
                    Err(errno) => Reply::Errno(errno),
                }
            }
            Op::ListXattrs { ino } => match self.core.list_xattrs(ino) {
                Ok(names) => Reply::Names(names),
                Err(errno) => Reply::Errno(errno),
            },
            Op::RemoveXattr { ino, name } => match self.core.remove_xattr(ino, &name) {
                Ok(()) => Reply::Ok,
                Err(errno) => Reply::Errno(errno),
            },
        }
    }
}

/// True for an AppleDouble (`._name`) sidecar.
///
/// The OakFS extension doesn't implement FSKit's xattr operations, so macOS
/// stores every extended attribute — including the `com.apple.provenance` tag
/// the kernel stamps on each file a provenance-tracked process writes — in a
/// sibling `._name` file. Those must be creatable, or `setxattr` fails and
/// `cp` exits non-zero on every file. But nobody wants to see them: Oak never
/// versions them (`oak_core::is_system_metadata_path`), and a source walker
/// that reads every `.rs` file trips over `._lib.rs`. So the daemon keeps them
/// reachable by name (the kernel's xattr fallback only ever looks them up) and
/// leaves them out of directory listings.
fn is_appledouble(name: &OsStr) -> bool {
    name.as_encoded_bytes().starts_with(b"._")
}

impl MountServer {
    /// Before `rmdir`, delete the hidden AppleDouble sidecars of a directory
    /// that holds nothing else. The kernel normally does this itself by
    /// listing the directory, which can't see them now; without it, removing
    /// a directory whose files ever carried an xattr fails with `ENOTEMPTY`.
    fn remove_orphaned_appledouble(&self, parent: u64, name: &OsStr) {
        let Ok(dir) = self.core.lookup(parent, name) else {
            return;
        };
        let Ok(entries) = self.core.readdir(dir.ino) else {
            return;
        };
        let children: Vec<_> = entries
            .iter()
            .map(|e| e.name.as_os_str())
            .filter(|n| *n != "." && *n != "..")
            .collect();
        if children.iter().all(|n| is_appledouble(n)) {
            for n in children {
                let _ = self.core.unlink(dir.ino, n);
            }
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
            mounted_branch: None,
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
    fn hello_accepts_older_compatible_extension() {
        let (srv, _rt, _dir) = server_with(&[]);
        // A v2 extension never sends the xattr ops, so the daemon answers with
        // v2 and the extension's exact-match check passes.
        for v in MIN_EXTENSION_PROTOCOL_VERSION..=PROTOCOL_VERSION {
            assert_eq!(
                srv.handle(Op::Hello {
                    protocol_version: v
                }),
                Reply::Hello {
                    protocol_version: v
                }
            );
        }
        assert_eq!(
            srv.handle(Op::Hello {
                protocol_version: MIN_EXTENSION_PROTOCOL_VERSION - 1
            }),
            Reply::Hello {
                protocol_version: PROTOCOL_VERSION
            }
        );
    }

    fn lookup_ino(srv: &MountServer, name: &str) -> u64 {
        match srv.handle(Op::Lookup {
            parent: 1,
            name: name.into(),
        }) {
            Reply::Attr(a) => a.ino,
            other => panic!("expected attr for {name}, got {other:?}"),
        }
    }

    fn set_xattr(srv: &MountServer, ino: u64, name: &str, value: &[u8], policy: u8) -> Reply {
        srv.handle(Op::SetXattr {
            ino,
            name: name.into(),
            value: value.to_vec(),
            policy,
        })
    }

    #[test]
    fn xattr_set_get_list_remove() {
        let (srv, _rt, _dir) = server_with(&[("foo.txt", b"hi")]);
        let ino = lookup_ino(&srv, "foo.txt");
        let enoattr = Reply::Errno(crate::commands::mount::core::ENOATTR);

        assert_eq!(srv.handle(Op::ListXattrs { ino }), Reply::Names(vec![]));
        assert_eq!(
            srv.handle(Op::GetXattr {
                ino,
                name: "com.apple.provenance".into()
            }),
            enoattr
        );

        assert_eq!(
            set_xattr(
                &srv,
                ino,
                "com.apple.provenance",
                &[1, 0, 255],
                XATTR_ALWAYS
            ),
            Reply::Ok
        );
        assert_eq!(
            set_xattr(&srv, ino, "b.attr", b"x", XATTR_ALWAYS),
            Reply::Ok
        );
        assert_eq!(
            srv.handle(Op::GetXattr {
                ino,
                name: "com.apple.provenance".into()
            }),
            Reply::Data(vec![1, 0, 255])
        );
        assert_eq!(
            srv.handle(Op::ListXattrs { ino }),
            Reply::Names(vec!["b.attr".into(), "com.apple.provenance".into()])
        );

        assert_eq!(
            srv.handle(Op::RemoveXattr {
                ino,
                name: "b.attr".into()
            }),
            Reply::Ok
        );
        assert_eq!(
            srv.handle(Op::RemoveXattr {
                ino,
                name: "b.attr".into()
            }),
            enoattr
        );
        assert_eq!(
            srv.handle(Op::ListXattrs { ino }),
            Reply::Names(vec!["com.apple.provenance".into()])
        );
    }

    #[test]
    fn xattr_set_policies() {
        let (srv, _rt, _dir) = server_with(&[("foo.txt", b"hi")]);
        let ino = lookup_ino(&srv, "foo.txt");

        assert_eq!(
            set_xattr(&srv, ino, "a", b"1", XATTR_REPLACE),
            Reply::Errno(crate::commands::mount::core::ENOATTR)
        );
        assert_eq!(set_xattr(&srv, ino, "a", b"1", XATTR_CREATE), Reply::Ok);
        assert_eq!(
            set_xattr(&srv, ino, "a", b"2", XATTR_CREATE),
            Reply::Errno(libc::EEXIST)
        );
        assert_eq!(set_xattr(&srv, ino, "a", b"3", XATTR_REPLACE), Reply::Ok);
        assert_eq!(
            srv.handle(Op::GetXattr {
                ino,
                name: "a".into()
            }),
            Reply::Data(b"3".to_vec())
        );
        assert_eq!(
            set_xattr(&srv, ino, "a", b"4", 9),
            Reply::Errno(libc::EINVAL)
        );
        assert_eq!(
            set_xattr(&srv, ino, "", b"4", XATTR_ALWAYS),
            Reply::Errno(libc::EINVAL)
        );
        assert_eq!(
            set_xattr(&srv, ino, &"n".repeat(128), b"4", XATTR_ALWAYS),
            Reply::Errno(libc::ENAMETOOLONG)
        );
        assert_eq!(
            srv.handle(Op::GetXattr {
                ino: 999_999,
                name: "a".into()
            }),
            Reply::Errno(libc::ENOENT)
        );
    }

    #[test]
    fn xattrs_follow_rename_and_go_with_unlink() {
        let (srv, _rt, _dir) = server_with(&[("a.txt", b"1"), ("b.txt", b"2")]);
        let a = lookup_ino(&srv, "a.txt");
        let b = lookup_ino(&srv, "b.txt");
        assert_eq!(set_xattr(&srv, a, "from.a", b"a", XATTR_ALWAYS), Reply::Ok);
        assert_eq!(set_xattr(&srv, b, "from.b", b"b", XATTR_ALWAYS), Reply::Ok);

        // Renaming a over b replaces b, and b's xattrs with it.
        assert_eq!(
            srv.handle(Op::Rename {
                parent: 1,
                name: "a.txt".into(),
                newparent: 1,
                newname: "b.txt".into(),
            }),
            Reply::Ok
        );
        let moved = lookup_ino(&srv, "b.txt");
        assert_eq!(
            srv.handle(Op::ListXattrs { ino: moved }),
            Reply::Names(vec!["from.a".into()])
        );

        // A new file at an unlinked path starts with no xattrs.
        assert_eq!(
            srv.handle(Op::Unlink {
                parent: 1,
                name: "b.txt".into()
            }),
            Reply::Ok
        );
        let fresh = match srv.handle(Op::Create {
            parent: 1,
            name: "b.txt".into(),
            exec: false,
        }) {
            Reply::Attr(a) => a.ino,
            other => panic!("expected attr, got {other:?}"),
        };
        assert_eq!(
            srv.handle(Op::ListXattrs { ino: fresh }),
            Reply::Names(vec![])
        );
    }

    #[test]
    fn xattrs_are_saved_to_state_dir_not_overlay() {
        let (srv, _rt, dir) = server_with(&[("foo.txt", b"hi")]);
        let ino = lookup_ino(&srv, "foo.txt");
        assert_eq!(
            set_xattr(&srv, ino, "com.apple.provenance", b"p", XATTR_ALWAYS),
            Reply::Ok
        );
        // Setting an xattr is not a content change: nothing to commit.
        assert!(crate::commands::mount::state::overlay_is_clean(dir.path()));

        let saved = crate::commands::mount::state::load_xattrs(dir.path());
        assert_eq!(saved["foo.txt"]["com.apple.provenance"], b"p".to_vec());
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

    fn create(srv: &MountServer, parent: u64, name: &str) -> u64 {
        match srv.handle(Op::Create {
            parent,
            name: name.into(),
            exec: false,
        }) {
            Reply::Attr(a) => a.ino,
            other => panic!("expected attr for {name}, got {other:?}"),
        }
    }

    fn listed(srv: &MountServer, ino: u64) -> Vec<String> {
        match srv.handle(Op::Readdir { ino }) {
            Reply::Entries(es) => es.into_iter().map(|e| e.name).collect(),
            other => panic!("expected entries, got {other:?}"),
        }
    }

    #[test]
    fn appledouble_sidecars_work_but_are_not_listed() {
        let (srv, _rt, _dir) = server_with(&[("a.txt", b"1")]);
        let ino = create(&srv, 1, "._a.txt");
        assert_eq!(
            srv.handle(Op::Write {
                ino,
                offset: 0,
                data: b"xattrs".to_vec()
            }),
            Reply::Written(6)
        );
        // The kernel's xattr fallback finds the sidecar by name...
        assert!(matches!(
            srv.handle(Op::Lookup {
                parent: 1,
                name: "._a.txt".into(),
            }),
            Reply::Attr(a) if a.ino == ino
        ));
        // ...but walkers listing the directory never see it. Ordinary dotfiles
        // and names that merely contain `._` stay visible.
        create(&srv, 1, ".env");
        create(&srv, 1, "a._b");
        let names = listed(&srv, 1);
        assert!(!names.contains(&"._a.txt".to_string()), "{names:?}");
        for name in ["a.txt", ".env", "a._b"] {
            assert!(names.contains(&name.to_string()), "{name} in {names:?}");
        }
    }

    #[test]
    fn rmdir_clears_hidden_appledouble_sidecars() {
        let (srv, _rt, _dir) = server_with(&[]);
        let dir = match srv.handle(Op::Mkdir {
            parent: 1,
            name: "d".into(),
        }) {
            Reply::Attr(a) => a.ino,
            other => panic!("expected attr, got {other:?}"),
        };
        create(&srv, dir, "keep.txt");
        create(&srv, dir, "._keep.txt");
        let rmdir = || Op::Rmdir {
            parent: 1,
            name: "d".into(),
        };
        // A real file still blocks removal, and its sidecar is left alone.
        assert_eq!(srv.handle(rmdir()), Reply::Errno(libc::ENOTEMPTY));
        assert!(matches!(
            srv.handle(Op::Lookup {
                parent: dir,
                name: "._keep.txt".into(),
            }),
            Reply::Attr(_)
        ));
        // Once only sidecars remain, the directory comes out cleanly.
        assert_eq!(
            srv.handle(Op::Unlink {
                parent: dir,
                name: "keep.txt".into(),
            }),
            Reply::Ok
        );
        assert_eq!(srv.handle(rmdir()), Reply::Ok);
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
