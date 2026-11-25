import AppKit
import SwiftUI

struct ExportSheet: View {
    @EnvironmentObject var state: AppState
    @Environment(\.dismiss) private var dismiss
    @State private var o = ExportOptions()
    @State private var presetName = ""
    @State private var showSavePreset = false

    private var ids: [Int64] { state.exportTargets }

    var body: some View {
        Form {
            Section {
                HStack {
                    Menu("Presets") {
                        if state.exportPresets.isEmpty { Text("No saved presets") }
                        ForEach(state.exportPresets) { p in
                            Button(p.name) { o = p.settings }
                        }
                        if !state.exportPresets.isEmpty {
                            Divider()
                            Menu("Delete") {
                                ForEach(state.exportPresets) { p in
                                    Button(p.name) { state.deleteExportPreset(p.id) }
                                }
                            }
                        }
                    }
                    .fixedSize()
                    Button("Save as Preset…") { showSavePreset = true }
                    Spacer()
                    Text("\(ids.count) photo\(ids.count == 1 ? "" : "s")").foregroundStyle(.secondary)
                }
            }
            Section("Location") {
                HStack {
                    Text(o.destination).lineLimit(1).truncationMode(.head)
                    Spacer()
                    Button("Choose…") { if let u = chooseFolder() { o.destination = u.path } }
                }
                TextField("Subfolder (optional)", text: $o.subfolder)
            }
            Section("File naming") {
                TextField("Name", text: $o.fileNaming)
                Text("Tokens: {name} {seq} {seq:3} {date} {year} {month} {day} {time} {camera}")
                    .font(.caption).foregroundStyle(.secondary)
                Stepper("Start numbering at \(o.startNumber)", value: $o.startNumber, in: 0...999_999)
                Picker("If a file exists", selection: $o.collision) {
                    Text("Add a number").tag("rename")
                    Text("Overwrite").tag("overwrite")
                    Text("Skip").tag("skip")
                }
                LabeledContent("Example", value: o.sampleName(for: state.images.first { $0.id == ids.first }))
            }
            Section("File settings") {
                Picker("Format", selection: $o.format) {
                    ForEach(ExportFormat.allCases) { Text($0.label).tag($0) }
                }
                if o.format == .jpeg {
                    LabeledContent("Quality \(o.quality)") {
                        Slider(value: Binding(get: { Double(o.quality) }, set: { o.quality = Int($0) }), in: 10...100)
                    }
                }
                if o.format == .png || o.format == .tiff {
                    Picker("Bit depth", selection: $o.sixteenBit) {
                        Text("8 bits/channel").tag(false)
                        Text("16 bits/channel").tag(true)
                    }
                }
                if o.format == .tiff {
                    Picker("Compression", selection: $o.tiffCompression) {
                        Text("None").tag("none")
                        Text("LZW").tag("lzw")
                        Text("Deflate").tag("deflate")
                    }
                }
                if o.format != .original {
                    Picker("Colour space", selection: $o.colorSpace) {
                        Text("sRGB").tag("srgb")
                        Text("Display P3").tag("displayP3")
                        Text("Adobe RGB (1998) compatible").tag("adobeRgb")
                        Text("ProPhoto RGB").tag("proPhoto")
                    }
                    if o.format == .jpeg && o.colorSpace == "proPhoto" {
                        Text("ProPhoto in 8 bits can show banding; consider 16-bit TIFF or PNG.")
                            .font(.caption).foregroundStyle(.orange)
                    }
                }
            }
            if o.format != .original {
                Section("Image sizing") {
                    Picker("Resize", selection: $o.resizeMode) {
                        ForEach(ResizeMode.allCases) { Text($0.label).tag($0) }
                    }
                    switch o.resizeMode {
                    case .original: EmptyView()
                    case .longEdge, .shortEdge, .width, .height:
                        TextField("Pixels", value: $o.pixels, format: .number)
                    case .fit:
                        TextField("Width (px)", value: $o.width, format: .number)
                        TextField("Height (px)", value: $o.height, format: .number)
                    case .megapixels:
                        TextField("Megapixels", value: $o.megapixels, format: .number)
                    case .percent:
                        TextField("Percent", value: $o.percent, format: .number)
                    }
                    if o.resizeMode != .original { Toggle("Don't enlarge", isOn: $o.dontEnlarge) }
                    TextField("Resolution (pixels per inch)", value: $o.ppi, format: .number)
                }
                Section("Output sharpening") {
                    Toggle("Sharpen for", isOn: $o.sharpen)
                    if o.sharpen {
                        Picker("Medium", selection: $o.sharpenMedium) {
                            Text("Screen").tag("screen")
                            Text("Matte paper").tag("matte")
                            Text("Glossy paper").tag("glossy")
                        }
                        Picker("Amount", selection: $o.sharpenAmount) {
                            Text("Low").tag("low")
                            Text("Standard").tag("standard")
                            Text("High").tag("high")
                        }
                        .pickerStyle(.segmented)
                    }
                }
                Section("Metadata") {
                    Picker("Include", selection: $o.metadata) {
                        Text("All (camera, exposure, date)").tag("all")
                        Text("Copyright only").tag("copyrightOnly")
                        Text("None").tag("none")
                    }
                    if o.metadata != "none" {
                        TextField("Artist", text: $o.artist)
                        TextField("Copyright", text: $o.copyright)
                    }
                }
            }
        }
        .formStyle(.grouped)
        .frame(width: 560, height: 640)
        .onAppear { o = state.lastExportOptions }
        .toolbar {
            ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
            ToolbarItem(placement: .confirmationAction) {
                Button(ids.count == 1 ? "Export" : "Export \(ids.count) Photos") {
                    state.runExport(ids: ids, options: o)
                    dismiss()
                }
                .disabled(ids.isEmpty || o.destination.isEmpty)
            }
        }
        .sheet(isPresented: $showSavePreset) {
            VStack(alignment: .leading, spacing: 12) {
                Text("Save Export Preset").font(.headline)
                TextField("Preset name", text: $presetName).frame(width: 280)
                HStack {
                    Spacer()
                    Button("Cancel") { showSavePreset = false }
                    Button("Save") {
                        state.saveExportPreset(name: presetName, options: o)
                        presetName = ""
                        showSavePreset = false
                    }
                    .keyboardShortcut(.defaultAction)
                    .disabled(presetName.trimmingCharacters(in: .whitespaces).isEmpty)
                }
            }
            .padding(20)
        }
    }

    private func chooseFolder() -> URL? {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.canCreateDirectories = true
        panel.prompt = "Choose"
        return panel.runModal() == .OK ? panel.url : nil
    }
}

struct ExportStatus: View {
    @EnvironmentObject var state: AppState

    var body: some View {
        if let p = state.exportProgress {
            HStack {
                ProgressView(value: Double(p.done), total: Double(max(p.total, 1))).frame(width: 200)
                Text("Exporting \(min(p.done + 1, p.total)) of \(p.total)")
            }
            .padding(8)
            .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 8))
            .padding()
        } else if let r = state.lastExport {
            HStack(spacing: 12) {
                Text("Exported \(r.written.count)"
                     + (r.skipped.isEmpty ? "" : " · skipped \(r.skipped.count)")
                     + (r.failed.isEmpty ? "" : " · failed \(r.failed.count)"))
                    .help(r.failed.map { $0.joined(separator: ": ") }.joined(separator: "\n"))
                if !r.written.isEmpty {
                    Button("Show in Finder") {
                        NSWorkspace.shared.activateFileViewerSelecting(r.written.map { URL(fileURLWithPath: $0) })
                    }
                }
                Button { state.lastExport = nil } label: { Image(systemName: "xmark") }.buttonStyle(.plain)
            }
            .padding(8)
            .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 8))
            .padding()
        }
    }
}
