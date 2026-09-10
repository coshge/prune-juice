import Foundation

/// Everything the interface reads. Mutated only on the main actor.
@MainActor
final class AppModel: ObservableObject {
    enum State: Equatable {
        case idle
        case scanning
        case ready
        case failed(String)
    }

    @Published var state: State = .idle
    @Published var status: String = ""
    @Published var context: String = ""
    @Published var runtime: String = ""

    @Published var totals = Totals()
    @Published var seen: [String: UInt32] = [:]
    @Published var warnings: [String] = []

    /// Everything classified, keyed by tier.
    @Published var byTier: [String: [Classified]] = [:]

    private let service: PruneJuiceService
    let entitlements: Entitlements

    init(service: PruneJuiceService = SubprocessService(),
         entitlements: Entitlements = AlwaysEntitled()) {
        self.service = service
        self.entitlements = entitlements
    }

    var helperAvailable: Bool { SubprocessService.helperAvailable }

    var freeItems: [Classified] { byTier["free"] ?? [] }
    var orphanItems: [Classified] { byTier["orphan"] ?? [] }
    var staleItems: [Classified] { byTier["stale"] ?? [] }
    var unattributedItems: [Classified] { byTier["unattributed"] ?? [] }

    /// What the safe tier would reclaim, including the build cache.
    ///
    /// The build cache is aggregate — it never appears as a classified item —
    /// so it has to be added from the totals rather than summed from rows.
    var safeBytes: UInt64 {
        freeItems.compactMap(\.size).reduce(0, +) + totals.buildCacheBytes
    }

    var orphanBytes: UInt64 { orphanItems.compactMap(\.size).reduce(0, +) }

    func scan() {
        guard state != .scanning else { return }
        state = .scanning
        status = "starting…"
        byTier = [:]
        seen = [:]
        warnings = []

        service.scan(
            onEvent: { event in
                Task { @MainActor [weak self] in self?.apply(event) }
            },
            onFinish: { error in
                Task { @MainActor [weak self] in
                    guard let self else { return }
                    if let error {
                        self.state = .failed(error.localizedDescription)
                    } else {
                        self.state = .ready
                    }
                }
            })
    }

    private func apply(_ event: PJEvent) {
        switch event {
        case .scanStarted(_, let ctx, let rt, _):
            context = ctx
            runtime = rt
            status = "scanning \(ctx)…"

        case .phase(let phase, _, _):
            // Naming the slow phase stops the pause reading as a hang.
            status =
                switch phase {
                case "sizing": "measuring volume sizes (the slow part)…"
                case "probing": "reading volume contents…"
                case "listing": "listing resources…"
                case "attributing": "working out who owns what…"
                default: "\(phase)…"
                }

        case .resourceFound(let kind):
            seen[kind, default: 0] += 1

        case .warning(_, let message):
            if !warnings.contains(message) { warnings.append(message) }

        case .scanFinished(let t, _, let stale):
            totals = t
            if stale {
                let m = "sizes are incomplete — the figures are a floor"
                if !warnings.contains(m) { warnings.append(m) }
            }
            status = ""

        case .classified(let c):
            byTier[c.tier, default: []].append(c)

        case .hostReclaim, .unknown:
            // Unknown events are ignored by design: adding one is not a
            // breaking change, so a newer helper must not break an older app.
            break
        }
    }
}
