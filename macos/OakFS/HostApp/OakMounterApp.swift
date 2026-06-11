// main.swift — the "Oak Mount" host app.
//
// FSKit extensions can't ship standalone: they live inside an app bundle. This
// app's only job is to carry `OakFS.appex` so macOS discovers it and surfaces
// it under System Settings → General → Login Items & Extensions → File System
// Extensions, where the user enables it once.
//
// It also offers a one-button "enable" affordance and shows whether the OakFS
// module is currently registered. Everything else (mounting, the filesystem)
// is driven by the `oak` CLI; this app does not need to run to mount.

import SwiftUI

@main
struct OakMounterApp: App {
    var body: some Scene {
        WindowGroup("Oak Mount") {
            ContentView()
                .frame(minWidth: 460, minHeight: 280)
        }
        .windowResizability(.contentSize)
    }
}

struct ContentView: View {
    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("Oak Mount")
                .font(.largeTitle).bold()
            Text("""
                 This app installs the **OakFS** file-system extension so `oak \
                 mount` can mount repositories with no kernel extension.
                 """)
            Divider()
            Text("To enable:")
                .font(.headline)
            Text("""
                 1. Keep this app in /Applications.
                 2. Open System Settings → General → Login Items & Extensions.
                 3. Under “File System Extensions”, turn on **OakFS**.
                 """)
            Spacer()
            Button("Open Login Items & Extensions") {
                if let url = URL(string:
                    "x-apple.systempreferences:com.apple.LoginItems-Settings.extension") {
                    NSWorkspace.shared.open(url)
                }
            }
        }
        .padding(24)
    }
}
