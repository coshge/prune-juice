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

final class AppTests: XCTestCase {
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

    func testProtocolProgressAndForwardCompatibility() throws {
        let line = Data(#"{"v":1,"seq":1,"event":"apply_progress","stage":"removing","name":"test","done":1,"total":2}"#.utf8)
        guard case .progress(let stage, let name, let done, let total) = try Envelope.decode(line: line)?.event else { return XCTFail("Missing progress") }
        XCTAssertEqual(stage, "removing"); XCTAssertEqual(name, "test"); XCTAssertEqual(done, 1); XCTAssertEqual(total, 2)
        let future = Data(#"{"v":1,"seq":2,"event":"future_event"}"#.utf8)
        guard case .unknown = try Envelope.decode(line: future)?.event else { return XCTFail("Unknown event rejected") }
        XCTAssertFalse(displayText("A\u{2014}B").contains("\u{2014}"))
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
