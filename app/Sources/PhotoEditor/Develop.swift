import AppKit
import SwiftUI

/// Editing session for one photo: working settings, the committed develop state, and a live preview.
@MainActor
final class DevelopModel: ObservableObject {
    let item: ImageItem
    private let core: Core
    private let onCommitted: (Int64) -> Void

    @Published var settings = DevelopSettings()
    @Published private(set) var state: DevelopState?
    @Published private(set) var image: NSImage?
    @Published private(set) var loading = true
    @Published private(set) var lastRenderMs: Int?
    @Published var error: String?

    private var preview: PreviewImage?
    private var rendering = false
    private var pending = false

    var asShotTemperature: Double { preview?.asShotTemperature ?? 5500 }
    var asShotTint: Double { preview?.asShotTint ?? 0 }
    var isRaw: Bool { !["jpg", "jpeg"].contains((item.path as NSString).pathExtension.lowercased()) }

    init(item: ImageItem, core: Core, onCommitted: @escaping (Int64) -> Void) {
        self.item = item
        self.core = core
        self.onCommitted = onCommitted
        do {
            let st = try core.developState(item.id)
            state = st
            settings = st.settings
        } catch { self.error = error.localizedDescription }
    }

    /// Decode the photo once for interactive editing, then render.
    func load() async {
        guard isRaw else { loading = false; return }
        let path = item.path
        preview = await Task.detached(priority: .userInitiated) { PreviewImage(path: path, longEdge: 2048) }.value
        loading = false
        requestRender()
    }

    /// Re-render with the current working settings, coalescing bursts of slider changes.
    func requestRender() {
        guard let preview else { return }
        if rendering { pending = true; return }
        rendering = true
        let s = settings
        Task {
            let t0 = Date()
            let img = await Task.detached(priority: .userInitiated) { preview.render(s) }.value
            lastRenderMs = Int(Date().timeIntervalSince(t0) * 1000)
            if let img { image = img }
            rendering = false
            if pending { pending = false; requestRender() }
        }
    }

    /// Commit the working settings as one history step (end of a gesture).
    func commit(label: String? = nil) {
        var cmd: [String: Any] = ["command": "set", "settings": settings.jsonObject]
        if let label { cmd["label"] = label }
        run(cmd)
    }

    func run(_ command: [String: Any]) {
        do {
            if let st = try core.develop([item.id], command).first {
                state = st
                if st.settings != settings { settings = st.settings }
                requestRender()
                onCommitted(item.id)
            }
        } catch { self.error = error.localizedDescription }
    }

    func undo() { run(["command": "undo"]) }
    func redo() { run(["command": "redo"]) }
    func reset() { run(["command": "reset"]) }
    func goTo(_ step: Int64) { run(["command": "goToStep", "stepId": step]) }
    func snapshot(named name: String) { run(["command": "createSnapshot", "name": name]) }
    func applySnapshot(_ id: Int64) { run(["command": "applySnapshot", "snapshotId": id]) }
    func deleteSnapshot(_ id: Int64) { run(["command": "deleteSnapshot", "snapshotId": id]) }
    func applyPreset(_ id: Int64) { run(["command": "applyPreset", "presetId": id]) }

    func setWhiteBalanceMode(_ mode: String) {
        switch mode {
        case "custom":
            settings.whiteBalance = WhiteBalance(mode: "custom",
                                                 temperature: settings.whiteBalance.temperature ?? asShotTemperature.rounded(),
                                                 tint: settings.whiteBalance.tint ?? asShotTint.rounded())
        default:
            settings.whiteBalance = WhiteBalance(mode: mode)
        }
        requestRender()
        commit()
    }
}

// MARK: - Panel

struct DevelopPanel: View {
    @EnvironmentObject var app: AppState
    @ObservedObject var model: DevelopModel
    @State private var showSavePreset = false

    private let schema = DevelopSchema.shared

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 14) {
                header
                if !model.isRaw {
                    Text("Editing JPEG files is not supported yet; adjustments apply to raw files.")
                        .font(.caption).foregroundStyle(.secondary)
                }
                section("White Balance") { whiteBalance }
                section("Tone") {
                    ForEach(schema.params, id: \.key) { p in
                        ParamSlider(spec: p, value: binding(p.key), onCommit: { model.commit() })
                    }
                }
                section("Presets") { presets }
                section("Snapshots") { snapshots }
                section("History") { history }
            }
            .padding(12)
        }
        .frame(width: 300)
        .background(Color(nsColor: .windowBackgroundColor))
        .sheet(isPresented: $showSavePreset) { SavePresetSheet(settings: model.settings) }
    }

    private var header: some View {
        HStack {
            Button { model.undo() } label: { Image(systemName: "arrow.uturn.backward") }
                .disabled(!(model.state?.canUndo ?? false)).help("Undo (⌘Z)")
            Button { model.redo() } label: { Image(systemName: "arrow.uturn.forward") }
                .disabled(!(model.state?.canRedo ?? false)).help("Redo (⇧⌘Z)")
            Button("Reset") { model.reset() }.disabled(!(model.state?.edited ?? false))
            Spacer()
            Menu("Settings") {
                Button("Copy Settings") { app.copiedSettings = model.settings }
                Button("Paste Settings") { app.pasteSettings() }.disabled(app.copiedSettings == nil)
                Button("Sync to Selected Photos") { app.syncSettings(from: model.settings) }
                    .disabled(app.selection.count < 2)
            }
            .fixedSize()
        }
    }

    @ViewBuilder private var whiteBalance: some View {
        Picker("Mode", selection: Binding(
            get: { model.settings.whiteBalance.mode },
            set: { model.setWhiteBalanceMode($0) })) {
            Text("As Shot").tag("asShot")
            Text("Auto").tag("auto")
            Text("Custom").tag("custom")
        }
        .pickerStyle(.segmented)
        .labelsHidden()
        ParamSlider(spec: schema.temperature, value: wbBinding(\.temperature, fallback: model.asShotTemperature),
                    onCommit: { model.commit() })
        ParamSlider(spec: schema.tint, value: wbBinding(\.tint, fallback: model.asShotTint),
                    onCommit: { model.commit() })
    }

    @ViewBuilder private var presets: some View {
        if app.presets.isEmpty {
            Text("No presets yet").font(.caption).foregroundStyle(.secondary)
        }
        ForEach(app.presets) { p in
            Button { model.applyPreset(p.id) } label: {
                HStack { Text(p.name); Spacer(); Text("\(p.groups.count) groups").font(.caption).foregroundStyle(.secondary) }
            }
            .buttonStyle(.plain)
            .contextMenu { Button("Delete Preset", role: .destructive) { app.deletePreset(p.id) } }
        }
        Button("Save Preset…") { showSavePreset = true }.controlSize(.small)
    }

    @ViewBuilder private var snapshots: some View {
        ForEach(model.state?.snapshots ?? []) { s in
            Button(s.name) { model.applySnapshot(s.id) }
                .buttonStyle(.plain)
                .contextMenu { Button("Delete Snapshot", role: .destructive) { model.deleteSnapshot(s.id) } }
        }
        Button("Create Snapshot") { model.snapshot(named: "Snapshot \((model.state?.snapshots.count ?? 0) + 1)") }
            .controlSize(.small)
    }

    @ViewBuilder private var history: some View {
        let steps = (model.state?.history ?? []).reversed()
        if steps.isEmpty { Text("No edits yet").font(.caption).foregroundStyle(.secondary) }
        ForEach(Array(steps)) { step in
            Button { model.goTo(step.id) } label: {
                HStack {
                    Text(step.label).lineLimit(1)
                    Spacer()
                    if step.id == model.state?.currentStep { Image(systemName: "checkmark").font(.caption) }
                }
                .foregroundStyle(step.id == model.state?.currentStep ? Color.accentColor : .primary)
            }
            .buttonStyle(.plain)
        }
    }

    private func section<C: View>(_ title: String, @ViewBuilder content: () -> C) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(title).font(.headline)
            content()
            Divider()
        }
    }

    private func binding(_ key: String) -> Binding<Double> {
        Binding(get: { model.settings[key] }, set: { model.settings[key] = $0; model.requestRender() })
    }

    private func wbBinding(_ field: WritableKeyPath<WhiteBalance, Double?>, fallback: Double) -> Binding<Double> {
        Binding(
            get: { model.settings.whiteBalance[keyPath: field] ?? fallback },
            set: { v in
                if model.settings.whiteBalance.mode != "custom" {
                    model.settings.whiteBalance = WhiteBalance(mode: "custom",
                                                               temperature: model.asShotTemperature.rounded(),
                                                               tint: model.asShotTint.rounded())
                }
                model.settings.whiteBalance[keyPath: field] = v
                model.requestRender()
            })
    }
}

/// Labelled slider: live updates while dragging, one commit when released; double-click label to reset.
struct ParamSlider: View {
    let spec: ParamSpec
    @Binding var value: Double
    let onCommit: () -> Void

    private var formatted: String {
        spec.key == "temperature" ? "\(Int(value.rounded())) K" : String(format: "%+.\(spec.precision)f", value)
    }

    var body: some View {
        VStack(spacing: 2) {
            HStack {
                Text(spec.label)
                    .onTapGesture(count: 2) { value = spec.default; onCommit() }
                    .help("Double-click to reset")
                Spacer()
                Text(formatted).monospacedDigit().foregroundStyle(.secondary)
            }
            .font(.callout)
            Slider(value: $value, in: spec.min...spec.max, step: spec.step) { editing in
                if !editing { onCommit() }
            }
            .controlSize(.small)
        }
    }
}

struct SavePresetSheet: View {
    @EnvironmentObject var app: AppState
    @Environment(\.dismiss) private var dismiss
    let settings: DevelopSettings
    @State private var name = ""
    @State private var groups: Set<String> = Set(DevelopSchema.shared.groups.filter(\.look).map(\.id))

    var body: some View {
        Form {
            TextField("Preset name", text: $name)
            Section("Include") {
                ForEach(DevelopSchema.shared.groups, id: \.id) { g in
                    Toggle(g.label, isOn: Binding(
                        get: { groups.contains(g.id) },
                        set: { if $0 { groups.insert(g.id) } else { groups.remove(g.id) } }))
                }
            }
        }
        .formStyle(.grouped)
        .frame(width: 340)
        .toolbar {
            ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
            ToolbarItem(placement: .confirmationAction) {
                Button("Save") {
                    let ordered = DevelopSchema.shared.groups.map(\.id).filter { groups.contains($0) }
                    app.savePreset(name: name, groups: ordered, settings: settings)
                    dismiss()
                }
                .disabled(name.trimmingCharacters(in: .whitespaces).isEmpty || groups.isEmpty)
            }
        }
    }
}
