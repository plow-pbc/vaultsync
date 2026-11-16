import Foundation

/// The folders the user gave this app (as security-scoped bookmarks, the
/// only way a sandboxed app keeps access across launches) and the embedded
/// engine that syncs them: one `vaultsync run --all` child process.
///
/// A child process inherits the folders the app holds open when it starts,
/// so the engine is restarted whenever a folder is added.
@MainActor
final class Engine {
    static let shared = Engine()

    private let defaultsKey = "folderBookmarks"
    private var process: Process?
    private var open: [URL] = []
    private var stopping = false

    /// Keeps access to `folder` from now on.
    func grant(_ folder: URL) throws {
        var bookmarks = UserDefaults.standard.dictionary(forKey: defaultsKey) as? [String: Data] ?? [:]
        bookmarks[folder.path] = try folder.bookmarkData(options: .withSecurityScope)
        UserDefaults.standard.set(bookmarks, forKey: defaultsKey)
    }

    /// (Re)starts the engine with every granted folder open.
    func restart() {
        stop()
        stopping = false
        let bookmarks = UserDefaults.standard.dictionary(forKey: defaultsKey) as? [String: Data] ?? [:]
        for (path, data) in bookmarks {
            var stale = false
            guard let url = try? URL(resolvingBookmarkData: data, options: .withSecurityScope,
                                     bookmarkDataIsStale: &stale),
                  url.startAccessingSecurityScopedResource()
            else {
                NSLog("VaultSync: lost access to \(path)")
                continue
            }
            if stale { try? grant(url) }
            open.append(url)
        }
        let process = Process()
        process.executableURL = CLI.binary
        process.arguments = ["run", "--all"]
        process.environment = CLI.environment
        process.terminationHandler = { [weak self] _ in
            // The engine exits only on failure; try again shortly.
            Task { @MainActor in
                guard let self, !self.stopping else { return }
                try? await Task.sleep(for: .seconds(5))
                if !self.stopping { self.restart() }
            }
        }
        do {
            try process.run()
            self.process = process
        } catch {
            NSLog("VaultSync: engine didn't start: \(error)")
        }
    }

    func stop() {
        stopping = true
        if let process, process.isRunning {
            process.terminate()
            process.waitUntilExit()
        }
        process = nil
        open.forEach { $0.stopAccessingSecurityScopedResource() }
        open = []
    }
}
