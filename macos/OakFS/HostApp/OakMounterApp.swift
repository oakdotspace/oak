// OakMounterApp.swift — the "Oak Mount" host app.
//
// FSKit extensions can't ship standalone: they live inside an app bundle. This
// app's only job is to carry `OakFS.appex` so macOS discovers it and surfaces
// it under System Settings → General → Login Items & Extensions → File System
// Extensions, where the user enables it once.
//
// It also offers a one-button "enable" affordance and reports whether the
// OakFS module is currently registered with the system. Everything else
// (mounting, the filesystem) is driven by the `oak` CLI; this app does not need
// to run to mount.
//
// Styling tracks the oak.space web redesign: a black & white, square-cornered,
// monospace "terminal" aesthetic. Hue is reserved for the brand mark (the
// canonical oak green) and genuinely semantic states (amber = warning). See
// crates/oak-web/src/layout.rs in the oakspace repo for the source tokens.

import SwiftUI

@main
struct OakMounterApp: App {
    var body: some Scene {
        WindowGroup("Oak Mount") {
            ContentView()
                .frame(width: 480, height: 580)
                .preferredColorScheme(.dark)
        }
        .windowResizability(.contentSize)
    }
}

// MARK: - Theme

/// Mirrors oak.space's dark-mode design tokens (`--oak-paper`, `--oak-ink`,
/// `--oak-line`, `--oak-brand`, …). The site leans hard into black & white;
/// green is the constant brand mark and amber is the lone semantic warning.
private enum Theme {
    // Core — pure black on near-white, the dark-mode oak.space chrome.
    static let bg = Color(red: 0, green: 0, blue: 0)                       // #000000  --oak-paper
    static let panel = Color(red: 0.078, green: 0.078, blue: 0.078)       // #141414  --oak-paper-2
    static let line = Color(red: 0.251, green: 0.251, blue: 0.251)        // #404040  --oak-line
    static let lineStrong = Color(red: 0.353, green: 0.353, blue: 0.353)  // #5A5A5A  --oak-line-strong
    static let ink = Color(red: 0.961, green: 0.961, blue: 0.961)         // #F5F5F5  --oak-ink
    static let inkSoft = Color(red: 0.627, green: 0.627, blue: 0.627)     // #A0A0A0  --oak-ink-soft
    static let inkMute = Color(red: 0.45, green: 0.45, blue: 0.45)        // #737373  muted chrome

    // Brand + semantic accents (used sparingly).
    static let brand = Color(red: 0.561, green: 0.808, blue: 0.275)       // #8FCE46  --oak-brand
    static let amber = Color(red: 0.902, green: 0.667, blue: 0.353)       // #E6AA5A  --oak-amber
    static let vermilion = Color(red: 0.941, green: 0.510, blue: 0.471)   // #F08278  --oak-vermilion

    /// Shared monospace face — the redesign's terminal aesthetic. SF Mono via
    /// the `.monospaced` design is the native stand-in for the web's Geist Mono.
    static func mono(_ size: CGFloat, _ weight: Font.Weight = .regular) -> Font {
        .system(size: size, weight: weight, design: .monospaced)
    }
}

// MARK: - Extension status

/// Whether macOS currently knows about (and has enabled) the OakFS module.
enum ExtensionStatus: Equatable {
    case checking
    case enabled
    case installedNotEnabled
    case notInstalled
    case unknown

    var label: String {
        switch self {
        case .checking: return "Checking…"
        case .enabled: return "OakFS is enabled"
        case .installedNotEnabled: return "OakFS is installed but turned off"
        case .notInstalled: return "OakFS isn’t registered yet"
        case .unknown: return "Status unavailable"
        }
    }

    var detail: String {
        switch self {
        case .checking: return "Asking the system about the file-system extension."
        case .enabled: return "`oak mount` will mount repositories through OakFS."
        case .installedNotEnabled: return "Turn it on under File System Extensions to start mounting."
        case .notInstalled: return "Keep this app in /Applications, then enable it in System Settings."
        case .unknown: return "Open System Settings to check the extension manually."
        }
    }

    var tint: Color {
        switch self {
        case .enabled: return Theme.brand
        case .installedNotEnabled: return Theme.amber
        case .notInstalled, .unknown: return Theme.inkMute
        case .checking: return Theme.inkMute
        }
    }
}

/// Best-effort probe of registered FSKit modules via `pluginkit`. The host app
/// is unsandboxed, so it can shell out; failures degrade to `.unknown`.
enum ExtensionProbe {
    static let bundleID = "com.oakvcs.mount.Extension"

    static func current() -> ExtensionStatus {
        let proc = Process()
        proc.executableURL = URL(fileURLWithPath: "/usr/bin/pluginkit")
        proc.arguments = ["-m", "-v", "-p", "com.apple.fskit.fsmodule"]
        let pipe = Pipe()
        proc.standardOutput = pipe
        proc.standardError = Pipe()
        do {
            try proc.run()
            let data = pipe.fileHandleForReading.readDataToEndOfFile()
            proc.waitUntilExit()
            guard let out = String(data: data, encoding: .utf8) else { return .unknown }
            for raw in out.split(separator: "\n") {
                let line = raw.trimmingCharacters(in: .whitespaces)
                guard line.contains(bundleID) else { continue }
                // pluginkit marks each plug-in with a leading status flag:
                // '+' enabled, '-' disabled/ignored.
                if line.hasPrefix("+") { return .enabled }
                if line.hasPrefix("-") || line.hasPrefix("!") { return .installedNotEnabled }
                return .installedNotEnabled
            }
            return .notInstalled
        } catch {
            return .unknown
        }
    }
}

// MARK: - Root view

struct ContentView: View {
    @State private var status: ExtensionStatus = .checking

    var body: some View {
        ZStack {
            // Flat pure black — the oak.space dark-mode page background.
            Theme.bg.ignoresSafeArea()

            VStack(alignment: .leading, spacing: 20) {
                header
                statusCard
                stepsCard
                Spacer(minLength: 0)
                footer
            }
            .padding(26)
        }
        .onAppear(perform: refresh)
        // Re-check whenever the window comes back to the foreground (e.g. the
        // user just toggled the switch in System Settings and tabbed back).
        .onReceive(NotificationCenter.default.publisher(
            for: NSApplication.didBecomeActiveNotification)) { _ in refresh() }
    }

    // Header: app mark + name + one-line purpose.
    private var header: some View {
        HStack(spacing: 16) {
            AppMark()
                .frame(width: 58, height: 58)
            VStack(alignment: .leading, spacing: 4) {
                Text("Oak Mount")
                    .font(Theme.mono(22, .bold))
                    .kerning(-0.5)
                    .foregroundStyle(Theme.ink)
                Text("File-system extension for `oak mount`")
                    .font(Theme.mono(11.5))
                    .foregroundStyle(Theme.inkSoft)
            }
        }
    }

    // Live status pill.
    private var statusCard: some View {
        HStack(alignment: .top, spacing: 14) {
            StatusDot(status: status)
                .padding(.top, 3)
            VStack(alignment: .leading, spacing: 4) {
                Text(status.label)
                    .font(Theme.mono(13.5, .semibold))
                    .foregroundStyle(Theme.ink)
                Text(.init(status.detail))
                    .font(Theme.mono(11.5))
                    .foregroundStyle(Theme.inkSoft)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Spacer(minLength: 0)
            Button(action: refresh) {
                Image(systemName: "arrow.clockwise")
                    .font(.system(size: 12, weight: .semibold))
                    .foregroundStyle(Theme.inkSoft)
            }
            .buttonStyle(.plain)
            .help("Re-check extension status")
        }
        .oakCard()
    }

    // The one-time enable instructions.
    private var stepsCard: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("Enable in three steps")
                .font(Theme.mono(11, .semibold))
                .foregroundStyle(Theme.inkMute)
                .textCase(.uppercase)
                .kerning(1.4)

            Step(number: 1, text: "Keep **Oak Mount** in your Applications folder.")
            Step(number: 2, text: "Open **Login Items & Extensions**, then **File System Extensions**.")
            Step(number: 3, text: "Turn on **OakFS**.")

            Button(action: openSettings) {
                HStack(spacing: 7) {
                    Image(systemName: "switch.2")
                    Text("Open Login Items & Extensions")
                }
                .frame(maxWidth: .infinity)
            }
            .buttonStyle(PrimaryButtonStyle())
            .padding(.top, 2)
        }
        .oakCard(padding: 18)
    }

    private var footer: some View {
        HStack(spacing: 6) {
            Text("Built on FSKit — no kernel extension required.")
            Spacer()
            Text("v\(appVersion)")
        }
        .font(Theme.mono(10.5))
        .foregroundStyle(Theme.inkMute)
    }

    private var appVersion: String {
        Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString")
            as? String ?? "0.1.0"
    }

    private func refresh() {
        status = .checking
        DispatchQueue.global(qos: .userInitiated).async {
            let result = ExtensionProbe.current()
            DispatchQueue.main.async { withAnimation(.easeOut(duration: 0.2)) { status = result } }
        }
    }

    private func openSettings() {
        if let url = URL(string:
            "x-apple.systempreferences:com.apple.LoginItems-Settings.extension") {
            NSWorkspace.shared.open(url)
        }
    }
}

// MARK: - Pieces

/// Square card surface — flat panel fill with a 1px neutral hairline, square
/// corners. Mirrors the web `.surface` treatment (`bg-oak-paper-2` + border,
/// `--radius-* : 0`).
private extension View {
    func oakCard(padding: CGFloat = 16) -> some View {
        self
            .padding(padding)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(Theme.panel)
            .overlay(Rectangle().strokeBorder(Theme.line, lineWidth: 1))
    }
}

/// The Oak brand mark — the pixel-art compass glyph from the oak.space web
/// chrome (`OAK_MARK_SVG`), redrawn as crisp cells in the brand green inside a
/// square tile. A literal reproduction of the 19×19 grid so it matches the site
/// exactly.
private struct AppMark: View {
    // (x, y, width, height) cells on the 19×19 grid, straight from OAK_MARK_SVG.
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

    var body: some View {
        Canvas { ctx, size in
            let u = size.width / 19.0
            for c in Self.cells {
                let rect = CGRect(x: c.0 * u, y: c.1 * u, width: c.2 * u, height: c.3 * u)
                ctx.fill(Path(rect), with: .color(Theme.brand))
            }
        }
        .padding(11)
        .background(Theme.panel)
        .overlay(Rectangle().strokeBorder(Theme.line, lineWidth: 1))
    }
}

private struct StatusDot: View {
    let status: ExtensionStatus
    var body: some View {
        Circle()
            .fill(status.tint)
            .frame(width: 10, height: 10)
            .overlay(Circle().fill(status.tint).blur(radius: 5).opacity(0.7))
            .overlay(Circle().strokeBorder(Color.white.opacity(0.15), lineWidth: 1))
    }
}

private struct Step: View {
    let number: Int
    let text: String
    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            // Circles survive the square-corner rule (avatars, dots, badges).
            Text("\(number)")
                .font(Theme.mono(11, .bold))
                .foregroundStyle(Theme.ink)
                .frame(width: 22, height: 22)
                .background(Color.white.opacity(0.04), in: Circle())
                .overlay(Circle().strokeBorder(Theme.line, lineWidth: 1))
            Text(.init(text))
                .font(Theme.mono(12.5))
                .foregroundStyle(Theme.ink)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
            Spacer(minLength: 0)
        }
    }
}

/// Primary action — flat brand-green fill, near-black label, square corners.
/// Matches the web `.btn-oak-brand` (the brand-accented primary button).
private struct PrimaryButtonStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(Theme.mono(12.5, .semibold))
            .foregroundStyle(Color.black.opacity(0.88))
            .padding(.vertical, 10)
            .background(Theme.brand)
            .overlay(Rectangle().strokeBorder(Color.white.opacity(0.12), lineWidth: 1))
            .brightness(configuration.isPressed ? -0.05 : 0)
            .opacity(configuration.isPressed ? 0.92 : 1)
    }
}
