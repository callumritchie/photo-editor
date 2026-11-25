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
                NSApp.terminate(nil)
            }
        }
    }

    static func snapshot(to url: URL) {
        guard let view = NSApp.windows.first(where: { $0.isVisible && $0.contentView != nil })?.contentView,
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
                }
            }
        }
        .onChange(of: state.sidebar) { state.reload() }
        .onChange(of: state.minRating) { state.reload() }
        .sheet(isPresented: $state.showImport) { ImportSheet() }
        .overlay(alignment: .bottom) { StatusBar() }
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
        .task(id: item.id) { image = await state.thumbnail(item.id) }
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
    @FocusState private var focused: Bool
    @State private var image: NSImage?
    @State private var rendered = false

    var body: some View {
        let item = state.focusedItem
        ZStack(alignment: .bottom) {
            Color.black.opacity(0.92)
            Group {
                if let image {
                    Image(nsImage: image).resizable().scaledToFit().padding(16).padding(.bottom, 36)
                } else {
                    ProgressView()
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            if let item {
                HStack(spacing: 14) {
                    Text(item.name).bold()
                    Text(item.camera ?? "")
                    Text(item.lens ?? "").foregroundStyle(.secondary)
                    Text(item.exposureSummary)
                    Text(item.captureTime ?? "").foregroundStyle(.secondary)
                    Spacer()
                    Text(String(repeating: "★", count: item.rating)).foregroundStyle(.yellow)
                    if item.flag == "picked" { Image(systemName: "flag.fill") }
                    if item.flag == "rejected" { Image(systemName: "xmark.circle.fill").foregroundStyle(.red) }
                    if let l = item.label { Circle().fill(labelColor(l)).frame(width: 10, height: 10) }
                    Text(rendered ? "rendered" : "preview").font(.caption).foregroundStyle(.secondary)
                }
                .font(.callout)
                .foregroundStyle(.white)
                .padding(10)
                .background(.black.opacity(0.55))
            }
        }
        .focusable()
        .focused($focused)
        .focusEffectDisabled()
        .onAppear { focused = true }
        .onKeyPress(.escape) { state.mode = .grid; return .handled }
        .onKeyPress(.return) { state.mode = .grid; return .handled }
        .onKeyPress(.leftArrow) { state.move(by: -1); return .handled }
        .onKeyPress(.rightArrow) { state.move(by: 1); return .handled }
        .onKeyPress { press in state.handleKey(press.characters) ? .handled : .ignored }
        .onTapGesture(count: 2) { state.mode = .grid }
        .task(id: item?.id) { await load(item) }
    }

    private func load(_ item: ImageItem?) async {
        guard let item else { return }
        rendered = false
        image = await state.thumbnail(item.id)
        let (path, orientation) = (item.path, item.orientation)
        // 1. Embedded preview at screen size (fast).
        if let quick = await Task.detached(priority: .userInitiated, operation: {
            Core.quickImage(path: path, orientation: orientation, longEdge: 2560)
        }).value, !Task.isCancelled {
            image = quick
        }
        // 2. Full render with our pipeline (slower), for raw files.
        let isRaw = !["jpg", "jpeg"].contains((path as NSString).pathExtension.lowercased())
        guard isRaw else { return }
        if let full = await Task.detached(priority: .utility, operation: {
            Core.render(path: path, longEdge: 2560)
        }).value, !Task.isCancelled {
            image = full
            rendered = true
        }
    }
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
