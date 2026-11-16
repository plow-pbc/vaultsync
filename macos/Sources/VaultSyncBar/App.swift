import AppKit
import SwiftUI

import ServiceManagement

@main
struct VaultSyncBarApp: App {
    @NSApplicationDelegateAdaptor private var delegate: AppDelegate
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

/// Starts the engine with the app, opens at login unless the user turned
/// that off, and stops the engine on quit.
final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationDidFinishLaunching(_ notification: Notification) {
        if !UserDefaults.standard.bool(forKey: "loginItemChosen") {
            try? SMAppService.mainApp.register()
            UserDefaults.standard.set(true, forKey: "loginItemChosen")
        }
        MainActor.assumeIsolated { Engine.shared.restart() }
    }

    func applicationWillTerminate(_ notification: Notification) {
        MainActor.assumeIsolated { Engine.shared.stop() }
    }
}

@MainActor @Observable
final class Model {
    var vaults: [VaultStatus] = []
    var problem: String?
    private var timer: Timer?
    private var refreshing = false

    init() {
        refresh()
        timer = Timer.scheduledTimer(withTimeInterval: 3, repeats: true) { [weak self] _ in
            Task { @MainActor in self?.refresh() }
        }
    }

    /// Asks the CLI off the main thread, one request at a time, so a slow
    /// answer never freezes the menu.
    func refresh() {
        guard !refreshing else { return }
        refreshing = true
        Task.detached {
            let result = Result { try CLI.status() }
            await MainActor.run {
                switch result {
                case .success(let vaults):
                    self.vaults = vaults
                    self.problem = nil
                case .failure(let error):
                    self.problem = error.localizedDescription
                }
                self.refreshing = false
            }
        }
    }

    /// Filled when every folder is in sync, an exclamation mark otherwise.
    var symbol: String {
        let healthy = problem == nil && vaults.allSatisfy {
            $0.error == nil && $0.running == true && ($0.conflicts ?? []).isEmpty && ($0.pending ?? 0) == 0
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
        Button("Add a folder…") { Actions.addFolder { model.refresh() } }
        Button("Join with a code…") { Actions.join { model.refresh() } }
        Divider()
        Toggle("Open at Login", isOn: Binding(
            get: { SMAppService.mainApp.status == .enabled },
            set: { on in try? on ? SMAppService.mainApp.register() : SMAppService.mainApp.unregister() }
        ))
        Button("Quit") { NSApplication.shared.terminate(nil) }.keyboardShortcut("q")
    }

    private func header(_ vault: VaultStatus) -> String {
        if vault.error != nil { return "\(vault.name) · can't read it" }
        if vault.running != true { return "\(vault.name) · not running" }
        let conflicts = vault.conflicts?.count ?? 0
        if conflicts > 0 { return "\(vault.name) · \(conflicts) conflict\(conflicts == 1 ? "" : "s")" }
        if let pending = vault.pending, pending > 0 {
            return "\(vault.name) · saving \(pending) change\(pending == 1 ? "" : "s")"
        }
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
