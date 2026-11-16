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

    static func addFolder() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.canCreateDirectories = true
        panel.prompt = "Sync this folder"
        NSApp.activate(ignoringOtherApps: true)
        guard panel.runModal() == .OK, let url = panel.url else { return }
        do {
            try CLI.run(["init", url.path])
        } catch {
            alert("Couldn't add \(url.lastPathComponent)", error.localizedDescription)
        }
    }

    static func join() {
        let field = NSTextField(frame: NSRect(x: 0, y: 0, width: 360, height: 24))
        field.placeholderString = "https://plow.co/sync#vs1… or the code"
        let prompt = NSAlert()
        prompt.messageText = "Join a synced folder"
        prompt.informativeText = "Paste the invite link from a machine that already syncs it."
        prompt.accessoryView = field
        prompt.addButton(withTitle: "Join")
        prompt.addButton(withTitle: "Cancel")
        NSApp.activate(ignoringOtherApps: true)
        guard prompt.runModal() == .alertFirstButtonReturn else { return }
        let code = field.stringValue.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !code.isEmpty else { return }
        do {
            try CLI.run(["join", code])
        } catch {
            alert("Couldn't join", error.localizedDescription)
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
