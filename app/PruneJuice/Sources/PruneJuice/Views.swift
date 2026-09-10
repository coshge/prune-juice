import AppKit
import SwiftUI

private let plum = Color(red: 0.55, green: 0.39, blue: 0.78)

enum Destination: String, CaseIterable, Identifiable {
    case resources = "Resources", reclaim = "Reclaim", vault = "Vault", waivers = "Waivers", activity = "Activity", settings = "Settings"
    var id: String { rawValue }
    var symbol: String {
        switch self {
        case .resources: "square.stack.3d.up"
        case .reclaim: "sparkles"
        case .vault: "externaldrive.badge.checkmark"
        case .waivers: "hand.raised"
        case .activity: "waveform.path"
        case .settings: "slider.horizontal.3"
        }
    }
}

private struct PendingAction: Identifiable {
    let id = UUID()
    let title: String
    let detail: String
    let action: () -> Void
}

struct RootView: View {
    @ObservedObject var model: AppModel
    @ObservedObject var settings: Settings
    @ObservedObject var updates: UpdateStatus
    @State private var destination: Destination = .resources
    @State private var search = ""
    @State private var tier = "all"
    @State private var selected: String?
    @State private var pending: PendingAction?
    @State private var volume = ""
    @State private var entry = ""
    @State private var vaultReason = ""
    @State private var selector = ""
    @State private var waiverReason = ""

    init(
        model: AppModel, settings: Settings, updates: UpdateStatus = UpdateStatus(),
        initialDestination: Destination = .resources
    ) {
        self.model = model
        self.settings = settings
        self.updates = updates
        _destination = State(initialValue: initialDestination)
    }

    var body: some View {
        HStack(spacing: 0) {
            sidebar
            Divider()
            VStack(spacing: 0) {
                toolbar
                Divider()
                if let version = updates.pendingVersion {
                    // Sparkle found this and agreed not to interrupt. It sits
                    // in the window until the user chooses to look at it —
                    // and installing still waits for any running operation.
                    HStack(alignment: .center, spacing: 10) {
                        Image(systemName: "arrow.down.circle.fill").foregroundStyle(plum)
                        Text("Prune Juice \(version) is available.")
                        Text("Includes the command-line helper.").foregroundStyle(.secondary)
                        Spacer()
                        Button("Show update") { updates.checkForUpdates() }
                    }
                    .font(.callout).padding(.horizontal, 26).padding(.vertical, 12)
                    .background(plum.opacity(0.08))
                }
                if case .failed(let reason) = model.state {
                    HStack(alignment: .top, spacing: 10) {
                        Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(.orange)
                        Text(reason).textSelection(.enabled)
                        Spacer()
                    }
                    .font(.callout).padding(16).background(.orange.opacity(0.08))
                }
                page.frame(maxWidth: .infinity, maxHeight: .infinity)
                Divider()
                statusBar
            }
            .background(Color(nsColor: .windowBackgroundColor))
        }
        .tint(plum)
        .frame(minWidth: 980, minHeight: 660)
        .sheet(item: $pending) { action in
            VStack(alignment: .leading, spacing: 20) {
                Image(systemName: "exclamationmark.shield").font(.system(size: 32)).foregroundStyle(plum)
                Text(action.title).font(.title2.bold())
                Text(action.detail).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                HStack {
                    Spacer()
                    Button("Cancel") { pending = nil }.keyboardShortcut(.cancelAction)
                    Button(action.title) { pending = nil; action.action() }
                        .buttonStyle(.borderedProminent).disabled(model.busy)
                }
            }.padding(28).frame(width: 470)
        }
    }

    private var sidebar: some View {
        VStack(alignment: .leading, spacing: 28) {
            HStack(spacing: 10) {
                Image(systemName: "drop.halffull").font(.system(size: 25, weight: .medium)).foregroundStyle(plum)
                Text("Prune Juice").font(.system(size: 16, weight: .semibold))
            }.padding(.horizontal, 12).padding(.top, 18)
            VStack(spacing: 5) {
                ForEach(Destination.allCases) { item in
                    Button {
                        destination = item
                    } label: {
                        HStack(spacing: 11) {
                            Image(systemName: item.symbol).frame(width: 20)
                            Text(item.rawValue).lineLimit(1).fixedSize(horizontal: true, vertical: false)
                            Spacer()
                            if item == .resources, !model.items.isEmpty {
                                Text("\(model.items.count)").font(.caption).monospacedDigit().lineLimit(1).fixedSize().foregroundStyle(.secondary)
                            }
                        }
                        .font(.system(size: 13, weight: destination == item ? .semibold : .regular))
                        .padding(.horizontal, 12).padding(.vertical, 10)
                        .background(destination == item ? plum.opacity(0.15) : .clear, in: RoundedRectangle(cornerRadius: 8))
                        .contentShape(Rectangle())
                    }.buttonStyle(.plain)
                }
            }
            Spacer()
            VStack(alignment: .leading, spacing: 8) {
                Label(model.context.isEmpty ? "Docker" : model.context, systemImage: "circle.inset.filled")
                    .font(.caption.weight(.medium)).lineLimit(2)
                Text(model.runtime.isEmpty ? "Local resource intelligence" : displayText(model.runtime))
                    .font(.caption).foregroundStyle(.secondary)
                if let date = model.lastScan {
                    Text("Scanned \(date.formatted(date: .omitted, time: .shortened))")
                        .font(.caption2).foregroundStyle(.tertiary)
                }
            }.padding(12)
        }
        .padding(12).frame(width: 220).frame(maxHeight: .infinity)
        .background(.regularMaterial)
    }

    private var toolbar: some View {
        HStack {
            VStack(alignment: .leading, spacing: 4) {
                Text(destination.rawValue).font(.system(size: 21, weight: .semibold))
                Text(subtitle).font(.callout).foregroundStyle(.secondary)
            }
            Spacer()
            Button { selected = nil; model.scan() } label: {
                Label("Scan again", systemImage: "arrow.clockwise")
            }.disabled(model.busy).keyboardShortcut("r", modifiers: .command)
        }.padding(.horizontal, 26).padding(.vertical, 20)
    }

    private var subtitle: String {
        switch destination {
        case .resources: "Know what is here. See why it can stay or go."
        case .reclaim: "Choose the recovery costs you are comfortable with."
        case .vault: "Preserved volumes, ready when you need them."
        case .waivers: "Keep specific resources out of cleanup."
        case .activity: "Progress, results, and messages from the engine."
        case .settings: "Control how Prune Juice explores your Docker environment."
        }
    }

    @ViewBuilder private var page: some View {
        switch destination {
        case .resources: resources
        case .reclaim: reclaim
        case .vault: vault
        case .waivers: waivers
        case .activity: activity
        case .settings: preferences
        }
    }

    private var filtered: [Classified] {
        model.items.filter { item in
            (tier == "all" || item.tier == tier) && (search.isEmpty ||
                "\(item.name) \(item.kind) \(item.owner ?? "") \(item.context)".localizedCaseInsensitiveContains(search))
        }
    }

    private var resources: some View {
        VStack(spacing: 0) {
            HStack(spacing: 26) {
                // The layer figure, not the sum of stack sizes: fifteen images
                // on one base layer occupy that base once, not fifteen times.
                metric("Images", value: humanBytes(model.totals.imageDiskBytes), count: "\(model.totals.images) images")
                Divider().frame(height: 44)
                metric("Volumes", value: humanBytes(model.totals.volumeBytes), count: "\(model.totals.volumes) volumes")
                Divider().frame(height: 44)
                metric("Build cache", value: humanBytes(model.totals.buildCacheBytes), count: "\(model.totals.buildCacheRecords) records")
                Spacer()
            }.padding(26)
            HStack {
                Image(systemName: "magnifyingglass").foregroundStyle(.secondary)
                TextField("Search name, project, kind, or context", text: $search).textFieldStyle(.plain)
                Picker("Tier", selection: $tier) {
                    Text("All resources").tag("all")
                    ForEach(Array(model.byTier.keys).sorted(), id: \.self) { key in
                        Text(tierTitle(key)).tag(key)
                    }
                }.labelsHidden().frame(width: 170)
            }.padding(12).background(.quaternary.opacity(0.4), in: RoundedRectangle(cornerRadius: 10))
                .padding(.horizontal, 26).padding(.bottom, 16)
            Divider()
            HSplitView {
                if filtered.isEmpty {
                    empty(model.busy ? "Exploring Docker" : "No matching resources", symbol: model.busy ? "viewfinder" : "square.stack.3d.up", detail: model.busy ? model.status : "Scan Docker or adjust your search and tier filter.")
                } else {
                    List(selection: $selected) {
                        ForEach(filtered) { item in
                            HStack(spacing: 12) {
                                Image(systemName: symbol(item.kind)).foregroundStyle(plum).frame(width: 24)
                                VStack(alignment: .leading, spacing: 5) {
                                    Text(item.name).font(.system(size: 13, weight: .medium)).lineLimit(1).truncationMode(.middle)
                                    Text("\(item.owner ?? "No project attributed") · \(item.kind)").font(.caption).foregroundStyle(.secondary).lineLimit(1)
                                }
                                Spacer(minLength: 8)
                                VStack(alignment: .trailing, spacing: 5) {
                                    Text(item.reclaimable.map(humanBytes) ?? "Unknown size").monospacedDigit().font(.callout)
                                    Text(tierTitle(item.tier)).font(.caption).foregroundStyle(item.tier == "free" ? .green : .secondary)
                                }
                            }.padding(.vertical, 7).tag(item.id)
                        }
                    }.listStyle(.inset).frame(minWidth: 320)
                }
                if let item = model.items.first(where: { $0.id == selected }) {
                    inspector(item).frame(minWidth: 235, idealWidth: 275, maxWidth: 340)
                }
            }
            HStack {
                Text("\(filtered.count) resources · \(model.totals.containers) containers · \(model.totals.networks) networks")
                Spacer()
                Text("Docker logical sizes").help("Image totals may count shared layers more than once. Host reclamation is measured separately after cleanup.")
            }.font(.caption).foregroundStyle(.secondary).padding(14)
        }
    }

    private func inspector(_ item: Classified) -> some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 20) {
                HStack { Image(systemName: symbol(item.kind)).font(.title).foregroundStyle(plum); Spacer(); Button { selected = nil } label: { Image(systemName: "xmark") }.buttonStyle(.plain).help("Close details") }
                Text(item.name).font(.headline).textSelection(.enabled)
                Label(tierTitle(item.tier), systemImage: "shield.lefthalf.filled").foregroundStyle(plum)
                detail("Why", displayText(item.because))
                detail("Project", item.owner ?? "No project attributed")
                detail("Context", item.context)
                detail("Evidence", item.provenance.isEmpty ? "No additional provenance recorded." : displayText(item.provenance.joined(separator: "\n\n")))
                Divider()
                Button("Create waiver") { selector = "\(item.kind):\(item.name)"; destination = .waivers }
                if item.kind == "volume" {
                    Button("Preserve in vault") { volume = item.name; destination = .vault }
                }
            }.padding(20)
        }
    }

    private var reclaim: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 22) {
                HStack(alignment: .center, spacing: 24) {
                    Image(systemName: "externaldrive.badge.checkmark").font(.system(size: 46, weight: .ultraLight)).foregroundStyle(plum)
                    VStack(alignment: .leading, spacing: 5) {
                        Text(humanBytes(model.selectedBytes)).font(.system(size: 38, weight: .light, design: .rounded)).monospacedDigit()
                        Text("Estimated reclaimable from selected tiers").foregroundStyle(.secondary)
                    }
                    Spacer()
                }.padding(.vertical, 10)
                Text("Cleanup applies to every eligible resource in your selected tiers. Docker checks each resource again before removal.")
                    .font(.callout).foregroundStyle(.secondary)
                VStack(spacing: 0) {
                    ForEach(ReclaimTier.all) { item in
                        Toggle(isOn: Binding(get: { model.selectedTiers.contains(item.id) }, set: { enabled in
                            if enabled { model.selectedTiers.insert(item.id) } else { model.selectedTiers.remove(item.id) }
                        })) {
                            HStack(alignment: .top, spacing: 14) {
                                Image(systemName: item.symbol).font(.title3).foregroundStyle(plum).frame(width: 26)
                                VStack(alignment: .leading, spacing: 5) {
                                    HStack {
                                        Text(item.title).font(.headline)
                                        Spacer()
                                        Text(resourceCount((model.byTier[item.id] ?? []).count)).font(.caption).foregroundStyle(.secondary)
                                    }
                                    Text(item.detail).font(.callout).foregroundStyle(.secondary)
                                }
                            }
                        }.toggleStyle(.checkbox).padding(18)
                        if item.id != "stale" { Divider().padding(.leading, 18) }
                    }
                }.background(.background, in: RoundedRectangle(cornerRadius: 14))
                    .overlay(RoundedRectangle(cornerRadius: 14).strokeBorder(.quaternary))
                    .disabled(model.busy)
                detail("Scope", model.scope + (model.options.label.isEmpty ? "" : "\nLabel: \(model.options.label)"))
                if !model.options.vault {
                    Label("Vault preservation is off. Irreversible resources will be refused.", systemImage: "exclamationmark.shield").foregroundStyle(.orange)
                }
                if !model.options.sizes {
                    Text("Sizing is off, so build cache is excluded from cleanup.").foregroundStyle(.orange)
                }
                if !model.canReclaim && !model.busy {
                    Text("Scan with your current settings before reclaiming, and select at least one tier.").font(.callout).foregroundStyle(.secondary)
                }
            }.padding(28)
        }
        .safeAreaInset(edge: .bottom, spacing: 0) {
            VStack(spacing: 0) {
                Divider()
                HStack {
                    Spacer()
                    Button("Preview cleanup") {
                        model.command("Cleanup preview", model.options.arguments + ["--tiers", model.selectedTiers.sorted().joined(separator: ",")])
                        destination = .activity
                    }.disabled(!model.canReclaim)
                    Button("Review cleanup") {
                        let costs = ReclaimTier.all.filter { model.selectedTiers.contains($0.id) }.map { "\($0.title): \($0.detail)" }.joined(separator: "\n\n")
                        pending = PendingAction(title: "Reclaim selected tiers", detail: "Scope: \(model.scope).\n\n\(costs)\n\nThis applies to entire tiers, including newly eligible resources found during the fresh scan.\(model.options.vault ? "" : " Vault preservation is off; irreversible resources will be refused.")") {
                            model.reclaim(); destination = .activity
                        }
                    }.buttonStyle(.borderedProminent).controlSize(.large).disabled(!model.canReclaim)
                }
                .padding(16)
            }.background(.bar)
        }
    }

    private var vault: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 24) {
                Label("Copies are stored on the host, outside Docker.", systemImage: "lock.shield").foregroundStyle(.secondary)
                HStack {
                    Button("List preserved copies") { model.command("Vault list", ["--vault"]); destination = .activity }
                    Button("Verify all copies") { model.command("Vault verification", ["--vault-verify"]); destination = .activity }
                }
                formSection("Preserve a volume", detail: "Creates and verifies a copy without deleting the volume. The CLI uses its first discovered local Docker context.") {
                    TextField("Volume name", text: $volume)
                    Button("Preserve volume") {
                        model.command("Preserve volume", ["--vault-dump", volume]); destination = .activity
                    }.disabled(volume.trimmingCharacters(in: .whitespaces).isEmpty)
                }
                formSection("Restore a copy", detail: "Use an entry ID from the vault list. Restores into the first discovered local Docker context and keeps the preserved copy.") {
                    TextField("Vault entry ID", text: $entry)
                    Button("Restore volume") {
                        pending = PendingAction(title: "Restore volume", detail: "Recreate the volume from copy \(entry). The vault copy is kept. The CLI will refuse an unsafe restore.") {
                            model.command("Restore volume", ["--vault-restore", entry], changesResources: true); destination = .activity
                        }
                    }.disabled(entry.isEmpty)
                }
                formSection("Delete a preserved copy", detail: "Permanently removes the entry above. If it is the last copy, its contents cannot be recovered.") {
                    TextField("Reason, at least 12 characters", text: $vaultReason)
                    Button("Delete preserved copy", role: .destructive) {
                        pending = PendingAction(title: "Delete preserved copy", detail: "Permanently delete \(entry)? This cannot be undone.\n\nReason: \(vaultReason)") {
                            model.command("Delete preserved copy", ["--vault-forget", entry, "--reason", vaultReason]); destination = .activity
                        }
                    }.disabled(entry.isEmpty || vaultReason.trimmingCharacters(in: .whitespacesAndNewlines).count < 12)
                }
            }.padding(28).disabled(model.busy)
        }
    }

    private var waivers: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 24) {
                Text("Waivers protect matching resources across cleanup runs. Each one records your reason for keeping it.").foregroundStyle(.secondary)
                Button("List waivers") { model.command("Waivers", ["--waivers"]); destination = .activity }
                formSection("Keep a resource", detail: "Enter a selector such as volume:project_mysql. A blanket * selector is refused.") {
                    TextField("Resource selector", text: $selector)
                    TextField("Reason, at least 12 characters", text: $waiverReason)
                    Button("Save waiver") {
                        model.command("Save waiver", ["--waive", selector, "--reason", waiverReason], changesResources: true); destination = .activity
                    }.disabled(selector.isEmpty || selector == "*" || waiverReason.trimmingCharacters(in: .whitespacesAndNewlines).count < 12)
                }
                formSection("Remove a waiver", detail: "The selector above will be eligible for classification again on the next scan.") {
                    Button("Remove waiver") {
                        pending = PendingAction(title: "Remove waiver", detail: "Stop holding back \(selector)? The resource will be evaluated normally on your next scan.") {
                            model.command("Remove waiver", ["--unwaive", selector], changesResources: true); destination = .activity
                        }
                    }.disabled(selector.isEmpty)
                }
            }.padding(28).disabled(model.busy)
        }
    }

    private var activity: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 20) {
                if model.busy {
                    HStack(spacing: 10) {
                        ProgressView().controlSize(.small)
                        Text(model.status).font(.callout).foregroundStyle(.secondary)
                        Spacer()
                    }
                }
                if let docker = model.dockerReclaimed {
                    HStack(spacing: 40) {
                        metric("Docker reported", value: humanBytes(docker), count: "Logical bytes reclaimed")
                        metric("Host measured", value: model.hostReclaimed.map(humanBytes) ?? "Unavailable", count: "Physical disk space returned")
                    }
                }
                if !model.warnings.isEmpty {
                    DisclosureGroup("\(model.warnings.count) scan notices") {
                        VStack(alignment: .leading, spacing: 10) {
                            ForEach(Array(model.warnings.enumerated()), id: \.offset) { _, warning in
                                Label(warning, systemImage: "exclamationmark.triangle").textSelection(.enabled)
                            }
                        }.font(.callout).padding(.top, 12)
                    }
                }
                HStack {
                    Text(model.operation.isEmpty ? "Activity" : model.operation).font(.headline)
                    Spacer()
                    Button("Copy output") {
                        NSPasteboard.general.clearContents()
                        NSPasteboard.general.setString(model.output, forType: .string)
                    }.disabled(model.output.isEmpty)
                }
                Text(model.output.isEmpty ? (model.busy ? model.status : "Run a scan, cleanup, or vault action to see its output here.") : model.output)
                    .font(.system(size: 12, design: .monospaced)).textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading).padding(18)
                    .background(.background, in: RoundedRectangle(cornerRadius: 10))
            }.padding(28)
        }
    }

    private var preferences: some View {
        Form {
            Section("Scan scope") {
                TextField("Docker context", text: $model.options.context, prompt: Text("All discovered contexts"))
                TextField("Project roots", text: $model.options.roots, prompt: Text("Default project folders"))
                Text("Separate absolute folder paths with a colon.").font(.caption).foregroundStyle(.secondary)
                TextField("Only label", text: $model.options.label, prompt: Text("key=value"))
                Text("A label filter excludes build cache because cache records have no labels.").font(.caption).foregroundStyle(.secondary)
            }
            Section("Inspection") {
                TextField("Deadline in seconds", text: $model.options.deadline)
                Text("Use 0 to wait indefinitely. Default: 120 seconds.").font(.caption).foregroundStyle(.secondary)
                Toggle("Measure volume sizes and build cache", isOn: $model.options.sizes)
                Toggle("Inspect volume contents", isOn: $model.options.probe)
                Text("Without inspection, no volume can be proven safe.").font(.caption).foregroundStyle(.secondary)
                Toggle("Always inspect through a container", isOn: $model.options.containerProbe)
            }
            Section("Preservation") {
                Toggle("Preserve irreversible resources in the vault", isOn: $model.options.vault)
                Text("If disabled, irreversible resources are refused. They are never deleted without a copy.").font(.caption).foregroundStyle(.secondary)
            }
            Section("App") {
                Toggle("Show menu bar icon", isOn: $settings.showMenuBarIcon)
                Button("Show CLI help") { model.command("CLI help", ["--help"]); destination = .activity }
            }
            if updates.isConfigured {
                Section("Updates") {
                    Toggle(
                        "Check for updates automatically",
                        isOn: Binding(
                            get: { updates.automaticallyChecks },
                            set: { updates.setAutomaticChecks($0) })
                    )
                    Text("Checks in the background at most once a day, never during a scan or cleanup. Updates replace the whole app, including its command-line helper, so the two stay the same version.")
                        .font(.caption).foregroundStyle(.secondary)
                    HStack {
                        Button("Check now") { updates.checkForUpdates() }
                        if let last = updates.lastCheck {
                            Text("Last checked \(last.formatted(date: .abbreviated, time: .shortened))")
                                .font(.caption).foregroundStyle(.tertiary)
                        }
                    }
                }
            }
            if let error = model.options.validation { Text(error).foregroundStyle(.orange) }
            Button("Apply settings and scan") { selected = nil; model.scan(); destination = .resources }
                .disabled(model.options.validation != nil)
        }.formStyle(.grouped).disabled(model.busy)
    }

    private var statusBar: some View {
        HStack(spacing: 10) {
            if model.busy { ProgressView().controlSize(.mini) }
            else { Image(systemName: model.needsScan ? "circle.dashed" : "checkmark.circle").foregroundStyle(plum) }
            Text(model.status).lineLimit(1).truncationMode(.middle)
            Spacer()
            if let progress = model.progress, model.busy { ProgressView(value: progress).frame(width: 90) }
            if !model.warnings.isEmpty {
                Button { destination = .activity } label: { Label("\(model.warnings.count)", systemImage: "exclamationmark.triangle") }.buttonStyle(.plain)
            }
            if model.needsScan, !model.busy { Text("Scan to refresh resources").foregroundStyle(.secondary) }
        }.font(.caption).padding(.horizontal, 20).padding(.vertical, 11)
    }

    private func metric(_ title: String, value: String, count: String) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(title).font(.callout).foregroundStyle(.secondary)
            Text(value).font(.system(size: 24, weight: .medium, design: .rounded)).monospacedDigit()
            Text(count).font(.caption).foregroundStyle(.tertiary)
        }
    }
    private func detail(_ title: String, _ value: String) -> some View {
        VStack(alignment: .leading, spacing: 7) {
            Text(title).font(.subheadline.weight(.semibold))
            Text(value).font(.callout).foregroundStyle(.secondary).textSelection(.enabled)
        }
    }
    private func formSection<Content: View>(_ title: String, detail: String, @ViewBuilder content: () -> Content) -> some View {
        VStack(alignment: .leading, spacing: 13) {
            Text(title).font(.headline)
            Text(detail).font(.callout).foregroundStyle(.secondary)
            content().textFieldStyle(.roundedBorder)
        }.padding(20).frame(maxWidth: .infinity, alignment: .leading)
            .background(.background, in: RoundedRectangle(cornerRadius: 12))
    }
    private func empty(_ title: String, symbol: String, detail: String) -> some View {
        VStack(spacing: 14) {
            Image(systemName: symbol).font(.system(size: 38, weight: .ultraLight)).foregroundStyle(plum)
            Text(title).font(.title3.weight(.medium))
            Text(detail).foregroundStyle(.secondary).multilineTextAlignment(.center)
        }.padding(30).frame(maxWidth: .infinity, maxHeight: .infinity)
    }
    private func resourceCount(_ count: Int) -> String { "\(count) resource\(count == 1 ? "" : "s")" }
    private func tierTitle(_ tier: String) -> String { ReclaimTier.all.first { $0.id == tier }?.title ?? tier.capitalized }
    private func symbol(_ kind: String) -> String {
        switch kind { case "volume": "externaldrive"; case "image": "square.stack.3d.up"; case "network": "point.3.connected.trianglepath.dotted"; default: "shippingbox" }
    }
}
