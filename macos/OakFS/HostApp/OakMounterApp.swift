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

import SwiftUI

@main
struct OakMounterApp: App {
    var body: some Scene {
        WindowGroup("Oak Mount") {
            ContentView()
                .frame(width: 480, height: 600)
                .preferredColorScheme(.dark)
        }
        .windowResizability(.contentSize)
    }
}

// MARK: - Theme

private enum Theme {
    static let green = Color(red: 0.471, green: 0.631, blue: 0.267)      // #78A144
    static let greenBright = Color(red: 0.557, green: 0.733, blue: 0.318) // #8EBB51
    static let bgTop = Color(red: 0.102, green: 0.118, blue: 0.075)       // #1A1E13
    static let bgBottom = Color(red: 0.035, green: 0.039, blue: 0.024)    // #090A06
    static let card = Color.white.opacity(0.04)
    static let cardStroke = Color.white.opacity(0.07)
    static let textPrimary = Color.white.opacity(0.92)
    static let textSecondary = Color.white.opacity(0.55)
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
        case .enabled: return Theme.greenBright
        case .installedNotEnabled: return Color(red: 0.92, green: 0.70, blue: 0.30)
        case .notInstalled, .unknown: return Color.white.opacity(0.35)
        case .checking: return Color.white.opacity(0.35)
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
            LinearGradient(colors: [Theme.bgTop, Theme.bgBottom],
                           startPoint: .top, endPoint: .bottom)
                .ignoresSafeArea()

            VStack(alignment: .leading, spacing: 22) {
                header
                statusCard
                stepsCard
                Spacer(minLength: 0)
                footer
            }
            .padding(28)
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
                .frame(width: 64, height: 64)
            VStack(alignment: .leading, spacing: 3) {
                Text("Oak Mount")
                    .font(.system(size: 26, weight: .bold))
                    .foregroundStyle(Theme.textPrimary)
                Text("File-system extension for `oak mount`")
                    .font(.system(size: 13))
                    .foregroundStyle(Theme.textSecondary)
            }
        }
    }

    // Live status pill.
    private var statusCard: some View {
        HStack(alignment: .top, spacing: 14) {
            StatusDot(status: status)
                .padding(.top, 2)
            VStack(alignment: .leading, spacing: 4) {
                Text(status.label)
                    .font(.system(size: 15, weight: .semibold))
                    .foregroundStyle(Theme.textPrimary)
                Text(.init(status.detail))
                    .font(.system(size: 12.5))
                    .foregroundStyle(Theme.textSecondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Spacer(minLength: 0)
            Button(action: refresh) {
                Image(systemName: "arrow.clockwise")
                    .font(.system(size: 13, weight: .semibold))
                    .foregroundStyle(Theme.textSecondary)
            }
            .buttonStyle(.plain)
            .help("Re-check extension status")
        }
        .padding(16)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Theme.card, in: RoundedRectangle(cornerRadius: 14))
        .overlay(RoundedRectangle(cornerRadius: 14)
            .strokeBorder(Theme.cardStroke, lineWidth: 1))
    }

    // The one-time enable instructions.
    private var stepsCard: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("Enable in three steps")
                .font(.system(size: 13, weight: .semibold))
                .foregroundStyle(Theme.textSecondary)
                .textCase(.uppercase)
                .kerning(0.6)

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
        .padding(18)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Theme.card, in: RoundedRectangle(cornerRadius: 14))
        .overlay(RoundedRectangle(cornerRadius: 14)
            .strokeBorder(Theme.cardStroke, lineWidth: 1))
    }

    private var footer: some View {
        HStack(spacing: 6) {
            Text("No kernel extension required — built on FSKit.")
            Spacer()
            Text("v\(appVersion)")
        }
        .font(.system(size: 11))
        .foregroundStyle(Theme.textSecondary.opacity(0.8))
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

/// The app icon, drawn from the asset catalog so it always matches the bundle.
private struct AppMark: View {
    var body: some View {
        Group {
            if let icon = NSImage(named: "AppIcon") {
                Image(nsImage: icon).resizable()
            } else {
                RoundedRectangle(cornerRadius: 14).fill(Theme.bgBottom)
            }
        }
        .aspectRatio(1, contentMode: .fit)
        .clipShape(RoundedRectangle(cornerRadius: 14, style: .continuous))
        .overlay(RoundedRectangle(cornerRadius: 14, style: .continuous)
            .strokeBorder(Color.white.opacity(0.08), lineWidth: 1))
        .shadow(color: .black.opacity(0.4), radius: 6, y: 3)
    }
}

private struct StatusDot: View {
    let status: ExtensionStatus
    var body: some View {
        Circle()
            .fill(status.tint)
            .frame(width: 11, height: 11)
            .overlay(Circle().fill(status.tint).blur(radius: 5).opacity(0.8))
            .overlay(Circle().strokeBorder(Color.white.opacity(0.15), lineWidth: 1))
    }
}

private struct Step: View {
    let number: Int
    let text: String
    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            Text("\(number)")
                .font(.system(size: 12, weight: .bold, design: .rounded))
                .foregroundStyle(Theme.greenBright)
                .frame(width: 24, height: 24)
                .background(Theme.green.opacity(0.15), in: Circle())
                .overlay(Circle().strokeBorder(Theme.green.opacity(0.35), lineWidth: 1))
            Text(.init(text))
                .font(.system(size: 13.5))
                .foregroundStyle(Theme.textPrimary)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
            Spacer(minLength: 0)
        }
    }
}

private struct PrimaryButtonStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(.system(size: 13.5, weight: .semibold))
            .foregroundStyle(.black.opacity(0.85))
            .padding(.vertical, 10)
            .background(
                LinearGradient(colors: [Theme.greenBright, Theme.green],
                               startPoint: .top, endPoint: .bottom),
                in: RoundedRectangle(cornerRadius: 10))
            .overlay(RoundedRectangle(cornerRadius: 10)
                .strokeBorder(Color.white.opacity(0.18), lineWidth: 1))
            .opacity(configuration.isPressed ? 0.85 : 1)
            .scaleEffect(configuration.isPressed ? 0.99 : 1)
    }
}
