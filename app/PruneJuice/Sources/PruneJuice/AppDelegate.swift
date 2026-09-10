import AppKit
import SwiftUI

/// An AppKit shell hosting the SwiftUI interface.
///
/// Deliberately not `@main` on an `App` with a `Window` scene. A bundle
/// assembled by hand rather than by Xcode does not get the LaunchServices
/// registration SwiftUI's scene lifecycle depends on: the delegate runs, the
/// activation policy is set, and the window still never materialises — so the
/// app launches, shows nothing, and never scans. Creating the `NSWindow`
/// ourselves works regardless of how the bundle was produced, which is the
/// property that matters for a helper-bundling app that will never go through
/// the App Store.
///
/// It also gives the menu bar item its natural implementation: an
/// `NSStatusItem`, created only when the setting asks for it.
@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    private var window: NSWindow?
    private var statusItem: NSStatusItem?
    private let model = AppModel()
    private let settings = Settings()
    private var settingsObserver: NSObjectProtocol?

    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.regular)
        Diagnostics.log("did finish launching")

        buildMenu()
        showWindow()
        syncStatusItem()

        // The toggle lives in SwiftUI state; mirror it into AppKit.
        settingsObserver = NotificationCenter.default.addObserver(
            forName: Settings.changed, object: nil, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.syncStatusItem() }
        }

        NSApp.activate(ignoringOtherApps: true)
        model.scan()
    }

    nonisolated func applicationShouldHandleReopen(
        _ sender: NSApplication, hasVisibleWindows flag: Bool
    ) -> Bool {
        MainActor.assumeIsolated { showWindow() }
        return true
    }

    /// A window app, not an agent — unless the user asked for a menu bar icon,
    /// in which case closing the window should leave it running.
    nonisolated func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        MainActor.assumeIsolated { !settings.showMenuBarIcon }
    }

    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        guard model.busy else { return .terminateNow }
        showWindow()
        let alert = NSAlert()
        alert.messageText = "An operation is still running"
        alert.informativeText = "Wait for it to finish before quitting so Prune Juice can show the complete result."
        alert.addButton(withTitle: "Keep running")
        alert.runModal()
        return .terminateCancel
    }

    private func showWindow() {
        if let window {
            window.makeKeyAndOrderFront(nil)
            return
        }
        let root = RootView(model: model, settings: settings)
        let w = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1180, height: 780),
            styleMask: [.titled, .closable, .miniaturizable, .resizable],
            backing: .buffered,
            defer: false
        )
        w.title = "Prune Juice"
        w.titlebarAppearsTransparent = true
        w.toolbarStyle = .unified
        w.minSize = NSSize(width: 980, height: 690)
        w.contentView = NSHostingView(rootView: root)
        w.center()
        w.isReleasedWhenClosed = false
        w.makeKeyAndOrderFront(nil)
        window = w
        Diagnostics.log("window created")
    }

    private func syncStatusItem() {
        if settings.showMenuBarIcon {
            guard statusItem == nil else { return }
            let item = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
            item.button?.image = NSImage(
                systemSymbolName: "trash.slash", accessibilityDescription: "Prune Juice")
            let menu = NSMenu()
            menu.addItem(
                NSMenuItem(
                    title: "Open Prune Juice", action: #selector(openFromMenu), keyEquivalent: ""))
            menu.addItem(
                NSMenuItem(title: "Scan again", action: #selector(scanFromMenu), keyEquivalent: ""))
            menu.addItem(.separator())
            menu.addItem(
                NSMenuItem(title: "Quit", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q"))
            menu.items.forEach { $0.target = $0.action == #selector(NSApplication.terminate(_:)) ? nil : self }
            item.menu = menu
            statusItem = item
        } else if let item = statusItem {
            NSStatusBar.system.removeStatusItem(item)
            statusItem = nil
        }
    }

    @objc private func openFromMenu() {
        showWindow()
        NSApp.activate(ignoringOtherApps: true)
    }

    @objc private func scanFromMenu() {
        model.scan()
    }

    /// Without a menu bar an app has no Cmd-Q, no Cmd-W, and no edit commands.
    private func buildMenu() {
        let main = NSMenu()
        let appItem = NSMenuItem()
        let appMenu = NSMenu()
        appMenu.addItem(
            withTitle: "About Prune Juice",
            action: #selector(NSApplication.orderFrontStandardAboutPanel(_:)), keyEquivalent: "")
        appMenu.addItem(.separator())
        appMenu.addItem(
            withTitle: "Scan Again", action: #selector(scanFromMenu), keyEquivalent: "r")
        appMenu.addItem(.separator())
        appMenu.addItem(
            withTitle: "Hide Prune Juice", action: #selector(NSApplication.hide(_:)),
            keyEquivalent: "h")
        appMenu.addItem(
            withTitle: "Quit Prune Juice", action: #selector(NSApplication.terminate(_:)),
            keyEquivalent: "q")
        appItem.submenu = appMenu
        main.addItem(appItem)

        let editItem = NSMenuItem()
        let editMenu = NSMenu(title: "Edit")
        for (title, action, key) in [("Undo", Selector(("undo:")), "z"),
                                     ("Cut", #selector(NSText.cut(_:)), "x"),
                                     ("Copy", #selector(NSText.copy(_:)), "c"),
                                     ("Paste", #selector(NSText.paste(_:)), "v"),
                                     ("Select All", #selector(NSText.selectAll(_:)), "a")] {
            editMenu.addItem(withTitle: title, action: action, keyEquivalent: key)
        }
        editItem.submenu = editMenu
        main.addItem(editItem)

        let windowItem = NSMenuItem()
        let windowMenu = NSMenu(title: "Window")
        windowMenu.addItem(
            withTitle: "Close", action: #selector(NSWindow.performClose(_:)), keyEquivalent: "w")
        windowMenu.addItem(
            withTitle: "Minimise", action: #selector(NSWindow.performMiniaturize(_:)),
            keyEquivalent: "m")
        windowItem.submenu = windowMenu
        main.addItem(windowItem)

        NSApp.mainMenu = main
    }
}
