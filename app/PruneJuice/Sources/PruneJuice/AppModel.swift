import Foundation

struct ScanOptions: Equatable {
    var context = ""
    var roots = ""
    var label = ""
    var deadline = "120"
    var sizes = true
    var probe = true
    var containerProbe = false
    var vault = true

    var validation: String? {
        if UInt64(deadline) == nil { return "Enter a whole number of seconds for the deadline." }
        if !label.isEmpty && (!label.contains("=") || label.hasPrefix("=")) {
            return "Use key=value for the label filter."
        }
        return nil
    }

    var arguments: [String] {
        var args = ["--deadline", deadline]
        for (flag, value) in [("--context", context), ("--roots", roots), ("--only-label", label)] {
            if !value.isEmpty { args += [flag, value] }
        }
        if !sizes { args.append("--no-sizes") }
        if !probe { args.append("--no-probe") }
        if containerProbe { args.append("--container-probe") }
        if !vault { args.append("--no-vault") }
        return args
    }
}

struct ReclaimTier: Identifiable {
    let id: String
    let title: String
    let symbol: String
    let detail: String
    static let all: [Self] = [
        .init(id: "free", title: "Safe", symbol: "checkmark.shield", detail: "Reclaims safe resources and eligible build cache. No irreplaceable data is offered."),
        .init(id: "repullable", title: "Pull again", symbol: "arrow.down.circle", detail: "Images must be downloaded again on next use. This costs bandwidth."),
        .init(id: "rebuildable", title: "Build again", symbol: "hammer", detail: "Images must be rebuilt. This costs time, and an old build may no longer reproduce."),
        .init(id: "orphan", title: "Orphaned", symbol: "folder.badge.questionmark", detail: "The owning project is missing. Irreversible volumes are preserved and verified before removal."),
        .init(id: "stale", title: "Dormant", symbol: "moon", detail: "The project still exists but is dormant. Irreversible volumes are preserved and verified before removal.")
    ]
}

@MainActor
final class AppModel: ObservableObject {
    enum State: Equatable { case idle, scanning, ready, failed(String) }
    @Published var state: State = .idle
    @Published var status = "Ready to scan"
    @Published var context = ""
    @Published var runtime = ""
    @Published var totals = Totals()
    @Published var seen: [String: UInt32] = [:]
    @Published var warnings: [String] = []
    @Published var byTier: [String: [Classified]] = [:]
    @Published var options = ScanOptions()
    @Published var selectedTiers: Set<String> = ["free"]
    @Published var output = ""
    @Published var operation = ""
    @Published var needsScan = true
    @Published var dockerReclaimed: UInt64?
    @Published var hostReclaimed: UInt64?
    @Published var progress: Double?
    @Published var lastScan: Date?
    private var loggedPhase: String?
    private var receivedScanReport = false
    private var hostMeasurementIncomplete = false
    private var scanOptions: ScanOptions?
    private var currentContext = ""
    private var contextTotals: [String: Totals] = [:]
    private let service: PruneJuiceService
    let entitlements: Entitlements
    /// Set by the composition root. Told when an operation ends so an install
    /// the updater postponed can go ahead — the app never installs over a
    /// running scan, cleanup, or vault operation.
    weak var updates: UpdateStatus?

    init(service: PruneJuiceService = SubprocessService(), entitlements: Entitlements = AlwaysEntitled()) {
        self.service = service
        self.entitlements = entitlements
    }

    var busy: Bool { state == .scanning }
    var items: [Classified] { byTier.values.flatMap { $0 }.sorted { ($0.reclaimable ?? 0) > ($1.reclaimable ?? 0) } }
    var safeBytes: UInt64 { (byTier["free"] ?? []).compactMap(\.reclaimable).reduce(0, +) + (options.label.isEmpty ? totals.buildCacheBytes : 0) }
    var selectedBytes: UInt64 {
        let resourceBytes = selectedTiers.reduce(UInt64(0)) { total, tier in
            total + (byTier[tier] ?? []).compactMap(\.reclaimable).reduce(0, +)
        }
        let cacheBytes = selectedTiers.contains("free") && options.label.isEmpty && options.sizes
            ? totals.buildCacheBytes : 0
        return resourceBytes + cacheBytes
    }
    var canReclaim: Bool { !busy && !needsScan && scanOptions == options && !selectedTiers.isEmpty }
    var scope: String { options.context.isEmpty ? "All discovered Docker contexts" : options.context }

    func scan() {
        guard !busy else { return }
        guard options.validation == nil else { state = .failed(options.validation!); return }
        byTier = [:]; seen = [:]; totals = Totals(); contextTotals = [:]; warnings = []
        needsScan = true
        receivedScanReport = false
        selectedTiers = ["free"]
        scanOptions = options
        run("Scan", arguments: options.arguments, isScan: true)
    }

    func reclaim() {
        guard canReclaim else { return }
        needsScan = true
        dockerReclaimed = nil; hostReclaimed = nil; hostMeasurementIncomplete = false
        run("Reclaim", arguments: options.arguments + ["--apply", "--tiers", selectedTiers.sorted().joined(separator: ",")])
    }

    func command(_ title: String, _ arguments: [String], changesResources: Bool = false) {
        guard !busy else { return }
        if changesResources { needsScan = true }
        run(title, arguments: arguments)
    }

    private func run(_ title: String, arguments: [String], isScan: Bool = false) {
        operation = title; status = "\(title) in progress"; state = .scanning; output = ""; progress = nil
        loggedPhase = nil
        dockerReclaimed = nil; hostReclaimed = nil; hostMeasurementIncomplete = false
        appendOutput("\(title) started.\n")
        if isScan { appendOutput("Scope: \(scope). Nothing will be deleted.\n") }
        // A single serial callback queue preserves event order, including completion.
        let delivery = DispatchQueue(label: "dev.prunejuice.delivery")
        service.run(arguments: arguments, onOutput: { [weak self] text in
            delivery.async { DispatchQueue.main.async { self?.appendOutput(text) } }
        }, onEvent: { [weak self] event in
            delivery.async { DispatchQueue.main.async { self?.apply(event, collecting: isScan) } }
        }, onFinish: { [weak self] error in
            delivery.async { DispatchQueue.main.async {
                guard let self else { return }
                self.progress = nil
                // Before anything else about the outcome: whatever happened,
                // the helper is no longer running.
                self.updates?.hostBecameIdle()
                if let error {
                    self.state = .failed(displayText(error.localizedDescription))
                    self.status = "\(title) needs attention"
                    self.appendOutput("\(title) failed: \(error.localizedDescription)\n")
                } else if isScan && !self.receivedScanReport {
                    self.state = .failed("The helper returned no scan report. Check the Docker context and scan again.")
                    self.status = "No scan report received"
                    self.appendOutput("No scan report received. Check the Docker context and scan again.\n")
                } else {
                    self.state = .ready
                    self.status = "\(title) complete"
                    if isScan {
                        self.needsScan = false; self.lastScan = Date()
                        self.appendOutput("\nScan complete. \(self.items.count) resources classified across \(self.contextTotals.count) Docker context(s).\n")
                        self.appendOutput("Safe tier: \(humanBytes(self.safeBytes)). Review resource details before cleanup.\n")
                        if !self.warnings.isEmpty { self.appendOutput("\(self.warnings.count) notice(s). Review the scan notices above; some results may be incomplete.\n") }
                        self.appendOutput("Nothing was deleted.\n")
                    } else { self.appendOutput("\(title) complete.\n") }
                }
            } }
        })
    }

    private func appendOutput(_ text: String) {
        output += displayText(text)
        if output.count > 200_000 { output = String(output.suffix(200_000)) }
    }

    private func apply(_ event: PJEvent, collecting: Bool) {
        switch event {
        case .scanStarted(_, let ctx, let rt, _):
            currentContext = ctx; context = ctx; runtime = rt
            loggedPhase = nil
            appendOutput("Inspecting \(ctx) (\(rt)).\n")
        case .phase(let phase, let done, let total):
            status = "\(phase.capitalized) in \(currentContext)"
            let phaseKey = "\(currentContext):\(phase)"
            if loggedPhase != phaseKey {
                loggedPhase = phaseKey
                appendOutput(status + "…\n")
            }
            progress = total.flatMap { $0 > 0 ? Double(done) / Double($0) : nil }
        case .resourceFound(let kind):
            if collecting { seen[kind, default: 0] += 1 }
        case .warning(_, let message):
            let text = displayText(message)
            if !warnings.contains(text) { warnings.append(text); appendOutput("Notice: \(text)\n") }
        case .scanFinished(let t, let durationMs, let stale):
            appendOutput("Inventory for \(currentContext.isEmpty ? "Docker" : currentContext): \(t.containers) containers, \(t.images) images, \(t.volumes) volumes, \(t.networks) networks (\(String(format: "%.1f", Double(durationMs) / 1000)) s).\n")
            if collecting {
                receivedScanReport = true
                contextTotals[currentContext] = t
                totals = contextTotals.values.reduce(Totals()) { a, b in
                    var t = a
                    t.containers += b.containers; t.images += b.images; t.volumes += b.volumes
                    t.networks += b.networks; t.buildCacheRecords += b.buildCacheRecords
                    t.imageBytes += b.imageBytes; t.volumeBytes += b.volumeBytes; t.buildCacheBytes += b.buildCacheBytes
                    return t
                }
                // Layers are not shared between engines, so these add — but
                // only if every engine reported one. A single daemon that did
                // not compute the overlap makes the total unknown, not smaller.
                let unique = contextTotals.values.map(\.imageUniqueBytes)
                totals.imageUniqueBytes = unique.contains(where: { $0 == nil })
                    ? nil : unique.compactMap { $0 }.reduce(0, +)
            }
            if stale {
                let message = "The scan is incomplete. Sizes are lower bounds; unverified resources remain protected."
                if !warnings.contains(message) { warnings.append(message); appendOutput("Notice: \(message)\n") }
            }
        case .classified(var item):
            if collecting {
                item.context = currentContext
                byTier[item.tier, default: []].append(item)
            }
        case .progress(let stage, let name, let done, let total):
            let text = "\(stage.replacingOccurrences(of: "_", with: " ").capitalized)\(name.map { ": \($0)" } ?? "")"
            status = displayText(text)
            progress = total > 0 ? Double(done) / Double(total) : nil
            if !collecting { appendOutput(text + "\n") }
        case .hostReclaim(let docker, let host, let confidence):
            dockerReclaimed = (dockerReclaimed ?? 0) + docker
            if let host, !hostMeasurementIncomplete { hostReclaimed = (hostReclaimed ?? 0) + host }
            if host == nil { hostMeasurementIncomplete = true; hostReclaimed = nil }
            appendOutput("Docker reported: \(humanBytes(docker)). Host measured: \(host.map(humanBytes) ?? "Could not be measured"). \(confidence)\n")
        case .unknown: break
        }
    }
}
