// MountMonitor.swift — live state behind the menu-bar extra.
//
// The app holds no mount state of its own: everything comes from the `oak` CLI
// (`oak mount list --json`, `oak mount end`, …) so the menu can never disagree
// with what `oak` itself reports. On top of that it probes the pieces a mount
// depends on — the OakFS extension (pluginkit), the `com.oakvcs.mount` XPC
// broker LaunchAgent (`oak __fskit-broker-ping`) — and sizes the on-disk caches
// under `~/.oak/mounts/`.

import Foundation
import Observation

// MARK: - `oak mount list --json`

/// One registration from `oak mount list --json` (see `MountListEntryJson` in
/// cli/src/commands/mount/mod.rs). Decoded leniently: the CLI's JSON is
/// append-only, and absent optional fields mean their default.
struct MountEntry: Decodable, Identifiable, Hashable, Sendable {
    struct Repo: Decodable, Hashable, Sendable {
        let spec: String
        let owner: String
        let name: String
    }

    struct Base: Decodable, Hashable, Sendable {
        let branch: String
        let commit: String
    }

    struct Dirty: Decodable, Hashable, Sendable {
        let clean: Bool
        let modifiedOrCreated: Int?
        let deleted: Int?
        let renamed: Int?

        var changedFiles: Int { (modifiedOrCreated ?? 0) + (deleted ?? 0) + (renamed ?? 0) }
    }

    let mountPoint: String
    let id: String
    let repo: Repo?
    let remoteUrl: String?
    let virtualBranch: String?
    let base: Base?
    let status: String
    let dirtyOverlay: Dirty
    let unpushedCommits: Int?
    let configError: String?
    let orphaned: Bool?

    var isLive: Bool { status == "live" }
    var isOrphaned: Bool { orphaned ?? false }
    var url: URL { URL(fileURLWithPath: mountPoint, isDirectory: true) }
    var title: String { repo?.spec ?? url.lastPathComponent }
    var unpushed: Int { unpushedCommits ?? 0 }
    var changedFiles: Int { dirtyOverlay.clean ? 0 : max(dirtyOverlay.changedFiles, 1) }
    /// Uncommitted or unpushed work that `oak mount end` would refuse to drop.
    var hasWork: Bool { changedFiles > 0 || unpushed > 0 }

    /// The mount point with the home directory abbreviated to `~`.
    var displayPath: String { (mountPoint as NSString).abbreviatingWithTildeInPath }

    var workSummary: String {
        var parts: [String] = []
        if changedFiles > 0 { parts.append(plural(changedFiles, "changed file")) }
        if unpushed > 0 { parts.append(plural(unpushed, "unpushed commit")) }
        return parts.isEmpty ? "Clean — nothing to push" : parts.joined(separator: " · ")
    }
}

func plural(_ n: Int, _ noun: String) -> String {
    "\(n) \(noun)\(n == 1 ? "" : "s")"
}

// MARK: - Locating and running `oak`

/// Finds and runs the `oak` binary. A GUI app doesn't inherit the user's shell
/// PATH, so we look where `oak` actually lives instead of trusting `$PATH`.
enum OakCLI {
    struct Output: Sendable {
        let status: Int32
        let stdout: String
        let stderr: String
        var ok: Bool { status == 0 }
        /// The most useful human-readable message from a failed run.
        var message: String {
            let err = stderr.trimmingCharacters(in: .whitespacesAndNewlines)
            return err.isEmpty ? stdout.trimmingCharacters(in: .whitespacesAndNewlines) : err
        }
    }

    static let launchAgentPlist = FileManager.default.homeDirectoryForCurrentUser
        .appendingPathComponent("Library/LaunchAgents/com.oakvcs.mount.plist")

    /// Preference order: the binary the broker LaunchAgent points at (the one
    /// that last mounted — `oak mount` rewrites it on every mount), then the
    /// usual install locations, then a login shell's `command -v oak`.
    static func locate() -> URL? {
        let fm = FileManager.default
        if let data = try? Data(contentsOf: launchAgentPlist),
           let plist = try? PropertyListSerialization.propertyList(from: data, format: nil) as? [String: Any],
           let args = plist["ProgramArguments"] as? [String],
           let exe = args.first, fm.isExecutableFile(atPath: exe) {
            return URL(fileURLWithPath: exe)
        }
        let home = fm.homeDirectoryForCurrentUser.path
        let candidates = [
            "\(home)/.local/bin/oak",
            "\(home)/.cargo/bin/oak",
            "/opt/homebrew/bin/oak",
            "/usr/local/bin/oak",
        ]
        if let hit = candidates.first(where: { fm.isExecutableFile(atPath: $0) }) {
            return URL(fileURLWithPath: hit)
        }
        let shell = run(URL(fileURLWithPath: "/bin/zsh"), ["-lc", "command -v oak"], timeout: 5)
        let path = shell.stdout.trimmingCharacters(in: .whitespacesAndNewlines)
        if shell.ok, fm.isExecutableFile(atPath: path) { return URL(fileURLWithPath: path) }
        return nil
    }

    /// Run a process to completion (blocking — call off the main actor),
    /// killing it after `timeout` seconds.
    static func run(_ exe: URL, _ args: [String], cwd: URL? = nil,
                    timeout: TimeInterval = 30) -> Output {
        let proc = Process()
        proc.executableURL = exe
        proc.arguments = args
        if let cwd { proc.currentDirectoryURL = cwd }
        var env = ProcessInfo.processInfo.environment
        // Keep `oak` quiet and non-interactive: no update nag on stderr, and
        // don't have a remount relaunch this app.
        env["OAK_NO_UPDATE_CHECK"] = "1"
        env["OAK_NO_MOUNT_TRAY"] = "1"
        env["NO_COLOR"] = "1"
        proc.environment = env
        let out = Pipe(), err = Pipe()
        proc.standardOutput = out
        proc.standardError = err
        proc.standardInput = FileHandle.nullDevice
        do { try proc.run() } catch {
            return Output(status: -1, stdout: "", stderr: error.localizedDescription)
        }
        let killer = DispatchWorkItem { if proc.isRunning { proc.terminate() } }
        DispatchQueue.global().asyncAfter(deadline: .now() + timeout, execute: killer)
        // Drain both pipes concurrently so a chatty child can't fill one and
        // deadlock against our read of the other.
        nonisolated(unsafe) var errData = Data()
        let group = DispatchGroup()
        group.enter()
        DispatchQueue.global().async {
            errData = err.fileHandleForReading.readDataToEndOfFile()
            group.leave()
        }
        let outData = out.fileHandleForReading.readDataToEndOfFile()
        group.wait()
        proc.waitUntilExit()
        killer.cancel()
        return Output(status: proc.terminationStatus,
                      stdout: String(decoding: outData, as: UTF8.self),
                      stderr: String(decoding: errData, as: UTF8.self))
    }
}

// MARK: - Health probes

/// Whether the `com.oakvcs.mount` XPC broker — the hop between the sandboxed
/// extension and each mount daemon — answers.
enum BrokerHealth: Equatable, Sendable {
    case unknown
    case checking
    /// No LaunchAgent yet: `oak mount` installs it on the first mount.
    case notInstalled
    case ok
    case failing(String)

    var label: String {
        switch self {
        case .unknown: return "XPC broker: not checked"
        case .checking: return "XPC broker: checking…"
        case .notInstalled: return "XPC broker: installs on first mount"
        case .ok: return "XPC broker responding"
        case .failing: return "XPC broker not responding"
        }
    }
}

/// Bytes on disk under a directory (allocated size, so sparse/cloned files
/// count what they really cost). Blocking.
func directorySize(_ url: URL) -> Int64 {
    let keys: [URLResourceKey] = [.totalFileAllocatedSizeKey, .isRegularFileKey]
    guard let walker = FileManager.default.enumerator(
        at: url, includingPropertiesForKeys: keys, options: [], errorHandler: { _, _ in true })
    else { return 0 }
    var total: Int64 = 0
    for case let file as URL in walker {
        guard let v = try? file.resourceValues(forKeys: Set(keys)), v.isRegularFile == true else { continue }
        total += Int64(v.totalFileAllocatedSize ?? 0)
    }
    return total
}

func formatBytes(_ n: Int64) -> String {
    ByteCountFormatter.string(fromByteCount: n, countStyle: .file)
}

// MARK: - Monitor

@MainActor
@Observable
final class MountMonitor {
    static let shared = MountMonitor()

    nonisolated static let mountsRoot = FileManager.default.homeDirectoryForCurrentUser
        .appendingPathComponent(".oak/mounts", isDirectory: true)
    nonisolated static var repoCacheDir: URL { mountsRoot.appendingPathComponent("repo-cache", isDirectory: true) }

    private(set) var mounts: [MountEntry] = []
    /// Set when `oak mount list` itself failed (as opposed to "no mounts").
    private(set) var listError: String?
    private(set) var extensionStatus: ExtensionStatus = .checking
    private(set) var broker: BrokerHealth = .unknown
    private(set) var cliPath: URL?
    private(set) var cliVersion: String?
    private(set) var account: String?
    /// Per-mount state dir size, keyed by mount id.
    private(set) var mountCacheBytes: [String: Int64] = [:]
    private(set) var repoCacheBytes: Int64 = 0
    /// Mount ids with an unmount/remount/forget in flight.
    private(set) var busy: Set<String> = []
    private(set) var lastRefresh: Date?
    private(set) var started = false

    var liveMounts: [MountEntry] { mounts.filter(\.isLive) }
    var inactiveMounts: [MountEntry] { mounts.filter { !$0.isLive } }
    var totalCacheBytes: Int64 { mountCacheBytes.values.reduce(0, +) + repoCacheBytes }

    /// Something the user needs to fix before mounts can work.
    var needsAttention: Bool {
        switch extensionStatus {
        case .enabled, .checking: break
        default: return true
        }
        if case .failing = broker { return true }
        return cliPath == nil
    }

    private var timer: Timer?
    private var watcher: DispatchSourceFileSystemObject?
    private var refreshing = false
    private var refreshQueued = false
    private var lastSizing = Date.distantPast

    private init() {}

    /// Kick off the first full check, then keep the mount list fresh: a
    /// directory watcher on `~/.oak/mounts` (every registry/state write there is
    /// an atomic rename) for instant updates, plus a timer to catch daemons
    /// that die without touching the registry.
    func start() async {
        guard !started else { return }
        started = true
        watchMountsRoot()
        timer = Timer.scheduledTimer(withTimeInterval: 5, repeats: true) { _ in
            Task { @MainActor in await MountMonitor.shared.refresh() }
        }
        await runDiagnostics()
    }

    /// Everything: CLI discovery, version and account, extension and broker
    /// health, cache sizes, mounts.
    func runDiagnostics() async {
        extensionStatus = .checking
        broker = .checking
        let found = await Task.detached { OakCLI.locate() }.value
        cliPath = found
        async let ext = Task.detached { ExtensionProbe.current() }.value
        async let info = Task.detached { () -> (String?, String?) in
            guard let found else { return (nil, nil) }
            let v = OakCLI.run(found, ["--version"], timeout: 10)
            let a = OakCLI.run(found, ["auth", "status", "--json"], timeout: 15)
            return (v.ok ? v.stdout.trimmingCharacters(in: .whitespacesAndNewlines) : nil,
                    Self.accountName(fromAuthStatus: a.stdout))
        }.value
        async let brk = Task.detached { Self.probeBroker(found) }.value
        extensionStatus = await ext
        (cliVersion, account) = await info
        broker = await brk
        lastSizing = .distantPast
        await refresh()
    }

    func refreshExtension() async {
        extensionStatus = await Task.detached { ExtensionProbe.current() }.value
    }

    /// Re-read the mount list (and, at most every 30s, the cache sizes).
    func refresh() async {
        if refreshing { refreshQueued = true; return }
        refreshing = true
        defer { refreshing = false }
        repeat {
            refreshQueued = false
            await refreshOnce()
        } while refreshQueued
    }

    private func refreshOnce() async {
        guard let cli = cliPath ?? OakCLI.locate() else {
            listError = "Couldn't find the `oak` CLI. Install it from oak.space."
            mounts = []
            return
        }
        cliPath = cli
        let out = await Task.detached { OakCLI.run(cli, ["mount", "list", "--json"], timeout: 20) }.value
        if out.ok, let data = out.stdout.data(using: .utf8) {
            let decoder = JSONDecoder()
            decoder.keyDecodingStrategy = .convertFromSnakeCase
            do {
                mounts = try decoder.decode([MountEntry].self, from: data)
                listError = nil
            } catch {
                listError = "Unexpected `oak mount list` output: \(error.localizedDescription)"
            }
        } else if !out.ok {
            listError = out.message.isEmpty ? "`oak mount list` failed (exit \(out.status))" : out.message
        }
        lastRefresh = Date()

        let ids = mounts.map(\.id)
        let needSizing = Date().timeIntervalSince(lastSizing) > 30
            || Set(ids) != Set(mountCacheBytes.keys)
        if needSizing {
            lastSizing = Date()
            let (perMount, repo) = await Task.detached { () -> ([String: Int64], Int64) in
                var sizes: [String: Int64] = [:]
                for id in ids {
                    sizes[id] = directorySize(Self.mountsRoot.appendingPathComponent(id, isDirectory: true))
                }
                return (sizes, directorySize(Self.repoCacheDir))
            }.value
            mountCacheBytes = perMount
            repoCacheBytes = repo
        }
    }

    // MARK: Actions

    /// `oak mount end [--force] <dest>`. Returns the CLI's output so the caller
    /// can surface a refusal (e.g. dirty work without `--force`).
    func unmount(_ mount: MountEntry, force: Bool) async -> OakCLI.Output {
        var args = ["mount", "end"]
        if force { args.append("--force") }
        args.append(mount.mountPoint)
        return await perform(mount, args, timeout: 120)
    }

    /// Bring a stale registration back: `oak mount <spec> <dest>` respawns the
    /// daemon from its existing state dir (no work is lost).
    func remount(_ mount: MountEntry) async -> OakCLI.Output {
        guard let spec = mount.repo?.spec else {
            return OakCLI.Output(status: -1, stdout: "", stderr: "This mount's config is unreadable; it can't be remounted.")
        }
        var args = ["mount"]
        if let remote = mount.remoteUrl { args += ["--remote", remote] }
        args += [spec, mount.mountPoint]
        return await perform(mount, args, timeout: 180)
    }

    /// `oak mount forget <dest>` — drops the registration, keeps state on disk.
    func forget(_ mount: MountEntry) async -> OakCLI.Output {
        await perform(mount, ["mount", "forget", mount.mountPoint], timeout: 30)
    }

    /// `oak open --print` inside the mount: the branch's web URL.
    func webURL(for mount: MountEntry) async -> URL? {
        guard let cli = cliPath else { return nil }
        let out = await Task.detached {
            OakCLI.run(cli, ["open", "--print"], cwd: mount.url, timeout: 15)
        }.value
        guard out.ok else { return nil }
        let line = out.stdout.split(separator: "\n").last.map(String.init) ?? ""
        // A branch the server hasn't seen yet prints just the site root —
        // not a page worth opening.
        guard let url = URL(string: line.trimmingCharacters(in: .whitespaces)),
              url.path.count > 1 else { return nil }
        return url
    }

    private func perform(_ mount: MountEntry, _ args: [String], timeout: TimeInterval) async -> OakCLI.Output {
        guard let cli = cliPath else {
            return OakCLI.Output(status: -1, stdout: "", stderr: "Couldn't find the `oak` CLI.")
        }
        busy.insert(mount.id)
        let out = await Task.detached { OakCLI.run(cli, args, timeout: timeout) }.value
        busy.remove(mount.id)
        await refresh()
        return out
    }

    // MARK: Helpers

    private func watchMountsRoot() {
        let fd = open(Self.mountsRoot.path, O_EVTONLY)
        guard fd >= 0 else { return }  // created on first mount; the timer covers it
        let src = DispatchSource.makeFileSystemObjectSource(
            fileDescriptor: fd, eventMask: [.write, .rename, .delete], queue: .main)
        src.setEventHandler {
            Task { @MainActor in await MountMonitor.shared.refresh() }
        }
        src.setCancelHandler { close(fd) }
        src.resume()
        watcher = src
    }

    nonisolated private static func probeBroker(_ cli: URL?) -> BrokerHealth {
        guard FileManager.default.fileExists(atPath: OakCLI.launchAgentPlist.path) else {
            return .notInstalled
        }
        guard let cli else { return .failing("`oak` CLI not found") }
        let out = OakCLI.run(cli, ["mount", "__fskit-broker-ping", "oak-mount-health"], timeout: 10)
        return out.ok ? .ok : .failing(out.message)
    }

    nonisolated private static func accountName(fromAuthStatus json: String) -> String? {
        guard let data = json.data(using: .utf8),
              let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let identity = obj["identity"] as? [String: Any],
              identity["state"] as? String == "authenticated"
        else { return nil }
        return identity["username"] as? String
    }
}
