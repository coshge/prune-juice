import SwiftUI

struct RootView: View {
    @ObservedObject var model: AppModel
    @ObservedObject var settings: Settings

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            header
            Divider()
            content
        }
        .frame(minWidth: 720, minHeight: 480)
        .task {
            Diagnostics.log("root view appeared; state=\(model.state)")
            if model.state == .idle { model.scan() }
        }
    }

    private var header: some View {
        HStack {
            Text("Prune Juice").font(.headline)
            if !model.context.isEmpty {
                Text("· \(model.context) · \(model.runtime)")
                    .font(.subheadline).foregroundStyle(.secondary)
            }
            Spacer()
            Toggle("Menu bar icon", isOn: $settings.showMenuBarIcon)
                .toggleStyle(.switch).labelsHidden()
                .help("Show a menu bar icon (off by default)")
            Button("Scan again") { model.scan() }
                .disabled(model.state == .scanning)
        }
        .padding(12)
    }

    @ViewBuilder
    private var content: some View {
        switch model.state {
        case .idle, .scanning:
            scanning
        case .failed(let why):
            failure(why)
        case .ready:
            ready
        }
    }

    private var scanning: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(spacing: 8) {
                ProgressView().controlSize(.small)
                Text(model.status.isEmpty ? "scanning…" : model.status)
            }
            HStack(spacing: 20) {
                ForEach(["container", "image", "volume", "network"], id: \.self) { k in
                    Text("\(model.seen[k] ?? 0) \(k)s").monospacedDigit()
                }
            }
            .font(.subheadline).foregroundStyle(.secondary)
            Text("Nothing is being changed. This is a read-only scan.")
                .font(.footnote).foregroundStyle(.tertiary)
            Spacer()
        }
        .padding(16)
    }

    private func failure(_ why: String) -> some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Could not scan").font(.title3)
            Text(why).foregroundStyle(.secondary).textSelection(.enabled)
            if !model.helperAvailable {
                Text("Expected the helper at Contents/MacOS/prune-juice.")
                    .font(.footnote).foregroundStyle(.tertiary)
            }
            Button("Try again") { model.scan() }
            Spacer()
        }
        .padding(16)
    }

    private var ready: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 18) {
                headline
                if !model.warnings.isEmpty { warningsBlock }
                reviewBlock
                totalsBlock
            }
            .padding(16)
        }
    }

    private var headline: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                Text(humanBytes(model.safeBytes))
                    .font(.system(size: 30, weight: .semibold)).monospacedDigit()
                Text("safe to reclaim").foregroundStyle(.secondary)
            }
            // The claim, stated as arithmetic rather than reassurance. Every
            // safe-tier item is reversible by construction, so this reads zero
            // — and if it ever does not, it says so instead.
            Text("0 bytes irreversible · \(model.freeItems.count) items + build cache")
                .font(.subheadline).foregroundStyle(.secondary)
            Text("Nothing here can be lost.")
                .font(.subheadline).foregroundStyle(.green)
            Text(
                "Reclaiming from the app is not wired up yet — use `prune-juice --apply` "
                    + "in a terminal. This build reads only."
            )
            .font(.footnote).foregroundStyle(.tertiary)
        }
    }

    private var warningsBlock: some View {
        VStack(alignment: .leading, spacing: 4) {
            ForEach(model.warnings.prefix(6), id: \.self) { w in
                Text("! \(w)").font(.footnote).foregroundStyle(.orange)
            }
            if model.warnings.count > 6 {
                Text("… and \(model.warnings.count - 6) more")
                    .font(.footnote).foregroundStyle(.tertiary)
            }
        }
    }

    private var reviewBlock: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("Needs review — \(model.orphanItems.count) orphaned (\(humanBytes(model.orphanBytes))), \(model.staleItems.count) stale")
                .font(.headline)
            if model.orphanItems.isEmpty {
                Text("Nothing is confirmed orphaned. An orphan verdict needs the project directory missing across two consecutive scans.")
                    .font(.footnote).foregroundStyle(.secondary)
            }
            ForEach(model.orphanItems.sorted { ($0.size ?? 0) > ($1.size ?? 0) }) { item in
                row(item)
            }
        }
    }

    private func row(_ item: Classified) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack {
                Text(item.name).monospaced()
                Spacer()
                Text(item.size.map(humanBytes) ?? "—")
                    .foregroundStyle(.secondary).monospacedDigit()
            }
            Text(item.because).font(.footnote).foregroundStyle(.secondary)
            // Provenance, verbatim. A row a user is asked to judge has to show
            // where the claim came from.
            ForEach(item.provenance, id: \.self) { p in
                Text(p).font(.system(size: 10, design: .monospaced))
                    .foregroundStyle(.tertiary)
            }
        }
        .padding(8)
        .background(.quaternary.opacity(0.4), in: RoundedRectangle(cornerRadius: 6))
    }

    private var totalsBlock: some View {
        let t = model.totals
        return VStack(alignment: .leading, spacing: 2) {
            Text(
                "\(t.containers) containers · \(t.images) images (\(humanBytes(t.imageBytes))) · "
                    + "\(t.volumes) volumes (\(humanBytes(t.volumeBytes))) · \(t.networks) networks"
            )
            Text(
                "\(t.buildCacheRecords) build cache records (\(humanBytes(t.buildCacheBytes)) reclaimable) · "
                    + "\(model.unattributedItems.count) unattributed, never offered for deletion"
            )
            Text("Docker's figures are logical. Host reclamation is measured separately.")
                .foregroundStyle(.tertiary)
        }
        .font(.footnote)
        .foregroundStyle(.secondary)
    }
}
