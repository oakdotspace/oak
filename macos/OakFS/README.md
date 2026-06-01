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
cp -R build/Build/Products/Release/"Oak Mounter.app" /Applications/
open "/Applications/Oak Mounter.app"
```

Then enable it: **System Settings → General → Login Items & Extensions →
File System Extensions → OakFS** (the host app has a button that opens this).

Once enabled, `oak mount start …` works exactly as before — same command, same
flags. The CLI shells out to `/sbin/mount -t OakFS` and the extension connects
back to the daemon.

## Transport: development vs. production

`DaemonClient.swift` speaks the framed JSON protocol over a **Unix domain
socket** (byte-identical to `ipc.rs`). The App Sandbox blocks unix sockets, so:

- **Bring-up / development:** set `com.apple.security.app-sandbox` to `<false/>`
  in `OakFS.entitlements`. The extension then connects to the daemon socket
  directly and the whole stack works end to end. Use this to validate the
  FSKit ↔ daemon round-trips on-device.
- **Shipping:** the sandbox must be ON. Move the transport to an **XPC mach
  service** — the daemon registers `com.oak.mount.<id>` and the extension
  connects with `NSXPCConnection` / `xpc_connection_create_mach_service`,
  carrying the same `Op`/`Reply` frames. The entitlement template already
  whitelists the `com.oak.mount` mach-lookup name. This is the one remaining
  integration step (the daemon's XPC listener is not yet wired; today it
  listens on the unix socket).

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
| `HostApp/` | minimal "Oak Mounter" app that carries the extension |
| `project.yml` | XcodeGen spec for the app + extension |
