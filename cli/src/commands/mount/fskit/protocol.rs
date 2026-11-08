//! Wire protocol between the sandboxed FSKit extension and the `oak` daemon.
//!
//! The FSKit extension (`OakFS.appex`, Swift) runs App-Sandboxed and cannot
//! make oak's HTTP blob fetches, read `~/.oak`, or open unix sockets. So it
//! does not own the filesystem logic — it forwards each VFS operation to the
//! unsandboxed daemon, which owns [`MountCore`](super::super::core::MountCore)
//! and answers with the result.
//!
//! These types are the request/reply contract. They are serialized as JSON
//! and length-prefixed on the wire (see `ipc.rs`). The Swift side encodes the
//! mirror of these shapes; keep the two in sync (see
//! `macos/OakFS/Extension/Wire.swift`).

use serde::{Deserialize, Serialize};

use crate::commands::mount::core::EntryKind;

/// Version of this wire protocol. The Swift extension carries its own copy
/// (`kOakProtocolVersion` in `macos/OakFS/Extension/Wire.swift`); the two
/// perform an [`Op::Hello`] handshake at mount time (see
/// `DaemonClient.handshake`) so a mismatched `Oak Mount` app and `oak` CLI —
/// which upgrade independently — fail the mount cleanly instead of exchanging
/// misinterpreted frames. Bump this on any incompatible change to [`Op`] or
/// [`Reply`], and bump `kOakProtocolVersion` in lockstep.
///
/// History: v1 → v2 added [`Op::Readlink`] (symlink target resolution).
/// v2 → v3 added the extended-attribute ops ([`Op::GetXattr`],
/// [`Op::SetXattr`], [`Op::ListXattrs`], [`Op::RemoveXattr`]) and
/// [`Reply::Names`]. v3 → v4 added [`Op::Symlink`] (symlink creation).
pub const PROTOCOL_VERSION: u32 = 4;

/// Oldest extension protocol this daemon still serves. v3 and v4 only added
/// ops, so a v2 extension (which never sends them) works unchanged against a
/// newer daemon.
/// [`Op::Hello`] answers such an extension with its own version, which passes
/// its exact-match check.
pub const MIN_EXTENSION_PROTOCOL_VERSION: u32 = 2;

/// [`Op::SetXattr`] policy: set the value whether or not it exists.
pub const XATTR_ALWAYS: u8 = 0;
/// [`Op::SetXattr`] policy: fail with `EEXIST` if the attribute exists.
pub const XATTR_CREATE: u8 = 1;
/// [`Op::SetXattr`] policy: fail with `ENOATTR` if the attribute is missing.
pub const XATTR_REPLACE: u8 = 2;

/// File-type tag on the wire. Mirrors [`EntryKind`].
pub const KIND_DIR: u8 = 0;
pub const KIND_FILE: u8 = 1;
pub const KIND_SYMLINK: u8 = 2;

pub fn kind_to_u8(k: EntryKind) -> u8 {
    match k {
        EntryKind::Dir => KIND_DIR,
        EntryKind::File => KIND_FILE,
        EntryKind::Symlink => KIND_SYMLINK,
    }
}

/// One VFS operation, forwarded from the extension. Names are passed as UTF-8
/// strings — FSKit gives us `FSFileName`s which are UTF-8 in practice; the
/// daemon rejects non-UTF-8 with `EINVAL`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum Op {
    /// Version handshake, sent once by the extension before any VFS op. Carries
    /// the extension's [`PROTOCOL_VERSION`]; the daemon answers with
    /// [`Reply::Hello`] reporting its own, so the extension can refuse a
    /// mismatched pair.
    Hello {
        protocol_version: u32,
    },
    Lookup {
        parent: u64,
        name: String,
    },
    Getattr {
        ino: u64,
    },
    Readdir {
        ino: u64,
    },
    Read {
        ino: u64,
        offset: i64,
        size: u32,
    },
    /// Resolve a symlink's target. The daemon answers with [`Reply::Data`]
    /// carrying the raw target-path bytes (oak stores a symlink's target as its
    /// blob content), or [`Reply::Errno`] — `EINVAL` if `ino` is a directory,
    /// `ENOENT` if it doesn't exist.
    Readlink {
        ino: u64,
    },
    Write {
        ino: u64,
        offset: i64,
        data: Vec<u8>,
    },
    Create {
        parent: u64,
        name: String,
        exec: bool,
    },
    Mkdir {
        parent: u64,
        name: String,
    },
    /// Create a symlink `name` under `parent` whose target is `target` (raw
    /// path bytes). Answered with [`Reply::Attr`] for the new link.
    Symlink {
        parent: u64,
        name: String,
        target: Vec<u8>,
    },
    Unlink {
        parent: u64,
        name: String,
    },
    Rmdir {
        parent: u64,
        name: String,
    },
    Rename {
        parent: u64,
        name: String,
        newparent: u64,
        newname: String,
    },
    Truncate {
        ino: u64,
        size: u64,
    },
    /// Read one extended attribute. Answered with [`Reply::Data`], or
    /// [`Reply::Errno`] carrying `ENOATTR` if it isn't set.
    GetXattr {
        ino: u64,
        name: String,
    },
    /// Set one extended attribute. `policy` is one of [`XATTR_ALWAYS`],
    /// [`XATTR_CREATE`] or [`XATTR_REPLACE`] (FSKit's `SetXattrPolicy`; its
    /// `delete` case is sent as [`Op::RemoveXattr`]).
    SetXattr {
        ino: u64,
        name: String,
        value: Vec<u8>,
        policy: u8,
    },
    /// List the extended attribute names set on `ino` ([`Reply::Names`]).
    ListXattrs {
        ino: u64,
    },
    /// Remove one extended attribute; `ENOATTR` if it isn't set.
    RemoveXattr {
        ino: u64,
        name: String,
    },
}

/// A stat result on the wire.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttrWire {
    pub ino: u64,
    pub size: u64,
    pub kind: u8,
    pub perm: u16,
    /// mtime as nanoseconds since the Unix epoch.
    pub mtime_ns: u128,
}

/// A directory entry on the wire.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DirEntryWire {
    pub ino: u64,
    pub kind: u8,
    pub name: String,
}

/// The daemon's answer to an [`Op`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum Reply {
    /// Answer to [`Op::Hello`]: the daemon's [`PROTOCOL_VERSION`].
    Hello {
        protocol_version: u32,
    },
    Attr(AttrWire),
    Entries(Vec<DirEntryWire>),
    /// File-read bytes ([`Op::Read`]) or a symlink target ([`Op::Readlink`]).
    Data(Vec<u8>),
    Written(u32),
    /// Extended attribute names ([`Op::ListXattrs`]).
    Names(Vec<String>),
    Ok,
    /// A POSIX errno the extension surfaces to the kernel.
    Errno(i32),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn op_roundtrips_through_json() {
        let ops = vec![
            Op::Hello {
                protocol_version: PROTOCOL_VERSION,
            },
            Op::Lookup {
                parent: 1,
                name: "foo.txt".into(),
            },
            Op::Read {
                ino: 7,
                offset: 4096,
                size: 65536,
            },
            Op::Readlink { ino: 7 },
            Op::Symlink {
                parent: 1,
                name: "link".into(),
                target: b"../target".to_vec(),
            },
            Op::Write {
                ino: 7,
                offset: 0,
                data: vec![1, 2, 3, 255],
            },
            Op::Rename {
                parent: 1,
                name: "a".into(),
                newparent: 2,
                newname: "b".into(),
            },
            Op::GetXattr {
                ino: 7,
                name: "com.apple.provenance".into(),
            },
            Op::SetXattr {
                ino: 7,
                name: "user.x".into(),
                value: vec![0, 1, 255],
                policy: XATTR_CREATE,
            },
            Op::ListXattrs { ino: 7 },
            Op::RemoveXattr {
                ino: 7,
                name: "user.x".into(),
            },
        ];
        for op in ops {
            let bytes = serde_json::to_vec(&op).unwrap();
            let back: Op = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(op, back);
        }
    }

    #[test]
    fn reply_roundtrips_through_json() {
        let replies = vec![
            Reply::Hello {
                protocol_version: PROTOCOL_VERSION,
            },
            Reply::Attr(AttrWire {
                ino: 3,
                size: 12,
                kind: KIND_FILE,
                perm: 0o644,
                mtime_ns: 1234567890,
            }),
            Reply::Data(vec![0, 1, 2]),
            Reply::Written(11),
            Reply::Names(vec!["com.apple.provenance".into()]),
            Reply::Ok,
            Reply::Errno(libc::ENOENT),
        ];
        for r in replies {
            let bytes = serde_json::to_vec(&r).unwrap();
            let back: Reply = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(r, back);
        }
    }

    /// The Swift side hand-encodes these shapes (see `Wire.swift`); pin the
    /// exact JSON so a serde change can't silently break it.
    #[test]
    fn xattr_wire_shapes_match_swift() {
        let op = Op::SetXattr {
            ino: 3,
            name: "n".into(),
            value: vec![1, 2],
            policy: XATTR_REPLACE,
        };
        assert_eq!(
            serde_json::to_string(&op).unwrap(),
            r#"{"SetXattr":{"ino":3,"name":"n","value":[1,2],"policy":2}}"#
        );
        assert_eq!(
            serde_json::to_string(&Op::ListXattrs { ino: 3 }).unwrap(),
            r#"{"ListXattrs":{"ino":3}}"#
        );
        assert_eq!(
            serde_json::to_string(&Reply::Names(vec!["a".into()])).unwrap(),
            r#"{"Names":["a"]}"#
        );
    }
}
