import AppKit
import Sparkle

/// Everything the interface knows about updates.
///
/// A plain observable object with no Sparkle types in it, for the same reason
/// `PJEvent` has no `bollard` types in it: the view layer should not have to
/// change if the mechanism does, and the tests should not need a real
/// `SPUUpdater` — which needs a real app bundle with a real feed URL, and
/// would log failures into every `swift test` run.
///
/// `isConfigured == false` is the honest state for a `swift run` or a bundle
/// built without a signing key, and the interface leaves the update controls
/// out entirely rather than showing buttons that do nothing.
@MainActor
final class UpdateStatus: ObservableObject {
    @Published private(set) var isConfigured = false

    /// A release that was found and deliberately not shown yet, because the
    /// app was busy or in the background. This is what puts the quiet line in
    /// the window instead of a modal over a running scan.
    @Published private(set) var pendingVersion: String?

    @Published private(set) var automaticallyChecks = false
    @Published private(set) var lastCheck: Date?

    private var onCheck: (@MainActor () -> Void)?
    private var onSetAutomatic: (@MainActor (Bool) -> Void)?
    /// An install the updater asked to perform while the helper was running.
    private var postponedInstall: (() -> Void)?

    // MARK: What the interface does

    /// The user asked. Always allowed, even mid-operation: only installing
    /// waits.
    func checkForUpdates() { onCheck?() }

    func setAutomaticChecks(_ on: Bool) {
        automaticallyChecks = on
        onSetAutomatic?(on)
    }

    /// A scan, cleanup or vault operation has finished.
    ///
    /// Called on every completion, successful or not — a failed run releases a
    /// postponed install just as much as a clean one, because either way the
    /// helper has stopped. Releases it once: the handler is taken before it is
    /// invoked, so a second completion has nothing to run.
    func hostBecameIdle() {
        guard let install = postponedInstall else { return }
        postponedInstall = nil
        Diagnostics.log("operation finished; releasing the postponed update install")
        install()
    }

    // MARK: What the mechanism reports
    //
    // Named transitions rather than settable properties. Every one of these
    // is a thing that happened, which is why the state is `private(set)` and
    // this is the only way in.

    func mechanismStarted(
        automatic: Bool, lastCheck: Date?,
        onCheck: @escaping @MainActor () -> Void,
        onSetAutomatic: @escaping @MainActor (Bool) -> Void
    ) {
        isConfigured = true
        automaticallyChecks = automatic
        self.lastCheck = lastCheck
        self.onCheck = onCheck
        self.onSetAutomatic = onSetAutomatic
    }

    func checkCompleted(at date: Date?) { lastCheck = date }

    /// An update was found and is being held for a better moment.
    func hold(version: String) { pendingVersion = version }

    /// The update is being presented properly, or the session ended, so the
    /// stand-in for it can go.
    func reminderRetired() { pendingVersion = nil }

    /// Hold an install until the helper stops.
    func postpone(_ install: @escaping () -> Void) { postponedInstall = install }
}

/// Sparkle, wired to Prune Juice's rules about when it may interrupt.
///
/// Three rules, and all three exist because this app spends minutes at a time
/// doing something that must not be disturbed:
///
/// 1. **A background check never interrupts work.** `mayPerformUpdateCheck`
///    refuses a scheduled check while an operation is running. A check the
///    user asked for is always allowed — they can see what the app is doing.
/// 2. **An install is never performed mid-operation.** Sparkle asks before
///    relaunching; while the helper is running the answer is "postpone", and
///    its handler is held until the operation finishes. Relaunching over a
///    running `--apply` would kill the helper between two deletions.
/// 3. **A found update waits its turn.** Sparkle's gentle reminders let an app
///    decline to show the alert now and take responsibility for surfacing it
///    later — here a line in the window and a badge on the Dock icon.
///
/// On trust: this app is not Developer ID signed, so Sparkle's code-signature
/// check cannot be what makes an update safe. `SUPublicEDKey` is. Without it
/// this returns `nil` and there is no update system, which is the behaviour we
/// want rather than a weaker fallback.
@MainActor
final class SparkleUpdater: NSObject {
    /// Build one, or explain why not.
    ///
    /// Sparkle needs at minimum a feed to read and a key to verify with. A
    /// build missing either has no update system.
    static func configured(status: UpdateStatus, isBusy: @escaping @MainActor () -> Bool)
        -> SparkleUpdater?
    {
        let info = Bundle.main.infoDictionary ?? [:]
        let feed = info["SUFeedURL"] as? String ?? ""
        let key = info["SUPublicEDKey"] as? String ?? ""
        guard !feed.isEmpty else {
            Diagnostics.log("no SUFeedURL in this bundle; updates are unavailable")
            return nil
        }
        guard !key.isEmpty else {
            Diagnostics.log("no SUPublicEDKey in this bundle; refusing to enable updates")
            return nil
        }
        return SparkleUpdater(status: status, isBusy: isBusy)
    }

    private let status: UpdateStatus
    private let isBusy: @MainActor () -> Bool
    private var controller: SPUStandardUpdaterController!

    private init(status: UpdateStatus, isBusy: @escaping @MainActor () -> Bool) {
        self.status = status
        self.isBusy = isBusy
        super.init()

        // `startingUpdater: true` runs the scheduled check itself, on the
        // interval in Info.plist. Nothing here calls `checkForUpdates` on
        // launch: doing that by hand is what turns "at most once a day" into
        // "every time the app opens".
        controller = SPUStandardUpdaterController(
            startingUpdater: true, updaterDelegate: self, userDriverDelegate: self)

        status.mechanismStarted(
            automatic: controller.updater.automaticallyChecksForUpdates,
            lastCheck: controller.updater.lastUpdateCheckDate,
            onCheck: { [weak self] in self?.check() },
            onSetAutomatic: { [weak self] on in
                self?.controller.updater.automaticallyChecksForUpdates = on
            })

        Diagnostics.log(
            "updater started; feed=\(controller.updater.feedURL?.absoluteString ?? "?") "
                + "automatic=\(controller.updater.automaticallyChecksForUpdates) "
                + "interval=\(Int(controller.updater.updateCheckInterval))s")
    }

    private func check() {
        // Clearing the reminder here rather than on arrival: the update is
        // about to be presented properly, so the stand-in for it can go.
        status.reminderRetired()
        NSApp.dockTile.badgeLabel = nil
        controller.updater.checkForUpdates()
        status.checkCompleted(at: controller.updater.lastUpdateCheckDate)
    }
}

// MARK: - SPUUpdaterDelegate

extension SparkleUpdater: SPUUpdaterDelegate {
    /// Rule 1. A scheduled check during a scan or cleanup is declined; an
    /// explicit one never is.
    nonisolated func updater(
        _ updater: SPUUpdater, mayPerform updateCheck: SPUUpdateCheck, error: NSErrorPointer
    ) -> Bool {
        guard updateCheck == .updatesInBackground else { return true }
        return MainActor.assumeIsolated {
            let busy = isBusy()
            if busy {
                Diagnostics.log("declining a background update check: an operation is running")
            }
            return !busy
        }
    }

    /// Rule 2. Sparkle is ready to install and relaunch. While the helper is
    /// mid-operation, hold the handler and say so; Sparkle then waits for us
    /// instead of terminating the app under a running `--apply`.
    nonisolated func updater(
        _ updater: SPUUpdater, shouldPostponeRelaunchForUpdate item: SUAppcastItem,
        untilInvokingBlock installHandler: @escaping () -> Void
    ) -> Bool {
        // Sparkle calls this on the main thread, but the handler is not
        // `Sendable`, so it travels in the same box `SubprocessService` uses.
        let handler = Box(installHandler)
        return MainActor.assumeIsolated {
            guard isBusy() else { return false }
            Diagnostics.log("postponing the update install until the operation finishes")
            status.postpone(handler.value)
            return true
        }
    }

    nonisolated func updater(
        _ updater: SPUUpdater, didFinishUpdateCycleFor updateCheck: SPUUpdateCheck, error: Error?
    ) {
        MainActor.assumeIsolated {
            status.checkCompleted(at: controller.updater.lastUpdateCheckDate)
        }
        if let error {
            // Never surfaced in the interface. Someone reclaiming disk space
            // does not need to hear that an update check failed.
            Diagnostics.log("update check finished with: \(error.localizedDescription)")
        }
    }
}

// MARK: - SPUStandardUserDriverDelegate

extension SparkleUpdater: SPUStandardUserDriverDelegate {
    /// Rule 3: this app takes responsibility for choosing the moment.
    nonisolated var supportsGentleScheduledUpdateReminders: Bool { true }

    /// Sparkle may show its own alert only when interrupting is harmless: the
    /// app is frontmost and nothing is running.
    nonisolated func standardUserDriverShouldHandleShowingScheduledUpdate(
        _ update: SUAppcastItem, andInImmediateFocus immediateFocus: Bool
    ) -> Bool {
        MainActor.assumeIsolated { !isBusy() && NSApp.isActive }
    }

    /// Called either way. When Sparkle is not showing the update, this is
    /// where the quiet reminder goes up.
    nonisolated func standardUserDriverWillHandleShowingUpdate(
        _ handleShowingUpdate: Bool, forUpdate update: SUAppcastItem, state: SPUUserUpdateState
    ) {
        MainActor.assumeIsolated {
            guard !handleShowingUpdate else { return }
            status.hold(version: update.displayVersionString)
            // A badge rather than a bounce: a new release is news, not a demand.
            NSApp.dockTile.badgeLabel = "1"
            Diagnostics.log("holding update \(update.displayVersionString) for a better moment")
        }
    }

    nonisolated func standardUserDriverDidReceiveUserAttention(forUpdate update: SUAppcastItem) {
        MainActor.assumeIsolated {
            status.reminderRetired()
            NSApp.dockTile.badgeLabel = nil
        }
    }

    nonisolated func standardUserDriverWillFinishUpdateSession() {
        MainActor.assumeIsolated {
            status.reminderRetired()
            NSApp.dockTile.badgeLabel = nil
        }
    }
}
