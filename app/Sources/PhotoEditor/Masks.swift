import SwiftUI

enum MaskKind: String, CaseIterable, Identifiable {
    case linear = "Linear Gradient", radial = "Radial Gradient", brush = "Brush"
    case luminance = "Luminance Range", color = "Colour Range", subject = "Select Subject", sky = "Select Sky"
    var id: String { rawValue }
}

// MARK: - Model actions

extension DevelopModel {
    private func editMasks(_ label: String, commitNow: Bool = true, _ f: (inout [MaskModel]) -> Void) {
        var m = settings.masks
        f(&m)
        settings.masks = m
        requestRender()
        if commitNow { commit(label: label) }
    }

    /// Shape for a new component of `kind` (subject / sky are made asynchronously).
    private func shape(for kind: MaskKind) -> ShapeModel? {
        switch kind {
        case .linear: return .linear(x0: 0.5, y0: 0.05, x1: 0.5, y1: 0.5)
        case .radial: return .radial(cx: 0.5, cy: 0.5, rx: 0.25, ry: 0.25, angle: 0, feather: 0.5)
        case .brush: return .brush(strokes: [])
        case .luminance: return .luminance(lo: 0.6, hi: 1, smoothness: 0.5)
        case .color: return .color(colors: [], tolerance: 40)
        case .subject, .sky: return nil
        }
    }

    /// Add a new mask, or (with `to`) a component of `mode` to an existing mask.
    func addMask(_ kind: MaskKind, to index: Int? = nil, mode: String = "add") {
        if let s = shape(for: kind) {
            insert(ComponentModel(mode: mode, invert: false, shape: s), kind: kind, to: index)
            return
        }
        guard let preview else { return }
        busy = kind == .subject ? "Finding the subject…" : "Finding the sky…"
        let s = settings
        Task {
            let raster = await Task.detached(priority: .userInitiated) {
                kind == .subject ? preview.selectSubject(s) : preview.selectSky()
            }.value
            busy = nil
            guard let raster else {
                error = kind == .subject ? "No subject was found in this photo." : "No sky was found in this photo."
                return
            }
            do {
                let id = try core.saveRaster(imageId: item.id, raster)
                preview.addRaster(id: id, raster)
                insert(ComponentModel(mode: mode, invert: false,
                                      shape: .raster(rasterId: id, label: kind == .subject ? "Subject" : "Sky")),
                       kind: kind, to: index)
            } catch { self.error = error.localizedDescription }
        }
    }

    private func insert(_ c: ComponentModel, kind: MaskKind, to index: Int?) {
        var newIndex = 0
        editMasks(index == nil ? "Add \(kind.rawValue)" : "Add to Mask") { masks in
            if let i = index, masks.indices.contains(i) {
                masks[i].components.append(c)
                newIndex = i
            } else {
                masks.append(MaskModel(name: "Mask \(masks.count + 1)", components: [c]))
                newIndex = masks.count - 1
            }
        }
        selectedMask = newIndex
        activeComponent = settings.masks[newIndex].components.count - 1
        pickingColor = kind == .color
    }

    func deleteMask(_ i: Int) {
        editMasks("Delete Mask") { $0.remove(at: i) }
        selectedMask = nil
    }

    func setMask(_ i: Int, label: String, commitNow: Bool = true, _ f: (inout MaskModel) -> Void) {
        editMasks(label, commitNow: commitNow) { masks in
            guard masks.indices.contains(i) else { return }
            f(&masks[i])
        }
    }

    func removeComponent(_ i: Int, _ k: Int) {
        setMask(i, label: "Remove from Mask") { m in
            m.components.remove(at: k)
            if !m.components.isEmpty { m.components[0].mode = "add" }
        }
        if settings.masks[safe: i]?.components.isEmpty ?? false { deleteMask(i) }
        activeComponent = max(0, min(activeComponent, (settings.masks[safe: i]?.components.count ?? 1) - 1))
    }

    /// Update the active component's shape (drags call this live, then commit at the end).
    func updateShape(commitNow: Bool, _ f: (inout ShapeModel) -> Void) {
        guard let i = selectedMask else { return }
        let k = activeComponent
        setMask(i, label: "Edit Mask", commitNow: commitNow) { m in
            guard m.components.indices.contains(k) else { return }
            f(&m.components[k].shape)
        }
    }

    var activeShape: ShapeModel? {
        guard let i = selectedMask, let m = settings.masks[safe: i] else { return nil }
        return m.components[safe: activeComponent]?.shape
    }

    // Brush painting in frame fractions.
    func beginStroke(_ p: CGPoint) {
        let s = StrokeModel(points: [[p.x, p.y]], size: brushSize, feather: brushFeather, flow: brushFlow, erase: brushErase)
        updateShape(commitNow: false) { shape in
            if case .brush(var strokes) = shape { strokes.append(s); shape = .brush(strokes: strokes) }
        }
    }

    func extendStroke(_ p: CGPoint) {
        updateShape(commitNow: false) { shape in
            if case .brush(var strokes) = shape, !strokes.isEmpty {
                let last = strokes[strokes.count - 1].points.last ?? [p.x, p.y]
                // Skip points closer than a quarter of the brush radius.
                if hypot(last[0] - p.x, last[1] - p.y) > brushSize * 0.25 {
                    strokes[strokes.count - 1].points.append([p.x, p.y])
                    shape = .brush(strokes: strokes)
                }
            }
        }
    }

    func pickColor(at p: CGPoint) {
        guard let preview, let lab = preview.pickColor(settings.withoutGeometry, fx: p.x, fy: p.y) else { return }
        updateShape(commitNow: true) { shape in
            if case .color(var colors, let tol) = shape { colors.append(lab); shape = .color(colors: colors, tolerance: tol) }
        }
    }
}

extension Array {
    subscript(safe i: Int) -> Element? { indices.contains(i) ? self[i] : nil }
}

// MARK: - Panel

struct MaskPanel: View {
    @ObservedObject var model: DevelopModel
    private let schema = DevelopSchema.shared

    var body: some View {
        let masks = model.settings.masks
        VStack(alignment: .leading, spacing: 8) {
            HStack {
                Menu {
                    ForEach(MaskKind.allCases) { k in Button(k.rawValue) { model.addMask(k) } }
                } label: { Label("Add Mask", systemImage: "plus") }
                    .fixedSize()
                Spacer()
                if model.selectedMask != nil {
                    Button("Done") { model.selectedMask = nil; model.pickingColor = false }
                }
            }
            .controlSize(.small)
            if let b = model.busy {
                HStack { ProgressView().controlSize(.small); Text(b).font(.caption) }
            }
            if masks.isEmpty {
                Text("Masks apply adjustments to part of the photo.").font(.caption).foregroundStyle(.secondary)
            }
            ForEach(Array(masks.enumerated()), id: \.element.id) { i, m in
                HStack {
                    Button { model.setMask(i, label: m.enabled ? "Hide Mask" : "Show Mask") { $0.enabled.toggle() } } label: {
                        Image(systemName: m.enabled ? "eye" : "eye.slash")
                    }
                    .buttonStyle(.plain)
                    Image(systemName: m.components.first?.shape.icon ?? "circle")
                    Text(m.name.isEmpty ? "Mask \(i + 1)" : m.name)
                    Spacer()
                    Button { model.deleteMask(i) } label: { Image(systemName: "trash") }.buttonStyle(.plain)
                }
                .padding(4)
                .background(RoundedRectangle(cornerRadius: 5).fill(model.selectedMask == i ? Color.accentColor.opacity(0.25) : .clear))
                .contentShape(Rectangle())
                .onTapGesture { model.selectedMask = model.selectedMask == i ? nil : i; model.activeComponent = 0 }
            }
            if let i = model.selectedMask, let m = masks[safe: i] {
                Divider()
                selected(i, m)
            }
        }
    }

    @ViewBuilder private func selected(_ i: Int, _ m: MaskModel) -> some View {
        Toggle("Show overlay", isOn: $model.showOverlay).controlSize(.small)
        ParamSlider(
            spec: ParamSpec(key: "amount", label: "Amount", group: "masks", min: 0, max: 100, step: 1, default: 100, precision: 0),
            value: Binding(get: { m.amount }, set: { v in model.setMask(i, label: "Mask Amount", commitNow: false) { $0.amount = v } }),
            onCommit: { model.commit(label: "Mask Amount") })
        Text("Components").font(.subheadline.bold())
        ForEach(Array(m.components.enumerated()), id: \.offset) { k, c in
            HStack(spacing: 6) {
                Image(systemName: c.shape.icon)
                Text(c.shape.title).lineLimit(1)
                Spacer()
                if k > 0 {
                    Picker("", selection: Binding(get: { c.mode }, set: { v in model.setMask(i, label: "Mask Mode") { $0.components[k].mode = v } })) {
                        Text("Add").tag("add")
                        Text("Subtract").tag("subtract")
                        Text("Intersect").tag("intersect")
                    }
                    .labelsHidden().fixedSize()
                }
                Toggle("Invert", isOn: Binding(get: { c.invert }, set: { v in model.setMask(i, label: "Invert Mask") { $0.components[k].invert = v } }))
                    .toggleStyle(.button)
                Button { model.removeComponent(i, k) } label: { Image(systemName: "minus.circle") }.buttonStyle(.plain)
            }
            .controlSize(.small)
            .padding(3)
            .background(RoundedRectangle(cornerRadius: 4).stroke(model.activeComponent == k ? Color.accentColor : .clear))
            .contentShape(Rectangle())
            .onTapGesture { model.activeComponent = k }
            if model.activeComponent == k { shapeControls(i, k, c.shape) }
        }
        Menu("Add to Mask") {
            ForEach(["add", "subtract", "intersect"], id: \.self) { mode in
                Menu(mode.capitalized) {
                    ForEach(MaskKind.allCases) { kind in Button(kind.rawValue) { model.addMask(kind, to: i, mode: mode) } }
                }
            }
        }
        .controlSize(.small).fixedSize()
        Divider()
        Text("Adjustments").font(.subheadline.bold())
        ForEach(schema.maskParams, id: \.key) { p in
            ParamSlider(spec: p, value: Binding(
                get: { model.settings.number("masks.\(i).adjust.\(p.key)") ?? 0 },
                set: { model.settings.setNumber("masks.\(i).adjust.\(p.key)", $0); model.requestRender() }),
                onCommit: { model.commit(label: "Mask \(p.label)") })
        }
    }

    @ViewBuilder private func shapeControls(_ i: Int, _ k: Int, _ shape: ShapeModel) -> some View {
        let spec = { (label: String, min: Double, max: Double, step: Double, def: Double, prec: Int) in
            ParamSpec(key: label, label: label, group: "masks", min: min, max: max, step: step, default: def, precision: prec)
        }
        switch shape {
        case .radial(_, _, _, _, _, let feather):
            ParamSlider(spec: spec("Feather", 0, 100, 1, 50, 0), value: Binding(
                get: { feather * 100 },
                set: { v in model.updateShape(commitNow: false) { if case .radial(let a, let b, let c, let d, let e, _) = $0 { $0 = .radial(cx: a, cy: b, rx: c, ry: d, angle: e, feather: v / 100) } } }),
                onCommit: { model.commit(label: "Edit Mask") })
        case .luminance(let lo, let hi, let sm):
            ParamSlider(spec: spec("Lowest", 0, 100, 1, 60, 0), value: Binding(
                get: { lo * 100 }, set: { v in model.updateShape(commitNow: false) { $0 = .luminance(lo: v / 100, hi: hi, smoothness: sm) } }),
                onCommit: { model.commit(label: "Edit Mask") })
            ParamSlider(spec: spec("Highest", 0, 100, 1, 100, 0), value: Binding(
                get: { hi * 100 }, set: { v in model.updateShape(commitNow: false) { $0 = .luminance(lo: lo, hi: v / 100, smoothness: sm) } }),
                onCommit: { model.commit(label: "Edit Mask") })
            ParamSlider(spec: spec("Smoothness", 0, 100, 1, 50, 0), value: Binding(
                get: { sm * 100 }, set: { v in model.updateShape(commitNow: false) { $0 = .luminance(lo: lo, hi: hi, smoothness: v / 100) } }),
                onCommit: { model.commit(label: "Edit Mask") })
        case .color(let colors, let tol):
            HStack {
                Toggle(isOn: $model.pickingColor) { Label("Pick Colour", systemImage: "eyedropper") }.toggleStyle(.button)
                Text("\(colors.count) colour\(colors.count == 1 ? "" : "s")").font(.caption).foregroundStyle(.secondary)
                if !colors.isEmpty {
                    Button("Clear") { model.updateShape(commitNow: true) { $0 = .color(colors: [], tolerance: tol) } }
                }
            }
            .controlSize(.small)
            ParamSlider(spec: spec("Tolerance", 0, 100, 1, 40, 0), value: Binding(
                get: { tol }, set: { v in model.updateShape(commitNow: false) { $0 = .color(colors: colors, tolerance: v) } }),
                onCommit: { model.commit(label: "Edit Mask") })
        case .brush:
            ParamSlider(spec: spec("Size", 0.5, 20, 0.5, 3, 1), value: Binding(get: { model.brushSize * 100 }, set: { model.brushSize = $0 / 100 }), onCommit: {})
            ParamSlider(spec: spec("Feather", 0, 100, 1, 50, 0), value: Binding(get: { model.brushFeather * 100 }, set: { model.brushFeather = $0 / 100 }), onCommit: {})
            ParamSlider(spec: spec("Flow", 1, 100, 1, 100, 0), value: Binding(get: { model.brushFlow * 100 }, set: { model.brushFlow = $0 / 100 }), onCommit: {})
            Toggle("Erase (or hold ⌥)", isOn: $model.brushErase).controlSize(.small)
            Text("Paint on the photo.").font(.caption).foregroundStyle(.secondary)
        case .linear:
            Text("Drag the two points on the photo: full effect at the first, none at the second.")
                .font(.caption).foregroundStyle(.secondary)
        case .raster:
            EmptyView()
        }
    }
}

// MARK: - Canvas

/// Mask overlay and handles, drawn over the uncropped image while a mask is selected.
struct MaskCanvas: View {
    @ObservedObject var model: DevelopModel
    @State private var dragStart: ShapeModel?
    @State private var painting = false

    var body: some View {
        GeometryReader { geo in
            let size = geo.size
            ZStack(alignment: .topLeading) {
                if let overlay = model.maskOverlay {
                    Image(nsImage: overlay).resizable().frame(width: size.width, height: size.height).allowsHitTesting(false)
                }
                interaction(size)
                if let shape = model.activeShape { handles(shape, size) }
            }
        }
    }

    private func frac(_ p: CGPoint, _ size: CGSize) -> CGPoint {
        CGPoint(x: min(max(p.x / size.width, 0), 1), y: min(max(p.y / size.height, 0), 1))
    }

    @ViewBuilder private func interaction(_ size: CGSize) -> some View {
        let shape = model.activeShape
        Rectangle().fill(Color.white.opacity(0.001))
            .gesture(DragGesture(minimumDistance: 0)
                .onChanged { g in
                    let p = frac(g.location, size)
                    if model.pickingColor { return }
                    if case .brush = shape {
                        if !painting {
                            painting = true
                            if NSEvent.modifierFlags.contains(.option) { model.brushErase = true }
                            model.beginStroke(p)
                        } else {
                            model.extendStroke(p)
                        }
                    }
                }
                .onEnded { g in
                    if model.pickingColor { model.pickColor(at: frac(g.location, size)); return }
                    if painting {
                        painting = false
                        model.commit(label: "Brush")
                        if NSEvent.modifierFlags.contains(.option) { model.brushErase = false }
                    }
                })
    }

    @ViewBuilder private func handles(_ shape: ShapeModel, _ size: CGSize) -> some View {
        let pt = { (x: Double, y: Double) in CGPoint(x: x * size.width, y: y * size.height) }
        switch shape {
        case .linear(let x0, let y0, let x1, let y1):
            Path { p in p.move(to: pt(x0, y0)); p.addLine(to: pt(x1, y1)) }
                .stroke(Color.white, style: StrokeStyle(lineWidth: 1, dash: [4, 3]))
                .allowsHitTesting(false)
            handle(pt(x0, y0), filled: true) { d in
                model.updateShape(commitNow: false) { if case .linear(_, _, let a, let b) = $0 { $0 = .linear(x0: d.x, y0: d.y, x1: a, y1: b) } }
            }
            handle(pt(x1, y1), filled: false) { d in
                model.updateShape(commitNow: false) { if case .linear(let a, let b, _, _) = $0 { $0 = .linear(x0: a, y0: b, x1: d.x, y1: d.y) } }
            }
        case .radial(let cx, let cy, let rx, let ry, let angle, let feather):
            Ellipse()
                .stroke(Color.white, lineWidth: 1)
                .frame(width: rx * 2 * size.width, height: ry * 2 * size.height)
                .position(pt(cx, cy))
                .allowsHitTesting(false)
            handle(pt(cx, cy), filled: true) { d in
                model.updateShape(commitNow: false) { $0 = .radial(cx: d.x, cy: d.y, rx: rx, ry: ry, angle: angle, feather: feather) }
            }
            handle(pt(cx + rx, cy), filled: false) { d in
                model.updateShape(commitNow: false) { $0 = .radial(cx: cx, cy: cy, rx: max(0.01, abs(d.x - cx)), ry: ry, angle: angle, feather: feather) }
            }
            handle(pt(cx, cy + ry), filled: false) { d in
                model.updateShape(commitNow: false) { $0 = .radial(cx: cx, cy: cy, rx: rx, ry: max(0.01, abs(d.y - cy)), angle: angle, feather: feather) }
            }
        default:
            EmptyView()
        }
    }

    private func handle(_ at: CGPoint, filled: Bool, onDrag: @escaping (CGPoint) -> Void) -> some View {
        GeometryReader { geo in
            Circle()
                .fill(filled ? Color.white : Color.black.opacity(0.4))
                .overlay(Circle().stroke(Color.white, lineWidth: 2))
                .frame(width: 14, height: 14)
                .position(at)
                .gesture(DragGesture(coordinateSpace: .named("maskCanvas"))
                    .onChanged { g in
                        let s = geo.size
                        onDrag(CGPoint(x: min(max(g.location.x / s.width, 0), 1), y: min(max(g.location.y / s.height, 0), 1)))
                    }
                    .onEnded { _ in model.commit(label: "Edit Mask") })
        }
        .coordinateSpace(name: "maskCanvas")
    }
}
