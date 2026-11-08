// StatusMenu.swift — the Oak Mount menu-bar extra.
//
// Makes a running `oak mount` obvious: the oak mark sits in the menu bar with
// a count of live mounts beside it, and the dropdown lists every mount with
// its repo, virtual branch, pending work and cache size, plus actions to open
// or unmount it, and an item opens the window that starts a new mount. Below
// that, the health of everything a mount depends on: the
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

/// The oak mark on a 512×512 grid, the same outline and rings as `OAK_MARK_SVG`. Each loop
/// is a flat x, y list of quadratic control points, curving through the
/// midpoints between them. A ring is two loops wound in opposite directions, so
/// filling with the non-zero rule leaves its middle clear.
enum OakMarkShape {
    static let side: CGFloat = 512
    static let loops: [[CGFloat]] = [
        [166, 80, 153, 84, 141, 89, 129, 95, 117, 102, 107, 111, 97, 120, 88, 130, 80, 141, 74, 153, 68, 165, 64, 178, 61, 191, 59, 204, 58, 218, 59, 231, 61, 245, 64, 258, 69, 271, 75, 284, 82, 295, 90, 307, 99, 317, 110, 326, 121, 334, 133, 342, 146, 348, 159, 352, 172, 356, 186, 358, 200, 359, 214, 359, 226, 358, 238, 358, 247, 359, 255, 365, 262, 374, 269, 385, 277, 396, 286, 406, 297, 416, 308, 424, 321, 431, 335, 436, 349, 439, 364, 440, 378, 439, 393, 435, 407, 430, 419, 422, 431, 413, 441, 402, 449, 389, 456, 376, 460, 362, 462, 347, 462, 332, 459, 318, 455, 304, 449, 291, 441, 279, 431, 268, 420, 259, 408, 251, 396, 245, 383, 240, 371, 236, 359, 232, 350, 226, 345, 218, 343, 208, 340, 195, 337, 182, 332, 169, 327, 157, 320, 144, 312, 133, 303, 122, 294, 112, 283, 104, 271, 96, 259, 89, 246, 84, 233, 80, 220, 78, 206, 76, 193, 76, 179, 78],
        [205, 88, 218, 90, 230, 93, 241, 98, 252, 103, 263, 109, 272, 117, 281, 125, 289, 134, 296, 144, 302, 154, 308, 165, 312, 177, 315, 188, 319, 201, 322, 214, 328, 228, 339, 240, 353, 246, 366, 250, 378, 254, 390, 259, 400, 265, 409, 272, 418, 281, 425, 290, 430, 300, 434, 311, 437, 322, 438, 334, 437, 345, 435, 356, 431, 366, 426, 376, 420, 385, 413, 393, 404, 401, 395, 406, 384, 411, 374, 414, 363, 416, 351, 416, 340, 414, 329, 411, 318, 407, 308, 401, 299, 393, 291, 385, 283, 375, 276, 364, 268, 352, 257, 341, 241, 335, 226, 334, 213, 335, 201, 335, 188, 334, 176, 333, 165, 331, 153, 327, 142, 323, 131, 318, 121, 311, 112, 304, 103, 295, 95, 286, 88, 276, 82, 265, 78, 254, 74, 242, 71, 230, 70, 218, 70, 205, 71, 193, 74, 180, 77, 169, 82, 157, 88, 146, 95, 136, 104, 126, 113, 118, 123, 110, 133, 103, 144, 98, 156, 93, 168, 90, 181, 88, 193, 88],
        [169, 115, 156, 119, 144, 125, 133, 133, 123, 142, 114, 153, 106, 164, 100, 176, 96, 189, 93, 203, 92, 216, 93, 230, 95, 244, 100, 257, 106, 270, 114, 281, 123, 292, 133, 302, 145, 310, 158, 316, 172, 321, 186, 323, 201, 324, 215, 323, 229, 321, 243, 316, 256, 311, 269, 304, 281, 294, 291, 283, 298, 269, 303, 255, 306, 241, 307, 226, 307, 212, 305, 198, 301, 184, 296, 171, 289, 158, 280, 147, 270, 137, 259, 128, 246, 121, 233, 116, 220, 112, 206, 110, 193, 110, 182, 112],
        [205, 123, 217, 125, 228, 129, 239, 135, 249, 142, 258, 150, 265, 159, 271, 169, 276, 179, 280, 190, 282, 202, 283, 213, 282, 225, 281, 236, 278, 248, 274, 258, 269, 267, 263, 275, 254, 281, 244, 287, 233, 292, 222, 296, 211, 299, 200, 300, 188, 300, 177, 299, 166, 296, 155, 291, 145, 286, 135, 279, 127, 271, 119, 261, 113, 251, 109, 240, 105, 228, 104, 216, 104, 204, 106, 192, 109, 180, 115, 169, 121, 158, 129, 149, 138, 140, 149, 134, 160, 128, 172, 124, 184, 122, 193, 122],
        [333, 267, 320, 272, 308, 279, 297, 289, 290, 301, 286, 314, 284, 328, 284, 342, 287, 356, 291, 369, 298, 382, 307, 393, 319, 403, 332, 410, 346, 415, 361, 417, 376, 416, 391, 411, 405, 404, 417, 394, 427, 382, 434, 368, 438, 353, 439, 338, 437, 323, 432, 308, 424, 296, 414, 285, 402, 276, 389, 269, 376, 265, 362, 264, 348, 264, 346, 265],
        [361, 276, 372, 278, 383, 283, 393, 289, 401, 297, 407, 306, 412, 317, 414, 327, 415, 338, 413, 348, 410, 358, 405, 367, 398, 375, 390, 382, 381, 387, 371, 391, 361, 393, 350, 392, 339, 390, 329, 386, 320, 380, 312, 372, 305, 362, 300, 352, 297, 340, 296, 328, 296, 316, 299, 305, 305, 294, 313, 287, 324, 281, 336, 277, 348, 275, 348, 276],
        [171, 150, 158, 157, 148, 165, 139, 176, 132, 188, 128, 202, 126, 216, 127, 230, 131, 244, 138, 257, 147, 268, 159, 278, 172, 285, 187, 289, 202, 290, 217, 288, 232, 282, 245, 274, 256, 263, 265, 250, 270, 235, 272, 220, 271, 204, 267, 189, 260, 176, 250, 165, 238, 156, 225, 149, 212, 145, 197, 144, 184, 146],
        [209, 158, 219, 163, 229, 169, 236, 177, 242, 186, 246, 197, 248, 207, 248, 218, 245, 228, 241, 238, 236, 246, 228, 253, 220, 259, 210, 263, 200, 266, 190, 266, 179, 264, 169, 260, 159, 255, 151, 247, 145, 238, 140, 227, 138, 215, 138, 204, 141, 192, 147, 181, 154, 172, 164, 164, 174, 159, 186, 156, 197, 156],
        [336, 295, 325, 304, 316, 315, 311, 329, 310, 343, 313, 358, 320, 372, 331, 383, 345, 391, 361, 394, 378, 391, 393, 384, 405, 372, 413, 357, 416, 341, 413, 325, 406, 310, 395, 299, 381, 291, 367, 288, 353, 289, 349, 290],
        [365, 301, 375, 305, 383, 312, 389, 321, 391, 330, 391, 340, 389, 349, 384, 356, 378, 363, 370, 367, 361, 369, 351, 369, 342, 366, 333, 361, 327, 352, 323, 342, 322, 331, 325, 320, 332, 310, 341, 304, 353, 300, 353, 300],
        [174, 187, 165, 198, 160, 213, 162, 229, 170, 243, 184, 253, 202, 256, 219, 250, 232, 237, 238, 220, 235, 202, 225, 188, 211, 179, 196, 178, 187, 180],
        [205, 193, 211, 200, 214, 208, 214, 216, 210, 223, 205, 228, 198, 232, 190, 232, 182, 229, 175, 223, 172, 213, 174, 203, 181, 194, 191, 190, 195, 190],
        [341, 321, 333, 335, 335, 353, 348, 366, 368, 369, 385, 359, 392, 339, 385, 321, 370, 312, 355, 312],
        [369, 332, 369, 338, 366, 343, 362, 346, 356, 347, 350, 344, 346, 337, 349, 328, 357, 324, 364, 326],
        [163, 41, 150, 44, 137, 48, 124, 53, 111, 59, 99, 66, 88, 74, 78, 83, 68, 93, 59, 103, 50, 114, 43, 126, 37, 138, 31, 151, 27, 164, 23, 177, 21, 191, 20, 205, 20, 219, 20, 232, 22, 246, 25, 260, 30, 273, 35, 286, 41, 298, 48, 310, 56, 321, 65, 332, 74, 342, 85, 351, 96, 359, 107, 367, 119, 373, 132, 379, 145, 383, 158, 387, 172, 389, 185, 391, 198, 393, 211, 394, 223, 396, 233, 399, 243, 404, 252, 412, 262, 420, 272, 429, 283, 437, 295, 445, 308, 451, 321, 456, 334, 459, 348, 461, 363, 462, 377, 460, 391, 458, 404, 453, 417, 448, 429, 440, 441, 432, 451, 422, 460, 411, 467, 399, 474, 386, 479, 373, 482, 359, 483, 345, 483, 331, 482, 317, 479, 303, 474, 289, 468, 277, 460, 265, 452, 254, 442, 243, 431, 234, 420, 226, 409, 219, 399, 212, 389, 205, 382, 197, 376, 187, 372, 176, 367, 164, 362, 152, 356, 139, 350, 127, 343, 116, 334, 104, 325, 94, 316, 84, 305, 75, 294, 67, 282, 60, 270, 54, 257, 48, 244, 44, 231, 41, 217, 39, 203, 38, 189, 38, 177, 39],
        [202, 59, 214, 60, 226, 62, 238, 64, 250, 68, 261, 73, 272, 78, 282, 85, 292, 92, 301, 100, 310, 108, 318, 118, 325, 127, 332, 138, 338, 149, 343, 160, 348, 172, 352, 184, 358, 197, 365, 209, 375, 220, 387, 229, 398, 236, 408, 243, 418, 251, 427, 259, 436, 267, 443, 277, 449, 287, 455, 297, 458, 309, 461, 320, 462, 332, 462, 344, 461, 355, 458, 367, 454, 378, 449, 389, 443, 398, 435, 408, 427, 416, 417, 423, 407, 429, 397, 434, 385, 437, 374, 440, 362, 441, 350, 440, 338, 439, 327, 436, 316, 432, 305, 426, 295, 420, 285, 413, 275, 405, 266, 396, 255, 387, 242, 380, 227, 375, 213, 373, 200, 372, 188, 370, 175, 369, 163, 366, 151, 363, 140, 359, 129, 354, 118, 348, 108, 342, 98, 335, 89, 327, 80, 318, 72, 308, 65, 298, 59, 288, 54, 277, 49, 266, 46, 254, 43, 242, 41, 230, 41, 218, 41, 206, 42, 194, 44, 182, 47, 170, 51, 158, 56, 147, 61, 136, 68, 126, 75, 116, 83, 107, 92, 99, 101, 91, 111, 84, 121, 78, 132, 72, 144, 68, 155, 64, 167, 61, 179, 59, 190, 59],
    ]

    /// Calls back with `start` once per loop, then `curve(end, control)`.
    static func trace(scale u: CGFloat, start: (CGPoint) -> Void, curve: (CGPoint, CGPoint) -> Void) {
        for loop in loops {
            let n = loop.count / 2
            func point(_ i: Int) -> CGPoint {
                CGPoint(x: loop[(i % n) * 2] * u, y: loop[(i % n) * 2 + 1] * u)
            }
            func mid(_ i: Int) -> CGPoint {
                CGPoint(x: (point(i).x + point(i + 1).x) / 2, y: (point(i).y + point(i + 1).y) / 2)
            }
            start(mid(0))
            for i in 1...n {
                curve(mid(i), point(i))
            }
        }
    }
}

/// The oak mark as a template image for the menu bar — the same outline as
/// `AppMark`. With `attention`, a small dot is punched into the lower-right
/// corner (a template image can only carry one color, so the dot is cut out
/// with a clear ring to stay legible).
enum MenuBarIcon {
    @MainActor private static var cache: [Bool: NSImage] = [:]

    @MainActor static func image(attention: Bool) -> NSImage {
        if let hit = cache[attention] { return hit }
        let side: CGFloat = 17
        let img = NSImage(size: NSSize(width: side, height: side), flipped: true) { rect in
            let u = rect.width / OakMarkShape.side
            NSColor.black.setFill()
            let mark = NSBezierPath()
            mark.windingRule = .nonZero
            OakMarkShape.trace(
                scale: u,
                start: { mark.close(); mark.move(to: $0) },
                curve: { mark.curve(to: $0, controlPoint: $1) }
            )
            mark.close()
            mark.fill()
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

    func showMount() {
        NSApp.activate()
        openWindow?(id: OakMounterApp.mountWindowID)
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

        Button("Mount Repository…") { WindowRouter.shared.showMount() }
            .keyboardShortcut("m")

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
