import AppKit

enum Actions {
    static func openInObsidian(_ vault: VaultStatus) {
        open(path: vault.folder)
    }

    /// Opens a note in Obsidian, or in the default app when Obsidian isn't installed.
    static func open(file: String, in vault: VaultStatus) {
        open(path: (vault.folder as NSString).appendingPathComponent(file))
    }

    private static func open(path: String) {
        var parts = URLComponents()
        parts.scheme = "obsidian"
        parts.host = "open"
        parts.queryItems = [URLQueryItem(name: "path", value: path)]
        if let url = parts.url, NSWorkspace.shared.urlForApplication(toOpen: url) != nil {
            NSWorkspace.shared.open(url)
        } else {
            NSWorkspace.shared.open(URL(fileURLWithPath: path))
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

    /// Keeps access to the folder, runs `work` (a join waits on the network)
    /// off the main thread, then restarts the engine so it holds the folder.
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
                switch result {
                case .success: Engine.shared.restart()
                case .failure(let error): alert(failure, error.localizedDescription)
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
