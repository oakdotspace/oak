// StatusMenu.swift — the Oak Mount menu-bar extra.
//
// Makes a running `oak mount` obvious: the oak mark sits in the menu bar with
// a count of live mounts beside it, and the dropdown lists every mount with
// its repo, virtual branch, pending work and cache size, plus actions to open
// or unmount it. Below that, the health of everything a mount depends on: the
// OakFS extension, the XPC broker, and the `oak` CLI itself.

import AppKit
import ServiceManagement
import SwiftUI

// MARK: - Menu-bar label

struct StatusLabel: View {
    let monitor: MountMonitor
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        let live = monitor.liveMounts.count
        HStack(spacing: 3) {
            Image(nsImage: MenuBarIcon.image(attention: monitor.needsAttention))
            if live > 0 {
                Text("\(live)")
                    .monospacedDigit()
            }
        }
        .accessibilityLabel(live > 0 ? "Oak Mount, \(plural(live, "active mount"))" : "Oak Mount")
        // The label exists from launch, so it's where the app wires up the
        // window opener and starts monitoring.
        .task {
            WindowRouter.shared.openWindow = openWindow
            await monitor.start()
            WindowRouter.shared.showSetupIfNeeded(extension: monitor.extensionStatus)
        }
    }
}

/// The oak mark as a template image for the menu bar — the same 19×19 grid as
/// `AppMark`. With `attention`, a small dot is punched into the lower-right
/// corner (a template image can only carry one color, so the dot is cut out
/// with a clear ring to stay legible).
enum MenuBarIcon {
    private static let cells: [(CGFloat, CGFloat, CGFloat, CGFloat)] = [
        (9, 0, 1, 1), (8, 1, 3, 1), (7, 2, 5, 1), (6, 3, 7, 1), (9, 4, 1, 1), (9, 5, 1, 1),
        (3, 6, 1, 1), (6, 6, 7, 1), (15, 6, 1, 1),
        (2, 7, 2, 1), (6, 7, 7, 1), (15, 7, 2, 1),
        (1, 8, 3, 1), (6, 8, 3, 1), (10, 8, 3, 1), (15, 8, 3, 1),
        (0, 9, 8, 1), (11, 9, 8, 1),
        (1, 10, 3, 1), (6, 10, 3, 1), (10, 10, 3, 1), (15, 10, 3, 1),
        (2, 11, 2, 1), (6, 11, 7, 1), (15, 11, 2, 1),
        (3, 12, 1, 1), (6, 12, 7, 1), (15, 12, 1, 1),
        (9, 13, 1, 1), (9, 14, 1, 1),
        (6, 15, 7, 1), (7, 16, 5, 1), (8, 17, 3, 1), (9, 18, 1, 1),
    ]

    @MainActor private static var cache: [Bool: NSImage] = [:]

    @MainActor static func image(attention: Bool) -> NSImage {
        if let hit = cache[attention] { return hit }
        let side: CGFloat = 17
        let img = NSImage(size: NSSize(width: side, height: side), flipped: true) { rect in
            let u = rect.width / 19
            NSColor.black.setFill()
            for c in cells {
                NSRect(x: c.0 * u, y: c.1 * u, width: c.2 * u, height: c.3 * u).fill()
            }
            if attention {
                let d: CGFloat = 7
                let dot = NSRect(x: rect.maxX - d, y: rect.maxY - d, width: d, height: d)
                NSGraphicsContext.current?.compositingOperation = .clear
                NSBezierPath(ovalIn: dot.insetBy(dx: -1.5, dy: -1.5)).fill()
                NSGraphicsContext.current?.compositingOperation = .sourceOver
                NSBezierPath(ovalIn: dot).fill()
            }
            return true
        }
        img.isTemplate = true
        cache[attention] = img
        return img
    }
}

// MARK: - Window routing

/// Lets non-view code (the app delegate, menu actions) open the setup window.
@MainActor
final class WindowRouter {
    static let shared = WindowRouter()
    var openWindow: OpenWindowAction?
    private init() {}

    func showSetup() {
        NSApp.activate()
        openWindow?(id: OakMounterApp.setupWindowID)
    }

    /// At launch: show the setup window when the extension needs enabling, or
    /// on the very first launch the user started themselves. `oak mount`
    /// launches the app with `--background`, which only gets a window when
    /// something is wrong.
    func showSetupIfNeeded(extension status: ExtensionStatus) {
        let background = CommandLine.arguments.contains("--background")
        let defaults = UserDefaults.standard
        let firstRun = !defaults.bool(forKey: "shownSetupOnce")
        if status != .enabled || (firstRun && !background) {
            defaults.set(true, forKey: "shownSetupOnce")
            showSetup()
        }
    }
}

// MARK: - Dropdown

struct StatusMenu: View {
    let monitor: MountMonitor

    var body: some View {
        let live = monitor.liveMounts
        let inactive = monitor.inactiveMounts

        Text(headline(live: live.count))

        if let err = monitor.listError {
            Label(err, systemImage: "exclamationmark.triangle")
        }

        Divider()

        if live.isEmpty {
            Text("No active mounts")
            Text("Run `oak mount <org>/<repo>` to start one")
        } else {
            Section("Active Mounts") {
                ForEach(live) { MountMenu(mount: $0, monitor: monitor) }
            }
        }

        if !inactive.isEmpty {
            Section("Not Running") {
                ForEach(inactive) { InactiveMountMenu(mount: $0, monitor: monitor) }
            }
        }

        if live.count > 1 {
            Button("Unmount All Clean Mounts") { Actions.unmountAllClean(monitor) }
        }

        Divider()

        Section("Health") {
            extensionItem
            brokerItem
            cliItem
            cacheMenu
        }

        Divider()

        Button("Run Diagnostics") { Task { await monitor.runDiagnostics() } }
            .keyboardShortcut("r")
        Button("Setup & Help…") { WindowRouter.shared.showSetup() }
        LoginItemToggle()

        Divider()

        Button("Quit Oak Mount") { NSApp.terminate(nil) }
            .keyboardShortcut("q")
    }

    private func headline(live: Int) -> String {
        live == 0 ? "Oak Mount — idle" : "Oak Mount — \(plural(live, "active mount"))"
    }

    @ViewBuilder private var extensionItem: some View {
        switch monitor.extensionStatus {
        case .enabled:
            Label("OakFS extension enabled", systemImage: "checkmark.circle")
        case .checking:
            Label("OakFS extension: checking…", systemImage: "ellipsis.circle")
        case .installedNotEnabled, .notInstalled, .unknown:
            Button {
                Actions.openExtensionSettings()
            } label: {
                Label("\(monitor.extensionStatus.label) — Enable…", systemImage: "exclamationmark.triangle")
            }
        }
    }

    @ViewBuilder private var brokerItem: some View {
        switch monitor.broker {
        case .ok:
            Label(monitor.broker.label, systemImage: "checkmark.circle")
        case .failing(let why):
            Button {
                Actions.alert("The XPC broker didn’t respond",
                              "The sandboxed OakFS extension reaches each mount daemon through the `com.oakvcs.mount` LaunchAgent. Running any `oak mount` reinstalls and reloads it.\n\n\(why)")
            } label: {
                Label(monitor.broker.label, systemImage: "exclamationmark.triangle")
            }
        default:
            Label(monitor.broker.label, systemImage: "circle.dashed")
        }
    }

    @ViewBuilder private var cliItem: some View {
        if let path = monitor.cliPath {
            Menu {
                Text(monitor.cliVersion ?? "oak (version unknown)")
                Text((path.path as NSString).abbreviatingWithTildeInPath)
                Text(monitor.account.map { "Signed in as \($0)" } ?? "Not signed in — run `oak login`")
            } label: {
                Label(monitor.cliVersion ?? "oak CLI", systemImage: monitor.account == nil
                      ? "person.crop.circle.badge.exclamationmark" : "terminal")
            }
        } else {
            Button {
                if let url = URL(string: "https://oak.space/docs") { NSWorkspace.shared.open(url) }
            } label: {
                Label("oak CLI not found — Install…", systemImage: "exclamationmark.triangle")
            }
        }
    }

    private var cacheMenu: some View {
        Menu {
            ForEach(monitor.mounts) { m in
                Text("\(m.title): \(formatBytes(monitor.mountCacheBytes[m.id] ?? 0))")
            }
            Text("Shared repo cache: \(formatBytes(monitor.repoCacheBytes))")
            Divider()
            Button("Show in Finder") {
                NSWorkspace.shared.activateFileViewerSelecting([MountMonitor.mountsRoot])
            }
        } label: {
            Label("Cache: \(formatBytes(monitor.totalCacheBytes))", systemImage: "internaldrive")
        }
    }
}

/// One live mount and what you can do with it.
private struct MountMenu: View {
    let mount: MountEntry
    let monitor: MountMonitor

    var body: some View {
        let busy = monitor.busy.contains(mount.id)
        Menu {
            Text(mount.displayPath)
            if let branch = mount.virtualBranch { Text("Branch: \(branch)") }
            if let base = mount.base { Text("Base: \(base.branch) @ \(String(base.commit.prefix(12)))") }
            Text(mount.workSummary)
            Text("Cache: \(formatBytes(monitor.mountCacheBytes[mount.id] ?? 0))")
            Divider()
            Button("Open in Finder") { NSWorkspace.shared.open(mount.url) }
            Button("Open in Terminal") { Actions.openInTerminal(mount.url) }
            Button("Copy Path") { Actions.copy(mount.mountPoint) }
            Button("Open on oak.space") { Actions.openWeb(mount, monitor) }
            Divider()
            Button(mount.hasWork ? "Unmount…" : "Unmount") { Actions.unmount(mount, monitor) }
                .disabled(busy)
        } label: {
            Label(busy ? "\(mount.title) — working…" : "\(mount.title)  \(mount.displayPath)",
                  systemImage: icon)
        }
    }

    private var icon: String {
        if mount.changedFiles > 0 { return "pencil.circle" }
        if mount.unpushed > 0 { return "arrow.up.circle" }
        return "externaldrive.fill.badge.checkmark"
    }
}

/// A registration whose daemon isn't running (crash, reboot) or whose
/// directory is gone.
private struct InactiveMountMenu: View {
    let mount: MountEntry
    let monitor: MountMonitor

    var body: some View {
        let busy = monitor.busy.contains(mount.id)
        Menu {
            Text(mount.displayPath)
            Text(mount.isOrphaned ? "Directory is gone (orphaned)" : "Daemon isn’t running (stale)")
            if let branch = mount.virtualBranch { Text("Branch: \(branch)") }
            Text(mount.workSummary)
            if let err = mount.configError { Text("Config error: \(err)") }
            Divider()
            if !mount.isOrphaned {
                Button("Remount") { Actions.remount(mount, monitor) }.disabled(busy)
            }
            Button("Forget (keeps local state)") { Actions.forget(mount, monitor) }.disabled(busy)
            Button("Show State in Finder") {
                NSWorkspace.shared.activateFileViewerSelecting(
                    [MountMonitor.mountsRoot.appendingPathComponent(mount.id, isDirectory: true)])
            }
        } label: {
            Label(busy ? "\(mount.title) — working…" : "\(mount.title)  \(mount.displayPath)",
                  systemImage: mount.isOrphaned ? "questionmark.folder" : "exclamationmark.arrow.circlepath")
        }
    }
}

/// "Open at Login" via SMAppService, so mounts that come back after a reboot
/// are visible without running `oak mount` first.
private struct LoginItemToggle: View {
    @State private var enabled = SMAppService.mainApp.status == .enabled

    var body: some View {
        Toggle("Open at Login", isOn: Binding(
            get: { enabled },
            set: { on in
                do {
                    if on { try SMAppService.mainApp.register() } else { try SMAppService.mainApp.unregister() }
                } catch {
                    Actions.alert("Couldn’t change the login item", error.localizedDescription)
                }
                enabled = SMAppService.mainApp.status == .enabled
            }))
    }
}

// MARK: - Actions

@MainActor
enum Actions {
    static func unmount(_ mount: MountEntry, _ monitor: MountMonitor) {
        if mount.hasWork {
            let choice = ask(
                "Unmount \(mount.title)?",
                "This mount has \(mount.workSummary.lowercased()). Unmounting discards that work — commit and `oak push` inside the mount first to keep it.",
                buttons: ["Open in Terminal", "Discard & Unmount", "Cancel"],
                destructive: 1)
            switch choice {
            case 0: openInTerminal(mount.url)
            case 1: run(monitor, "Couldn’t unmount \(mount.title)") { await monitor.unmount(mount, force: true) }
            default: break
            }
            return
        }
        Task {
            let out = await monitor.unmount(mount, force: false)
            guard !out.ok else { return }
            // `end` refuses when it finds work the list didn't show yet (a write
            // landed in between). Offer the forced path explicitly.
            let choice = ask("Couldn’t unmount \(mount.title)", out.message,
                             buttons: ["Discard & Unmount", "Cancel"], destructive: 0)
            if choice == 0 {
                run(monitor, "Couldn’t unmount \(mount.title)") { await monitor.unmount(mount, force: true) }
            }
        }
    }

    static func unmountAllClean(_ monitor: MountMonitor) {
        let live = monitor.liveMounts
        let clean = live.filter { !$0.hasWork }
        let kept = live.filter(\.hasWork)
        Task {
            var failures: [String] = []
            for m in clean {
                let out = await monitor.unmount(m, force: false)
                if !out.ok { failures.append("\(m.title): \(out.message)") }
            }
            var lines: [String] = []
            if !kept.isEmpty {
                lines.append("Kept \(plural(kept.count, "mount")) with unsaved work: "
                             + kept.map(\.title).joined(separator: ", ") + ".")
            }
            lines += failures
            if !lines.isEmpty {
                alert("Unmounted \(plural(clean.count - failures.count, "mount"))", lines.joined(separator: "\n\n"))
            }
        }
    }

    static func remount(_ mount: MountEntry, _ monitor: MountMonitor) {
        run(monitor, "Couldn’t remount \(mount.title)") { await monitor.remount(mount) }
    }

    static func forget(_ mount: MountEntry, _ monitor: MountMonitor) {
        run(monitor, "Couldn’t forget \(mount.title)") { await monitor.forget(mount) }
    }

    static func openWeb(_ mount: MountEntry, _ monitor: MountMonitor) {
        Task {
            if let url = await monitor.webURL(for: mount) {
                NSWorkspace.shared.open(url)
            } else {
                alert("No web page for \(mount.title)",
                      "`oak open --print` didn’t return a URL. Push the branch first with `oak push` inside the mount.")
            }
        }
    }

    static func openInTerminal(_ dir: URL) {
        guard let terminal = NSWorkspace.shared.urlForApplication(withBundleIdentifier: "com.apple.Terminal")
        else { return }
        NSWorkspace.shared.open([dir], withApplicationAt: terminal,
                                configuration: NSWorkspace.OpenConfiguration())
    }

    static func openExtensionSettings() {
        if let url = URL(string: "x-apple.systempreferences:com.apple.LoginItems-Settings.extension") {
            NSWorkspace.shared.open(url)
        }
    }

    static func copy(_ s: String) {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(s, forType: .string)
    }

    /// Run a CLI action and surface a failure as an alert.
    private static func run(_ monitor: MountMonitor, _ failureTitle: String,
                            _ op: @escaping @MainActor () async -> OakCLI.Output) {
        Task {
            let out = await op()
            if !out.ok { alert(failureTitle, out.message) }
        }
    }

    static func alert(_ title: String, _ message: String) {
        _ = ask(title, message, buttons: ["OK"], destructive: nil)
    }

    /// Modal alert; returns the index of the clicked button. The app is a
    /// menu-bar agent, so it activates first or the alert would open behind
    /// whatever has focus.
    @discardableResult
    static func ask(_ title: String, _ message: String, buttons: [String], destructive: Int?) -> Int {
        NSApp.activate()
        let a = NSAlert()
        a.messageText = title
        a.informativeText = message
        for (i, b) in buttons.enumerated() {
            let button = a.addButton(withTitle: b)
            if i == destructive { button.hasDestructiveAction = true }
        }
        return a.runModal().rawValue - NSApplication.ModalResponse.alertFirstButtonReturn.rawValue
    }
}
