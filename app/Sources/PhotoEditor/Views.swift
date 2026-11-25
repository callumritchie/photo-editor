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
        guard let view = (main?.attachedSheet ?? main)?.contentView,
              let rep = view.bitmapImageRepForCachingDisplay(in: view.bounds) else { return }
        view.cacheDisplay(in: view.bounds, to: rep)
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
                    Slider(value: $state.thumbSize, in: 100...360).frame(width: 120)
                    Button { state.showImport = true } label: { Label("Import", systemImage: "square.and.arrow.down") }
                    Button { state.showExport = true } label: { Label("Export", systemImage: "square.and.arrow.up") }
                        .disabled(state.exportTargets.isEmpty)
                }
            }
        }
        .onChange(of: state.sidebar) { state.reload() }
        .onChange(of: state.minRating) { state.reload() }
        .sheet(isPresented: $state.showImport) { ImportSheet() }
        .sheet(isPresented: $state.showExport) { ExportSheet() }
        .overlay(alignment: .bottom) { StatusBar() }
        .overlay(alignment: .bottomTrailing) { ExportStatus() }
        .alert("Error", isPresented: .constant(state.errorMessage != nil)) {
            Button("OK") { state.errorMessage = nil }
        } message: { Text(state.errorMessage ?? "") }
    }
}

struct SidebarView: View {
    @EnvironmentObject var state: AppState

    var body: some View {
        List(selection: $state.sidebar) {
            Section("Library") {
                Label("All Photographs", systemImage: "photo.on.rectangle").tag(SidebarItem.all)
            }
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
        .help("\(item.name)\n\(item.camera ?? "")  \(item.exposureSummary)")
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
                if let image = model.image ?? fallback {
                    Image(nsImage: image).resizable().scaledToFit().padding(16).padding(.bottom, 36)
                } else {
                    ProgressView()
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            HStack(spacing: 12) {
                Text(item.name).bold().lineLimit(1)
                Text([item.camera ?? "", item.exposureSummary].filter { !$0.isEmpty }.joined(separator: "  ·  "))
                    .lineLimit(1).truncationMode(.tail).foregroundStyle(.white.opacity(0.85))
                Spacer(minLength: 8)
                Text(String(repeating: "★", count: currentRating)).foregroundStyle(.yellow)
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
        .onKeyPress(.escape) { state.mode = .grid; return .handled }
        .onKeyPress(.leftArrow) { state.move(by: -1); return .handled }
        .onKeyPress(.rightArrow) { state.move(by: 1); return .handled }
        .onKeyPress { press in state.handleKey(press.characters) ? .handled : .ignored }
        .onTapGesture(count: 2) { state.mode = .grid }
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
        }
        .formStyle(.grouped)
        .frame(width: 520)
        .toolbar {
            ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
            ToolbarItem(placement: .confirmationAction) {
                Button("Import") {
                    state.runImport(sources: sources, copyTo: copy ? destination : nil, template: template,
                                    skipDuplicates: skipDuplicates)
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
