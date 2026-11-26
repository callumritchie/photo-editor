import AppKit
import SwiftUI

/// Editing session for one photo: working settings, the committed develop state, and a live preview.
@MainActor
final class DevelopModel: ObservableObject {
    let item: ImageItem
    let core: Core
    private let onCommitted: (Int64) -> Void

    @Published var settings = DevelopSettings()
    @Published private(set) var state: DevelopState?
    @Published private(set) var image: NSImage?
    @Published private(set) var loading = true
    @Published private(set) var lastRenderMs: Int?
    /// Crop tool active: render the whole frame and show the crop overlay.
    // Development aid for UI snapshots: PE_START_CROP=1 opens the crop tool.
    @Published var cropping = ProcessInfo.processInfo.environment["PE_START_CROP"] != nil {
        didSet { if cropping != oldValue { requestRender() } }
    }
    @Published var cropAspect = CropAspect.presets[0]

    // Masking: the selected mask (and component) is edited on the uncropped frame with an overlay.
    @Published var selectedMask: Int? { didSet { if selectedMask != oldValue { requestRender() } } }
    @Published var activeComponent = 0
    @Published var showOverlay = true { didSet { requestRender() } }
    @Published private(set) var maskOverlay: NSImage?
    @Published var brushSize = 0.03
    @Published var brushFeather = 0.5
    @Published var brushFlow = 1.0
    @Published var brushErase = false
    @Published var pickingColor = false
    @Published var busy: String?
    var maskEditing: Bool { selectedMask != nil }

    // 1:1 zoom: centre in fractions of the full-resolution output; viewport in device pixels.
    @Published var zoomed = false
    @Published var zoomCenter = CGPoint(x: 0.5, y: 0.5)
    @Published var zoomImage: NSImage?
    @Published private(set) var fullSize = CGSize.zero
    var viewport = CGSize(width: 1600, height: 1000)
    private var zoomRendering = false
    private var zoomPending = false

    /// Render the visible 1:1 rectangle, coalescing bursts (drags, slider moves).
    func requestZoomRender() {
        guard zoomed, let preview else { return }
        if zoomRendering { zoomPending = true; return }
        zoomRendering = true
        let s = settings
        let vp = viewport
        let centre = zoomCenter
        Task {
            let (size, img, rect) = await Task.detached(priority: .userInitiated) { () -> (CGSize, NSImage?, CGRect) in
                let full = preview.outputSize(s)
                let w = min(vp.width.rounded(), full.width), h = min(vp.height.rounded(), full.height)
                let x = min(max(centre.x * full.width - w / 2, 0), full.width - w).rounded()
                let y = min(max(centre.y * full.height - h / 2, 0), full.height - h).rounded()
                let r = CGRect(x: x, y: y, width: w, height: h)
                return (full, preview.renderRegion(s, r), r)
            }.value
            fullSize = size
            // Keep the centre where the rectangle actually is (after clamping at the edges).
            if size.width > 0, size.height > 0, !zoomPending {
                zoomCenter = CGPoint(x: rect.midX / size.width, y: rect.midY / size.height)
            }
            if let img { zoomImage = img }
            zoomRendering = false
            if zoomPending { zoomPending = false; requestZoomRender() }
        }
    }
    @Published var error: String?

    private(set) var preview: PreviewImage?
    private var rendering = false
    private var pending = false

    var asShotTemperature: Double { preview?.asShotTemperature ?? 5500 }
    var asShotTint: Double { preview?.asShotTint ?? 0 }
    /// Width / height of the corrected (oriented, uncropped) frame.
    var frameAspect: Double {
        guard let p = preview else { return 1.5 }
        let swap = (5...8).contains(item.orientation) && settings.value[["applyOrientation"][...]] != .bool(false)
        return swap ? Double(p.height) / Double(p.width) : Double(p.width) / Double(p.height)
    }

    /// Straighten: set the angle and re-fit the crop to the largest rectangle inside the rotated frame.
    func setAngle(_ angle: Double) {
        settings.cropAngle = angle
        let k = straightenFit(aspect: frameAspect, angle: angle)
        let m = (1 - k) / 2
        settings.crop = CropRect(left: m, top: m, right: 1 - m, bottom: 1 - m)
        requestRender()
    }

    func applyAspect(_ a: CropAspect) {
        cropAspect = a
        guard let r = a.ratio else { return }
        settings.crop = fitAspect(settings.crop, aspect: r == 0 ? frameAspect : r, frameAspect: frameAspect)
        commit(label: "Crop")
    }

    /// Raw (scene-referred) rather than an already-rendered JPEG.
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
        let path = item.path
        preview = await Task.detached(priority: .userInitiated) { PreviewImage(path: path, longEdge: 2048) }.value
        preview?.attachRasters(core: core, imageId: item.id)
        loading = false
        // Development aids for UI snapshots: PE_SELECT_MASK=<index>, PE_ADD_MASK=subject|sky|radial|...
        let env = ProcessInfo.processInfo.environment
        if let i = Int(env["PE_SELECT_MASK"] ?? ""), settings.masks.indices.contains(i) { selectedMask = i }
        // PE_START_ZOOM="x,y" (fractions of the photo): open zoomed to 1:1 there.
        if let z = env["PE_START_ZOOM"]?.split(separator: ",").compactMap({ Double($0) }), z.count == 2 {
            setZoom(true, at: CGPoint(x: z[0], y: z[1]))
        }
        if let k = env["PE_ADD_MASK"], let kind = MaskKind.allCases.first(where: { $0.rawValue.lowercased().contains(k) }) {
            addMask(kind)
        }
        requestRender()
    }

    /// Re-render with the current working settings, coalescing bursts of slider changes.
    func requestRender() {
        guard let preview else { return }
        if rendering { pending = true; return }
        rendering = true
        var s = settings
        if cropping { s.crop = .full }
        let overlayIndex = (maskEditing && showOverlay) ? selectedMask : nil
        Task {
            let t0 = Date()
            let (img, overlay) = await Task.detached(priority: .userInitiated) {
                (preview.render(s), overlayIndex.flatMap { preview.maskOverlay(s, index: $0) })
            }.value
            lastRenderMs = Int(Date().timeIntervalSince(t0) * 1000)
            if let img { image = img }
            maskOverlay = overlay
            if zoomed { requestZoomRender() }
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
    // Development aid for UI snapshots: PE_PANEL_SECTIONS="Color Mixer,Color Grading".
    @State private var expanded: Set<String> = Set(
        ProcessInfo.processInfo.environment["PE_PANEL_SECTIONS"]?.split(separator: ",").map(String.init)
            ?? ["Tone", "Presence"])
    // PE_PANEL_TAB="Geometry=Lens" opens a tab (snapshots).
    @State private var tabs: [String: String] = {
        guard let v = ProcessInfo.processInfo.environment["PE_PANEL_TAB"], let i = v.firstIndex(of: "=") else { return [:] }
        return [String(v[..<i]): String(v[v.index(after: i)...])]
    }()

    private let schema = DevelopSchema.shared

    /// Section names in schema order.
    private var sectionNames: [String] {
        var seen: [String] = []
        for p in schema.params where !seen.contains(p.section) { seen.append(p.section) }
        return seen
    }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 14) {
                header
                section("White Balance") { whiteBalance }
                collapsible("Masks") { MaskPanel(model: model) }
                ForEach(sectionNames, id: \.self) { name in
                    let params = schema.params.filter { $0.section == name }
                    if params.allSatisfy({ $0.when == nil || $0.when == model.settings.treatment }) {
                        collapsible(name) { paramSection(name, params) }
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

    @ViewBuilder private func cropControls() -> some View {
        HStack {
            Toggle(isOn: $model.cropping) { Label("Crop", systemImage: "crop") }
                .toggleStyle(.button)
                .help("Show the crop overlay (R)")
            Picker("Aspect", selection: Binding(get: { model.cropAspect }, set: { model.applyAspect($0) })) {
                ForEach(CropAspect.presets) { Text($0.label).tag($0) }
            }
            .labelsHidden()
            .fixedSize()
            Spacer()
            Button("Reset") {
                model.settings.crop = .full
                model.settings.cropAngle = 0
                model.requestRender()
                model.commit(label: "Reset Crop")
            }
            .disabled(model.settings.crop == .full && model.settings.cropAngle == 0)
        }
        .controlSize(.small)
    }

    private func choicePicker(_ c: ChoiceSpec) -> some View {
        Picker(c.label, selection: Binding(
            get: { model.settings.string(c.key) ?? c.options.first?.first ?? "" },
            set: { v in model.settings.setString(c.key, v); model.requestRender(); model.commit() })) {
            ForEach(c.options, id: \.self) { o in Text(o.last ?? "").tag(o.first ?? "") }
        }
        .pickerStyle(.segmented)
    }

    @ViewBuilder private func paramSection(_ name: String, _ params: [ParamSpec]) -> some View {
        if name == "Geometry" { cropControls() }
        ForEach(schema.choices.filter { $0.section == name && $0.subsection == nil }, id: \.key) { choicePicker($0) }
        let subs = params.compactMap(\.subsection).reduce(into: [String]()) { if !$0.contains($1) { $0.append($1) } }
        if !subs.isEmpty {
            let current = tabs[name] ?? subs[0]
            Picker(name, selection: Binding(get: { current }, set: { tabs[name] = $0 })) {
                ForEach(subs, id: \.self) { Text($0).tag($0) }
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            if current == "Lens", let info = model.preview?.lensProfile() {
                Text(info.profile.map { "Profile: \($0)" }
                     ?? info.lens.map { "No profile for \($0)" } ?? "Lens not recorded")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(2)
                    .help("Lens profiles come from the Lensfun database (CC BY-SA 3.0)")
            }
            ForEach(schema.choices.filter { $0.section == name && $0.subsection == current }, id: \.key) { choicePicker($0) }
            ForEach(params.filter { $0.subsection == current }, id: \.key) { p in
                ParamSlider(spec: p, value: binding(p), onCommit: { model.commit() })
            }
        }
        ForEach(params.filter { $0.subsection == nil }, id: \.key) { p in
            ParamSlider(spec: p, value: binding(p), onCommit: { model.commit() })
        }
    }

    private func collapsible<C: View>(_ title: String, @ViewBuilder content: @escaping () -> C) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            DisclosureGroup(isExpanded: Binding(
                get: { expanded.contains(title) },
                set: { if $0 { expanded.insert(title) } else { expanded.remove(title) } })) {
                VStack(alignment: .leading, spacing: 8) { content() }.padding(.top, 6)
            } label: {
                Text(title).font(.headline)
            }
            Divider()
        }
    }

    private func section<C: View>(_ title: String, @ViewBuilder content: () -> C) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(title).font(.headline)
            content()
            Divider()
        }
    }

    private func binding(_ p: ParamSpec) -> Binding<Double> {
        if p.key == "crop.angle" {
            return Binding(get: { model.settings.cropAngle }, set: { model.setAngle($0) })
        }
        return Binding(get: { model.settings.number(p.key) ?? p.default },
                       set: { model.settings.setNumber(p.key, $0); model.requestRender() })
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
        if spec.key == "temperature" && spec.group == "whiteBalance" { return "\(Int(value.rounded())) K" }
        if spec.hue { return "\(Int(value.rounded()))°" }
        return String(format: spec.min >= 0 ? "%.\(spec.precision)f" : "%+.\(spec.precision)f", value)
    }

    var body: some View {
        VStack(spacing: 2) {
            HStack {
                Text(spec.label)
                    .onTapGesture(count: 2) { value = spec.default; onCommit() }
                    .help("Double-click to reset")
                Spacer()
                if spec.hue {
                    Circle().fill(Color(hue: value / 360, saturation: 0.75, brightness: 0.9)).frame(width: 10, height: 10)
                }
                Text(formatted).monospacedDigit().foregroundStyle(.secondary)
            }
            .font(.callout)
            // Rounded in the binding: a stepped Slider draws one tick mark per step.
            Slider(value: Binding(get: { value }, set: { value = ($0 / spec.step).rounded() * spec.step }),
                   in: spec.min...spec.max) { editing in
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
