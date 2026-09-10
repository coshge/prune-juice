import Foundation

/// User preferences.
///
/// The menu bar icon is off by default, which is a deliberate choice rather
/// than an oversight: the app opens a window and is closed when you are done.
/// The consequence to accept is that there is no ambient disk-pressure prompt.
@MainActor
final class Settings: ObservableObject {
    /// Posted so the AppKit shell can add or remove the status item.
    static let changed = Notification.Name("dev.prunejuice.settingsChanged")

    @Published var showMenuBarIcon: Bool {
        didSet {
            UserDefaults.standard.set(showMenuBarIcon, forKey: Keys.menuBar)
            NotificationCenter.default.post(name: Settings.changed, object: nil)
        }
    }

    private enum Keys {
        static let menuBar = "dev.prunejuice.showMenuBarIcon"
    }

    init() {
        self.showMenuBarIcon = UserDefaults.standard.bool(forKey: Keys.menuBar)
    }
}
