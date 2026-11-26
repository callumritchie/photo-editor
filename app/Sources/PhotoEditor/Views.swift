import AppKit
import SwiftUI

// MARK: - App

final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationDidFinishLaunching(_ notification: Notification) {
        // Needed when launched as a bare executable (e.g. `swift run`); harmless inside an .app bundle.
        NSApp.setActivationPolicy(.regular)
        NSApp.activate(ignoringOtherApps: true)

        // Development aid: PE_SNAPSHOT=/path/out.png renders the app's own window to a PNG after
        // PE_SNAPSHOT_DELAY seconds (default 6) and quits. Used for UI smoke tests; needs no permissions.
        let env = ProcessInfo.processInfo.environment
        if let out = env["PE_SNAPSHOT"] {
            let delay = Double(env["PE_SNAPSHOT_DELAY"] ?? "") ?? 6
            DispatchQueue.main.asyncAfter(deadline: .now() + delay) {
                Self.snapshot(to: URL(fileURLWithPath: out))
                exit(0) // terminate() is refused while a sheet is open
            }
        }
    }

    static func snapshot(to url: URL) {
        let main = NSApp.windows.first(where: { $0.isVisible && $0.contentView != nil && $0.sheetParent == nil })
        // Render the whole window's layer tree (includes toolbar, inspector and other layer-backed
        // content that cacheDisplay can miss); fall back to cacheDisplay.
        guard let window = main?.attachedSheet ?? main, let content = window.contentView else { return }
        let view = content.superview ?? content
        let scale = window.backingScaleFactor
        let size = view.bounds.size
        if let layer = view.layer,
           let ctx = CGContext(data: nil, width: Int(size.width * scale), height: Int(size.height * scale),
                               bitsPerComponent: 8, bytesPerRow: 0, space: CGColorSpace(name: CGColorSpace.sRGB)!,
                               bitmapInfo: CGImageAlphaInfo.premultipliedFirst.rawValue) {
            ctx.scaleBy(x: scale, y: scale)
            if !view.isFlipped {} else { ctx.translateBy(x: 0, y: size.height); ctx.scaleBy(x: 1, y: -1) }
            layer.render(in: ctx)
            if let cg = ctx.makeImage() {
                try? NSBitmapImageRep(cgImage: cg).representation(using: .png, properties: [:])?.write(to: url)
                return
            }
        }
        guard let rep = content.bitmapImageRepForCachingDisplay(in: content.bounds) else { return }
        content.cacheDisplay(in: content.bounds, to: rep)
        try? rep.representation(using: .png, properties: [:])?.write(to: url)
    }
    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { true }
}

@main
struct PhotoEditorApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) var delegate
    @StateObject private var state = AppState()

    var body: some Scene {
        WindowGroup("Photo Editor") {
            ContentView()
                .environmentObject(state)
                .frame(minWidth: 900, minHeight: 600)
        }
        .commands {
            CommandGroup(after: .newItem) {
                Button("Import Photos…") { state.showImport = true }
                    .keyboardShortcut("i", modifiers: [.command, .shift])
                Button("Build Previews") { state.buildPreviews() }
                    .help("Render loupe previews for the selected photos (or all shown) in the background")
                Button("Export…") { state.showExport = true }
                    .keyboardShortcut("e", modifiers: [.command, .shift])
                    .disabled(state.exportTargets.isEmpty)
            }
            CommandGroup(replacing: .undoRedo) {
                Button("Undo Edit") { state.undo() }.keyboardShortcut("z", modifiers: .command)
                Button("Redo Edit") { state.redo() }.keyboardShortcut("z", modifiers: [.command, .shift])
            }
            CommandMenu("Photo") {
                Button("Copy Settings") { state.copiedSettings = state.developSession()?.settings }
                    .keyboardShortcut("c", modifiers: [.command, .shift])
                Button("Paste Settings") { state.pasteSettings() }
                    .keyboardShortcut("v", modifiers: [.command, .shift])
                    .disabled(state.copiedSettings == nil)
                Divider()
                Button("Keywords…") { state.showKeywords.toggle() }
                    .keyboardShortcut("k", modifiers: .command)
                Button("Create Virtual Copy") { state.createVirtualCopies(state.exportTargets) }
                    .keyboardShortcut("'", modifiers: .command)
                    .disabled(state.exportTargets.isEmpty)
                Button("Group into Stack") { state.stack(state.exportTargets) }
                    .keyboardShortcut("g", modifiers: .command)
                    .disabled(state.exportTargets.count < 2)
                Button("Remove from Stack") { state.unstack(state.exportTargets) }
                    .keyboardShortcut("g", modifiers: [.command, .shift])
                    .disabled(state.exportTargets.isEmpty)
                Divider()
                Button("Remove from Library…") { state.confirmRemove = state.exportTargets }
                    .keyboardShortcut(.delete, modifiers: .command)
                    .disabled(state.exportTargets.isEmpty)
            }
        }
    }
}

// MARK: - Main layout

struct ContentView: View {
    @EnvironmentObject var state: AppState

    var body: some View {
        NavigationSplitView {
            SidebarView()
        } detail: {
            Group {
                if state.mode == .loupe, state.focusedItem != nil {
                    LoupeView()
                } else {
                    GridView()
                }
            }
            .toolbar {
                ToolbarItemGroup {
                    Picker("Rating", selection: $state.minRating) {
                        Text("All").tag(0)
                        ForEach(1...5, id: \.self) { Text(String(repeating: "★", count: $0) + "+").tag($0) }
                    }
                    .frame(width: 110)
                    Menu {
                        Picker("Flag", selection: $state.flagFilter) {
                            Text("Any flag").tag(Int?.none)
                            Text("Picked").tag(Int?.some(1))
                            Text("Unflagged").tag(Int?.some(0))
                            Text("Rejected").tag(Int?.some(-1))
                        }
                        Picker("Colour label", selection: $state.labelFilter) {
                            Text("Any label").tag(String?.none)
                            ForEach(["red", "yellow", "green", "blue", "purple"], id: \.self) { Text($0.capitalized).tag(String?.some($0)) }
                        }
                        Divider()
                        Button("Clear Filters") { state.flagFilter = nil; state.labelFilter = nil; state.minRating = 0; state.searchText = "" }
                    } label: {
                        Label("Filter", systemImage: state.flagFilter != nil || state.labelFilter != nil
                              ? "line.3.horizontal.decrease.circle.fill" : "line.3.horizontal.decrease.circle")
                    }
                    Menu {
                        Picker("Sort by", selection: $state.sort) {
                            Text("Capture Time").tag("captureTime")
                            Text("File Name").tag("fileName")
                            Text("Rating").tag("rating")
                            Text("Import Order").tag("imported")
                        }
                        Toggle("Descending", isOn: $state.descending)
                        Divider()
                        Toggle("Collapse Stacks", isOn: $state.collapseStacks)
                    } label: { Label("Sort", systemImage: "arrow.up.arrow.down") }
                    Slider(value: $state.thumbSize, in: 100...360).frame(width: 120)
                    Button { state.showKeywords.toggle() } label: { Label("Keywords", systemImage: "tag") }
                        .help("Keywords of the selected photos (⌘K)")
                    Button { state.showImport = true } label: { Label("Import", systemImage: "square.and.arrow.down") }
                    Button { state.showExport = true } label: { Label("Export", systemImage: "square.and.arrow.up") }
                        .disabled(state.exportTargets.isEmpty)
                }
            }
        }
        .onChange(of: state.sidebar) { state.reload() }
        .onChange(of: state.minRating) { state.reload() }
        .onChange(of: state.searchText) { state.reload() }
        .onChange(of: state.sort) { state.reload() }
        .onChange(of: state.descending) { state.reload() }
        .onChange(of: state.collapseStacks) { state.reload() }
        .onChange(of: state.flagFilter) { state.reload() }
        .onChange(of: state.labelFilter) { state.reload() }
        .searchable(text: $state.searchText, placement: .toolbar, prompt: "Search names, keywords, cameras")
        .inspector(isPresented: $state.showKeywords) { KeywordPanel().inspectorColumnWidth(min: 240, ideal: 280, max: 400) }
        .sheet(item: $state.collectionSheet) { CollectionSheetView(sheet: $0) }
        .sheet(item: $state.keywordRename) { k in RenameKeywordSheet(keyword: k) }
        .alert("Remove from Library?", isPresented: Binding(get: { state.confirmRemove != nil },
                                                           set: { if !$0 { state.confirmRemove = nil } })) {
            Button("Remove", role: .destructive) {
                if let ids = state.confirmRemove { state.removeFromLibrary(ids) }
                state.confirmRemove = nil
            }
            Button("Cancel", role: .cancel) { state.confirmRemove = nil }
        } message: {
            Text("\(state.confirmRemove?.count ?? 0) photo(s) will be removed from the library with their edits. The files on disk are not touched. Removing an original also removes its virtual copies.")
        }
        .sheet(isPresented: $state.showImport) { ImportSheet() }
        .sheet(isPresented: $state.showExport) { ExportSheet() }
        .overlay(alignment: .bottom) { StatusBar() }
        .overlay(alignment: .bottomTrailing) { ExportStatus() }
        .alert("Error", isPresented: .constant(state.errorMessage != nil)) {
            Button("OK") { state.errorMessage = nil }
        } message: { Text(state.errorMessage ?? "") }
        .alert("Presets Imported", isPresented: .constant(state.presetImportNote != nil)) {
            Button("OK") { state.presetImportNote = nil }
        } message: { Text(state.presetImportNote ?? "") }
    }
}

struct SidebarView: View {
    @EnvironmentObject var state: AppState

    var body: some View {
        List(selection: $state.sidebar) {
            Section("Library") {
                Label("All Photographs", systemImage: "photo.on.rectangle").tag(SidebarItem.all)
            }
            CollectionsSection()
            KeywordsSection()
            Section("Folders") {
                ForEach(state.folders) { f in
                    HStack {
                        Label(f.displayName, systemImage: "folder")
                        Spacer()
                        Text("\(f.imageCount)").foregroundStyle(.secondary).monospacedDigit()
                    }
                    .help(f.path)
                    .tag(SidebarItem.folder(f.id))
                }
            }
        }
        .listStyle(.sidebar)
        .frame(minWidth: 220)
    }
}

struct StatusBar: View {
    @EnvironmentObject var state: AppState

    var body: some View {
        VStack(spacing: 0) {
            importStatus
            if state.pendingRenders > 0 && state.importProgress == nil {
                HStack(spacing: 8) {
                    ProgressView().controlSize(.small)
                    Text("Rendering previews · \(state.pendingRenders) left").font(.callout)
                }
                .padding(8)
                .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 8))
                .padding(.bottom, 12)
            }
        }
    }

    @ViewBuilder private var importStatus: some View {
        if let p = state.importProgress {
            HStack {
                ProgressView(value: Double(p.done), total: Double(max(p.total, 1))).frame(width: 200)
                Text(p.total == 0 ? "Scanning…" : "Importing \(p.done) of \(p.total)")
            }
            .padding(8)
            .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 8))
            .padding()
        } else if let r = state.lastReport {
            Text("Imported \(r.imported) · duplicates skipped \(r.duplicates) · already in library \(r.alreadyInCatalog)"
                 + (r.failed.isEmpty ? "" : " · failed \(r.failed.count)"))
                .padding(8)
                .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 8))
                .padding()
                .onTapGesture { state.lastReport = nil }
                .task { try? await Task.sleep(for: .seconds(6)); state.lastReport = nil }
        }
    }
}

// MARK: - Grid

struct GridView: View {
    @EnvironmentObject var state: AppState
    @FocusState private var focused: Bool

    var body: some View {
        ScrollViewReader { proxy in
            ScrollView {
                LazyVGrid(columns: [GridItem(.adaptive(minimum: state.thumbSize, maximum: state.thumbSize * 1.4), spacing: 10)],
                          spacing: 10) {
                    ForEach(state.images) { item in
                        ThumbCell(item: item, selected: state.selection.contains(item.id) || state.focused == item.id)
                            .id(item.id)
                            .onTapGesture(count: 2) { state.select(item.id); state.mode = .loupe }
                            .onTapGesture {
                                let mods = NSEvent.modifierFlags
                                state.select(item.id, extend: mods.contains(.shift), toggle: mods.contains(.command))
                            }
                    }
                }
                .padding(12)
            }
            .overlay {
                if state.images.isEmpty && state.importProgress == nil {
                    ContentUnavailableView {
                        Label("No Photos", systemImage: "photo.stack")
                    } description: {
                        Text("Import a folder of photos to get started.")
                    } actions: {
                        Button("Import Photos…") { state.showImport = true }
                    }
                }
            }
            .focusable()
            .focused($focused)
            .focusEffectDisabled()
            .onAppear { focused = true }
            .onKeyPress(.leftArrow) { state.move(by: -1); return .handled }
            .onKeyPress(.rightArrow) { state.move(by: 1); return .handled }
            .onKeyPress(.upArrow) { state.move(by: -columns); return .handled }
            .onKeyPress(.downArrow) { state.move(by: columns); return .handled }
            .onKeyPress(.return) { if state.focused != nil { state.mode = .loupe }; return .handled }
            .onKeyPress(.space) { if state.focused != nil { state.mode = .loupe }; return .handled }
            .onKeyPress { press in state.handleKey(press.characters) ? .handled : .ignored }
            .onChange(of: state.focused) { _, id in
                if let id { withAnimation(.easeOut(duration: 0.15)) { proxy.scrollTo(id) } }
            }
        }
    }

    /// Approximate column count for up/down navigation.
    private var columns: Int {
        let width = (NSApp.keyWindow?.contentView?.bounds.width ?? 1200) - 260
        return max(1, Int(width / (state.thumbSize + 10)))
    }
}

struct ThumbCell: View {
    @EnvironmentObject var state: AppState
    let item: ImageItem
    let selected: Bool
    @State private var image: NSImage?

    var body: some View {
        VStack(spacing: 4) {
            ZStack {
                RoundedRectangle(cornerRadius: 6).fill(Color(nsColor: .controlBackgroundColor))
                if let image {
                    Image(nsImage: image).resizable().scaledToFit().padding(6)
                        .opacity(item.flag == "rejected" ? 0.35 : 1)
                } else {
                    ProgressView().controlSize(.small)
                }
            }
            .frame(height: state.thumbSize * 0.78)
            .overlay(alignment: .topLeading) {
                if item.flag == "picked" {
                    Image(systemName: "flag.fill").foregroundStyle(.white).padding(6)
                } else if item.flag == "rejected" {
                    Image(systemName: "xmark.circle.fill").foregroundStyle(.red).padding(6)
                }
            }
            HStack(spacing: 4) {
                Text(String(repeating: "★", count: item.rating) + String(repeating: "·", count: 5 - item.rating))
                    .font(.caption).foregroundStyle(item.rating > 0 ? .yellow : .secondary)
                Spacer()
                if let l = item.label { Circle().fill(labelColor(l)).frame(width: 9, height: 9) }
            }
            .padding(.horizontal, 4)
        }
        .padding(4)
        .background(RoundedRectangle(cornerRadius: 8).fill(selected ? Color.accentColor.opacity(0.35) : .clear))
        .overlay(alignment: .topTrailing) { ThumbBadges(item: item) }
        .contextMenu { PhotoContextMenu(item: item) }
        .draggable(String(item.id))
        .help("\(item.displayName)\n\(item.camera ?? "")  \(item.exposureSummary)")
        .task(id: "\(item.id)-\(state.thumbVersion[item.id, default: 0])") { image = await state.thumbnail(item.id) }
    }
}

func labelColor(_ l: String) -> Color {
    switch l {
    case "red": return .red
    case "yellow": return .yellow
    case "green": return .green
    case "blue": return .blue
    default: return .purple
    }
}

// MARK: - Loupe

struct LoupeView: View {
    @EnvironmentObject var state: AppState

    var body: some View {
        if let model = state.developSession() {
            HStack(spacing: 0) {
                LoupeImage(model: model)
                Divider()
                DevelopPanel(model: model)
            }
            .id(model.item.id)
        }
    }
}

struct LoupeImage: View {
    @EnvironmentObject var state: AppState
    @ObservedObject var model: DevelopModel
    @FocusState private var focused: Bool
    @State private var fallback: NSImage?

    var body: some View {
        let item = model.item
        ZStack(alignment: .bottom) {
            Color.black.opacity(0.92)
            Group {
                if model.zoomed && !model.cropping && !model.maskEditing && model.image != nil {
                    ZoomView(model: model).padding(.bottom, 36)
                } else if let image = model.image ?? fallback {
                    Image(nsImage: image).resizable().scaledToFit()
                        .overlay {
                            if model.maskEditing && !model.cropping && model.image != nil {
                                MaskCanvas(model: model)
                            }
                            if !model.maskEditing && !model.cropping && model.image != nil {
                                // Click to zoom to 1:1 at that point.
                                GeometryReader { g in
                                    Color.clear.contentShape(Rectangle())
                                        .onTapGesture { p in
                                            model.setZoom(true, at: CGPoint(x: p.x / g.size.width, y: p.y / g.size.height))
                                        }
                                }
                            }
                            if model.cropping && model.image != nil {
                                CropOverlay(
                                    rect: Binding(get: { model.settings.crop },
                                                  set: { model.settings.crop = $0 }),
                                    aspect: model.cropAspect.ratio.map { $0 == 0 ? model.frameAspect : $0 },
                                    frameAspect: model.frameAspect,
                                    onCommit: { model.commit(label: "Crop") })
                            }
                        }
                        .padding(16).padding(.bottom, 36)
                } else {
                    ProgressView()
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            HStack(spacing: 12) {
                Text(item.displayName).bold().lineLimit(1)
                Text([item.camera ?? "", item.exposureSummary].filter { !$0.isEmpty }.joined(separator: "  ·  "))
                    .lineLimit(1).truncationMode(.tail).foregroundStyle(.white.opacity(0.85))
                Spacer(minLength: 8)
                Text(String(repeating: "★", count: currentRating)).foregroundStyle(.yellow)
                if model.zoomed { Text("1:1").font(.caption.bold()).foregroundStyle(.yellow) }
                if let ms = model.lastRenderMs {
                    Text("\(ms) ms · \(Core.rendererName == "CPU" ? "CPU" : "GPU")")
                        .font(.caption).foregroundStyle(.secondary)
                        .help("Preview renderer: \(Core.rendererName)")
                }
                if model.loading { ProgressView().controlSize(.small) }
            }
            .font(.callout)
            .foregroundStyle(.white)
            .padding(10)
            .background(.black.opacity(0.55))
        }
        .focusable()
        .focused($focused)
        .focusEffectDisabled()
        .onAppear { focused = true }
        .onKeyPress(.escape) {
            if model.zoomed { model.setZoom(false) }
            else if model.cropping { model.cropping = false }
            else if model.maskEditing { model.selectedMask = nil; model.pickingColor = false }
            else { state.mode = .grid }
            return .handled
        }
        .onKeyPress(.return) {
            if model.cropping { model.cropping = false; return .handled }
            return .ignored
        }
        .onKeyPress("r") { model.cropping.toggle(); return .handled }
        .onKeyPress("z") { model.setZoom(!model.zoomed); return .handled }
        .onKeyPress(.leftArrow) { state.move(by: -1); return .handled }
        .onKeyPress(.rightArrow) { state.move(by: 1); return .handled }
        .onKeyPress { press in state.handleKey(press.characters) ? .handled : .ignored }
        .task(id: item.id) {
            fallback = nil
            if let cached = await state.cachedPreview(item.id) { fallback = cached }
            else { fallback = await state.thumbnail(item.id) }
            state.prefetchAround(item.id)
            if !model.isRaw {
                let (path, o) = (item.path, item.orientation)
                fallback = await Task.detached { Core.quickImage(path: path, orientation: o, longEdge: 2560) }.value ?? fallback
            }
            await model.load()
        }
    }

    private var currentRating: Int { state.images.first { $0.id == model.item.id }?.rating ?? model.item.rating }
}

// MARK: - Import

struct ImportSheet: View {
    @EnvironmentObject var state: AppState
    @Environment(\.dismiss) private var dismiss
    @State private var sources: [URL] = []
    @State private var copy = false
    @State private var destination: URL?
    @State private var template = "%Y/%Y-%m-%d"
    @State private var skipDuplicates = true
    @State private var developPreset: Int64?

    var body: some View {
        Form {
            Section("Source") {
                HStack {
                    Text(sources.isEmpty ? "No folders chosen" : sources.map(\.lastPathComponent).joined(separator: ", "))
                        .foregroundStyle(sources.isEmpty ? .secondary : .primary)
                        .lineLimit(2)
                    Spacer()
                    Button("Choose…") { sources = choose(directories: true, multiple: true) }
                }
            }
            Section("Files") {
                Picker("Action", selection: $copy) {
                    Text("Add (keep files where they are)").tag(false)
                    Text("Copy to a new location").tag(true)
                }
                .pickerStyle(.radioGroup)
                if copy {
                    HStack {
                        Text(destination?.path ?? "No destination chosen").foregroundStyle(destination == nil ? .secondary : .primary)
                        Spacer()
                        Button("Choose…") { destination = choose(directories: true, multiple: false).first }
                    }
                    TextField("Folder pattern", text: $template)
                    Text("Tokens: %Y year, %m month, %d day (from capture date)").font(.caption).foregroundStyle(.secondary)
                }
                Toggle("Don't import suspected duplicates", isOn: $skipDuplicates)
            }
            Section("Develop") {
                Picker("Apply preset", selection: $developPreset) {
                    Text("None").tag(Int64?.none)
                    ForEach(state.presets) { Text($0.name).tag(Int64?.some($0.id)) }
                }
            }
        }
        .formStyle(.grouped)
        .frame(width: 520)
        .toolbar {
            ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
            ToolbarItem(placement: .confirmationAction) {
                Button("Import") {
                    state.runImport(sources: sources, copyTo: copy ? destination : nil, template: template,
                                    skipDuplicates: skipDuplicates, developPreset: developPreset)
                    dismiss()
                }
                .disabled(sources.isEmpty || (copy && destination == nil))
            }
        }
    }

    private func choose(directories: Bool, multiple: Bool) -> [URL] {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = directories
        panel.canChooseFiles = !directories
        panel.allowsMultipleSelection = multiple
        return panel.runModal() == .OK ? panel.urls : []
    }
}

struct RenameKeywordSheet: View {
    @EnvironmentObject var state: AppState
    @Environment(\.dismiss) private var dismiss
    let keyword: KeywordItem
    @State private var name = ""

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Rename Keyword").font(.headline)
            TextField("Name", text: $name).frame(width: 280)
            HStack {
                Spacer()
                Button("Cancel") { dismiss() }
                Button("Rename") { state.renameKeyword(keyword.id, to: name); dismiss() }
                    .keyboardShortcut(.defaultAction)
                    .disabled(name.trimmingCharacters(in: .whitespaces).isEmpty)
            }
        }
        .padding(20)
        .onAppear { name = keyword.name }
    }
}
