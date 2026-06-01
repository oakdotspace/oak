# Migrating the macOS mount backend from `fuser` (macFUSE) to FSKit

## Implementation status (this branch)

**Done & verified (Rust, `cargo build`/`test --features mount` green on macOS 26):**
- `cli/src/commands/mount/core.rs` — backend-neutral `MountCore` extracted from
  the old fuser impl (inode tree, overlay, hydration, reconcile, all per-op
  logic). Neutral types `NodeAttr`/`DirEntry`/`ReadSource`, errno-coded errors.
  All original unit tests migrated + new op-level tests.
- `cli/src/commands/mount/fuse_fs.rs` — reduced to a thin `impl
  fuser::Filesystem` over `MountCore`, now **Linux-only** (`cfg(linux)`).
- `cli/src/commands/mount/fskit/` — macOS daemon backend: `protocol.rs` (wire
  `Op`/`Reply`), `server.rs` (`MountServer` dispatch over `MountCore`, unit
  tested), `ipc.rs` (framed JSON + unix-socket server), `mod.rs` (lifecycle:
  IPC server + `/sbin/mount -F -t OakFS` + unmount).
- `mod.rs` backend seam dispatches macOS→FSKit, Linux→fuser; `Cargo.toml`
  gates `fuser` to `cfg(linux)` only — **macOS links no libfuse**. Docs updated.

**Scaffolded (Swift, builds with Xcode — not exercised here):**
- `macos/OakFS/` — `Oak Mounter` host app + `OakFS.appex` extension
  (`FSUnaryFileSystem` + `FSVolume` forwarding each op to the daemon),
  `Wire.swift` mirroring `protocol.rs`, entitlements, `project.yml` (XcodeGen),
  `make macos-app`.

**Remaining on-device integration (needs Xcode + the fskit entitlement + a
macOS 26 host — see Risks):**
1. Reconcile `OakFSVolume.swift` method signatures with the installed FSKit SDK.
2. Build/sign/notarize the app; enable the extension; validate the full mount
   end-to-end (start with App Sandbox **off** so the unix-socket transport in
   `DaemonClient.swift` works).
3. Move the extension↔daemon transport from the unix socket to an **XPC mach
   service** (`com.oak.mount.<id>`) so it works with the sandbox **on** — the
   `Op`/`Reply` frames are unchanged; only `DaemonClient.send` and a new
   daemon-side XPC listener change.

## Goal

Mount oak repos on macOS **without requiring the user to install a kernel
extension** (macFUSE kext or `fuse-t`). Replace the macOS `fuser` backend with
an Apple **FSKit** file-system extension. Linux keeps `fuser`; Windows keeps
ProjFS. Backward compatibility with old mount state is *not* a hard requirement
— we can require re-mounting.

## Why this is a real architecture change, not a crate swap

`fuser` and FSKit are not drop-in equivalents:

| | `fuser` (today) | FSKit |
|---|---|---|
| Where FS code runs | In the `oak mount start` process (Rust, unsandboxed) | In a separate `.appex` extension loaded by `fskitd`, **App-Sandboxed** |
| Language | Rust (`impl fuser::Filesystem`) | Swift (`FSVolume.Operations` et al.) |
| Kernel requirement | macFUSE kext **or** `fuse-t` (user install) | None — built into macOS |
| Distribution | Local `--features mount` build links `libfuse.2.dylib` | Code-signed, notarized host **app** containing the extension, installed to `/Applications`, enabled in System Settings |
| Network/disk access | Free (process is unsandboxed) | Restricted by App Sandbox; no raw network, no `~/.oak` access, no unix sockets |
| Mount trigger | `fuser::mount2()` | `/sbin/mount -F -t <fsname>` against an `FSPathURLResource` |
| Min macOS | any with macFUSE | **macOS 26** (synthetic FS via `FSPathURLResource`; block-device FSKit is 15.0 but oak has no block device) |

Two facts dominate the design:

1. **FSKit extensions are App-Sandboxed.** They cannot make oak's HTTP blob
   fetches, cannot read `~/.oak/mounts/<id>/` (cache, overlay, config), and
   cannot open unix domain sockets. Today `fuse_fs.rs` does all of this inline
   in the `read`/`write`/reconcile paths.
2. **FSKit needs `FSPathURLResource` for a synthetic (no `/dev`) volume**, which
   is macOS 26+. oak mounts are fully synthetic — there is no block device.

Because of (1), we do **not** try to cram oak's networked, cache-backed core
into the sandbox. We keep the existing unsandboxed `oak` daemon and make the
FSKit extension a **thin shim that forwards VFS ops over XPC** to it. This
preserves almost all of today's Rust logic (blob hydration, SQLite cache,
overlay, reconciliation, virtual-branch state, push/pull) and sidesteps the
sandbox.

This split is exactly what the closest public analog —
[blocksense/agent-harbor](https://deepwiki.com/blocksense-network/agent-harbor/5.4-fskit-implementation-(macos))
— does: Rust core, Swift FSKit extension, C FFI for the fast path, XPC for the
control plane.

## Target architecture

```
                            user runs:  oak mount start oak/oak ./slug
                                                 │
        ┌────────────────────────────────────────┴───────────────────────────┐
        │  oak CLI daemon  (Rust, UNSANDBOXED — unchanged responsibilities)    │
        │  • virtual branch state, manifest, blob sizes                        │
        │  • SQLite cache  ~/.oak/mounts/<id>/                                  │
        │  • overlay (dirty/deletions/renames) + reconcile on commit           │
        │  • HTTP blob hydration from oakvcs.com (semaphore-bounded)           │
        │  • commit / push / pull / desc                                       │
        │  • shells out to /sbin/mount and umount (allowed: not sandboxed)     │
        │                                                                      │
        │   exposes an XPC service:  com.oak.mount.<mount-id>                   │
        └───────────────▲──────────────────────────────────────────────────────┘
                        │  XPC (lookup/getattr/readdir/read/write/create/…)
        ┌───────────────┴──────────────────────────────────────────────────────┐
        │  OakFS.appex  (Swift FSKit extension, App-Sandboxed)                  │
        │  • implements FSUnaryFileSystemOperations + FSVolume.Operations       │
        │  • holds the inode/FSItem tree (mirrors today's `Node` tree)          │
        │  • forwards each VFS op to the daemon over XPC, maps errors→errno     │
        │  • optional in-extension fast path (mmap’d read cache) via C FFI later │
        └────────────────────────────────────────────────────────────────────┘
                        ▲ loaded into its own process by fskitd
```

Two processes per mount: the unsandboxed daemon (today's `oak mount start`,
trimmed) and the sandboxed extension (`fskitd` loads it on mount). They rendezvous
over a per-mount XPC service whose name carries the mount id.

## Code today (what we're moving)

- `cli/Cargo.toml:96` — `fuser = "0.15"` under `cfg(target_os="macos")`, gated by
  the `mount` feature (`cli/Cargo.toml:120`).
- `cli/src/commands/mount/fuse_fs.rs` (1742 lines) — `MountFs` + `impl
  fuser::Filesystem` with 12 ops: `lookup, getattr, readdir, open, read, write,
  create, mkdir, unlink, rmdir, rename, setattr`. Plus the inode `Node` tree,
  overlay read/write fast paths, async blob hydration, and
  `reconcile_if_external_change()`.
- `cli/src/commands/mount/mod.rs:622` — the backend seam. `MountFs::new(cfg,
  cache, &manifest, &sizes, token, rt, state_dir, &prefixes, base_mtime)` is
  handed prepared state, then each platform runs its own loop. **fuser and
  ProjFS are already two backends sharing this seam — FSKit is the third.**
- `cli/src/commands/mount/mod.rs:641` — `mount2` blocks; runs on `spawn_blocking`.
- `cli/src/commands/mount/mod.rs:1111` — `platform_unmount` (macOS `umount` →
  `diskutil unmount force` fallback). Reusable as-is.
- `cli/src/commands/mount/fuse_fs.rs:1382` — mount options (`noappledouble`,
  `noapplexattr`, `daemon_timeout=600`). FSKit equivalents differ; see Phase 4.

The good news: the manifest→inode build, overlay model (`state.rs`
`OverlayMeta`/`DirtyEntry`), remote (`remote.rs`), and reconciliation are
**backend-agnostic already**. We want to lift them out of `fuse_fs.rs` so both
the fuser backend and the new XPC server share them.

## Migration phases

### Phase 0 — Spike & de-risk (before committing to the rewrite)
- Confirm on the team's macOS 26 machines: build the minimal Apple `FSKitSample`,
  enable it, mount a synthetic volume via `FSPathURLResource` +
  `/sbin/mount -F`. Verify a plain Rust staticlib can be linked into a Swift
  `.appex` and called via C FFI.
- Stand up a throwaway XPC round-trip: Swift extension ↔ a Rust process, measure
  per-call latency. **This is the #1 risk** — every `read`/`getattr` crosses XPC.
  FSKit's read/write path is already reported slower than macFUSE; adding XPC per
  op could be worse. If latency is unacceptable, fall back to the FFI-in-extension
  variant for the read fast path (see Alternatives).
- Confirm Apple has granted the `com.apple.developer.file-system.fskit`
  entitlement to our developer account (request early — it gates everything).

### Phase 1 — Extract a backend-neutral mount core (Rust, no behavior change)
Refactor so `fuse_fs.rs` no longer *owns* the logic, only adapts it to fuser.
- New module `cli/src/commands/mount/core.rs` (name TBD): move the `Node` inode
  tree, `insert_path`/manifest build, overlay read/write, blob hydration, and
  `reconcile_if_external_change` here as a `MountCore` with plain methods
  (`lookup(parent, name) -> NodeAttr`, `read(ino, off, len) -> Bytes`,
  `write(ino, off, data)`, `readdir(ino)`, `create/mkdir/unlink/rmdir/rename/
  setattr`). Each returns a neutral result + an errno-able error enum.
- Rewrite `impl fuser::Filesystem for MountFs` as a thin adapter over `MountCore`
  (translate `fuser` types ⇄ `MountCore` types). **Linux behavior must be byte-
  identical after this step** — this is the safety net and the regression gate.
- Land this first, verify Linux + current macOS-fuser builds still pass. No FSKit
  yet.

### Phase 2 — XPC server in the daemon (Rust)
- Add an XPC service to the `oak` daemon exposing `MountCore` ops. Define a stable
  message schema (one variant per VFS op + replies). Suggested transport:
  - Bridge to XPC via a small Swift/ObjC shim compiled into the daemon, OR
  - Use a Rust XPC crate; evaluate maturity. If none is solid, write a minimal
    ObjC `NSXPCListener` shim and call `MountCore` through a C FFI (`cbindgen`).
- Service name `com.oak.mount.<mount-id>`; registered when `oak mount start`
  brings the volume up, torn down on unmount.
- Keep all network/cache/overlay work on the daemon side of this boundary.

### Phase 3 — The FSKit extension (Swift) + host app + build
- Create an Xcode project under `macos/OakFS/` (new top-level dir in the repo):
  - **Host app** `Oak Mounter.app` (minimal; its job is to carry the extension
    and let macOS surface it in System Settings → Login Items & Extensions →
    File System Extensions).
  - **Extension** `OakFS.appex` implementing `FSUnaryFileSystemOperations`
    (probe/load/unload the `FSPathURLResource`) and `FSVolume.Operations`,
    `FSVolume.ReadWriteOperations`, `FSVolume.OpenCloseOperations`,
    `FSVolume.PathConfOperations`, and `FSVolume.XattrOperations` (xattr can stay
    a no-op like today).
  - Map the 12 ops to XPC calls; translate `MountCore` errors → POSIX errno.
  - Maintain the `FSItem`/inode tree from the manifest the daemon hands over at
    `volumeDidMount` (or fetch lazily via XPC `lookup`).
- Entitlements: `com.apple.developer.file-system.fskit`,
  `com.apple.security.app-sandbox`, a temporary-exception or
  `FSRequiresSecurityScopedPathURLResources` for the mount path, and the App
  Group / XPC mach-service name shared with the daemon so the sandbox allows the
  connection.
- Build wiring: `cargo build -p oakvcs-cli --features mount` produces the daemon;
  a separate `xcodebuild` step produces `Oak Mounter.app`. Add a `make
  macos-app` target (Makefile) that: builds the Rust staticlib for the
  XPC/FFI piece (`crate-type = ["staticlib"]`, universal `arm64;x86_64` via
  `lipo`), then `xcodebuild` links + signs + notarizes the app.

### Phase 4 — Wire `oak mount start/end` to FSKit on macOS
- Replace the macOS branch at `mod.rs:622`:
  - Ensure the extension is installed/enabled (probe via FSKit registration APIs;
    if missing, print actionable instructions: "open Oak Mounter.app once / enable
    in System Settings"). No more "brew install macfuse".
  - Register the per-mount XPC service, then shell out to
    `/sbin/mount -F -t OakFS <FSPathURLResource path> <dest>` (the daemon is
    unsandboxed so it may call `/sbin/mount`). Park on `ctrl_c` like the ProjFS
    branch (`mod.rs:677`) instead of blocking in `mount2`.
  - Translate today's mount options: `noappledouble`/`noapplexattr` → handle
    AppleDouble/xattr in the extension (return ENOTSUP for xattr); the
    `daemon_timeout` concern disappears (no macFUSE watchdog) but we still need
    the read path to stay responsive — keep async hydration off the XPC reply
    thread.
- `oak mount end`: `platform_unmount` (`mod.rs:1111`, `umount`) still works; then
  tear down the XPC service and state dir as today.

### Phase 5 — Cleanup, fallback policy, docs
- Decide the macOS < 26 story: simplest is **require macOS 26** for mounting and
  drop the macOS-fuser path entirely (we don't care about backward compat). If we
  want a transition window, keep `fuser` behind a hidden
  `--backend=fuse` flag for one release. Recommend: drop it.
- Remove `fuser` from `cfg(target_os="macos")` in `cli/Cargo.toml:96` (keep it for
  Linux). Update the feature comments at `cli/Cargo.toml:81`.
- Update install docs / README: replace "install macFUSE / fuse-t" with "install
  Oak Mounter.app + enable the extension."
- Update `oak/CLAUDE.md` and the space `CLAUDE.md` mount instructions if any
  user-facing steps change (they mostly shouldn't — `oak mount start` is the same
  command).

## What we keep vs. replace

**Keep (unchanged or lifted into `MountCore`):** manifest→inode build, overlay
model & `state.rs`, `remote.rs` blob fetch, reconciliation, SQLite cache,
commit/push/pull/desc, `platform_unmount`, the `mod.rs:622` backend seam, Linux
fuser backend, Windows ProjFS backend.

**Replace:** `impl fuser::Filesystem` on macOS → Swift FSKit extension + XPC
shim; macOS mount trigger (`mount2` → `/sbin/mount -F`); macOS dependency
(`libfuse.2.dylib`/macFUSE → none); macOS distribution (local feature build →
signed/notarized host app).

## Risks & open questions

1. **XPC-per-op latency** (highest). Mitigation: in-extension read fast path via
   C FFI + mmap'd cache the daemon writes; batch readdir; cache attrs in the
   extension. Decide in Phase 0.
2. **FSKit maturity** — `FSPathURLResource` is new (macOS 26); API churn and
   bugs likely. We're early adopters.
3. **`com.apple.developer.file-system.fskit` entitlement approval** from Apple —
   request now; it can take time and gates shipping.
4. **App Sandbox** blocks the extension from network/`~/.oak`/unix sockets —
   already designed around via the daemon+XPC split, but every new feature must
   respect it.
5. **Distribution friction** — users now install an app + toggle a System
   Settings switch. Different from macFUSE's kext approval, arguably lighter (no
   reduced-security boot, survives OS upgrades), but it's a new flow to document
   and support.
6. **Performance regression vs macFUSE** reported in the wild for FSKit
   read/write — benchmark large-file reads (oak hydrates remote blobs) before/
   after.

## Alternatives considered

- **FFI-in-extension (no daemon):** put `MountCore` entirely in the `.appex` via
  C FFI. Rejected as primary because the sandbox can't do oak's HTTP fetches /
  `~/.oak` cache. Could be a *partial* adoption for the read fast path only.
- **Keep macFUSE, ship fuse-t by default:** still a third-party userspace NFS
  shim with its own friction; doesn't meet the "no extra install / native" goal.
- **FileProvider instead of FSKit:** designed for cloud-sync providers, not a
  general POSIX mount; semantics don't match oak's mount model.

## Suggested PR sequence (each independently mergeable)
1. Phase 1 refactor: `MountCore` extraction; fuser becomes a thin adapter. (Linux
   regression-tested, no FSKit.)
2. Phase 2: XPC service in the daemon, exercised by a tiny Rust test client.
3. Phase 3: `macos/OakFS/` Xcode project, host app + extension, `make macos-app`.
4. Phase 4: switch macOS `oak mount start/end` to FSKit.
5. Phase 5: drop macOS `fuser`, docs, cleanup.
