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
            Image(nsImage: model.state.icon).accessibilityLabel(model.state.label)
        }
        .menuBarExtraStyle(.menu)
    }
}

/// Starts the engine with the app, and shows the welcome on first launch.
final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationDidFinishLaunching(_ notification: Notification) {
        MainActor.assumeIsolated { Onboarding.showOnFirstLaunch() }
        NSLog("VaultSync: open at login: \(SMAppService.mainApp.status.rawValue)")
        MainActor.assumeIsolated { Engine.shared.start() }
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

    /// What the menu-bar icon shows, from what the menu lists: a problem, an
    /// unreadable or stopped folder, or a conflict needs the user; a folder
    /// saving changes or still starting is syncing.
    var state: SyncState {
        if problem != nil || vaults.contains(where: {
            $0.error != nil || !($0.conflicts ?? []).isEmpty || ($0.running != true && $0.started != false)
        }) { return .needsYou }
        if vaults.contains(where: { $0.running != true || ($0.pending ?? 0) > 0 }) { return .syncing }
        return .synced
    }
}

/// The menu-bar icons, template images macOS tints for the bar.
enum SyncState: String {
    case synced = "MenuSynced", syncing = "MenuSyncing", needsYou = "MenuNeedsYou"

    var icon: NSImage {
        let image = NSImage(named: rawValue)!
        image.isTemplate = true
        return image
    }

    var label: String {
        switch self {
        case .synced: "WikiSync by Plow"
        case .syncing: "WikiSync by Plow, syncing"
        case .needsYou: "WikiSync by Plow needs attention"
        }
    }
}

struct MenuContent: View {
    let model: Model

    var body: some View {
        Text("WikiSync by Plow")
        if let problem = model.problem {
            Text("WikiSync isn't answering: \(problem)")
        }
        if model.vaults.isEmpty && model.problem == nil {
            Text("No synced folders yet")
        }
        ForEach(model.vaults) { vault in
            Section(header(vault)) {
                if let error = vault.error {
                    Text(error)
                } else {
                    // Menu text is drawn gray; the dots' color carries the state.
                    Text("🟢 \(vault.machine ?? "") (this Mac)")
                    ForEach(vault.peers ?? []) { peer in
                        Text(peer.online ? "🟢 \(peer.name)" : "⚪️ \(peer.name)\(seen(peer.last_seen))")
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
            set: { Actions.openAtLogin($0) }
        ))
        Button("Quit") { NSApplication.shared.terminate(nil) }.keyboardShortcut("q")
    }

    private func header(_ vault: VaultStatus) -> String {
        if vault.error != nil { return "\(vault.name) · can't read it" }
        if vault.running != true {
            return "\(vault.name) · \(vault.started == false ? "starting…" : "not running")"
        }
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
