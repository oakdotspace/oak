# OakFS — the macOS FSKit file-system extension

This is the macOS mount backend for `oak mount`. It replaces FUSE/macFUSE:
**no kernel extension, no `brew install macfuse`, no reduced-security boot.**

## How it fits together

```
oak mount start oak/oak ./slug
        │
        ▼
oak CLI daemon (Rust, unsandboxed)            ← cli/src/commands/mount/fskit/
  • owns MountCore: inode tree, overlay,
    blob hydration, reconcile, push/pull
  • serves VFS ops over IPC (ipc.rs)
  • calls /sbin/mount to bring up the volume
        ▲
        │  framed JSON: Op → Reply  (protocol.rs ⇄ Wire.swift)
        ▼
OakFS.appex (Swift, this dir, App-Sandboxed)  ← loaded by fskitd
  • FSUnaryFileSystem + FSVolume
  • forwards every op to the daemon
  • holds NO filesystem state of its own
```

The extension is intentionally thin: the sandbox can't make oak's network
calls or read `~/.oak`, so all logic stays in the unsandboxed daemon and the
extension just forwards. See the migration plan in `FSKIT_MIGRATION.md` at the
repo root.

## Requirements

- **macOS 26+** — synthetic (non-block-device) FSKit volumes use
  `FSPathURLResource`, which is macOS 26.
- **Xcode 16+** with the macOS 26 SDK (FSKit.framework).
- The **`com.apple.developer.fskit.fsmodule` entitlement** granted to your
  Apple developer team. Request it on the Developer portal; signing fails
  without it. Put your Team ID in `project.yml` (`DEVELOPMENT_TEAM`).
- `xcodegen` (`brew install xcodegen`) to materialize the Xcode project from
  `project.yml`.

## Build & install

```bash
# from the repo root
make macos-app          # xcodegen generate + xcodebuild + (optional) install

# or by hand
cd macos/OakFS
xcodegen generate
xcodebuild -project OakFS.xcodeproj -scheme OakMounter \
           -configuration Release -derivedDataPath build
cp -R build/Build/Products/Release/"Oak Mount.app" /Applications/
open "/Applications/Oak Mount.app"
```

Then enable it: **System Settings → General → Login Items & Extensions →
File System Extensions → OakFS** (the host app has a button that opens this).

Once enabled, `oak mount start …` works exactly as before — same command, same
flags. The CLI shells out to `/sbin/mount -t OakFS` and the extension connects
back to the daemon.

## Transport: XPC broker → daemon socket

The App Sandbox blocks the extension from opening the daemon's unix socket
directly, so the transport runs in two hops, both shipping today with the
sandbox **ON**:

1. **Extension → broker, over XPC.** `DaemonClient.swift` connects to the
   launchd-vended mach service **`com.oakvcs.mount`** (whitelisted in
   `OakFS.entitlements`) via `xpc_connection_create_mach_service` and sends
   each framed `Op` as `{ socket, frame }`.
2. **Broker → daemon, over the unix socket.** The broker
   (`oak mount __fskit-broker`, started on demand by the `com.oakvcs.mount`
   LaunchAgent — see `fskit/broker.rs`) validates the socket path is under
   `~/.oak/mounts/` and forwards the framed `Op`/`Reply` bytes to that mount's
   `ipc.rs` socket. All filesystem logic stays in the unsandboxed per-mount
   daemon; the broker only bridges bytes.

The `Op`/`Reply` wire contract (`Wire.swift` ⇄ `protocol.rs`) is identical on
both hops — only the byte transport differs.

> **Validation status:** `oak __fskit-broker-ping` proves launchd vends the
> service and the XPC round-trip works, but it exercises the *echo* path (no
> `socket` key). The full chain through a **signed, sandboxed** `OakFS.appex`
> still needs an on-device run before shipping.

## API-surface caveat

FSKit's Swift API shifted across macOS 15.x betas and again for the macOS 26
synthetic-resource support. The method *signatures* in `OakFSVolume.swift` /
`OakFSUnaryFileSystem.swift` (e.g. `FSVolume.Operations`,
`FSDirectoryEntryPacker`, `FSItem.Attributes`) may need small adjustments to
match the SDK you build against — check the FSKit Swift interface
(`xcrun --sdk macosx --show-sdk-path`). The oak-specific logic (each op → a
`DaemonClient` call → reply mapping) is stable regardless.

## File map

| File | Role |
|------|------|
| `Extension/OakFSUnaryFileSystem.swift` | module entry: probe/load/unload the resource, read `oak_id`/`oak_socket` mount options |
| `Extension/OakFSVolume.swift` | the volume: every FSKit op → a daemon call |
| `Extension/OakFSItem.swift` | `FSItem` carrying an oak inode number + attr mapping |
| `Extension/DaemonClient.swift` | framed transport to the daemon |
| `Extension/Wire.swift` | Swift mirror of `protocol.rs` (`Op`/`Reply`) |
| `Extension/Info.plist` | `FSShortName=OakFS`, FSKit extension point |
| `Extension/OakFS.entitlements` | fskit + sandbox + mach-lookup |
| `HostApp/` | minimal "Oak Mount" app that carries the extension |
| `project.yml` | XcodeGen spec for the app + extension |
