import Foundation

/// The folders the user gave this app, kept as security-scoped bookmarks
/// (the only way a sandboxed app keeps access across launches), and the
/// engine that syncs them: `run --all` on a thread of its own for the app's
/// lifetime. Access to a folder holds for the whole process, so the engine
/// picks up a folder added later by itself.
@MainActor
final class Engine {
    static let shared = Engine()

    private let defaultsKey = "folderBookmarks"
    private var started = false

    /// Opens every folder the user granted before, then starts the engine.
    /// Outside the sandbox (the direct download) the app shares the
    /// command-line tool's settings, so where its service is installed that
    /// service already runs every vault, and the app only shows them.
    func start() {
        guard !started else { return }
        started = true
        if Self.serviceRunsVaults {
            NSLog("WikiSync: the vaultsync service runs the vaults; showing them")
            return
        }
        for (path, data) in bookmarks {
            var stale = false
            guard let url = try? URL(resolvingBookmarkData: data, options: .withSecurityScope,
                                     bookmarkDataIsStale: &stale),
                  url.startAccessingSecurityScopedResource()
            else {
                NSLog("VaultSync: lost access to \(path)")
                continue
            }
            if stale { try? remember(url) }
        }
        Thread.detachNewThread {
            // Returns only if the engine fails; then try again shortly.
            while true {
                do {
                    try CLI.run(["run", "--all"])
                } catch {
                    NSLog("VaultSync: engine stopped: \(error.localizedDescription)")
                }
                Thread.sleep(forTimeInterval: 5)
            }
        }
    }

    /// Whether this is the App Store build, inside the App Sandbox.
    static let sandboxed = ProcessInfo.processInfo.environment["APP_SANDBOX_CONTAINER_ID"] != nil

    private static var serviceRunsVaults: Bool {
        let service = FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/LaunchAgents/vaultsync.plist")
        return !sandboxed && FileManager.default.fileExists(atPath: service.path)
    }

    /// Keeps access to a folder the user just picked, from now on.
    func grant(_ folder: URL) throws {
        try remember(folder)
        _ = folder.startAccessingSecurityScopedResource()
    }

    private var bookmarks: [String: Data] {
        UserDefaults.standard.dictionary(forKey: defaultsKey) as? [String: Data] ?? [:]
    }

    private func remember(_ folder: URL) throws {
        var all = bookmarks
        all[folder.path] = try folder.bookmarkData(options: .withSecurityScope)
        UserDefaults.standard.set(all, forKey: defaultsKey)
    }
}
