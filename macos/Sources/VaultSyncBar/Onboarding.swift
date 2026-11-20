import AppKit
import WebKit

/// The first-run window: Latch's welcome page, trimmed to WikiSync's three
/// steps (Resources/Onboarding), in a web view so its CSS animation plays as
/// it does there. Its last step adds a folder the way the menu does. Shown
/// until the user closes it or picks a folder.
@MainActor
final class Onboarding: NSObject, NSWindowDelegate, WKScriptMessageHandler, WKNavigationDelegate {
    private static let doneKey = "onboarded"
    private static var shown: Onboarding?
    private let window: NSWindow
    private let page: URL
    private let scripts = WKUserContentController()

    /// Installs from before onboarding existed already chose a login item, so
    /// they skip it.
    static func showOnFirstLaunch() {
        let defaults = UserDefaults.standard
        if defaults.object(forKey: doneKey) == nil {
            defaults.set(defaults.bool(forKey: "loginItemChosen"), forKey: doneKey)
        }
        guard !defaults.bool(forKey: doneKey), shown == nil,
              let page = Bundle.main.url(forResource: "index", withExtension: "html", subdirectory: "Onboarding")
        else { return }
        shown = Onboarding(page: page)
    }

    private init(page: URL) {
        window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 560, height: 640),
                          styleMask: [.titled, .closable, .fullSizeContentView], backing: .buffered, defer: false)
        self.page = page
        super.init()
        scripts.add(self, name: "onboarding")
        let configuration = WKWebViewConfiguration()
        configuration.userContentController = scripts
        let web = WKWebView(frame: .zero, configuration: configuration)
        web.navigationDelegate = self
        web.loadFileURL(page, allowingReadAccessTo: page.deletingLastPathComponent())
        window.title = "WikiSync by Plow"
        window.titlebarAppearsTransparent = true
        window.titleVisibility = .hidden
        window.appearance = NSAppearance(named: .darkAqua)
        window.isReleasedWhenClosed = false
        window.contentView = web
        window.delegate = self
        window.center()
        NSApp.activate(ignoringOtherApps: true)
        window.makeKeyAndOrderFront(nil)
    }

    func userContentController(_ controller: WKUserContentController, didReceive message: WKScriptMessage) {
        guard message.body as? String == "chooseFolder" else { return }
        Actions.addFolder { [window] in window.close() }
    }

    /// The window shows only its own page: anything else, such as a link
    /// dropped on it, is refused.
    func webView(_ webView: WKWebView, decidePolicyFor action: WKNavigationAction,
                 decisionHandler: @escaping @MainActor (WKNavigationActionPolicy) -> Void) {
        decisionHandler(action.request.url == page ? .allow : .cancel)
    }

    func windowWillClose(_ notification: Notification) {
        UserDefaults.standard.set(true, forKey: Self.doneKey)
        scripts.removeScriptMessageHandler(forName: "onboarding")
        Self.shown = nil
    }
}
