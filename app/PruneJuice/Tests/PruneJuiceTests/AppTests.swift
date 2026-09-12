import XCTest
import SwiftUI
import AppKit
@testable import PruneJuice

final class StubService: PruneJuiceService, @unchecked Sendable {
    var calls: [[String]] = []
    var event: (@Sendable (PJEvent) -> Void)?
    var finish: (@Sendable (Error?) -> Void)?
    func run(arguments: [String], onOutput: @escaping @Sendable (String) -> Void,
             onEvent: @escaping @Sendable (PJEvent) -> Void, onFinish: @escaping @Sendable (Error?) -> Void) {
        calls.append(arguments); event = onEvent; finish = onFinish
    }
}

/// A counter a non-escaping-unfriendly closure can increment.
final class Counter: @unchecked Sendable { var value = 0 }

final class AppTests: XCTestCase {
    func testPinnedHelperAcceptsAppOptions() throws {
        guard let helper = ProcessInfo.processInfo.environment["PJ_TEST_HELPER"] else {
            throw XCTSkip("Set PJ_TEST_HELPER to test a release helper")
        }
        var options = ScanOptions()
        options.context = "compatibility-test"
        options.roots = "/tmp/project-one:/tmp/project-two"
        options.label = "project=test"
        options.deadline = "120"
        options.sizes = false
        options.probe = false
        options.containerProbe = true
        options.vault = false
        let process = Process()
        process.executableURL = URL(fileURLWithPath: helper)
        // --help exits during argument parsing, before any Docker or disk work.
        process.arguments = ["--json", "--no-tui"] + options.arguments + ["--help"]
        let output = Pipe()
        process.standardOutput = output
        process.standardError = output
        try process.run()
        let data = output.fileHandleForReading.readDataToEndOfFile()
        process.waitUntilExit()
        XCTAssertEqual(process.terminationStatus, 0, String(decoding: data, as: UTF8.self))
        XCTAssertTrue(String(decoding: data, as: UTF8.self).contains("--json"))
    }

    @MainActor func settle() async { try? await Task.sleep(for: .milliseconds(100)) }

    func testOptionsMapEveryCLIControlWithoutShellParsing() {
        var options = ScanOptions()
        options.context = "local context"
        options.roots = "/tmp/Project One:/tmp/Two"
        options.label = "project=one"
        options.deadline = "0"
        options.sizes = false; options.probe = false; options.containerProbe = true; options.vault = false
        XCTAssertNil(options.validation)
        XCTAssertEqual(options.arguments, ["--deadline", "0", "--context", "local context", "--roots", "/tmp/Project One:/tmp/Two", "--only-label", "project=one", "--no-sizes", "--no-probe", "--container-probe", "--no-vault"])
        options.deadline = "-1"
        XCTAssertNotNil(options.validation)
        options.deadline = "120"; options.label = "invalid"
        XCTAssertNotNil(options.validation)
    }

    @MainActor func testCleanupRequiresScanAndInvalidatesAfterMutation() async {
        let service = StubService(); let model = AppModel(service: service)
        XCTAssertFalse(model.canReclaim)
        model.scan(); model.scan()
        XCTAssertEqual(service.calls.count, 1)
        service.event?(.scanFinished(totals: Totals(), durationMs: 1, stale: false))
        service.finish?(nil); await settle()
        XCTAssertTrue(model.canReclaim)
        model.options.context = "different"
        XCTAssertFalse(model.canReclaim)
        model.options.context = ""
        model.selectedTiers = ["free", "orphan"]
        model.reclaim(); model.reclaim()
        XCTAssertEqual(service.calls.count, 2)
        XCTAssertEqual(Array(service.calls[1].suffix(3)), ["--apply", "--tiers", "free,orphan"])
        service.finish?(nil); await settle()
        XCTAssertFalse(model.canReclaim)
    }

    @MainActor func testContextsDoNotCollideAndRevalidationDoesNotDoubleTotals() async {
        let service = StubService(); let model = AppModel(service: service)
        model.scan()
        var totals = Totals(); totals.volumes = 1; totals.volumeBytes = 100
        let item = Classified(kind: "volume", name: "same", tier: "free", because: "empty", size: 100, owner: nil, provenance: [])
        for context in ["first", "second"] {
            service.event?(.scanStarted(daemon: context, context: context, runtime: "orbstack", apiVersion: "1"))
            service.event?(.classified(item))
            service.event?(.scanFinished(totals: totals, durationMs: 1, stale: false))
            service.event?(.scanFinished(totals: totals, durationMs: 1, stale: false))
        }
        service.finish?(nil); await settle()
        XCTAssertEqual(model.items.count, 2)
        XCTAssertEqual(Set(model.items.map(\.id)).count, 2)
        XCTAssertEqual(model.totals.volumeBytes, 200)
    }

    @MainActor func testLabelFenceExcludesBuildCacheAndFailuresStayVisible() async {
        let service = StubService(); let model = AppModel(service: service)
        model.options.label = "project=test"; model.scan()
        var totals = Totals(); totals.buildCacheBytes = 900
        service.event?(.scanFinished(totals: totals, durationMs: 1, stale: false))
        service.finish?(ServiceError.helperFailed(code: 5, stderr: "Partial failure")); await settle()
        XCTAssertEqual(model.safeBytes, 0)
        XCTAssertFalse(model.canReclaim)
        XCTAssertEqual(model.state, .failed("Partial failure"))
    }

    @MainActor func testMissingReportAndPartialHostMeasurementAreNotSuccess() async {
        let service = StubService(); let model = AppModel(service: service)
        model.scan(); service.finish?(nil); await settle()
        XCTAssertFalse(model.canReclaim)
        guard case .failed = model.state else { return XCTFail("Missing report treated as success") }
        model.command("Result", [])
        service.event?(.hostReclaim(dockerReported: 100, hostMeasured: nil, confidence: "unavailable"))
        service.event?(.hostReclaim(dockerReported: 200, hostMeasured: 150, confidence: "measured"))
        service.finish?(nil); await settle()
        XCTAssertEqual(model.dockerReclaimed, 300)
        XCTAssertNil(model.hostReclaimed)
    }

    @MainActor func testScanActivityStreamsWithoutPlainTextOutput() async {
        let service = StubService(); let model = AppModel(service: service)
        model.scan()
        XCTAssertTrue(model.output.contains("Scan started"))
        service.event?(.scanStarted(daemon: "one", context: "local", runtime: "OrbStack", apiVersion: "1"))
        service.event?(.phase(phase: "probing", done: 1, total: 3))
        service.event?(.phase(phase: "probing", done: 2, total: 3))
        await settle()
        XCTAssertTrue(model.busy)
        XCTAssertTrue(model.output.contains("Inspecting local"))
        XCTAssertEqual(model.output.components(separatedBy: "Probing in local").count - 1, 1)
        var totals = Totals(); totals.volumes = 3
        service.event?(.scanFinished(totals: totals, durationMs: 1200, stale: true))
        service.finish?(nil); await settle()
        XCTAssertTrue(model.output.contains("3 volumes"))
        XCTAssertTrue(model.output.contains("Scan complete"))
        XCTAssertTrue(model.output.contains("incomplete"))
        XCTAssertTrue(model.output.contains("Nothing was deleted"))
        model.scan()
        XCTAssertFalse(model.output.contains("Scan complete"))
        service.finish?(ServiceError.helperFailed(code: 3, stderr: "Docker unavailable")); await settle()
        XCTAssertTrue(model.output.contains("Scan failed: Docker unavailable"))
    }

    @MainActor func testSelectedReclaimEstimateTracksTiersAndCacheEligibility() {
        let model = AppModel(service: StubService())
        for (tier, size) in [("free", 100), ("orphan", 200), ("repullable", 300), ("stale", 400), ("rebuildable", 500)] {
            model.byTier[tier] = [Classified(kind: "volume", name: tier, tier: tier, because: "fixture", size: UInt64(size), owner: nil, provenance: [])]
        }
        model.totals.buildCacheBytes = 50
        XCTAssertEqual(model.selectedBytes, 150)
        model.selectedTiers.insert("orphan")
        XCTAssertEqual(model.selectedBytes, 350)
        model.selectedTiers = Set(ReclaimTier.all.map(\.id))
        XCTAssertEqual(model.selectedBytes, 1550)
        model.selectedTiers.remove("free")
        XCTAssertEqual(model.selectedBytes, 1400)
        model.selectedTiers = []
        XCTAssertEqual(model.selectedBytes, 0)
        model.selectedTiers = ["free"]
        model.options.label = "project=test"
        XCTAssertEqual(model.selectedBytes, 100)
        model.options.label = ""; model.options.sizes = false
        XCTAssertEqual(model.selectedBytes, 100)
        model.byTier["free"] = [Classified(kind: "volume", name: "unknown", tier: "free", because: "fixture", size: nil, owner: nil, provenance: [])]
        XCTAssertEqual(model.selectedBytes, 0)
    }

    @MainActor func testReclaimEstimateCountsASharedImageLayerOnce() {
        // Two 1 GB images, 800 MB of it the same base layer. Adding the stack
        // sizes promises 2 GB where removing both frees 1.2 GB.
        let model = AppModel(service: StubService())
        model.byTier["repullable"] = [
            Classified(kind: "image", name: "one", tier: "repullable", because: "fixture",
                       size: 1_000_000_000, exclusiveSize: 200_000_000, owner: nil, provenance: []),
            Classified(kind: "image", name: "two", tier: "repullable", because: "fixture",
                       size: 1_000_000_000, exclusiveSize: 200_000_000, owner: nil, provenance: []),
        ]
        model.selectedTiers = ["repullable"]
        XCTAssertEqual(model.selectedBytes, 400_000_000)

        // Where the daemon did not compute the overlap there is one figure,
        // and over-estimating is the honest direction to be wrong in.
        model.byTier["repullable"] = [
            Classified(kind: "image", name: "one", tier: "repullable", because: "fixture",
                       size: 1_000_000_000, owner: nil, provenance: []),
        ]
        XCTAssertEqual(model.selectedBytes, 1_000_000_000)
    }

    @MainActor func testImageMetricPrefersTheLayerFigure() {
        let model = AppModel(service: StubService())
        model.totals.imageBytes = 82_500_000_000
        XCTAssertEqual(model.totals.imageDiskBytes, 82_500_000_000)
        model.totals.imageUniqueBytes = 59_300_000_000
        XCTAssertEqual(model.totals.imageDiskBytes, 59_300_000_000)
    }

    func testProtocolProgressAndForwardCompatibility() throws {
        let line = Data(#"{"v":1,"seq":1,"event":"apply_progress","stage":"removing","name":"test","done":1,"total":2}"#.utf8)
        guard case .progress(let stage, let name, let done, let total) = try Envelope.decode(line: line)?.event else { return XCTFail("Missing progress") }
        XCTAssertEqual(stage, "removing"); XCTAssertEqual(name, "test"); XCTAssertEqual(done, 1); XCTAssertEqual(total, 2)
        let future = Data(#"{"v":1,"seq":2,"event":"future_event"}"#.utf8)
        guard case .unknown = try Envelope.decode(line: future)?.event else { return XCTFail("Unknown event rejected") }
        XCTAssertFalse(displayText("A\u{2014}B").contains("\u{2014}"))
    }

    // MARK: - Updates

    @MainActor func testUnconfiguredUpdatesOfferNothingRatherThanADeadButton() {
        // A `swift run` or a bundle built without a signing key has no update
        // mechanism, and the interface must leave the controls out instead of
        // showing a Check button that silently does nothing.
        let status = UpdateStatus()
        XCTAssertFalse(status.isConfigured)
        XCTAssertNil(status.pendingVersion)
        status.checkForUpdates()  // no closure wired: must not crash
        status.setAutomaticChecks(true)
        XCTAssertTrue(status.automaticallyChecks)
    }

    @MainActor func testAPostponedInstallWaitsForTheOperationAndThenRunsOnce() async {
        // The rule the whole design turns on: an update never relaunches the
        // app out from under a running helper.
        let service = StubService()
        let model = AppModel(service: service)
        let status = UpdateStatus()
        model.updates = status

        let installs = Counter()
        status.postpone { installs.value += 1 }

        model.scan()
        XCTAssertTrue(model.busy)
        XCTAssertEqual(installs.value, 0, "an install must not run during an operation")

        service.event?(.scanFinished(totals: Totals(), durationMs: 1, stale: false))
        service.finish?(nil)
        await settle()
        XCTAssertEqual(installs.value, 1)

        // And it is released exactly once — a second completion must not try
        // to install again.
        model.command("Vault", ["--vault"])
        service.finish?(nil)
        await settle()
        XCTAssertEqual(installs.value, 1)
    }

    @MainActor func testAFailedOperationStillReleasesAPostponedInstall() async {
        // The helper has stopped either way. Holding the install because a
        // scan failed would strand the update for the rest of the session.
        let service = StubService()
        let model = AppModel(service: service)
        let status = UpdateStatus()
        model.updates = status
        let installs = Counter()
        status.postpone { installs.value += 1 }

        model.scan()
        service.finish?(ServiceError.helperMissing)
        await settle()
        XCTAssertEqual(installs.value, 1)
        if case .failed = model.state {} else { XCTFail("expected a failed state") }
    }

    @MainActor func testAHeldUpdateAppearsInTheWindowAndIsClearedWhenShown() {
        _ = NSApplication.shared
        let status = UpdateStatus()
        let checks = Counter()
        status.mechanismStarted(
            automatic: true, lastCheck: nil,
            onCheck: { checks.value += 1; status.reminderRetired() },
            onSetAutomatic: { _ in })
        XCTAssertTrue(status.isConfigured)

        status.hold(version: "0.2.0")
        XCTAssertEqual(status.pendingVersion, "0.2.0")
        status.checkForUpdates()
        XCTAssertEqual(checks.value, 1)
        XCTAssertNil(status.pendingVersion, "presenting the update retires the reminder")
    }

    @MainActor func testRenderScreens() throws {
        guard let directory = ProcessInfo.processInfo.environment["PJ_SCREENSHOTS"] else { throw XCTSkip("Set PJ_SCREENSHOTS to render app screens") }
        _ = NSApplication.shared
        let model = AppModel(service: StubService())
        model.context = "orbstack"; model.runtime = "OrbStack"; model.state = .ready
        model.status = "Scan complete"; model.needsScan = false
        model.totals.images = 24; model.totals.imageBytes = 18_400_000_000
        model.totals.volumes = 12; model.totals.volumeBytes = 3_200_000_000
        model.totals.buildCacheRecords = 81; model.totals.buildCacheBytes = 2_700_000_000
        model.byTier = ["orphan": [Classified(kind: "volume", name: "old-project_postgres_data", tier: "orphan", because: "Project directory is missing across two scans.", size: 1_400_000_000, owner: "old-project", provenance: ["Compose label: old-project"])], "repullable": [Classified(kind: "image", name: "postgres:16", tier: "repullable", because: "Available from its registry.", size: 430_000_000, owner: nil, provenance: [])], "protected": [Classified(kind: "volume", name: "workspace_mysql", tier: "protected", because: "Used by a running container.", size: 800_000_000, owner: "workspace", provenance: [])]]
        // Exercise a three-digit sidebar count, which previously wrapped Resources.
        model.byTier["protected", default: []] += (1...300).map { index in
            Classified(kind: "container", name: "service-\(index)", tier: "protected", because: "In use", size: nil, owner: nil, provenance: [])
        }
        try FileManager.default.createDirectory(atPath: directory, withIntermediateDirectories: true)
        for screen in Destination.allCases {
            let view = NSHostingView(rootView: RootView(model: model, settings: Settings(), initialDestination: screen))
            let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 1180, height: 760), styleMask: [.titled], backing: .buffered, defer: false)
            window.contentView = view
            view.frame = NSRect(x: 0, y: 0, width: 1180, height: 760)
            view.layoutSubtreeIfNeeded()
            RunLoop.main.run(until: Date().addingTimeInterval(0.15))
            let bitmap = try XCTUnwrap(view.bitmapImageRepForCachingDisplay(in: view.bounds))
            view.cacheDisplay(in: view.bounds, to: bitmap)
            let data = try XCTUnwrap(bitmap.representation(using: .png, properties: [:]))
            try data.write(to: URL(fileURLWithPath: directory).appendingPathComponent("\(screen.rawValue).png"))
            window.orderOut(nil)
        }
    }
}
