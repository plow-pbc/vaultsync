import AppKit

@MainActor
enum Actions {
    static func openInObsidian(_ vault: VaultStatus) {
        open(path: vault.folder, in: vault.folder)
    }

    /// Opens a note in Obsidian, or in the default app when Obsidian isn't installed.
    static func open(file: String, in vault: VaultStatus) {
        open(path: (vault.folder as NSString).appendingPathComponent(file), in: vault.folder)
    }

    /// Obsidian opens a path by URL only inside a folder it knows as a vault,
    /// from the list it reads as it starts and rewrites as it quits. A folder
    /// it doesn't know yet is added there with Obsidian closed, then opened.
    private static func open(path: String, in folder: String) {
        var parts = URLComponents()
        parts.scheme = "obsidian"
        parts.host = "open"
        parts.queryItems = [URLQueryItem(name: "path", value: path)]
        guard let url = parts.url, NSWorkspace.shared.urlForApplication(toOpen: url) != nil else {
            NSWorkspace.shared.open(URL(fileURLWithPath: path))
            return
        }
        // The App Store build can't reach Obsidian's settings.
        if Engine.sandboxed || ObsidianVaults.contains(folder) {
            NSWorkspace.shared.open(url)
            return
        }
        Task {
            for app in NSRunningApplication.runningApplications(withBundleIdentifier: "md.obsidian") {
                app.terminate()
                let asked = Date()
                while !app.isTerminated {
                    guard Date().timeIntervalSince(asked) < 60 else {
                        return alert("Obsidian didn't quit", "Quit Obsidian, then choose Open in Obsidian again.")
                    }
                    try? await Task.sleep(for: .milliseconds(250))
                }
            }
            do {
                try ObsidianVaults.add(folder)
            } catch {
                return alert("Couldn't add \((folder as NSString).lastPathComponent) to Obsidian",
                             error.localizedDescription)
            }
            NSWorkspace.shared.open(url)
        }
    }

    /// Copies a one-time plow.co/sync link another machine joins with.
    static func invite(_ vault: VaultStatus) {
        do {
            let out = try CLI.run(["--vault", vault.folder, "invite"])
            guard let link = out.split(separator: "\n")
                .first(where: { $0.hasPrefix("Link: ") })?
                .dropFirst("Link: ".count)
            else { throw CLIError(message: "vaultsync printed no link") }
            NSPasteboard.general.clearContents()
            NSPasteboard.general.setString(String(link), forType: .string)
            alert("Invite link copied",
                  "Open it on the other machine. It works once, for 10 minutes, while this Mac is on.")
        } catch {
            alert("Couldn't make an invite", error.localizedDescription)
        }
    }

    /// Syncs a folder: one already synced elsewhere on this Mac (by the
    /// command-line tool, say) is taken over as is; any other becomes a new vault.
    static func addFolder(done: @escaping @MainActor () -> Void) {
        guard let url = pickFolder(prompt: "Sync this folder", message: nil) else { return }
        inBackground(granting: url, failure: "Couldn't add \(url.lastPathComponent)", then: done) {
            do {
                try CLI.run(["--vault", url.path, "track"])
            } catch {
                try CLI.run(["init", url.path])
            }
        }
    }

    static func join(done: @escaping @MainActor () -> Void) {
        let field = NSTextField(frame: NSRect(x: 0, y: 0, width: 360, height: 24))
        field.placeholderString = "https://plow.co/sync#vs1… or the code"
        let prompt = NSAlert()
        prompt.messageText = "Join a synced folder"
        prompt.informativeText = "Paste the invite link from a machine that already syncs it."
        prompt.accessoryView = field
        prompt.addButton(withTitle: "Next")
        prompt.addButton(withTitle: "Cancel")
        NSApp.activate(ignoringOtherApps: true)
        guard prompt.runModal() == .alertFirstButtonReturn else { return }
        let code = field.stringValue.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !code.isEmpty,
              let url = pickFolder(prompt: "Put it here",
                                   message: "Choose a new, empty folder for the synced files.")
        else { return }
        inBackground(granting: url, failure: "Couldn't join", then: done) {
            try CLI.run(["join", code, "--folder", url.path])
        }
    }

    /// The sandbox grants this app a folder only when the user picks it.
    private static func pickFolder(prompt: String, message: String?) -> URL? {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.canCreateDirectories = true
        panel.prompt = prompt
        panel.message = message ?? ""
        NSApp.activate(ignoringOtherApps: true)
        return panel.runModal() == .OK ? panel.url : nil
    }

    /// Keeps access to the folder, then runs `work` (a join waits on the
    /// network) off the main thread; the engine picks the folder up itself.
    private static func inBackground(
        granting url: URL,
        failure: String,
        then done: @escaping @MainActor () -> Void,
        work: @escaping @Sendable () throws -> Void
    ) {
        do {
            try Engine.shared.grant(url)
        } catch {
            alert(failure, error.localizedDescription)
            return
        }
        Task.detached {
            let result = Result { try work() }
            await MainActor.run {
                if case .failure(let error) = result {
                    alert(failure, error.localizedDescription)
                }
                done()
            }
        }
    }

    private static func alert(_ title: String, _ text: String) {
        let alert = NSAlert()
        alert.messageText = title
        alert.informativeText = text
        NSApp.activate(ignoringOtherApps: true)
        alert.runModal()
    }
}

/// Obsidian's own list of vaults, the folders it opens by URL.
enum ObsidianVaults {
    private static let list = FileManager.default.homeDirectoryForCurrentUser
        .appendingPathComponent("Library/Application Support/obsidian/obsidian.json")

    /// The settings, or none if Obsidian has never run; a file that can't be
    /// read is an error, so it is never written over.
    private static func read() throws -> [String: Any] {
        guard FileManager.default.fileExists(atPath: list.path) else { return [:] }
        guard let object = try JSONSerialization.jsonObject(with: Data(contentsOf: list)) as? [String: Any]
        else { throw CocoaError(.fileReadCorruptFile, userInfo: [NSFilePathErrorKey: list.path]) }
        return object
    }

    /// Whether `folder` is a vault Obsidian knows, or inside one.
    static func contains(_ folder: String) -> Bool {
        let vaults = (try? read())?["vaults"] as? [String: [String: Any]] ?? [:]
        return vaults.values.contains {
            guard let path = $0["path"] as? String else { return false }
            return folder == path || folder.hasPrefix(path + "/")
        }
    }

    /// Adds `folder` as Obsidian adds a vault: a random 16-digit id, the path
    /// and the time in milliseconds. Every other setting is kept.
    static func add(_ folder: String) throws {
        guard !contains(folder) else { return }
        var settings = try read()
        var vaults = settings["vaults"] as? [String: Any] ?? [:]
        let id = (0..<8).map { _ in String(format: "%02x", UInt8.random(in: 0...255)) }.joined()
        vaults[id] = ["path": folder, "ts": Int(Date().timeIntervalSince1970 * 1000)]
        settings["vaults"] = vaults
        try FileManager.default.createDirectory(at: list.deletingLastPathComponent(),
                                                withIntermediateDirectories: true)
        try JSONSerialization.data(withJSONObject: settings).write(to: list, options: .atomic)
    }
}
