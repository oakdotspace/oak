//! Linux FUSE backend for `oak mount`.
//!
//! A thin adapter that maps the `fuser::Filesystem` trait onto the
//! backend-neutral [`MountCore`] (see `core.rs`), which owns the inode tree,
//! overlay, blob hydration, and reconciliation. This module only translates
//! between fuser's request/reply types and `MountCore`'s neutral shapes.
//!
//! macOS no longer uses FUSE — it mounts via FSKit (see the `fskit` module),
//! which needs no kernel extension. This backend is therefore Linux-only,
//! where `fuser` mounts through the `fusermount3` setuid helper and links no
//! libfuse into the shipped binary.

use std::ffi::OsStr;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyWrite, Request, TimeOrNow,
};
use oak_core::{OakError, Result};

use super::core::{EntryKind, MountCore, NodeAttr, ReadSource};

const TTL: Duration = Duration::from_secs(1);

/// fuser wrapper around the shared mount engine.
pub struct MountFs {
    core: MountCore,
}

impl MountFs {
    pub fn new(core: MountCore) -> Self {
        Self { core }
    }
}

fn to_file_attr(a: &NodeAttr) -> FileAttr {
    let kind = match a.kind {
        EntryKind::Dir => FileType::Directory,
        EntryKind::Symlink => FileType::Symlink,
        EntryKind::File => FileType::RegularFile,
    };
    let (size, blocks, nlink) = match a.kind {
        EntryKind::Dir => (0u64, 0u64, 2u32),
        _ => (a.size, a.size.div_ceil(512), 1),
    };
    FileAttr {
        ino: a.ino,
        size,
        blocks,
        atime: a.mtime,
        mtime: a.mtime,
        ctime: a.mtime,
        crtime: UNIX_EPOCH,
        kind,
        perm: a.perm,
        nlink,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

impl Filesystem for MountFs {
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        self.core.reconcile_if_external_change();
        match self.core.lookup(parent, name) {
            Some(attr) => reply.entry(&TTL, &to_file_attr(&attr), 0),
            None => reply.error(libc::ENOENT),
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        self.core.reconcile_if_external_change();
        match self.core.attr(ino) {
            Some(attr) => reply.attr(&TTL, &to_file_attr(&attr)),
            None => reply.error(libc::ENOENT),
        }
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        self.core.reconcile_if_external_change();
        let entries = match self.core.readdir(ino) {
            Ok(e) => e,
            Err(errno) => {
                reply.error(errno);
                return;
            }
        };
        for (i, e) in entries.into_iter().enumerate().skip(offset as usize) {
            let kind = match e.kind {
                EntryKind::Dir => FileType::Directory,
                EntryKind::Symlink => FileType::Symlink,
                EntryKind::File => FileType::RegularFile,
            };
            // i+1 because offset is the *next* offset to use, not the current.
            if reply.add(e.ino, (i + 1) as i64, kind, &e.name) {
                break;
            }
        }
        reply.ok();
    }

    fn open(&mut self, _req: &Request<'_>, _ino: u64, _flags: i32, reply: ReplyOpen) {
        // No per-fh state; hand back fh=0.
        //
        // FOPEN_DIRECT_IO: bypass the kernel page cache so every read()/write()
        // syscall maps 1:1 onto a FUSE read/write op with the exact offset and
        // bytes. Without it the kernel buffers writes in page-sized chunks and
        // flushes them back against a possibly-stale i_size, zero-padding
        // partial pages on writeback — which corrupted overlay files with
        // scattered NUL bytes. Our read/write handlers operate on whole files
        // anyway, so we gain nothing from page-cache buffering.
        reply.opened(0, fuser::consts::FOPEN_DIRECT_IO);
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        self.core.reconcile_if_external_change();

        let source = match self.core.read_source(ino) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(?e, ino, "mount read failed");
                reply.error(libc::EIO);
                return;
            }
        };

        match source {
            ReadSource::Dir => reply.error(libc::EISDIR),
            ReadSource::Ready(content) => {
                reply.data(super::core::slice_range(&content, offset, size))
            }
            ReadSource::Fetch(hash) => {
                // Cache miss: hydrate from the remote WITHOUT blocking the
                // single FUSE dispatch thread. fuser services requests one at a
                // time on one thread, so a synchronous fetch here would stall
                // every other request behind a network round-trip. Spawning the
                // fetch onto the runtime frees the dispatch loop immediately;
                // `ReplyData` is `Send`, so we answer from the completion.
                let ctx = self.core.fetch_ctx();
                ctx.rt.clone().spawn(async move {
                    match ctx.hydrate(hash).await {
                        Ok(content) => reply.data(super::core::slice_range(&content, offset, size)),
                        Err(e) => {
                            tracing::warn!(?e, ino, "mount read fetch failed");
                            reply.error(libc::EIO);
                        }
                    }
                });
            }
        }
    }

    fn write(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        match self.core.write(ino, offset, data) {
            Ok(n) => reply.written(n),
            Err(errno) => reply.error(errno),
        }
    }

    fn create(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let exec = (mode & 0o111) != 0;
        match self.core.create(parent, name, exec) {
            // FOPEN_DIRECT_IO on the freshly-created handle for the same reason
            // as `open` — keep file data off the kernel page cache.
            Ok(attr) => reply.created(
                &TTL,
                &to_file_attr(&attr),
                0,
                0,
                fuser::consts::FOPEN_DIRECT_IO,
            ),
            Err(errno) => reply.error(errno),
        }
    }

    fn mkdir(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        match self.core.mkdir(parent, name) {
            Ok(attr) => reply.entry(&TTL, &to_file_attr(&attr), 0),
            Err(errno) => reply.error(errno),
        }
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        match self.core.unlink(parent, name) {
            Ok(()) => reply.ok(),
            Err(errno) => reply.error(errno),
        }
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        match self.core.rmdir(parent, name) {
            Ok(()) => reply.ok(),
            Err(errno) => reply.error(errno),
        }
    }

    fn rename(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        _flags: u32,
        reply: ReplyEmpty,
    ) {
        match self.core.rename(parent, name, newparent, newname) {
            Ok(()) => reply.ok(),
            Err(errno) => reply.error(errno),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        // Only truncation (a `size` change) is honored; mode/uid/times are
        // read-only in this filesystem.
        if let Some(new_size) = size {
            match self.core.truncate(ino, new_size) {
                Ok(attr) => reply.attr(&TTL, &to_file_attr(&attr)),
                Err(errno) => reply.error(errno),
            }
            return;
        }
        match self.core.attr(ino) {
            Some(attr) => reply.attr(&TTL, &to_file_attr(&attr)),
            None => reply.error(libc::ENOENT),
        }
    }
}

/// Mount the FUSE filesystem on the given path and run until interrupted.
/// Blocks the caller's thread.
pub fn mount_fs(mount_point: &Path, fs: MountFs) -> Result<()> {
    let opts = vec![
        fuser::MountOption::FSName(format!("oak:{}", fs.core.mount_id())),
        fuser::MountOption::AutoUnmount,
        fuser::MountOption::DefaultPermissions,
    ];
    fuser::mount2(fs, mount_point, &opts).map_err(OakError::Io)
}
