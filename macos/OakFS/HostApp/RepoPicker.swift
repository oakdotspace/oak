// RepoPicker.swift — the "Mount Repository" window.
//
// Lists everything `oak repo list --json` can see (your repos plus every public
// repo on the server), filters it as you type, and mounts the one you pick with
// `oak mount`. Like the rest of the app it holds no state of its own beyond the
// listing: the CLI does the work, and the mount shows up through MountMonitor.

import AppKit
import SwiftUI

// MARK: - `oak repo list --json`

/// One row of `oak repo list --json` (see `RepoListEntryJson` in
/// cli/src/commands/repo.rs). `full_name` is null when the server reported no
/// owner, and such a row can't be mounted, so it's dropped on load.
struct RepoEntry: Decodable, Identifiable, Hashable, Sendable {
    let owner: String?
    let name: String
    let fullName: String?
    let description: String?
    let isPublic: Bool?
    let updatedAt: String?

    var id: String { fullName ?? name }
    var publiclyVisible: Bool { isPublic ?? false }
}

struct RepoListing: Decodable, Sendable {
    let remote: String
    let authenticated: Bool
    let repos: [RepoEntry]
    let truncated: Bool

    /// The CLI's own ceiling on `--limit`.
    static let maxRows = 5000
}

// MARK: - Search

/// Client-side search over the listing. The server has no JSON search endpoint,
/// but `GET /api/repos` already returns every repo the caller can see.
enum RepoSearch {
    /// Every whitespace-separated term must appear in the full name or the
    /// description. Better name matches sort first; with no query, the caller's
    /// own and private repos come before everyone else's public ones.
    static func results(_ repos: [RepoEntry], query: String, account: String?) -> [RepoEntry] {
        let terms = query.lowercased().split(whereSeparator: \.isWhitespace).map(String.init)
        let matches = repos.compactMap { repo -> (repo: RepoEntry, rank: Int)? in
            guard let rank = rank(repo, terms: terms, account: account) else { return nil }
            return (repo, rank)
        }
        return matches.sorted { a, b in
            if a.rank != b.rank { return a.rank < b.rank }
            // RFC 3339 timestamps from one server share a format, so they sort as text.
            let (at, bt) = (a.repo.updatedAt ?? "", b.repo.updatedAt ?? "")
            if at != bt { return at > bt }
            return a.repo.id.lowercased() < b.repo.id.lowercased()
        }.map(\.repo)
    }

    private static func rank(_ repo: RepoEntry, terms: [String], account: String?) -> Int? {
        if terms.isEmpty {
            return repo.owner == account || !repo.publiclyVisible ? 0 : 1
        }
        let id = repo.id.lowercased()
        let name = repo.name.lowercased()
        let text = "\(id)\n\((repo.description ?? "").lowercased())"
        guard terms.allSatisfy(text.contains) else { return nil }
        let whole = terms.joined(separator: " ")
        if id == whole || name == whole { return 0 }
        if name.hasPrefix(whole) { return 1 }
        if id.contains(whole) { return 2 }
        return 3
    }
}

// MARK: - Model

@MainActor
@Observable
final class RepoPicker {
    private static let parentKey = "mountParentDirectory"

    var query = ""
    private(set) var repos: [RepoEntry] = []
    private(set) var loading = false
    private(set) var loadError: String?
    private(set) var remote: String?
    private(set) var authenticated = true
    private(set) var truncated = false
    /// Repo ids with an `oak mount` in flight.
    private(set) var mounting: Set<String> = []
    /// Mounts land at `<parent>/<owner>/<repo>`; `~/oaktree` is the layout
    /// `oak mount end` sweeps when it's given no destination.
    private(set) var parent: URL

    init() {
        let saved = UserDefaults.standard.string(forKey: Self.parentKey)
        parent = saved.map { URL(fileURLWithPath: $0, isDirectory: true) }
            ?? FileManager.default.homeDirectoryForCurrentUser
                .appendingPathComponent("oaktree", isDirectory: true)
    }

    func destination(for repo: RepoEntry) -> URL {
        parent.appendingPathComponent(repo.owner ?? "", isDirectory: true)
            .appendingPathComponent(repo.name, isDirectory: true)
    }

    func load(_ monitor: MountMonitor) async {
        guard !loading else { return }
        loading = true
        defer { loading = false }
        guard let cli = await monitor.locateCLI() else {
            loadError = "Couldn’t find the `oak` CLI. Install it from oak.space."
            return
        }
        let out = await Task.detached {
            OakCLI.run(cli, ["repo", "list", "--json", "--limit", String(RepoListing.maxRows)],
                       timeout: 60)
        }.value
        guard out.ok else {
            loadError = out.message.isEmpty ? "`oak repo list` failed (exit \(out.status))" : out.message
            return
        }
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        do {
            let listing = try decoder.decode(RepoListing.self, from: Data(out.stdout.utf8))
            repos = listing.repos.filter { $0.fullName != nil && $0.owner != nil }
            remote = listing.remote
            authenticated = listing.authenticated
            truncated = listing.truncated
            loadError = nil
        } catch {
            loadError = "Unexpected `oak repo list` output: \(error.localizedDescription)"
        }
    }

    func mount(_ repo: RepoEntry, _ monitor: MountMonitor) {
        guard let remote, let spec = repo.fullName, !mounting.contains(repo.id) else { return }
        mounting.insert(repo.id)
        Task {
            let out = await monitor.mount(spec: spec, at: destination(for: repo), remote: remote)
            mounting.remove(repo.id)
            if !out.ok { Actions.alert("Couldn’t mount \(spec)", out.message) }
        }
    }

    func chooseParent() {
        NSApp.activate()
        let panel = NSOpenPanel()
        panel.canChooseFiles = false
        panel.canChooseDirectories = true
        panel.canCreateDirectories = true
        panel.directoryURL = parent
        panel.prompt = "Choose"
        panel.message = "Repositories mount into <folder>/<owner>/<repo>."
        guard panel.runModal() == .OK, let url = panel.url else { return }
        parent = url
        UserDefaults.standard.set(url.path, forKey: Self.parentKey)
    }
}

// MARK: - Window

struct MountRepositoryView: View {
    let monitor: MountMonitor
    @State private var picker = RepoPicker()
    @FocusState private var searchFocused: Bool

    private var results: [RepoEntry] {
        RepoSearch.results(picker.repos, query: picker.query, account: monitor.account)
    }

    var body: some View {
        ZStack {
            Theme.bg.ignoresSafeArea()
            VStack(alignment: .leading, spacing: 14) {
                Text("Mount a repository")
                    .font(Theme.mono(18, .bold))
                    .foregroundStyle(Theme.ink)
                searchField
                if let hint = extensionHint { hintLine(hint) }
                if !picker.authenticated {
                    hintLine("Not signed in, so only public repositories are listed. Run `oak login` to see yours.")
                }
                if picker.truncated {
                    hintLine("Showing the first \(RepoListing.maxRows) repositories.")
                }
                list
                footer
            }
            .padding(22)
        }
        .task {
            searchFocused = true
            await picker.load(monitor)
        }
    }

    private var extensionHint: String? {
        switch monitor.extensionStatus {
        case .installedNotEnabled, .notInstalled: return "OakFS isn’t enabled yet, so mounts will fail. See **Setup & Help** in the menu bar."
        default: return nil
        }
    }

    private func hintLine(_ text: String) -> some View {
        Text(.init(text))
            .font(Theme.mono(11))
            .foregroundStyle(Theme.inkSoft)
            .fixedSize(horizontal: false, vertical: true)
    }

    private var searchField: some View {
        HStack(spacing: 9) {
            Image(systemName: "magnifyingglass")
                .font(.system(size: 12, weight: .semibold))
                .foregroundStyle(Theme.inkMute)
            TextField("Search your repos and every public repo", text: $picker.query)
                .textFieldStyle(.plain)
                .font(Theme.mono(13))
                .foregroundStyle(Theme.ink)
                .focused($searchFocused)
            if !picker.query.isEmpty {
                Button { picker.query = "" } label: {
                    Image(systemName: "xmark.circle.fill").foregroundStyle(Theme.inkMute)
                }
                .buttonStyle(.plain)
                .help("Clear search")
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 10)
        .background(Theme.panel)
        .overlay(Rectangle().strokeBorder(Theme.line, lineWidth: 1))
    }

    @ViewBuilder private var list: some View {
        let rows = results
        Group {
            if picker.repos.isEmpty {
                placeholder
            } else if rows.isEmpty {
                message("No repositories match “\(picker.query)”.")
            } else {
                ScrollView {
                    LazyVStack(spacing: 0) {
                        ForEach(rows) { repo in
                            RepoRow(repo: repo, picker: picker, monitor: monitor)
                            Rectangle().fill(Theme.line).frame(height: 1)
                        }
                    }
                }
                .scrollIndicators(.automatic)
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .overlay(Rectangle().strokeBorder(Theme.line, lineWidth: 1))
    }

    @ViewBuilder private var placeholder: some View {
        if picker.loading {
            ProgressView().controlSize(.small)
        } else if let err = picker.loadError {
            VStack(spacing: 12) {
                message(err)
                Button("Try Again") { Task { await picker.load(monitor) } }
                    .buttonStyle(RowButtonStyle())
            }
        } else {
            message("No repositories found.")
        }
    }

    private func message(_ text: String) -> some View {
        Text(.init(text))
            .font(Theme.mono(11.5))
            .foregroundStyle(Theme.inkSoft)
            .multilineTextAlignment(.center)
            .padding(20)
    }

    private var footer: some View {
        HStack(spacing: 10) {
            Text("Mounts into \((picker.parent.path as NSString).abbreviatingWithTildeInPath)/<owner>/<repo>")
                .lineLimit(1)
                .truncationMode(.middle)
            Spacer(minLength: 0)
            Button("Change…") { picker.chooseParent() }
                .buttonStyle(.plain)
                .foregroundStyle(Theme.ink)
                .underline()
        }
        .font(Theme.mono(10.5))
        .foregroundStyle(Theme.inkMute)
    }
}

private struct RepoRow: View {
    let repo: RepoEntry
    let picker: RepoPicker
    let monitor: MountMonitor

    var body: some View {
        HStack(alignment: .center, spacing: 12) {
            VStack(alignment: .leading, spacing: 3) {
                HStack(spacing: 7) {
                    Text(repo.id)
                        .font(Theme.mono(12.5, .semibold))
                        .foregroundStyle(Theme.ink)
                        .lineLimit(1)
                    Text(repo.publiclyVisible ? "public" : "private")
                        .font(Theme.mono(9.5))
                        .foregroundStyle(Theme.inkMute)
                }
                if let about = repo.description {
                    Text(about)
                        .font(Theme.mono(10.5))
                        .foregroundStyle(Theme.inkSoft)
                        .lineLimit(1)
                }
            }
            Spacer(minLength: 0)
            action
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 9)
    }

    @ViewBuilder private var action: some View {
        if let live = monitor.liveMounts.first(where: { $0.repo?.spec == repo.id }) {
            Button("Open") { NSWorkspace.shared.open(live.url) }
                .buttonStyle(RowButtonStyle())
                .help(live.displayPath)
        } else if picker.mounting.contains(repo.id) {
            ProgressView().controlSize(.small).frame(width: 60)
        } else {
            Button("Mount") { picker.mount(repo, monitor) }
                .buttonStyle(RowButtonStyle(filled: true))
                .help((picker.destination(for: repo).path as NSString).abbreviatingWithTildeInPath)
        }
    }
}

/// Compact square button for list rows: the homepage's inverted primary when
/// `filled`, otherwise a hairline outline.
private struct RowButtonStyle: ButtonStyle {
    var filled = false

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(Theme.mono(11, .semibold))
            .foregroundStyle(filled ? Theme.bg : Theme.ink)
            .frame(minWidth: 44)
            .padding(.horizontal, 8)
            .padding(.vertical, 5)
            .background(filled ? Theme.ink : Color.clear)
            .overlay(Rectangle().strokeBorder(filled ? Theme.ink : Theme.line, lineWidth: 1))
            .contentShape(Rectangle())
            .opacity(configuration.isPressed ? 0.85 : 1)
    }
}
