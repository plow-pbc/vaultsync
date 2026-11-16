import AppKit
import SwiftUI

@main
struct VaultSyncBarApp: App {
    @State private var model = Model()

    var body: some Scene {
        MenuBarExtra {
            MenuContent(model: model)
        } label: {
            Image(systemName: model.symbol)
        }
        .menuBarExtraStyle(.menu)
    }
}

@Observable
final class Model {
    var vaults: [VaultStatus] = []
    var problem: String?
    private var timer: Timer?

    init() {
        refresh()
        timer = Timer.scheduledTimer(withTimeInterval: 3, repeats: true) { [weak self] _ in
            self?.refresh()
        }
    }

    func refresh() {
        do {
            vaults = try CLI.status()
            problem = nil
        } catch {
            problem = error.localizedDescription
        }
    }

    /// Filled when every folder is in sync, an exclamation mark otherwise.
    var symbol: String {
        let healthy = problem == nil && vaults.allSatisfy {
            $0.error == nil && $0.running == true && ($0.conflicts ?? []).isEmpty
        }
        return healthy ? "arrow.triangle.2.circlepath.circle.fill" : "exclamationmark.arrow.triangle.2.circlepath"
    }
}

struct MenuContent: View {
    let model: Model

    var body: some View {
        if let problem = model.problem {
            Text("vaultsync isn't answering: \(problem)")
        }
        if model.vaults.isEmpty && model.problem == nil {
            Text("No synced folders yet")
        }
        ForEach(model.vaults) { vault in
            Section(header(vault)) {
                if let error = vault.error {
                    Text(error)
                } else {
                    Text("● \(vault.machine ?? "") (this Mac)")
                    ForEach(vault.peers ?? []) { peer in
                        Text(peer.online ? "● \(peer.name)" : "○ \(peer.name)\(seen(peer.last_seen))")
                    }
                    ForEach(vault.conflicts ?? [], id: \.self) { file in
                        Button("Conflict: \(file)") { Actions.open(file: file, in: vault) }
                    }
                    Button("Open in Obsidian") { Actions.openInObsidian(vault) }
                    Button("Invite a machine…") { Actions.invite(vault) }
                }
            }
        }
        Divider()
        Button("Add a folder…") { Actions.addFolder(); model.refresh() }
        Button("Join with a code…") { Actions.join(); model.refresh() }
        Divider()
        Button("Quit") { NSApplication.shared.terminate(nil) }.keyboardShortcut("q")
    }

    private func header(_ vault: VaultStatus) -> String {
        if vault.error != nil { return "\(vault.name) · can't read it" }
        if vault.running != true { return "\(vault.name) · not running" }
        let conflicts = vault.conflicts?.count ?? 0
        if conflicts > 0 { return "\(vault.name) · \(conflicts) conflict\(conflicts == 1 ? "" : "s")" }
        guard let synced = vault.last_sync else { return "\(vault.name) · waiting for a machine" }
        return "\(vault.name) · synced \(ago(synced))"
    }

    private func seen(_ time: Int?) -> String {
        guard let time else { return ", not seen yet" }
        return ", seen \(ago(time))"
    }

    private func ago(_ time: Int) -> String {
        let seconds = max(0, Int(Date().timeIntervalSince1970) - time)
        if seconds < 60 { return "\(seconds)s ago" }
        let formatter = RelativeDateTimeFormatter()
        formatter.unitsStyle = .short
        return formatter.localizedString(fromTimeInterval: -Double(seconds))
    }
}
